//! Refusals before the upgrade and the bridge's metrics: a full bridge answers 503 and frees the
//! slot when a connection ends; an unreachable broker answers 502 without upgrading; an upgrade
//! without the `mqtt` subprotocol is refused; the connection and traffic metrics reach the
//! collector, with no connection left active after shutdown; frames and messages beyond the
//! gateway's 32 KiB end the connection.

mod support;

use futures_util::StreamExt as _;
use support::{
    Behaviour, Bridge, FakeBroker, FakeCollector, connect_ws, connect_ws_with, mqtt_session,
    refused_status, send_binary, via_cloudflare, wait_until, within,
};
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;

#[tokio::test]
async fn a_full_bridge_refuses_with_503_and_frees_the_slot_when_a_connection_ends() {
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    let mut bridge = Bridge::start(broker.address, &[("TRUSTIOT__MAX_CONNECTIONS", "1")]).await;
    let mut first = mqtt_session(bridge.address, &bridge.url(), "203.0.113.7").await;
    let status = refused_status(
        connect_ws(
            bridge.address,
            &bridge.url(),
            &via_cloudflare("203.0.113.8"),
        )
        .await,
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let refused = bridge.wait_for_line("refused a gateway WebSocket").await;
    assert_eq!(refused["fields"]["reason"], "over_capacity", "{refused}");
    assert_eq!(
        broker.connections().len(),
        1,
        "the refused upgrade never reached the broker"
    );

    within("the close", first.close(None)).await.expect("close");
    bridge.wait_for_line("gateway connection ended").await;
    // The slot is free once the relay has ended.
    mqtt_session(bridge.address, &bridge.url(), "203.0.113.8").await;
}

#[tokio::test]
async fn an_unreachable_broker_answers_502_without_upgrading() {
    // A port nothing listens on any more.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("address")
    };
    let mut bridge = Bridge::start(closed, &[]).await;
    let status = refused_status(
        connect_ws(
            bridge.address,
            &bridge.url(),
            &via_cloudflare("203.0.113.7"),
        )
        .await,
    );
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let refused = bridge.wait_for_line("refused a gateway WebSocket").await;
    assert_eq!(
        refused["fields"]["reason"], "broker_unavailable",
        "{refused}"
    );
    assert_eq!(
        refused["fields"]["client.address"], "203.0.113.7",
        "{refused}"
    );
}

#[tokio::test]
async fn an_upgrade_without_the_mqtt_subprotocol_is_refused() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    for subprotocol in [None, Some("MQTT"), Some("mqttv3.1, wamp")] {
        let status = refused_status(
            connect_ws_with(
                bridge.address,
                &bridge.url(),
                &via_cloudflare("203.0.113.7"),
                subprotocol,
            )
            .await,
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{subprotocol:?}");
    }
    let refused = bridge
        .wait_for_lines("refused a gateway WebSocket", 3)
        .await;
    assert!(
        refused
            .iter()
            .all(|line| line["fields"]["reason"] == "subprotocol"),
        "{}",
        bridge.dump()
    );
    assert!(broker.connections().is_empty());
}

