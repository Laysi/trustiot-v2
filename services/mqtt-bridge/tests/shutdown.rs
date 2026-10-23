//! SIGTERM in a rolling update: every connected gateway gets a WebSocket close (1001, going away)
//! from the old bridge before it exits, so it reconnects at once instead of waiting for the grace
//! period to end (the 2026-10-10 feasibility test lost 30 s to a bridge without this); upgrades
//! that still reach the draining bridge are refused with 503, which the ingress retries elsewhere;
//! the drain then ends cleanly, even with a gateway that never answers the close.

mod support;

use futures_util::StreamExt as _;
use support::{
    Behaviour, Bridge, FakeBroker, connect_ws, mqtt_session, refused_status, via_cloudflare,
    wait_until, within,
};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Gateways connected when SIGTERM arrives.
const GATEWAYS: usize = 5;

#[tokio::test]
async fn sigterm_closes_every_websocket_before_exit_and_gateways_reconnect_to_the_new_pod() {
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    // The old pod: a drain delay long enough to see a refused upgrade while it drains, a close
    // timeout for the gateway that never answers, and a drain timeout the test never reaches.
    let mut old = Bridge::start(
        broker.address,
        &[
            ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "10s"),
            ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "60s"),
            ("TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT", "2s"),
        ],
    )
    .await;
    let new = Bridge::start(broker.address, &[]).await;

    let mut gateways = Vec::new();
    for _ in 0..GATEWAYS {
        gateways.push(mqtt_session(old.address, &old.url(), "203.0.113.7").await);
    }
    // A gateway that never reads again: it never answers the close.
    let _silent = mqtt_session(old.address, &old.url(), "203.0.113.8").await;
    old.wait_for_lines("relaying a gateway connection", GATEWAYS + 1)
        .await;

    let signalled = Instant::now();
    old.terminate();
    let mut reconnected = Vec::new();
    let mut on_new_pod = Vec::new();
    for (index, mut gateway) in gateways.into_iter().enumerate() {
        let close = loop {
            match within("the old bridge's close", gateway.next()).await {
                Some(Ok(Message::Close(frame))) => break frame,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a close before the bridge exits, got {other:?}"),
            }
        };
        // A close frame can only come from the bridge while it runs: an exited process ends the
        // connection without one.
        assert_eq!(close.map(|frame| frame.code), Some(CloseCode::Away));
        if index == 0 {
            // The close proves SIGTERM arrived, and the drain delay (10 s) has just started: an
            // upgrade that still reaches the old pod is refused, and the ingress retries it.
            let status = refused_status(
                connect_ws(old.address, &old.url(), &via_cloudflare("203.0.113.9")).await,
            );
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        }
        // The gateway's reconnect, through the ingress, lands on the ready pod.
        on_new_pod.push(mqtt_session(new.address, &new.url(), "203.0.113.7").await);
        reconnected.push(signalled.elapsed());
    }
    println!("gateways reconnected to the new bridge after SIGTERM: {reconnected:?}");

    // Every relay ended because of the shutdown, and the drain ended within its timeout.
    let code = old.wait_exit().await;
    assert_eq!(code, 0, "drained cleanly:\n{}", old.dump());
    let ended = old.messages("gateway connection ended");
    assert_eq!(ended.len(), GATEWAYS + 1, "{}", old.dump());
    assert!(
        ended
            .iter()
            .all(|line| line["fields"]["reason"] == "shutdown"),
        "{}",
        old.dump()
    );
    let refused = old.messages("refused a gateway WebSocket");
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["fields"]["reason"], "draining");
    // The old bridge's broker connections (accepted first) are all closed; the new pod's stay open.
    let connections = wait_until("the old bridge's broker connections to close", || {
        let connections = broker.connections();
        connections[..=GATEWAYS]
            .iter()
            .all(|connection| connection.closed)
            .then_some(connections)
    })
    .await;
    assert_eq!(connections.len(), 2 * GATEWAYS + 1);
    assert!(
        connections[GATEWAYS + 1..]
            .iter()
            .all(|connection| !connection.closed),
        "the reconnected gateways' broker connections stay open"
    );
    drop(on_new_pod);
}
