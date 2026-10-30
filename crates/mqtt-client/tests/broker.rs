//! The client against a real Mosquitto 2.1.2 with dynamic security (Laysi/trustiot-v2#631),
//! the image the chart runs:
//!
//! - the service account logs in with its password and its subscriptions are answered with
//!   their SUBACK reason codes (a direct one, a shared one, and one the account may not make);
//!   a wrong password is refused and the caller gets the CONNACK reason code (Mosquitto 2.1.2
//!   with dynamic security answers 0x87, not authorized, rather than 0x86);
//! - a 1 MiB payload is published and comes back unchanged; a packet above the broker's limit
//!   is refused before it is sent and the connection stays up;
//! - a publish to a topic the account may not publish to fails with PUBACK 0x87;
//! - two instances with the same account connect at once with different client ids;
//! - after the broker restarts, the client reconnects and reports it; after the password is
//!   changed in the broker, the client is refused until the password file holds the new one,
//!   and the next attempt then connects.
//!
//! `CI=1 cargo test -p mqtt-client --test broker -- --nocapture` prints the evidence lines the
//! test report quotes.

#![allow(
    clippy::unwrap_used,
    reason = "tests unwrap (clippy.toml allows it in tests)"
)]

mod support;

use mqtt_client::{Client, ConnectionError, Event, MAX_PACKET_BYTES, PublishError, PublishOptions};
use support::{
    Broker, Scratch, USERNAME, evidence, expect_connected, expect_event, random_password,
};

/// A refused subscription: the role allows nothing outside `test/`.
const FORBIDDEN_FILTER: &str = "secret/#";

/// The subscriptions of the main client, in order.
fn subscriptions() -> Vec<String> {
    vec![
        "test/echo".to_owned(),
        "$share/workers/test/shared".to_owned(),
        FORBIDDEN_FILTER.to_owned(),
    ]
}

/// Random bytes of `len`, so a truncated or reordered payload cannot match by chance.
fn payload(len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    getrandom::fill(&mut bytes).unwrap();
    bytes
}