#[tokio::test]
async fn connection_and_traffic_metrics_reach_the_collector() {
    let collector = FakeCollector::start();
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    let endpoint = collector.url();
    let mut bridge = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("TRUSTIOT__MAX_CONNECTIONS", "2"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "1"),
        ],
    )
    .await;
    // Two relayed connections: one ends by the gateway's close, one by the shutdown.
    let mut first = mqtt_session(bridge.address, &bridge.url(), "203.0.113.7").await;
    let _second = mqtt_session(bridge.address, &bridge.url(), "203.0.113.8").await;
    // One refusal of each kind the test can cause cheaply.
    let status = refused_status(
        connect_ws(
            bridge.address,
            &bridge.url(),
            &via_cloudflare("203.0.113.9"),
        )
        .await,
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let status = refused_status(
        connect_ws(
            bridge.address,
            &bridge.url(),
            &via_cloudflare("203.0.113.7"),
        )
        .await,
    );
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let status = refused_status(connect_ws(bridge.address, &bridge.url(), &[]).await);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    send_binary(&mut first, &support::PINGREQ).await;
    support::read_bytes(&mut first, support::PINGRESP.len()).await;
    within("the close", first.close(None)).await.expect("close");
    bridge.wait_for_line("gateway connection ended").await;

    // The kit flushes the metrics at exit.
    bridge.terminate();
    assert_eq!(bridge.wait_exit().await, 0, "{}", bridge.dump());
    let sums = wait_until("the exported metrics", || {
        let sums = collector.sums();
        sums.contains_key(&("mqtt_bridge.connections.active".to_owned(), String::new()))
            .then_some(sums)
    })
    .await;
    let value = |name: &str, attributes: &str| {
        sums.get(&(name.to_owned(), attributes.to_owned()))
            .copied()
            .unwrap_or_else(|| panic!("no {name}{{{attributes}}} in {sums:?}"))
    };
    assert_eq!(value("mqtt_bridge.connections.opened", ""), 2);
    assert_eq!(value("mqtt_bridge.connections.active", ""), 0);
    assert_eq!(
        value("mqtt_bridge.connections.ended", "reason=gateway_closed"),
        1
    );
    assert_eq!(value("mqtt_bridge.connections.ended", "reason=shutdown"), 1);
    assert_eq!(
        value("mqtt_bridge.connections.refused", "reason=over_capacity"),
        1
    );
    assert_eq!(
        value(
            "mqtt_bridge.connections.refused",
            "reason=client_ip_missing"
        ),
        1
    );
    assert_eq!(
        value(
            "mqtt_bridge.connections.refused",
            "reason=over_source_limit"
        ),
        1
    );
    // CONNECT twice and PINGREQ once to the broker; CONNACK twice and PINGRESP once back.
    let to_broker = 2 * support::CONNECT_KEEPALIVE_0.len() + support::PINGREQ.len();
    let to_gateway = 2 * support::CONNACK.len() + support::PINGRESP.len();
    assert_eq!(
        value("mqtt_bridge.traffic", "direction=to_broker"),
        i64::try_from(to_broker).expect("small")
    );
    assert_eq!(
        value("mqtt_bridge.traffic", "direction=to_gateway"),
        i64::try_from(to_gateway).expect("small")
    );
}

/// A masked client frame header: `first` (FIN and opcode) and a 16-bit payload length, with the
/// all-zero mask key, so the payload goes on the wire as it is.
fn frame_header(first: u8, length: u16) -> Vec<u8> {
    let mut header = vec![first, 0x80 | 126];
    header.extend_from_slice(&length.to_be_bytes());
    header.extend_from_slice(&[0; 4]);
    header
}

/// Writes raw bytes under the WebSocket, as a client that ignores the gateway's limits would.
async fn write_raw(websocket: &mut WebSocketStream<TcpStream>, bytes: &[u8]) {
    within("writing raw bytes", websocket.get_mut().write_all(bytes))
        .await
        .expect("write raw bytes");
}

/// The bridge ends the connection: no data message and no close frame.
async fn expect_dropped(websocket: &mut WebSocketStream<TcpStream>) {
    loop {
        match within("the bridge to end the connection", websocket.next()).await {
            None | Some(Err(_)) => return,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(other)) => panic!("expected the connection to end, got {other:?}"),
        }
    }
}

// Security review of Laysi/trustiot-v2#85 (High): tungstenite reserves a frame's declared length
// when it reads the header, so the bridge accepts no frame or message larger than what the
// gateway sends (32 KiB). A client that declares more is cut off before it sends any payload, and
// one that assembles more from fragments is cut off too; nothing reaches the broker.
#[tokio::test]
async fn frames_and_messages_beyond_the_gateways_32_kib_end_the_connection() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;

    // A header that declares one byte too many, and nothing after it.
    let mut websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    write_raw(&mut websocket, &frame_header(0x82, 32 * 1024 + 1)).await;
    expect_dropped(&mut websocket).await;
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended["fields"]["reason"], "gateway_error", "{ended}");
    assert_eq!(ended["fields"]["bytes_to_broker"], 0, "{ended}");

    // A full 32 KiB fragment, then a continuation that takes the message past 32 KiB.
    let mut websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    let mut fragments = frame_header(0x02, 32 * 1024);
    fragments.extend_from_slice(&[0x17; 32 * 1024]);
    fragments.extend_from_slice(&frame_header(0x80, 1));
    fragments.push(0x17);
    write_raw(&mut websocket, &fragments).await;
    expect_dropped(&mut websocket).await;
    let ended = bridge.wait_for_lines("gateway connection ended", 2).await;
    assert_eq!(
        ended[1]["fields"]["reason"], "gateway_error",
        "{}",
        ended[1]
    );
    assert_eq!(ended[1]["fields"]["bytes_to_broker"], 0, "{}", ended[1]);
    let connections = wait_until("both broker connections to close", || {
        let connections = broker.connections();
        (connections.len() == 2 && connections.iter().all(|c| c.closed)).then_some(connections)
    })
    .await;
    assert!(
        connections.iter().all(|c| c.received.is_empty()),
        "nothing reached the broker"
    );
}
