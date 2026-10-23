//! The bridge's WebSocket pings keep an MQTT connection without keepalive open through a proxy
//! that cuts idle connections, as Cloudflare does after 125 s
//! (docs/feasibility/2026-10-10-edge-poc.md). The test scales the times down: the proxy cuts after
//! `CUTOFF`, the bridge pings every `PING`, and the connection stays idle for three cut-offs.
//! Without the pings (the longest interval the bridge accepts) the same proxy cuts it.

mod support;

use std::time::Duration;

use futures_util::StreamExt as _;
use support::{
    Behaviour, Bridge, FakeBroker, IdleCutProxy, PINGREQ, PINGRESP, mqtt_session, read_bytes,
    send_binary, within,
};
use tokio_tungstenite::tungstenite::Message;

/// The proxy's idle cut-off.
const CUTOFF: Duration = Duration::from_secs(3);
/// The bridge's ping interval while pinging: well inside the cut-off even on a loaded machine.
const PING: &str = "200ms";
/// How long the MQTT connection stays idle: three cut-offs.
const IDLE: Duration = Duration::from_secs(9);

#[tokio::test]
async fn pings_keep_a_keepalive_0_connection_open_past_the_idle_cutoff() {
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    let bridge = Bridge::start(
        broker.address,
        &[("TRUSTIOT__WEBSOCKET__PING_INTERVAL", PING)],
    )
    .await;
    let proxy = IdleCutProxy::start(bridge.address, CUTOFF).await;
    let mut websocket = mqtt_session(proxy.address, &bridge.url(), "203.0.113.7").await;

    // Idle MQTT: only the bridge's pings move. Reading answers them (tungstenite sends the pong).
    let mut pings = 0;
    let idle = tokio::time::sleep(IDLE);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            () = &mut idle => break,
            message = websocket.next() => match message {
                Some(Ok(Message::Ping(_))) => pings += 1,
                other => panic!("the idle connection got {other:?}; {} cut(s)", proxy.cuts()),
            },
        }
    }
    assert!(pings > 0, "the bridge pinged");
    assert_eq!(proxy.cuts(), 0, "the proxy never saw the connection idle");

    // Still the same MQTT connection: PINGREQ is answered.
    send_binary(&mut websocket, &PINGREQ).await;
    let (answer, _) = read_bytes(&mut websocket, PINGRESP.len()).await;
    assert_eq!(answer, PINGRESP);
    assert_eq!(broker.connections().len(), 1, "no reconnection happened");
    println!("{pings} pings over {IDLE:?} kept the connection through a {CUTOFF:?} idle cut-off");
}

#[tokio::test]
async fn without_pings_the_idle_cutoff_ends_the_connection() {
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    // The longest interval the bridge accepts: no ping within this test.
    let bridge = Bridge::start(
        broker.address,
        &[("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "100s")],
    )
    .await;
    let proxy = IdleCutProxy::start(bridge.address, CUTOFF).await;
    let mut websocket = mqtt_session(proxy.address, &bridge.url(), "203.0.113.7").await;
    // The proxy drops both sides without a close, as Cloudflare does.
    let end = within("the idle cut", websocket.next()).await;
    assert!(
        !matches!(end, Some(Ok(Message::Binary(_) | Message::Ping(_)))),
        "expected the connection to end, got {end:?}"
    );
    support::wait_until("the proxy to record the cut", || {
        (proxy.cuts() == 1).then_some(())
    })
    .await;
}
