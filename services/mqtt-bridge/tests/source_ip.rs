//! The recorded source IP (decisions 6 and 27): in the SaaS mode only CF-Connecting-IP counts.
//! What the client itself puts in X-Forwarded-For (the left-most entries, which Cloudflare passes
//! on unchanged) or True-Client-IP never changes it, and is no fallback when the trusted header is
//! missing.

mod support;

use support::{Behaviour, Bridge, FakeBroker, connect_ws, refused_status};
use tokio_tungstenite::tungstenite::http::StatusCode;

const GATEWAY: &str = "203.0.113.7";

#[tokio::test]
async fn spoofed_forwarding_headers_do_not_change_the_recorded_ip() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    // What reaches the origin when a client sends its own forwarding headers through Cloudflare:
    // Cloudflare sets CF-Connecting-IP and appends to X-Forwarded-For, whose left-most entries
    // stay the client's (docs/feasibility/2026-10-10-edge-poc.md).
    let headers = [
        ("x-forwarded-for", "198.51.100.1, 10.0.0.1"),
        ("x-forwarded-for", "192.0.2.9"),
        ("true-client-ip", "198.51.100.2"),
        ("x-real-ip", "198.51.100.3"),
        ("forwarded", "for=198.51.100.4"),
        ("cf-connecting-ip", GATEWAY),
    ];
    let websocket = connect_ws(bridge.address, &bridge.url(), &headers)
        .await
        .expect("open the WebSocket");
    let relaying = bridge.wait_for_line("relaying a gateway connection").await;
    assert_eq!(relaying["fields"]["client.address"], GATEWAY, "{relaying}");
    // The broker-side address of the connection, to match the broker's own log line.
    let local = relaying["fields"]["broker.local_address"]
        .as_str()
        .expect("broker.local_address");
    let seen_by_broker = support::wait_until("the broker's connection", || {
        broker.connections().into_iter().next()?.peer
    })
    .await;
    assert_eq!(local, seen_by_broker.to_string(), "{relaying}");
    drop(websocket);
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended["fields"]["client.address"], GATEWAY, "{ended}");
    assert_eq!(
        ended["fields"]["connection.id"], relaying["fields"]["connection.id"],
        "both lines carry the same connection id"
    );
    for spoofed in [
        "198.51.100.1",
        "198.51.100.2",
        "198.51.100.3",
        "198.51.100.4",
        "192.0.2.9",
        "10.0.0.1",
    ] {
        assert!(
            !bridge.dump().contains(spoofed),
            "{spoofed} was recorded:\n{}",
            bridge.dump()
        );
    }
}

#[tokio::test]
async fn without_the_trusted_header_the_upgrade_is_refused_and_the_broker_untouched() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    let only_spoofed = [
        ("x-forwarded-for", "198.51.100.1"),
        ("true-client-ip", "198.51.100.2"),
    ];
    let status = refused_status(connect_ws(bridge.address, &bridge.url(), &only_spoofed).await);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let refused = bridge.wait_for_line("refused a gateway WebSocket").await;
    assert_eq!(
        refused["fields"]["reason"], "client_ip_missing",
        "{refused}"
    );

    let twice = [
        ("cf-connecting-ip", GATEWAY),
        ("cf-connecting-ip", "198.51.100.1"),
    ];
    let status = refused_status(connect_ws(bridge.address, &bridge.url(), &twice).await);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let not_an_ip = [("cf-connecting-ip", "198.51.100.1, 203.0.113.7")];
    let status = refused_status(connect_ws(bridge.address, &bridge.url(), &not_an_ip).await);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let reasons: Vec<_> = bridge
        .wait_for_lines("refused a gateway WebSocket", 3)
        .await
        .iter()
        .map(|line| {
            line["fields"]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        reasons,
        [
            "client_ip_missing",
            "client_ip_repeated",
            "client_ip_invalid"
        ]
    );
    assert!(
        broker.connections().is_empty(),
        "a refused upgrade never opens a broker connection"
    );
    assert!(bridge.messages("relaying a gateway connection").is_empty());
}
