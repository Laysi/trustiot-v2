//! The protections before `/mqtt` is exposed (Laysi/trustiot-v2#600; README.md, "連線上限" and
//! "日誌"): connections whose inner connection makes no progress, or whose gateway stopped
//! answering pings, are ended and free their slot; one source cannot take more than its share of
//! the slots, whatever forwarding headers it forges; and the WebSocket library's TRACE logging,
//! which writes frame payloads, stays off whatever `telemetry.log_filter` says.
//!
//! Times are scaled down and set by each test; the assertions are about order (a connection that
//! started earlier is still open after a later one was ended), never about elapsed time, except
//! that nothing ends before the deadline the test configured.

mod support;

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use support::{
    Behaviour, Bridge, FakeBroker, connect_ws, mqtt_session, read_bytes, refused_status,
    send_binary, via_cloudflare, within,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Reads until the bridge's close frame; returns its code. Panics on data or on an end without
/// a close.
async fn expect_close(websocket: &mut WebSocketStream<TcpStream>) -> CloseCode {
    loop {
        match within("the bridge's close", websocket.next()).await {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(Some(frame)))) => return frame.code,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Sends `payload` and reads it back from the echoing broker: one turn each way.
async fn echo_turn(websocket: &mut WebSocketStream<TcpStream>, payload: &[u8]) {
    send_binary(websocket, payload).await;
    let (answer, _) = read_bytes(websocket, payload.len()).await;
    assert_eq!(answer, payload, "the echo");
}

fn ended_reason(line: &serde_json::Value) -> &str {
    line["fields"]["reason"].as_str().unwrap_or_default()
}

fn duration_ms(line: &serde_json::Value) -> u64 {
    line["fields"]["duration_ms"].as_u64().unwrap_or_default()
}

/// A broker that never answers is Mosquitto waiting for a ClientHello that never comes. The
/// connection is ended with close 1008 once the deadline passed, and its slot is free again.
#[tokio::test]
async fn a_connection_whose_broker_never_answers_is_ended_and_frees_its_slot() {
    const DEADLINE: Duration = Duration::from_millis(500);
    let broker = FakeBroker::start(Behaviour::Silent).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "500ms"),
            ("TRUSTIOT__MAX_CONNECTIONS", "1"),
        ],
    )
    .await;
    let mut holder = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    send_binary(&mut holder, b"\x16\x03\x01 not quite a ClientHello").await;
    // The only slot is taken while the connection waits.
    let status = refused_status(
        connect_ws(
            bridge.address,
            &bridge.url(),
            &via_cloudflare("203.0.113.8"),
        )
        .await,
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(expect_close(&mut holder).await, CloseCode::Policy);
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended_reason(&ended), "handshake_timeout", "{ended}");
    assert!(
        duration_ms(&ended) >= u64::try_from(DEADLINE.as_millis()).expect("small"),
        "ended before the configured deadline: {ended}"
    );

    // The slot is free: the next connection is relayed.
    let _next = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.8"),
    )
    .await
    .expect("the freed slot");
    bridge
        .wait_for_lines("relaying a gateway connection", 2)
        .await;
}

/// A client that sends a ClientHello, gets the broker's first flight and stops is ended; a
/// gateway that answered the broker (one more turn each way) is not, although it started earlier
/// and its deadline passed first.
#[tokio::test]
async fn progress_is_the_broker_answering_the_gateways_answer() {
    // Long enough for the control's two round trips on a loaded machine.
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "5s")],
    )
    .await;
    let mut gateway = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the gateway's WebSocket");
    echo_turn(&mut gateway, b"client hello").await;
    echo_turn(&mut gateway, b"certificate, finished").await;

    let mut stalled = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.8"),
    )
    .await
    .expect("open the stalled WebSocket");
    echo_turn(&mut stalled, b"client hello").await;
    assert_eq!(expect_close(&mut stalled).await, CloseCode::Policy);
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended_reason(&ended), "handshake_timeout", "{ended}");
    assert_eq!(ended["fields"]["client.address"], "203.0.113.8", "{ended}");
    assert!(duration_ms(&ended) >= 5000, "{ended}");

    // The gateway's deadline passed before the stalled one's; it is still relayed.
    echo_turn(&mut gateway, b"PUBLISH").await;
    assert_eq!(
        bridge.messages("gateway connection ended").len(),
        1,
        "{}",
        bridge.dump()
    );
}