async fn expect_message(events: &mut mqtt_client::Events, topic: &str) -> mqtt_client::Message {
    let what = format!("a message on {topic}");
    match expect_event(
        events,
        &what,
        |event| matches!(event, Event::Message(message) if message.topic == topic),
    )
    .await
    {
        Event::Message(message) => message,
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_in_publishes_receives_and_reports_refusals() {
    let Some(broker) = Broker::start() else {
        return;
    };
    let scratch = Scratch::new();
    let password = random_password();
    broker.create_account(&password);
    let config = broker.config(scratch.write("password", &format!("{password}\n")));

    // The right password: connected, with one reason code per subscription.
    let (client, mut events) = Client::start(&config, subscriptions()).unwrap();
    let codes = expect_connected(&mut events, "the first connection").await;
    evidence(format!(
        "connected as {USERNAME} with client id {}; SUBACK codes {codes:02x?}",
        client.client_id()
    ));
    assert_eq!(
        codes,
        [0x01, 0x01, 0x87],
        "test/echo, the shared subscription, {FORBIDDEN_FILTER}"
    );

    // 1 MiB out and back, byte for byte.
    let large = payload(protocol::topic::MAX_LARGE_BYTES);
    client
        .publish("test/echo", large.clone(), PublishOptions::default())
        .await
        .unwrap();
    let echoed = expect_message(&mut events, "test/echo").await;
    assert_eq!(echoed.payload.len(), large.len());
    assert!(
        echoed.payload == large,
        "the 1 MiB payload came back changed"
    );
    evidence(format!(
        "published {} bytes to test/echo and received the same {} bytes",
        large.len(),
        echoed.payload.len()
    ));

    // Through the shared subscription, with a message expiry.
    let options = PublishOptions {
        retain: false,
        message_expiry: Some(std::time::Duration::from_secs(600)),
    };
    client
        .publish("test/shared", "shared", options)
        .await
        .unwrap();
    let shared = expect_message(&mut events, "test/shared").await;
    assert_eq!(&shared.payload[..], b"shared");
    evidence("received test/shared through $share/workers/test/shared");

    // A topic the role does not allow: PUBACK 0x87 reaches the caller.
    let refused = client
        .publish("forbidden/topic", "x", PublishOptions::default())
        .await;
    evidence(format!("publish to forbidden/topic: {refused:?}"));
    assert!(
        matches!(refused, Err(PublishError::Refused { code: 0x87, .. })),
        "{refused:?}"
    );

    // Larger than the broker accepts: refused here, nothing sent, the connection stays up.
    let too_large = client
        .publish(
            "test/echo",
            payload(usize::try_from(MAX_PACKET_BYTES).unwrap()),
            PublishOptions::default(),
        )
        .await;
    evidence(format!(
        "publish of {MAX_PACKET_BYTES} payload bytes: {too_large:?}"
    ));
    assert!(
        matches!(too_large, Err(PublishError::TooLarge { max, .. }) if max == MAX_PACKET_BYTES),
        "{too_large:?}"
    );
    client
        .publish("test/echo", "still connected", PublishOptions::default())
        .await
        .unwrap();
    let after = expect_message(&mut events, "test/echo").await;
    assert_eq!(&after.payload[..], b"still connected");

    // A second instance of the same service: another client id, both connected at once.
    let (second, mut second_events) = Client::start(&config, Vec::new()).unwrap();
    expect_connected(&mut second_events, "the second instance's connection").await;
    assert_ne!(client.client_id(), second.client_id());
    for id in [client.client_id(), second.client_id()] {
        assert!(id.starts_with(&format!("{USERNAME}:")), "{id}");
    }
    second
        .publish(
            "test/echo",
            "from the second instance",
            PublishOptions::default(),
        )
        .await
        .unwrap();
    let from_second = expect_message(&mut events, "test/echo").await;
    assert_eq!(&from_second.payload[..], b"from the second instance");
    client
        .publish(
            "test/echo",
            "from the first instance",
            PublishOptions::default(),
        )
        .await
        .unwrap();
    expect_message(&mut events, "test/echo").await;
    evidence(format!(
        "two instances connected at once: {} and {}",
        client.client_id(),
        second.client_id()
    ));

    // A wrong password: refused, with the CONNACK reason code.
    let wrong = broker.config(scratch.write("wrong", &random_password()));
    let (refused_client, mut refused_events) = Client::start(&wrong, Vec::new()).unwrap();
    let refusal = expect_event(
        &mut refused_events,
        "the refusal of a wrong password",
        |event| matches!(event, Event::Disconnected { .. }),
    )
    .await;
    evidence(format!("wrong password: {refusal:?}"));
    assert!(
        matches!(
            refusal,
            Event::Disconnected {
                error: ConnectionError::Refused { code: 0x87, .. },
                ..
            }
        ),
        "{refusal:?}"
    );
    refused_client.shutdown().await;

    // Neither instance lost its connection: no Disconnected before the shutdown ends the
    // events.
    for (name, instance, mut events) in
        [("first", client, events), ("second", second, second_events)]
    {
        instance.shutdown().await;
        while let Some(event) = events.recv().await {
            assert!(
                !matches!(event, Event::Disconnected { .. }),
                "the {name} instance was disconnected: {event:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_a_restart_and_reads_the_rotated_password() {
    let Some(broker) = Broker::start() else {
        return;
    };
    let scratch = Scratch::new();
    let first_password = random_password();
    broker.create_account(&first_password);
    let password_file = scratch.write("password", &first_password);
    let config = broker.config(password_file.clone());
    let (client, mut events) = Client::start(&config, vec!["test/echo".to_owned()]).unwrap();
    assert_eq!(
        expect_connected(&mut events, "the first connection").await,
        [0x01]
    );

    // The broker restarts: the caller learns the connection was lost and that it is back, with
    // its subscription made again.
    tokio::task::block_in_place(|| broker.restart());
    let lost = expect_event(
        &mut events,
        "the lost connection after the restart",
        |event| matches!(event, Event::Disconnected { .. }),
    )
    .await;
    evidence(format!("broker restarted: {lost:?}"));
    let codes = expect_connected(&mut events, "the reconnection after the restart").await;
    evidence(format!(
        "reconnected after the restart; SUBACK codes {codes:02x?}"
    ));
    assert_eq!(codes, [0x01]);
    client
        .publish("test/echo", "after the restart", PublishOptions::default())
        .await
        .unwrap();
    assert_eq!(
        &expect_message_payload(&mut events).await[..],
        b"after the restart"
    );

    // The password changes in the broker; the file still holds the old one. A restart drops the
    // connection, and every new attempt is refused.
    let second_password = random_password();
    broker.set_password(&second_password);
    tokio::task::block_in_place(|| broker.restart());
    let refusal = expect_event(&mut events, "a refusal with the old password", |event| {
        matches!(
            event,
            Event::Disconnected {
                error: ConnectionError::Refused { .. },
                ..
            }
        )
    })
    .await;
    evidence(format!("old password after the change: {refusal:?}"));
    assert!(
        matches!(
            refusal,
            Event::Disconnected {
                error: ConnectionError::Refused { code: 0x87, .. },
                ..
            }
        ),
        "{refusal:?}"
    );

    // The new password in the file: the next attempt reads it and connects, without a restart
    // of the client.
    std::fs::write(&password_file, &second_password).unwrap();
    let codes = expect_connected(&mut events, "the connection with the new password").await;
    evidence(format!(
        "connected with the rotated password; SUBACK codes {codes:02x?}"
    ));
    client
        .publish(
            "test/echo",
            "with the new password",
            PublishOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        &expect_message_payload(&mut events).await[..],
        b"with the new password"
    );
    client.shutdown().await;
}

async fn expect_message_payload(events: &mut mqtt_client::Events) -> bytes::Bytes {
    expect_message(events, "test/echo").await.payload
}