/// A gateway that stops reading answers no ping and is ended once the pong deadline passed; one
/// that keeps reading (tungstenite answers each ping) stays, although it started earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_that_answers_no_ping_is_ended() {
    const PONG: Duration = Duration::from_secs(3);
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "300ms"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "3s"),
        ],
    )
    .await;

    // The answering gateway reads all the time, on its own task, until told to stop.
    let answering = mqtt_session(bridge.address, &bridge.url(), "203.0.113.7").await;
    let (stop, stopped) = oneshot::channel::<()>();
    let reader = tokio::spawn(async move {
        let mut websocket = answering;
        let mut pings = 0u32;
        tokio::pin!(stopped);
        loop {
            tokio::select! {
                _ = &mut stopped => return (websocket, pings),
                message = websocket.next() => match message {
                    Some(Ok(Message::Ping(_))) => pings += 1,
                    other => panic!("the answering gateway got {other:?}"),
                },
            }
        }
    });

    // The silent gateway: an MQTT session, then it never reads again, so it sends no pong.
    let mut silent = mqtt_session(bridge.address, &bridge.url(), "203.0.113.8").await;
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended_reason(&ended), "pong_timeout", "{ended}");
    assert_eq!(ended["fields"]["client.address"], "203.0.113.8", "{ended}");
    assert!(
        duration_ms(&ended) >= u64::try_from(PONG.as_millis()).expect("small"),
        "ended before the configured pong deadline: {ended}"
    );

    // The answering gateway, older than the silent one, is still relayed.
    stop.send(()).expect("stop the reader");
    let (mut answering, pings) = within("the reader", reader).await.expect("the reader");
    assert!(pings > 0, "the bridge pinged the answering gateway");
    send_binary(&mut answering, &support::PINGREQ).await;
    let (answer, _) = read_bytes(&mut answering, support::PINGRESP.len()).await;
    assert_eq!(answer, support::PINGRESP);
    assert_eq!(bridge.messages("gateway connection ended").len(), 1);

    // The silent gateway got no close: the bridge just dropped the connection.
    loop {
        match within("the end of the silent connection", silent.next()).await {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => panic!("unexpected close {frame:?}"),
            Some(Ok(other)) => panic!("unexpected {other:?}"),
            None | Some(Err(_)) => break,
        }
    }
}

/// A gateway that sends without reading fills the TCP buffers until the bridge's writes to it
/// wait; the pong deadline still ends it and frees the slot. (While the bridge waits to write
/// broker data, it cannot send the next ping: the deadline must not depend on that write.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_that_stops_reading_while_it_sends_is_ended() {
    const PONG: Duration = Duration::from_secs(3);
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "300ms"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "3s"),
            ("TRUSTIOT__MAX_CONNECTIONS", "1"),
        ],
    )
    .await;
    let mut flooding = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.8"),
    )
    .await
    .expect("open the flooding WebSocket");
    // Progress first, so that only the pong deadline can end it.
    echo_turn(&mut flooding, b"client hello").await;
    echo_turn(&mut flooding, b"certificate, finished").await;
    // From here on it only writes, until its own writes wait or the bridge drops it.
    let writer = tokio::spawn(async move {
        let chunk = support::noise(600, 8 * 1024);
        let mut sent = 0usize;
        while flooding
            .send(Message::Binary(chunk.clone().into()))
            .await
            .is_ok()
        {
            sent += 1;
        }
        sent
    });

    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended_reason(&ended), "pong_timeout", "{ended}");
    assert_eq!(ended["fields"]["client.address"], "203.0.113.8", "{ended}");
    assert!(
        duration_ms(&ended) >= u64::try_from(PONG.as_millis()).expect("small"),
        "ended before the configured pong deadline: {ended}"
    );
    let sent = within("the flooding writer", writer)
        .await
        .expect("the writer");
    assert!(sent > 0, "the gateway sent before it was ended");

    // The only slot is free again.
    let mut next = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.9"),
    )
    .await
    .expect("the freed slot");
    echo_turn(&mut next, b"client hello").await;
}

/// One source gets `max_connections_per_ip` slots and no more; forged forwarding headers do not
/// move a connection to another source, other sources are not affected, IPv6 counts by /64, and
/// a freed slot can be taken again.
#[tokio::test]
async fn one_source_cannot_take_more_than_its_share() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__MAX_CONNECTIONS", "16"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "2"),
        ],
    )
    .await;
    let forged = |claimed: &'static str| {
        vec![
            ("cf-connecting-ip", "203.0.113.7"),
            ("x-forwarded-for", claimed),
            ("true-client-ip", claimed),
            ("x-real-ip", claimed),
        ]
    };
    let (address, url) = (bridge.address, bridge.url());
    let open = |headers: Vec<(&'static str, &'static str)>| {
        let url = url.clone();
        async move { connect_ws(address, &url, &headers).await }
    };

    let mut held = vec![
        open(forged("198.51.100.1")).await.expect("first"),
        open(forged("198.51.100.2")).await.expect("second"),
    ];
    // Each claims another address; the trusted header says it is the same source.
    for claimed in ["198.51.100.3", "198.51.100.4"] {
        let status = refused_status(open(forged(claimed)).await);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{claimed}");
    }
    // Other sources still get in.
    held.push(
        open(via_cloudflare("203.0.113.8"))
            .await
            .expect("another IPv4"),
    );
    held.push(open(via_cloudflare("2001:db8:1:2::1")).await.expect("IPv6"));
    held.push(
        open(via_cloudflare("2001:db8:1:2::2"))
            .await
            .expect("IPv6, same /64"),
    );
    let status = refused_status(open(via_cloudflare("2001:db8:1:2:ffff::3")).await);
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a third address of the /64"
    );
    held.push(
        open(via_cloudflare("2001:db8:1:3::1"))
            .await
            .expect("another /64"),
    );

    let refused = bridge
        .wait_for_lines("refused a gateway WebSocket", 3)
        .await;
    for line in &refused {
        assert_eq!(line["fields"]["reason"], "over_source_limit", "{line}");
        assert_eq!(line["level"], "WARN", "{line}");
    }
    assert_eq!(refused[0]["fields"]["client.address"], "203.0.113.7");
    assert_eq!(refused[1]["fields"]["client.address"], "203.0.113.7");
    assert_eq!(
        refused[2]["fields"]["client.address"],
        "2001:db8:1:2:ffff::3"
    );
    // The forged addresses never reach the log, and refused upgrades never reach the broker.
    assert!(!bridge.dump().contains("198.51.100."), "{}", bridge.dump());
    assert_eq!(broker.connections().len(), held.len());

    // A freed slot of the source can be taken again.
    let mut first = held.remove(0);
    within("the close", first.close(None)).await.expect("close");
    bridge.wait_for_line("gateway connection ended").await;
    held.push(
        open(forged("198.51.100.5"))
            .await
            .expect("the source's freed slot"),
    );
}

/// `tungstenite=trace` in the log filter reaches the library (its DEBUG lines appear) but not its
/// TRACE lines, which would carry every frame's payload: neither the payload nor its hex form is
/// in any log line.
#[tokio::test]
async fn tungstenite_trace_logging_cannot_be_turned_on() {
    const CANARY: &[u8] = b"inner-tls-canary-5f2c9a";
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(
        broker.address,
        &[(
            "TRUSTIOT__TELEMETRY__LOG_FILTER",
            "info,tungstenite=trace,tokio_tungstenite=trace",
        )],
    )
    .await;
    let mut websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    echo_turn(&mut websocket, CANARY).await;
    within("the close", websocket.close(None))
        .await
        .expect("close");
    bridge.wait_for_line("gateway connection ended").await;

    let lines = bridge.lines();
    let library = |line: &serde_json::Value| {
        line["target"].as_str().is_some_and(|target| {
            target.starts_with("tungstenite") || target.starts_with("tokio_tungstenite")
        })
    };
    assert!(
        lines
            .iter()
            .any(|line| library(line) && line["level"] == "DEBUG"),
        "the filter reaches tungstenite (the close it received is logged at DEBUG):\n{}",
        bridge.dump()
    );
    assert!(
        !lines
            .iter()
            .any(|line| library(line) && line["level"] == "TRACE"),
        "{}",
        bridge.dump()
    );
    let dump = bridge.dump();
    let hex: String = CANARY.iter().map(|byte| format!("{byte:02x}")).collect();
    let text = std::str::from_utf8(CANARY).expect("ASCII");
    assert!(!dump.contains(text), "the payload is logged:\n{dump}");
    assert!(!dump.contains(&hex), "the payload's hex is logged:\n{dump}");
}
