//! The link's MQTT behavior against a scripted broker (`support::FakeBroker`) over in-memory
//! streams: the connection sequence and presence, the in-flight and incoming bounds
//! (Laysi/trustiot-v2#86, acceptance criterion 3), PUBACK outcomes, keepalive, reconnect
//! backoff, protocol violations and the clean stop.
//!
//! Every test runs on a paused clock: with nothing but in-memory streams, tokio moves the clock
//! only when every task waits, so a keepalive or backoff timer fires exactly when nothing else
//! can happen. A PINGREQ arriving while publishers wait therefore proves that the client sent
//! nothing else during a whole keepalive period.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::FutureExt;

use agent::ReasonCode;
use agent::backoff::{MIN_DELAY, STABLE_AFTER};
use agent::link::{
    self, ConnectError, ConnectStage, Connector, DownMessage, INCOMING_QUEUE, Incoming, Link,
    LinkSettings, LinkState, PublishError, Published, REQUEST_QUEUE, Refusal, Subscriptions,
};
use protocol::health::{Heartbeat, OfflineReason, Status};
use protocol::topic::{Channel, Direction, MIN_MAX_PACKET_BYTES};
use protocol::{AgentVersion, SessionId};
use support::{
    Connect, DuplexConnector, FakeBroker, GATEWAY, Packet, connack, disconnect, gateway, pingresp,
    puback, publish, suback,
};
use testkit::clock::bounded;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct Harness {
    link: Link,
    incoming: Incoming,
    brokers: mpsc::UnboundedReceiver<DuplexStream>,
    shutdown: CancellationToken,
    supervisor: JoinHandle<()>,
    /// What the link subscribes to, for the brokers to expect.
    subscriptions: Vec<Channel>,
}

/// Every `down` channel, in protocol order.
fn down_channels() -> Vec<Channel> {
    Channel::ALL
        .into_iter()
        .filter(|channel| channel.direction() == Direction::Down)
        .collect()
}

/// Settings that subscribe to every `down` channel, as the agent will once each has a handler.
fn settings() -> LinkSettings {
    LinkSettings {
        subscriptions: Subscriptions::new(down_channels()).unwrap(),
        ..LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap())
    }
}

fn start(settings: LinkSettings) -> Harness {
    start_with(settings, DuplexConnector::new())
}

fn start_with(
    settings: LinkSettings,
    (connector, brokers): (DuplexConnector, mpsc::UnboundedReceiver<DuplexStream>),
) -> Harness {
    let shutdown = CancellationToken::new();
    let subscriptions = settings.subscriptions.channels().to_vec();
    let (link, incoming, supervisor) = link::link(settings, connector, shutdown.clone());
    Harness {
        link,
        incoming,
        brokers,
        shutdown,
        supervisor: tokio::spawn(supervisor.run()),
        subscriptions,
    }
}

impl Harness {
    /// Accepts the connection like [`FakeBroker::accept`] and waits until the link reports it
    /// Connected: publishes are accepted from then on.
    async fn connected(&self, broker: &mut FakeBroker<DuplexStream>) -> (Connect, SessionId) {
        self.connected_with(broker, &connack(0, &[])).await
    }

    /// [`Harness::connected`] with the CONNACK `connack`.
    async fn connected_with(
        &self,
        broker: &mut FakeBroker<DuplexStream>,
        connack: &[u8],
    ) -> (Connect, SessionId) {
        let (connect, session) = broker.accept_with(connack).await;
        self.wait_state(
            "connected",
            |state| matches!(state, LinkState::Connected { session: now } if *now == session),
        )
        .await;
        (connect, session)
    }

    async fn broker(&mut self) -> FakeBroker<DuplexStream> {
        FakeBroker::new(
            bounded("the next connection", self.brokers.recv())
                .await
                .unwrap(),
        )
        .subscribing(&self.subscriptions)
    }

    async fn wait_state(&self, what: &str, wanted: impl FnMut(&LinkState) -> bool) -> LinkState {
        let mut state = self.link.state();
        bounded(what, state.wait_for(wanted)).await.unwrap().clone()
    }

    async fn recv(&mut self) -> DownMessage {
        bounded("a down message", self.incoming.recv())
            .await
            .unwrap()
    }
}

fn spawn_publish(
    link: &Link,
    channel: Channel,
    payload: Vec<u8>,
) -> JoinHandle<Result<Published, PublishError>> {
    let link = link.clone();
    tokio::spawn(async move { link.publish(channel, payload).await })
}

/// A publisher that waits for room with [`Link::reserve`] and builds its payload only once it
/// has a place.
fn spawn_waiting_publish(
    link: &Link,
    channel: Channel,
    payload: impl FnOnce() -> Vec<u8> + Send + 'static,
) -> JoinHandle<Result<Published, PublishError>> {
    let link = link.clone();
    tokio::spawn(async move { link.reserve(channel).await?.publish(payload()).await })
}

/// With `inflight_limit` 1 and a broker that withholds the PUBACK: one telemetry publish in
/// flight and the QoS 1 queue full. Returns the PUBLISH on the wire and the waiting publishers.
async fn fill_the_queue(
    harness: &Harness,
    broker: &mut FakeBroker<DuplexStream>,
) -> (
    support::Publish,
    Vec<JoinHandle<Result<Published, PublishError>>>,
) {
    // The first one goes out (and fills the window) before the queue fills up behind it.
    let mut publishers = vec![spawn_publish(&harness.link, Channel::Telemetry, vec![0, 0])];
    let Some(Packet::Publish(first)) = broker.read().await else {
        panic!("expected a PUBLISH")
    };
    publishers.extend((1..=REQUEST_QUEUE).map(|index| {
        spawn_publish(
            &harness.link,
            Channel::Telemetry,
            vec![0, u8::try_from(index).unwrap()],
        )
    }));
    // Let every task run until it waits.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(harness.link.in_flight(), 1);
    assert_eq!(harness.link.queued(), REQUEST_QUEUE);
    (first, publishers)
}

#[tokio::test(start_paused = true)]
async fn a_connection_starts_with_connect_and_will_then_online_then_subscriptions() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let (connect, session) = harness.connected(&mut broker).await;
    assert_eq!(
        connect.client_id, GATEWAY,
        "client ID = certificate CN = gateway ID"
    );
    assert_eq!(connect.keep_alive, 60);
    assert!(connect.clean_start);
    assert_eq!(connect.session_expiry, 0);
    assert_eq!(
        connect.receive_maximum,
        Some(u16::try_from(INCOMING_QUEUE).unwrap())
    );
    assert_eq!(
        connect.maximum_packet_size,
        Some(u32::try_from(MIN_MAX_PACKET_BYTES).unwrap())
    );
    assert!(
        !connect.username && !connect.password,
        "the identity is the certificate"
    );
    assert_eq!(connect.will_topic, Some(Channel::Status.topic(&gateway())));
    assert_eq!((connect.will_qos, connect.will_retain), (1, true));
    assert_eq!(
        Status::decode(&connect.will_payload).unwrap(),
        Status::last_will(session.clone()),
        "the will is this session's connection_lost"
    );
    let state = harness
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    assert_eq!(state, LinkState::Connected { session });
}

#[tokio::test(start_paused = true)]
async fn the_inflight_limit_bounds_unacknowledged_publishes() {
    const LIMIT: usize = 8;
    const PUBLISHES: usize = 50;
    let mut harness = start(LinkSettings {
        inflight_limit: u16::try_from(LIMIT).unwrap(),
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    // Callers that want to wait for room use reserve(); they hold no payload while they wait.
    let publishers: Vec<_> = (0..PUBLISHES)
        .map(|index| {
            spawn_waiting_publish(&harness.link, Channel::Telemetry, move || {
                vec![u8::try_from(index).unwrap(); 64]
            })
        })
        .collect();

    // The broker withholds every PUBACK: exactly LIMIT publishes arrive, and then nothing but a
    // PINGREQ a whole keepalive later.
    let mut outstanding = VecDeque::new();
    for _ in 0..LIMIT {
        let Some(Packet::Publish(publish)) = broker.read().await else {
            panic!("expected a PUBLISH")
        };
        assert_eq!((publish.qos, publish.retain), (1, false));
        outstanding.push_back(publish.id.unwrap());
    }
    let waited = Instant::now();
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    assert!(waited.elapsed() >= Duration::from_secs(60));
    assert_eq!(harness.link.in_flight(), LIMIT);
    assert_eq!(
        harness.link.queued(),
        REQUEST_QUEUE,
        "the request queue is full, not growing"
    );
    assert!(
        publishers.iter().all(|publisher| !publisher.is_finished()),
        "every publisher waits"
    );
    broker.send(&pingresp()).await;

    // One PUBACK lets exactly one more publish through.
    broker
        .send(&puback(outstanding.pop_front().unwrap(), 0))
        .await;
    let Some(Packet::Publish(next)) = broker.read().await else {
        panic!("expected one more PUBLISH")
    };
    outstanding.push_back(next.id.unwrap());
    // Then nothing for half a keepalive (the paused clock skips it at once). A whole one would
    // reach the PUBACK deadline of the first publishes (twice the keepalive), which ends the
    // connection (a_puback_that_never_comes_ends_the_connection_after_its_deadline).
    assert!(
        tokio::time::timeout(Duration::from_secs(30), broker.read())
            .await
            .is_err(),
        "only one more publish goes through"
    );
    assert_eq!(harness.link.in_flight(), LIMIT);

    // Acknowledge everything; the fifth PUBACK refuses its message with 0x87.
    let mut acknowledged = 1;
    while acknowledged < PUBLISHES {
        if let Some(id) = outstanding.pop_front() {
            let reason = if acknowledged == 5 { 0x87 } else { 0 };
            broker.send(&puback(id, reason)).await;
            acknowledged += 1;
            continue;
        }
        match broker.read().await {
            Some(Packet::Publish(publish)) => outstanding.push_back(publish.id.unwrap()),
            Some(Packet::PingReq) => broker.send(&pingresp()).await,
            other => panic!("unexpected {other:?}"),
        }
        assert!(harness.link.in_flight() <= LIMIT);
    }
    let mut refused = 0;
    for publisher in publishers {
        match bounded("a publisher", publisher).await.unwrap() {
            Ok(Published::Acknowledged(ReasonCode(0))) => {}
            Err(PublishError::Refused { reason, .. }) => {
                assert_eq!(reason, ReasonCode(0x87));
                refused += 1;
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(
        refused, 1,
        "a PUBACK of 0x87 is a failure for exactly that publish"
    );
    assert_eq!(harness.link.in_flight(), 0);
}

#[tokio::test(start_paused = true)]
async fn publish_never_waits_for_room_so_callers_beyond_the_queue_hold_nothing() {
    let mut harness = start(LinkSettings {
        inflight_limit: 1,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let (_first, _publishers) = fill_the_queue(&harness, &mut broker).await;

    // A thousand callers at once: every one is answered Busy on its first poll, so none of
    // them keeps a payload alive anywhere, and the link still holds only its own bound.
    let attempts: Vec<_> = (0..1000)
        .map(|_| harness.link.publish(Channel::Telemetry, vec![7; 1024]))
        .collect();
    let outcomes = futures_util::future::join_all(attempts)
        .now_or_never()
        .expect("publish waited for room instead of answering at once");
    assert!(
        outcomes
            .iter()
            .all(|outcome| *outcome == Err(PublishError::Busy)),
        "{:?}",
        outcomes
            .iter()
            .find(|outcome| **outcome != Err(PublishError::Busy))
    );
    assert_eq!(harness.link.queued(), REQUEST_QUEUE);
    assert_eq!(harness.link.in_flight(), 1);
    // The QoS 0 queue is separate: a heartbeat still goes out.
    let heartbeat = spawn_publish(&harness.link, Channel::Heartbeat, vec![1]);
    let Some(Packet::Publish(sent)) = broker.read().await else {
        panic!("expected the heartbeat")
    };
    assert_eq!(sent.topic, Channel::Heartbeat.topic(&gateway()));
    assert_eq!(
        bounded("the heartbeat", heartbeat).await.unwrap(),
        Ok(Published::Sent)
    );
}

#[tokio::test(start_paused = true)]
async fn reserve_waits_without_a_payload_and_hands_out_places_in_order() {
    let mut harness = start(LinkSettings {
        inflight_limit: 1,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let (first, publishers) = fill_the_queue(&harness, &mut broker).await;

    // Three waiters, asking in this order; none has built its payload yet.
    let built = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut waiters = Vec::new();
    for index in 1..=3_u8 {
        let built = std::sync::Arc::clone(&built);
        waiters.push(spawn_waiting_publish(
            &harness.link,
            Channel::Telemetry,
            move || {
                built.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![index]
            },
        ));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        built.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a waiter built its payload before it had a place"
    );
    assert_eq!(harness.link.queued(), REQUEST_QUEUE);
    assert!(waiters.iter().all(|waiter| !waiter.is_finished()));

    // Each PUBACK lets one queued message out, which makes room for the next waiter.
    let mut previous = first;
    let mut order = Vec::new();
    for _ in 0..REQUEST_QUEUE + 3 {
        broker.send(&puback(previous.id.unwrap(), 0)).await;
        let Some(Packet::Publish(next)) = broker.read_answering_pings().await else {
            panic!("expected the next PUBLISH")
        };
        order.push(next.payload.clone());
        previous = next;
    }
    broker.send(&puback(previous.id.unwrap(), 0)).await;
    assert!(order[..REQUEST_QUEUE].iter().all(|payload| payload[0] == 0));
    assert_eq!(
        order[REQUEST_QUEUE..],
        [vec![1], vec![2], vec![3]],
        "places go to the waiters in the order they asked"
    );
    for publisher in publishers.into_iter().chain(waiters) {
        assert_eq!(
            bounded("a publisher", publisher).await.unwrap(),
            Ok(Published::Acknowledged(ReasonCode(0)))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_brokers_limits_from_connack_are_respected() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    // Receive Maximum 2, Maximum Packet Size 1000 (room for the status and the SUBSCRIBE),
    // Server Keep Alive 10 seconds.
    harness
        .connected_with(
            &mut broker,
            &connack(0, &[0x21, 0, 2, 0x27, 0, 0, 0x03, 0xE8, 0x13, 0, 10]),
        )
        .await;
    let started = Instant::now();
    let too_large = harness
        .link
        .publish(Channel::Telemetry, vec![0; 2000])
        .await;
    assert!(
        matches!(too_large, Err(PublishError::TooLarge { limit: 1000, .. })),
        "{too_large:?}"
    );
    let publishers: Vec<_> = (0..3)
        .map(|_| spawn_publish(&harness.link, Channel::Log, vec![1; 10]))
        .collect();
    for _ in 0..2 {
        assert!(matches!(broker.read().await, Some(Packet::Publish(_))));
    }
    assert_eq!(
        broker.read().await,
        Some(Packet::PingReq),
        "nothing else was sent"
    );
    let elapsed = started.elapsed();
    assert!(
        (Duration::from_secs(10)..Duration::from_secs(11)).contains(&elapsed),
        "the server's keepalive of 10 s replaces the client's: {elapsed:?}"
    );
    assert_eq!(harness.link.in_flight(), 2);
    drop(publishers);
}

/// The client must not send a packet above the broker's Maximum Packet Size (MQTT-3.2.2-15).
/// A broker whose limit cannot hold the `online` status never gets it: the connection ends
/// with DISCONNECT 0x04, or without DISCONNECT when not even that fits; either way the broker
/// publishes the will.
#[tokio::test(start_paused = true)]
async fn packets_above_the_brokers_maximum_packet_size_are_never_sent() {
    let mut harness = start(settings());
    let disconnected = |state: &LinkState| matches!(state, LinkState::Disconnected { .. });
    // 50 bytes hold DISCONNECT 0x04 (3 bytes) but not the online status; 2 bytes hold neither.
    for (limit, end) in [
        (50_u8, Some(Packet::Disconnect { reason: 0x04 })),
        (2, None),
    ] {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(0, &[0x27, 0, 0, 0, limit])).await;
        assert_eq!(
            broker.read().await,
            end,
            "Maximum Packet Size {limit}: no online status"
        );
        if end.is_some() {
            assert_eq!(
                broker.read().await,
                None,
                "the client closes after DISCONNECT"
            );
        }
        let LinkState::Disconnected { error, .. } =
            harness.wait_state("disconnected", disconnected).await
        else {
            unreachable!()
        };
        assert!(
            error.contains(&format!("Maximum Packet Size of {limit} bytes")),
            "{error}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn down_messages_are_delivered_acknowledged_and_bounded() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let gateway = gateway();
    let config = Channel::Config.topic(&gateway);
    broker
        .send(&publish(&config, b"cfg", 1, true, Some(1)))
        .await;
    assert_eq!(
        harness.recv().await,
        DownMessage {
            channel: Channel::Config,
            payload: b"cfg".to_vec(),
            retained: true
        }
    );
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::PubAck { id: 1, reason: 0 })
    );

    // Messages that are not for this gateway's down channels, or too large for theirs, are
    // dropped (and still acknowledged); the marker after them arrives next.
    let dropped = [
        "trustiot/v1/gw/another-gw/down/config".to_owned(),
        Channel::Telemetry.topic(&gateway),
        "elsewhere/topic".to_owned(),
    ];
    for (index, topic) in dropped.iter().enumerate() {
        let id = u16::try_from(index).unwrap() + 2;
        broker.send(&publish(topic, b"x", 1, false, Some(id))).await;
        assert_eq!(
            broker.read_answering_pings().await,
            Some(Packet::PubAck { id, reason: 0 })
        );
    }
    let ack_topic = Channel::TelemetryAck.topic(&gateway);
    broker
        .send(&publish(&ack_topic, &vec![0; 4097], 1, false, Some(9)))
        .await;
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::PubAck { id: 9, reason: 0 })
    );
    broker
        .send(&publish(&ack_topic, b"marker", 0, false, None))
        .await;
    let marker = harness.recv().await;
    assert_eq!(
        (marker.channel, marker.payload.as_slice()),
        (Channel::TelemetryAck, &b"marker"[..])
    );

    // With nobody taking messages, the queue fills; the next QoS 1 message is neither queued
    // nor acknowledged until the application takes one.
    let requests = Channel::LogRequest.topic(&gateway);
    let first = 100_u16;
    let count = u16::try_from(INCOMING_QUEUE).unwrap() + 1;
    for id in first..first + count {
        broker
            .send(&publish(&requests, &id.to_be_bytes(), 1, false, Some(id)))
            .await;
    }
    for id in first..first + count - 1 {
        assert_eq!(broker.read().await, Some(Packet::PubAck { id, reason: 0 }));
    }
    assert_eq!(
        broker.read().await,
        Some(Packet::PingReq),
        "the last message waits unacknowledged"
    );
    broker.send(&pingresp()).await;
    let taken = harness.recv().await;
    assert_eq!(taken.payload, first.to_be_bytes());
    assert_eq!(
        broker.read().await,
        Some(Packet::PubAck {
            id: first + count - 1,
            reason: 0
        })
    );
    for id in first + 1..first + count {
        assert_eq!(harness.recv().await.payload, id.to_be_bytes());
    }
}

#[tokio::test(start_paused = true)]
async fn a_message_on_a_channel_the_link_did_not_subscribe_to_is_acknowledged_and_dropped() {
    let mut harness = start(LinkSettings {
        subscriptions: Subscriptions::new([Channel::Config]).unwrap(),
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let gateway = gateway();
    // A broker that delivers on a topic the agent did not subscribe to has a bug. Nothing in the
    // agent would take the message, so it is acknowledged and dropped.
    let unsubscribed = [
        Channel::TelemetryAck,
        Channel::LiveControl,
        Channel::LogRequest,
    ];
    for (id, channel) in (1..).zip(unsubscribed) {
        broker
            .send(&publish(&channel.topic(&gateway), b"x", 1, false, Some(id)))
            .await;
        assert_eq!(
            broker.read_answering_pings().await,
            Some(Packet::PubAck { id, reason: 0 })
        );
    }
    broker
        .send(&publish(
            &Channel::Config.topic(&gateway),
            b"cfg",
            1,
            false,
            Some(9),
        ))
        .await;
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::PubAck { id: 9, reason: 0 })
    );
    assert_eq!(
        harness.recv().await,
        DownMessage {
            channel: Channel::Config,
            payload: b"cfg".to_vec(),
            retained: false
        },
        "the first message delivered is the one on the subscribed channel"
    );
}

#[tokio::test(start_paused = true)]
async fn without_subscriptions_no_subscribe_follows_online_and_its_puback_connects() {
    let mut harness = start(LinkSettings {
        subscriptions: Subscriptions::none(),
        ..settings()
    });
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    let Status::Online(status) = Status::decode(&online.payload).unwrap() else {
        panic!("expected online")
    };
    let online_written = Instant::now();
    // Let the agent run until it waits: it must not report Connected before online's PUBACK.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(*harness.link.state().borrow(), LinkState::Connecting);
    broker.send(&puback(online.id.unwrap(), 0)).await;

    // No SUBSCRIBE follows (MQTT 5 has none without a topic filter): the next packet is the
    // PINGREQ a whole keepalive after online, by when the link is Connected.
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    assert!(online_written.elapsed() >= Duration::from_secs(60));
    assert_eq!(
        *harness.link.state().borrow(),
        LinkState::Connected {
            session: status.session
        }
    );
    broker.send(&pingresp()).await;
    let sent = spawn_publish(&harness.link, Channel::Telemetry, b"t".to_vec());
    let Some(Packet::Publish(telemetry)) = broker.read().await else {
        panic!("expected the telemetry PUBLISH")
    };
    assert_eq!(telemetry.topic, Channel::Telemetry.topic(&gateway()));
    broker.send(&puback(telemetry.id.unwrap(), 0)).await;
    assert_eq!(
        bounded("the publish", sent).await.unwrap(),
        Ok(Published::Acknowledged(ReasonCode(0)))
    );
}

/// With subscriptions, the link is Connected once `online` is acknowledged and the SUBACK
/// grants every subscription, whichever comes last.
#[tokio::test(start_paused = true)]
async fn connected_waits_for_both_the_puback_of_online_and_the_suback_in_either_order() {
    for suback_first in [true, false] {
        let mut harness = start(settings());
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(0, &[])).await;
        let Some(Packet::Publish(online)) = broker.read().await else {
            panic!("expected online")
        };
        let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
            panic!("expected SUBSCRIBE")
        };
        let puback = puback(online.id.unwrap(), 0);
        let suback = suback(id, &vec![1; filters.len()]);
        let (first, last) = if suback_first {
            (&suback, &puback)
        } else {
            (&puback, &suback)
        };
        broker.send(first).await;
        // Let the agent run until it waits.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(
            *harness.link.state().borrow(),
            LinkState::Connecting,
            "connected after only the {}",
            if suback_first { "SUBACK" } else { "PUBACK" }
        );
        broker.send(last).await;
        harness
            .wait_state("connected", |state| {
                matches!(state, LinkState::Connected { .. })
            })
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_missing_pingresp_ends_the_connection_and_the_link_reconnects_with_a_new_session() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let (_, first) = harness.connected(&mut broker).await;
    let connected = Instant::now();
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    assert!(connected.elapsed() >= Duration::from_secs(60));
    assert_eq!(
        broker.read().await,
        None,
        "the client gave up on the PINGRESP"
    );
    assert!(connected.elapsed() >= Duration::from_secs(90));
    let state = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    assert!(format!("{state:?}").contains("PINGRESP"), "{state:?}");
    let mut broker = harness.broker().await;
    let (_, second) = harness.connected(&mut broker).await;
    assert_ne!(first, second, "every connection has its own session");
}

#[tokio::test(start_paused = true)]
async fn refused_connections_back_off_with_jitter_and_never_give_up() {
    let mut harness = start(settings());
    let mut attempts = Vec::new();
    for _ in 0..6 {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        attempts.push(Instant::now());
        broker.send(&connack(0x87, &[])).await;
        assert_eq!(broker.read().await, None);
    }
    for (index, pair) in attempts.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        let ceiling = Duration::from_secs(1 << index).min(Duration::from_secs(60));
        assert!(
            (MIN_DELAY..=ceiling).contains(&gap),
            "attempt {index}: waited {gap:?}, expected {MIN_DELAY:?}..={ceiling:?}"
        );
    }
    let state = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    assert!(
        format!("{state:?}").contains("0x87 (not authorized)"),
        "{state:?}"
    );
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    harness
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
}

/// The jitter source that always draws the longest delay, so every delay equals its ceiling.
fn longest() -> u64 {
    u64::MAX
}

/// Waits for the next connection, accepts it, and returns when its CONNECT arrived.
async fn accept_next(harness: &mut Harness) -> (FakeBroker<DuplexStream>, Instant) {
    let mut broker = harness.broker().await;
    let (_, _) = harness.connected(&mut broker).await;
    (broker, Instant::now())
}

/// Asserts that `gap` is the longest delay below `ceiling` (the draw is `u64::MAX`).
fn assert_longest(what: &str, gap: Duration, ceiling: Duration) {
    assert!(
        (ceiling - Duration::from_millis(1)..=ceiling).contains(&gap),
        "{what}: waited {gap:?}, expected just under {ceiling:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn only_a_connection_that_stayed_up_resets_the_backoff() {
    let mut harness = start(LinkSettings {
        jitter: longest,
        ..settings()
    });
    // Connections the broker accepts and drops at once: CONNACK succeeded, but none stayed up
    // for STABLE_AFTER, so the delays keep doubling (1, 2, 4, ... s) instead of starting over.
    let mut accepted = Vec::new();
    for _ in 0..7 {
        let (mut broker, at) = accept_next(&mut harness).await;
        accepted.push(at);
        broker.send(&disconnect(0x8B)).await;
    }
    for (index, pair) in accepted.windows(2).enumerate() {
        assert_longest(
            &format!("after drop {index}"),
            pair[1] - pair[0],
            Duration::from_secs(1 << index),
        );
    }
    // A connection that stays up for STABLE_AFTER (60 s) and then drops reconnects after the
    // base delay again.
    let (mut broker, _) = accept_next(&mut harness).await;
    tokio::time::sleep(STABLE_AFTER + Duration::from_secs(1)).await;
    broker.send(&disconnect(0x8B)).await;
    let dropped = Instant::now();
    let (_, again) = accept_next(&mut harness).await;
    assert_longest(
        "after a stable connection",
        again - dropped,
        Duration::from_secs(1),
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_subscription_ends_the_connection_and_never_reports_connected() {
    let mut harness = start(LinkSettings {
        jitter: longest,
        ..settings()
    });
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    assert!(matches!(
        Status::decode(&online.payload).unwrap(),
        Status::Online(_)
    ));
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
        panic!("expected SUBSCRIBE after online")
    };
    // Let the agent run until it waits: it must not report Connected before the SUBACK.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(*harness.link.state().borrow(), LinkState::Connecting);

    // The broker refuses one down channel (0x87, not authorized) and grants the others.
    let refused = &filters[1].0;
    let mut reasons = vec![1; filters.len()];
    reasons[1] = 0x87;
    broker.send(&suback(id, &reasons)).await;
    assert_eq!(
        broker.read().await,
        Some(Packet::Disconnect { reason: 0x04 }),
        "DISCONNECT 0x04, so the broker publishes the will (Mosquitto drops it for 0x83)"
    );
    let ended = Instant::now();
    assert_eq!(broker.read().await, None);
    assert!(
        !matches!(*harness.link.state().borrow(), LinkState::Connected { .. }),
        "a refused subscription never counts as connected"
    );
    let LinkState::Disconnected { error, retry_in } = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await
    else {
        unreachable!()
    };
    assert!(
        error.contains(refused.as_str()) && error.contains("0x87 (not authorized)"),
        "{error}"
    );

    // The next attempt waits for the backoff (the longest first delay, 1 s), then connects;
    // the state reported that delay.
    assert_longest("retry_in", retry_in, Duration::from_secs(1));
    let (_next, _) = accept_next(&mut harness).await;
    assert_longest("the reconnect", ended.elapsed(), Duration::from_secs(1));
    harness
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
}

/// Every broken packet ends the connection with DISCONNECT 0x04, so that the broker publishes
/// the will (Mosquitto drops it after the matching codes 0x81, 0x82, 0x94 and 0x95); the kind of
/// error is in the link's state (and the agent's log).
#[tokio::test(start_paused = true)]
async fn broken_packets_end_the_connection_with_the_will() {
    let mut harness = start(settings());
    let gateway_config = Channel::Config.topic(&gateway());
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("reserved packet type", vec![0x00, 0x00], "malformed packet"),
        // A Remaining Length of 0 in two bytes: not its shortest encoding (MQTT-1.5.5-1).
        (
            "overlong Remaining Length",
            vec![0xD0, 0x80, 0x00],
            "not in its shortest encoding",
        ),
        (
            "wildcard in a response topic",
            vec![0x30, 9, 0, 1, b'a', 5, 0x08, 0, 2, b'r', b'#'],
            "wildcard in a response topic",
        ),
        ("AUTH", vec![0xF0, 0x00], "unexpected Auth packet"),
        (
            "declared size above the maximum",
            vec![0x30, 0x80, 0x80, 0x80, 0x01],
            "above the maximum packet size",
        ),
        (
            "topic alias",
            vec![0x30, 7, 0, 1, b'a', 3, 0x23, 0, 1],
            "topic alias",
        ),
        ("second CONNACK", connack(0, &[]), "protocol error"),
        (
            "QoS 2 PUBLISH",
            publish(&gateway_config, b"x", 2, false, Some(3)),
            "protocol error",
        ),
        ("SUBACK for nothing", suback(999, &[1]), "protocol error"),
        // Acknowledgements of nothing the agent sent.
        ("PUBACK for nothing", puback(999, 0), "protocol error"),
        (
            "PUBACK with packet identifier 0",
            puback(0, 0),
            "malformed packet: packet identifier 0",
        ),
        (
            "second PUBACK for the online status",
            puback(1, 0),
            "protocol error",
        ),
        (
            "PINGRESP without a PINGREQ",
            pingresp().to_vec(),
            "protocol error",
        ),
        // Reason codes MQTT 5 does not allow in that packet.
        (
            "PUBACK with reason code 0x01",
            puback(1, 0x01),
            "protocol error",
        ),
        (
            "PUBACK with reason code 0x7F",
            puback(1, 0x7F),
            "protocol error",
        ),
        (
            "DISCONNECT with reason code 0x04",
            disconnect(0x04),
            "protocol error",
        ),
        (
            "DISCONNECT with reason code 0x10",
            disconnect(0x10),
            "protocol error",
        ),
    ];
    let connected = |state: &LinkState| matches!(state, LinkState::Connected { .. });
    let disconnected = |state: &LinkState| matches!(state, LinkState::Disconnected { .. });
    for (what, bytes, kind) in cases {
        let mut broker = harness.broker().await;
        harness.connected(&mut broker).await;
        // Connected first, so the state checked below is this connection's end.
        harness.wait_state("connected", connected).await;
        broker.send(&bytes).await;
        // Bounded: an agent that ignores the packet would keep pinging forever.
        assert_eq!(
            bounded(what, broker.read_answering_pings()).await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "{what}: DISCONNECT 0x04, so the broker publishes the will"
        );
        assert_eq!(
            broker.read().await,
            None,
            "{what}: the client closes after DISCONNECT"
        );
        let LinkState::Disconnected { error, .. } =
            harness.wait_state("disconnected", disconnected).await
        else {
            unreachable!()
        };
        assert!(error.contains(kind), "{what}: {error}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_puback_with_a_reason_code_mqtt_5_does_not_allow_is_never_a_success() {
    let mut harness = start(settings());
    for code in [0x01, 0x02, 0x7F] {
        let mut broker = harness.broker().await;
        harness.connected(&mut broker).await;
        let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
        let Some(Packet::Publish(sent)) = broker.read().await else {
            panic!("expected the PUBLISH")
        };
        broker.send(&puback(sent.id.unwrap(), code)).await;
        assert_eq!(
            broker.read().await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "PUBACK 0x{code:02X} is a protocol error, which ends the connection with the will"
        );
        assert_eq!(broker.read().await, None);
        assert_eq!(
            bounded("the publisher", publisher).await.unwrap(),
            Err(PublishError::ConnectionLost),
            "PUBACK 0x{code:02X} must not count as delivered"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn connack_and_suback_codes_outside_their_sets_end_the_connection() {
    let mut harness = start(settings());
    // CONNACK 0x01 is no CONNACK reason code: neither a success nor a refusal.
    for code in [0x01, 0x10, 0x7F] {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(code, &[])).await;
        assert_eq!(
            broker.read().await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "CONNACK 0x{code:02X}"
        );
        assert_eq!(broker.read().await, None);
    }
    // SUBACK: 0x02 grants more than the QoS 1 asked for, 0x7F is no SUBACK code at all.
    for code in [0x02, 0x7F] {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(0, &[])).await;
        let Some(Packet::Publish(online)) = broker.read().await else {
            panic!("expected online")
        };
        broker.send(&puback(online.id.unwrap(), 0)).await;
        let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
            panic!("expected SUBSCRIBE")
        };
        let mut reasons = vec![1; filters.len()];
        reasons[0] = code;
        broker.send(&suback(id, &reasons)).await;
        assert_eq!(
            broker.read().await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "SUBACK 0x{code:02X}"
        );
        assert_eq!(broker.read().await, None);
        assert!(
            !matches!(*harness.link.state().borrow(), LinkState::Connected { .. }),
            "SUBACK 0x{code:02X} never counts as connected"
        );
    }
}

/// A CONNACK that breaks a rule of MQTT 5 is a protocol error like any other packet: DISCONNECT
/// 0x04, no online status, and the error names the rule.
#[tokio::test(start_paused = true)]
async fn a_connack_that_breaks_mqtt_5_ends_the_connection_with_the_will() {
    let mut harness = start(settings());
    let disconnected = |state: &LinkState| matches!(state, LinkState::Disconnected { .. });
    let cases: [(&str, Vec<u8>, &str); 5] = [
        (
            "Session Present after the clean start",
            vec![0x20, 3, 0x01, 0x00, 0],
            "session present after a clean start",
        ),
        (
            "Session Present on a refusal",
            vec![0x20, 3, 0x01, 0x87, 0],
            "Session Present on a refused CONNACK",
        ),
        (
            "CONNACK without a property length",
            vec![0x20, 2, 0x00, 0x00],
            "CONNACK without a property length",
        ),
        (
            "Authentication Method without one in CONNECT",
            vec![0x20, 8, 0, 0, 5, 0x15, 0, 2, b'a', b'b'],
            "property not allowed in this packet",
        ),
        (
            "Remaining Length 3 in two bytes",
            vec![0x20, 0x83, 0x00, 0, 0, 0],
            "not in its shortest encoding",
        ),
    ];
    for (what, bytes, kind) in cases {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&bytes).await;
        assert_eq!(
            broker.read().await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "{what}: DISCONNECT 0x04 and no online status"
        );
        assert_eq!(
            broker.read().await,
            None,
            "{what}: the client closes after DISCONNECT"
        );
        let LinkState::Disconnected { error, .. } =
            harness.wait_state("disconnected", disconnected).await
        else {
            unreachable!()
        };
        assert!(error.contains(kind), "{what}: {error}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_broker_that_cannot_deliver_at_qos_1_or_retain_is_never_connected() {
    let mut harness = start(settings());
    let disconnected = |state: &LinkState| matches!(state, LinkState::Disconnected { .. });

    // A CONNACK that accepts the QoS 1, retained will while announcing Maximum QoS 0 (0x24) or
    // Retain Available 0 (0x25), which MQTT 5 requires the broker to refuse: a protocol error,
    // before any status is published with QoS 1 and retain.
    for (property, what) in [
        ([0x24, 0], "Maximum QoS 0"),
        ([0x25, 0], "Retain Available 0"),
    ] {
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(0, &property)).await;
        assert_eq!(
            broker.read().await,
            Some(Packet::Disconnect { reason: 0x04 }),
            "no online status to a broker with {what}"
        );
        assert_eq!(broker.read().await, None);
        let LinkState::Disconnected { error, .. } =
            harness.wait_state("disconnected", disconnected).await
        else {
            unreachable!()
        };
        assert!(error.contains(what), "{error}");
    }

    // Maximum QoS 1 is fine, but a SUBACK that grants one down channel only QoS 0 ends the
    // connection like a refusal: DISCONNECT 0x04 (the broker publishes the will) and a backoff.
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[0x24, 1])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
        panic!("expected SUBSCRIBE")
    };
    let downgraded = filters[2].0.clone();
    let mut reasons = vec![1; filters.len()];
    reasons[2] = 0x00;
    broker.send(&suback(id, &reasons)).await;
    assert_eq!(
        broker.read().await,
        Some(Packet::Disconnect { reason: 0x04 }),
        "a QoS 0 grant is no subscription the gateway can rely on"
    );
    assert_eq!(broker.read().await, None);
    assert!(
        !matches!(*harness.link.state().borrow(), LinkState::Connected { .. }),
        "a QoS 0 grant never counts as connected"
    );
    let LinkState::Disconnected { error, .. } =
        harness.wait_state("disconnected", disconnected).await
    else {
        unreachable!()
    };
    assert!(
        error.contains(downgraded.as_str()) && error.contains("only QoS 0"),
        "{error}"
    );

    // Every subscription at QoS 1 (and retained messages available): connected.
    let mut broker = harness.broker().await;
    harness
        .connected_with(&mut broker, &connack(0, &[0x24, 1, 0x25, 1]))
        .await;
    harness
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_from_the_broker_is_reported_and_followed_by_a_reconnect() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    assert!(matches!(broker.read().await, Some(Packet::Publish(_))));
    broker.send(&disconnect(0x98)).await;
    assert_eq!(broker.read().await, None);
    assert_eq!(
        bounded("the publisher", publisher).await.unwrap(),
        Err(PublishError::ConnectionLost),
        "a publish without PUBACK when the connection ends may or may not have arrived"
    );
    let state = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    assert!(
        format!("{state:?}").contains("0x98 (administrative action)"),
        "{state:?}"
    );
    harness.broker().await.accept().await;
}

#[tokio::test(start_paused = true)]
async fn a_clean_stop_publishes_offline_then_disconnects_normally() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let (_, session) = harness.connected(&mut broker).await;
    harness.shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected the offline status")
    };
    assert_eq!(offline.topic, Channel::Status.topic(&gateway()));
    assert!(offline.retain && offline.qos == 1);
    let Status::Offline(status) = Status::decode(&offline.payload).unwrap() else {
        panic!("expected offline")
    };
    assert_eq!(
        (status.session, status.reason),
        (session, OfflineReason::Shutdown)
    );
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(
        broker.read().await,
        Some(Packet::Disconnect { reason: 0 }),
        "a normal DISCONNECT makes the broker discard the will"
    );
    assert_eq!(broker.read().await, None);
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(*harness.link.state().borrow(), LinkState::Stopped);
    assert_eq!(
        harness.link.publish(Channel::Telemetry, vec![1]).await,
        Err(PublishError::Stopped)
    );
}

/// However a stop ends the connection, a publish written without its PUBACK may have reached the
/// broker (`ConnectionLost`); only a request still in the queue was never sent (`Stopped`).
#[tokio::test(start_paused = true)]
async fn a_stop_reports_written_publishes_as_maybe_delivered_and_queued_ones_as_not_sent() {
    // A clean stop: offline/shutdown is acknowledged, then DISCONNECT 0x00.
    let mut harness = start(LinkSettings {
        inflight_limit: 2,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let written = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(_)) = broker.read().await else {
        panic!("expected the PUBLISH")
    };
    harness.shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected offline/shutdown")
    };
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(
        bounded("the written publish", written).await.unwrap(),
        Err(PublishError::ConnectionLost),
        "written without a PUBACK: it may have reached the broker"
    );

    // A stop with the in-flight window full: offline/shutdown cannot be sent, so after the stop
    // deadline the agent disconnects with the will.
    let mut harness = start(LinkSettings {
        inflight_limit: 1,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let written = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(_)) = broker.read().await else {
        panic!("expected the PUBLISH")
    };
    let queued: Vec<_> = (0..REQUEST_QUEUE)
        .map(|index| {
            spawn_publish(
                &harness.link,
                Channel::Telemetry,
                vec![2, u8::try_from(index).unwrap()],
            )
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(1)).await;
    let waiting = spawn_waiting_publish(&harness.link, Channel::Telemetry, || vec![3]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        (harness.link.in_flight(), harness.link.queued()),
        (1, REQUEST_QUEUE)
    );
    harness.shutdown.cancel();
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::Disconnect { reason: 0x04 })
    );
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(
        bounded("the written publish", written).await.unwrap(),
        Err(PublishError::ConnectionLost),
        "written without a PUBACK: it may have reached the broker"
    );
    for publisher in queued {
        assert_eq!(
            bounded("a queued publish", publisher).await.unwrap(),
            Err(PublishError::Stopped),
            "never written"
        );
    }
    assert_eq!(
        bounded("the waiter", waiting).await.unwrap(),
        Err(PublishError::Stopped),
        "a wait for a place ends with the stop"
    );
}

/// A QoS 0 publish whose write fails during a stop may have been sent in part: it is
/// `ConnectionLost`, not `Stopped` (which the stop would make of a dropped reply).
#[tokio::test(start_paused = true)]
async fn a_qos_0_write_that_fails_during_a_stop_is_not_reported_as_never_sent() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    // A heartbeat larger than the stream's buffer (256 KiB), which the broker never reads: the
    // write blocks until the stop's deadline cuts it short.
    let blocked = spawn_publish(&harness.link, Channel::Heartbeat, vec![0; 1024 * 1024]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    harness.shutdown.cancel();
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(
        bounded("the blocked publish", blocked).await.unwrap(),
        Err(PublishError::ConnectionLost)
    );
    drop(broker);
}

/// A stop while a write is stuck (a large message on an uplink that does not drain) ends at the
/// stop's deadline, not at the write timeout, which would hold the stop past the agent's
/// STOP_LIMIT. Nothing follows the part of the packet already written: no DISCONNECT, which
/// the broker would read as the rest of that packet, so it publishes the will.
#[tokio::test(start_paused = true)]
async fn a_stop_cuts_a_stuck_write_at_the_stop_deadline_and_sends_nothing_after_it() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    // A heartbeat larger than the stream's buffer (256 KiB), which the broker does not read.
    let blocked = spawn_publish(&harness.link, Channel::Heartbeat, vec![0; 1024 * 1024]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    let stopping = Instant::now();
    harness.shutdown.cancel();
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    // The stop's deadline, 5 seconds (plus at most the 2 seconds a close may take), not the
    // write timeout of 1.5 keepalives counted from the start of the write.
    let took = stopping.elapsed();
    assert!(
        (Duration::from_secs(5)..=Duration::from_secs(7)).contains(&took),
        "the stop took {took:?}; the write timeout is {:?}",
        settings().keep_alive * 3 / 2
    );
    assert_eq!(
        bounded("the blocked publish", blocked).await.unwrap(),
        Err(PublishError::ConnectionLost)
    );
    // What the broker has not read is the start of the heartbeat's PUBLISH and nothing else: its
    // payload is zeros, where a DISCONNECT would end in E0 00 or E0 01 04.
    let rest = bounded("the rest of the stream", broker.read_raw_to_end()).await;
    assert_eq!(rest.first(), Some(&0x30), "a QoS 0 PUBLISH comes first");
    assert!(rest.len() < 1024 * 1024, "{} bytes", rest.len());
    let tail = &rest[rest.len() - 64..];
    assert!(tail.iter().all(|&byte| byte == 0), "{tail:02x?}");
}

#[tokio::test(start_paused = true)]
async fn without_a_puback_for_offline_the_stop_asks_the_broker_for_the_will() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    harness.shutdown.cancel();
    let stopping = Instant::now();
    assert!(matches!(
        broker.read_answering_pings().await,
        Some(Packet::Publish(_))
    ));
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::Disconnect { reason: 0x04 }),
        "0x04: disconnect with will message"
    );
    assert!(stopping.elapsed() >= Duration::from_secs(5));
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
}

/// A connector that always fails at the TCP step.
struct Unreachable;

impl Connector for Unreachable {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        Err(ConnectError::new(ConnectStage::Tcp, "connection refused"))
    }
}

#[tokio::test(start_paused = true)]
async fn publishing_fails_at_once_while_offline_and_checks_the_channel_first() {
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = link::link(settings(), Unreachable, shutdown.clone());
    let supervisor = tokio::spawn(supervisor.run());
    let mut state = link.state();
    bounded(
        "a failed attempt",
        state.wait_for(|state| matches!(state, LinkState::Disconnected { .. })),
    )
    .await
    .unwrap();
    assert_eq!(
        link.publish(Channel::Telemetry, vec![1]).await,
        Err(PublishError::NotConnected)
    );
    assert_eq!(
        link.publish(Channel::Config, vec![1]).await,
        Err(PublishError::NotUp(Channel::Config))
    );
    assert_eq!(
        link.publish(Channel::Status, vec![1]).await,
        Err(PublishError::StatusIsReserved)
    );
    let too_large = link.publish(Channel::Live, vec![0; 64 * 1024 + 1]).await;
    assert_eq!(
        too_large,
        Err(PublishError::TooLarge {
            size: 64 * 1024 + 1,
            limit: 64 * 1024
        })
    );
    // `reserve` follows the same rules: the channel first, then no place while offline.
    assert_eq!(
        link.reserve(Channel::Telemetry)
            .now_or_never()
            .map(Result::err),
        Some(Some(PublishError::NotConnected))
    );
    assert_eq!(
        link.reserve(Channel::Config).await.err(),
        Some(PublishError::NotUp(Channel::Config))
    );
    assert_eq!(
        link.reserve(Channel::Status).await.err(),
        Some(PublishError::StatusIsReserved)
    );
    assert_eq!(link.queued(), 0);
    shutdown.cancel();
    bounded("the supervisor to stop", supervisor).await.unwrap();
    assert_eq!(
        link.reserve(Channel::Heartbeat).await.err(),
        Some(PublishError::Stopped)
    );
    assert_eq!(
        link.publish(Channel::Heartbeat, vec![1]).await,
        Err(PublishError::Stopped)
    );
}

/// The supervisor future dropped instead of stopped (with its runtime, say): a publish written
/// without its PUBACK may have arrived (`ConnectionLost`), one still queued was never written
/// (`Stopped`), and so is every later one.
#[tokio::test(start_paused = true)]
async fn when_the_supervisor_goes_away_written_publishes_are_lost_and_queued_ones_stopped() {
    let mut harness = start(LinkSettings {
        inflight_limit: 1,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let written = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(_)) = broker.read().await else {
        panic!("expected the first publish")
    };
    // The in-flight window (1) is full: the next one waits in the queue.
    let queued = spawn_publish(&harness.link, Channel::Telemetry, vec![2]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(harness.link.queued(), 1);
    harness.supervisor.abort();
    assert_eq!(
        bounded("the written publish", written).await.unwrap(),
        Err(PublishError::ConnectionLost)
    );
    assert_eq!(
        bounded("the queued publish", queued).await.unwrap(),
        Err(PublishError::Stopped)
    );
    assert_eq!(
        harness.link.publish(Channel::Telemetry, vec![3]).await,
        Err(PublishError::Stopped)
    );
}

/// What `publish` and `reserve` answer, at once and on both queues, while the link is not
/// Connected; nothing is queued.
fn refused_at_once(link: &Link, error: &PublishError, what: &str) {
    for channel in [Channel::Telemetry, Channel::Heartbeat] {
        assert_eq!(
            link.publish(channel, vec![1]).now_or_never(),
            Some(Err(error.clone())),
            "{what}: publish on {channel:?}"
        );
        assert_eq!(
            link.reserve(channel).now_or_never().map(Result::err),
            Some(Some(error.clone())),
            "{what}: reserve on {channel:?}"
        );
    }
    assert_eq!(link.queued(), 0, "{what}: nothing queued");
}

/// The next PUBLISH the broker reads is `marker`, published now: nothing refused or left over
/// earlier was kept for this connection.
async fn next_publish_is_a_fresh_one(harness: &Harness, broker: &mut FakeBroker<DuplexStream>) {
    let marker = spawn_publish(&harness.link, Channel::Heartbeat, b"marker".to_vec());
    let Some(Packet::Publish(first)) = broker.read_answering_pings().await else {
        panic!("expected a PUBLISH")
    };
    assert_eq!(first.payload, b"marker", "the first message after setup");
    assert_eq!(
        bounded("the marker", marker).await.unwrap(),
        Ok(Published::Sent)
    );
}

/// Publishes are accepted only while the link is Connected: during CONNECT, the CONNACK, the
/// `online` PUBACK and the SUBACK they fail at once and are never sent later.
#[tokio::test(start_paused = true)]
async fn publishes_during_connect_and_setup_fail_at_once_and_are_never_sent_later() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    refused_at_once(
        &harness.link,
        &PublishError::NotConnected,
        "CONNACK pending",
    );
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read_answering_pings().await else {
        panic!("expected online")
    };
    refused_at_once(
        &harness.link,
        &PublishError::NotConnected,
        "online's PUBACK pending",
    );
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { id, filters }) = broker.read_answering_pings().await else {
        panic!("expected SUBSCRIBE")
    };
    refused_at_once(&harness.link, &PublishError::NotConnected, "SUBACK pending");
    broker.send(&suback(id, &vec![1; filters.len()])).await;
    harness
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
}

/// Requests a connection accepted but had not written when it ended, on both queues and in a
/// reserved place, are answered `NotConnected` (nothing was sent), and the next connection
/// starts with empty queues.
#[tokio::test(start_paused = true)]
async fn requests_still_queued_when_a_connection_ends_are_answered_and_never_sent_later() {
    let mut harness = start(LinkSettings {
        inflight_limit: 1,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    // QoS 1: one in flight without a PUBACK, the next waits for the window.
    let written = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(_)) = broker.read().await else {
        panic!("expected the first publish")
    };
    let queued_qos_1 = spawn_publish(&harness.link, Channel::Telemetry, vec![2]);
    // QoS 0: a heartbeat larger than the stream's buffer (256 KiB), which the broker does not
    // read, holds the connection in its write; the next heartbeat waits in the queue.
    let stuck = spawn_publish(&harness.link, Channel::Heartbeat, vec![0; 1024 * 1024]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    let queued_qos_0 = spawn_publish(&harness.link, Channel::Heartbeat, vec![3]);
    let permit = harness.link.reserve(Channel::Log).await.unwrap();
    // The rest of the QoS 1 queue in reserved places, and a caller waiting for one.
    let places: Vec<_> = (2..REQUEST_QUEUE)
        .map(|_| {
            harness
                .link
                .reserve(Channel::Telemetry)
                .now_or_never()
                .unwrap()
                .unwrap()
        })
        .collect();
    let waiting = spawn_waiting_publish(&harness.link, Channel::Telemetry, || vec![5]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        harness.link.queued(),
        REQUEST_QUEUE + 1,
        "a full QoS 1 queue and one QoS 0 request"
    );
    assert!(!waiting.is_finished(), "the waiter waits for a place");
    drop(broker);
    for (what, publisher) in [
        ("the written publish", written),
        ("the stuck heartbeat", stuck),
    ] {
        assert_eq!(
            bounded(what, publisher).await.unwrap(),
            Err(PublishError::ConnectionLost),
            "{what} may have been sent"
        );
    }
    for (what, publisher) in [("QoS 1", queued_qos_1), ("QoS 0", queued_qos_0)] {
        assert_eq!(
            bounded(what, publisher).await.unwrap(),
            Err(PublishError::NotConnected),
            "the queued {what} request was never sent"
        );
    }
    assert_eq!(
        bounded("the waiter", waiting).await.unwrap(),
        Err(PublishError::NotConnected),
        "the wait for a place ends with its connection"
    );
    assert_eq!(
        harness.link.queued(),
        0,
        "the queues ended with the connection"
    );
    assert_eq!(
        bounded("the reserved place", permit.publish(vec![4])).await,
        Err(PublishError::NotConnected)
    );
    drop(places);
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    assert_eq!(
        harness.link.queued(),
        0,
        "the new connection's queues are empty"
    );
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
}

/// A message for a session (a heartbeat names its session) goes only to that session's
/// connection: once it has ended, it fails at once, even while the next connection is up.
#[tokio::test(start_paused = true)]
async fn a_message_for_an_ended_session_is_never_sent_on_the_next_connection() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let (_, first) = harness.connected(&mut broker).await;
    assert_eq!(
        harness
            .link
            .publish_in_session(&first, Channel::Heartbeat, b"one".to_vec())
            .await,
        Ok(Published::Sent)
    );
    let Some(Packet::Publish(sent)) = broker.read().await else {
        panic!("expected the heartbeat")
    };
    assert_eq!(sent.payload, b"one");
    drop(broker);
    let mut broker = harness.broker().await;
    let (_, second) = harness.connected(&mut broker).await;
    assert_ne!(first, second);
    assert_eq!(
        harness
            .link
            .publish_in_session(&first, Channel::Heartbeat, b"stale".to_vec())
            .now_or_never(),
        Some(Err(PublishError::NotConnected))
    );
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
}

/// A reserved place belongs to its connection: a permit refused for its size gives the place
/// back, one used after its connection ended fails, and once the link stops publishing and
/// reserving fail at once with `Stopped`, a permit included.
#[tokio::test(start_paused = true)]
async fn a_permit_belongs_to_the_connection_it_was_reserved_on() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let permit = harness.link.reserve(Channel::Live).await.unwrap();
    assert_eq!(permit.channel(), Channel::Live);
    assert_eq!(harness.link.queued(), 1);
    assert_eq!(
        permit.publish(vec![0; 64 * 1024 + 1]).await,
        Err(PublishError::TooLarge {
            size: 64 * 1024 + 1,
            limit: 64 * 1024
        })
    );
    assert_eq!(
        harness.link.queued(),
        0,
        "a refused permit gives its place back"
    );
    let old = harness.link.reserve(Channel::Telemetry).await.unwrap();
    drop(broker);
    harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    assert_eq!(
        bounded("a permit of the ended connection", old.publish(vec![1])).await,
        Err(PublishError::NotConnected)
    );
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
    let late = harness.link.reserve(Channel::Telemetry).await.unwrap();
    harness.shutdown.cancel();
    assert_eq!(
        late.publish(vec![1]).now_or_never(),
        Some(Err(PublishError::Stopped)),
        "a permit used once the stop began"
    );
    refused_at_once(&harness.link, &PublishError::Stopped, "stopping");
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    refused_at_once(&harness.link, &PublishError::Stopped, "stopped");
}

#[tokio::test(start_paused = true)]
async fn heartbeats_follow_every_connection_with_its_session() {
    let mut harness = start(settings());
    let heartbeats = tokio::spawn(agent::heartbeat::run(
        harness.link.clone(),
        agent::heartbeat::HeartbeatSettings {
            interval: Duration::from_secs(30),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: tokio::sync::watch::channel(protocol::UnixMillis::new(
                1_900_000_000_000,
            ))
            .1,
            buffer: agent::upload::Stats::default(),
            clock: agent::clock::GatewayClock::system(),
            root_crl: tokio::sync::watch::channel(None).1,
            health: agent::heartbeat::HealthSources::none(),
        },
        Instant::now(),
        harness.shutdown.clone(),
    ));
    let mut sessions: Vec<SessionId> = Vec::new();
    for _ in 0..2 {
        let mut broker = harness.broker().await;
        let (_, session) = harness.connected(&mut broker).await;
        let connected = Instant::now();
        for beat in 0..2 {
            let Some(Packet::Publish(message)) = broker.read_answering_pings().await else {
                panic!("expected a heartbeat")
            };
            assert_eq!(message.topic, Channel::Heartbeat.topic(&gateway()));
            assert_eq!((message.qos, message.retain), (0, false));
            let heartbeat = Heartbeat::decode(&message.payload).unwrap();
            assert_eq!(heartbeat.session, session);
            assert_eq!(heartbeat.interval_s, 30);
            let expected = Duration::from_secs(30 * beat);
            let elapsed = connected.elapsed();
            assert!(
                (expected..expected + Duration::from_secs(1)).contains(&elapsed),
                "heartbeat {beat} after {elapsed:?}"
            );
        }
        sessions.push(session);
        // End this connection; the next one gets its own session and heartbeats.
        broker.send(&disconnect(0x8B)).await;
    }
    assert_ne!(sessions[0], sessions[1]);
    harness.shutdown.cancel();
    bounded("the heartbeats to stop", heartbeats).await.unwrap();
}

/// Laysi/trustiot-v2#117, security review L1: a host file whose read never returns (here a named
/// pipe nobody writes to, standing for a sysfs file whose firmware call hangs) does not stop the
/// heartbeats: they keep coming, without the temperature. How late each one may be is bounded by
/// the limit in `heartbeat.rs`, whose unit test moves the clock to it; here the clock is moved
/// only to get past it, so the gaps between heartbeats are not asserted.
#[tokio::test(start_paused = true)]
async fn heartbeats_go_on_while_a_host_file_never_answers() {
    let scratch = testkit::mqtt::ScratchDir::new("agent-link-heartbeat-blocked");
    let fifo = scratch.path().join("temperature");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo {}", fifo.display());
    // Lets the stuck read through when the test ends, also when it fails: the runtime waits for
    // its blocking threads, and one stuck on the pipe would hang the test run.
    let _release = ReleasePipe(fifo.clone());
    let interval = Duration::from_secs(30);
    let mut harness = start(settings());
    let heartbeats = tokio::spawn(agent::heartbeat::run(
        harness.link.clone(),
        agent::heartbeat::HeartbeatSettings {
            interval,
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: tokio::sync::watch::channel(protocol::UnixMillis::new(
                1_900_000_000_000,
            ))
            .1,
            buffer: agent::upload::Stats::default(),
            clock: agent::clock::GatewayClock::system(),
            root_crl: tokio::sync::watch::channel(None).1,
            health: agent::heartbeat::HealthSources {
                host: agent::host::HostPaths {
                    temperature: fifo.clone(),
                    ..agent::host::HostPaths::nowhere()
                },
                ..agent::heartbeat::HealthSources::none()
            },
        },
        Instant::now(),
        harness.shutdown.clone(),
    ));
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    for beat in 0..4 {
        // The paused clock does not move on its own while a blocking thread is busy, and one
        // is stuck on the pipe: the test moves it, a second at a time, until the heartbeat
        // arrives.
        let received = {
            let mut read = std::pin::pin!(broker.read_answering_pings());
            loop {
                tokio::select! {
                    packet = &mut read => break packet,
                    () = tokio::time::advance(Duration::from_secs(1)) => {}
                }
            }
        };
        let Some(Packet::Publish(message)) = received else {
            panic!("expected heartbeat {beat} although a host file blocks")
        };
        assert_eq!(message.topic, Channel::Heartbeat.topic(&gateway()));
        let heartbeat = Heartbeat::decode(&message.payload).unwrap();
        assert_eq!(heartbeat.system.soc_temp_celsius, None, "heartbeat {beat}");
    }
    harness.shutdown.cancel();
    bounded("the heartbeats to stop", async {
        let mut heartbeats = heartbeats;
        loop {
            tokio::select! {
                done = &mut heartbeats => return done,
                () = tokio::time::advance(Duration::from_secs(1)) => {}
            }
        }
    })
    .await
    .unwrap();
}

/// Opens and closes the write end of a named pipe when dropped, which ends a read of it that
/// waits for a writer (the pipe is then empty). Without a reader waiting, nothing happens.
struct ReleasePipe(std::path::PathBuf);

impl Drop for ReleasePipe {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.0);
    }
}

#[tokio::test(start_paused = true)]
async fn a_server_keep_alive_of_zero_keeps_the_agents_own_keepalive() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    // Server Keep Alive 0: the broker does not expect pings, but the agent still needs to
    // notice a dead connection.
    harness
        .connected_with(&mut broker, &connack(0, &[0x13, 0, 0]))
        .await;
    let connected = Instant::now();
    assert_eq!(
        bounded("a PINGREQ", broker.read()).await,
        Some(Packet::PingReq)
    );
    let pinged = connected.elapsed();
    assert!(
        (Duration::from_secs(60)..Duration::from_secs(61)).contains(&pinged),
        "{pinged:?}"
    );
    assert_eq!(
        bounded("the close", broker.read()).await,
        None,
        "without a PINGRESP the agent gives up"
    );
    let closed = connected.elapsed();
    assert!(
        (Duration::from_secs(90)..Duration::from_secs(91)).contains(&closed),
        "{closed:?}"
    );
    let state = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    assert!(format!("{state:?}").contains("PINGRESP"), "{state:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_while_waiting_for_connack_still_ends_with_offline_and_a_normal_disconnect() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = bounded("CONNECT", broker.read()).await else {
        panic!("expected CONNECT")
    };
    harness.shutdown.cancel();
    tokio::time::sleep(Duration::from_secs(1)).await;
    broker.send(&connack(0, &[])).await;
    // The session's first message is still its online; offline/shutdown follows at once.
    let Some(Packet::Publish(online)) = bounded("online", broker.read()).await else {
        panic!("the stream was dropped instead of a clean stop")
    };
    let Status::Online(online_status) = Status::decode(&online.payload).unwrap() else {
        panic!("expected online first")
    };
    let Some(Packet::Publish(offline)) = bounded("offline", broker.read()).await else {
        panic!("expected offline/shutdown")
    };
    let Status::Offline(offline_status) = Status::decode(&offline.payload).unwrap() else {
        panic!("expected offline")
    };
    assert_eq!(
        (offline_status.session, offline_status.reason),
        (online_status.session, OfflineReason::Shutdown)
    );
    broker.send(&puback(online.id.unwrap(), 0)).await;
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(
        bounded("DISCONNECT", broker.read()).await,
        Some(Packet::Disconnect { reason: 0 }),
        "a normal DISCONNECT, so the broker discards the will"
    );
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(*harness.link.state().borrow(), LinkState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn a_stop_without_any_connack_gives_up_after_the_stop_deadline() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = bounded("CONNECT", broker.read()).await else {
        panic!("expected CONNECT")
    };
    harness.shutdown.cancel();
    let stopping = Instant::now();
    assert_eq!(bounded("the close", broker.read()).await, None);
    assert!(stopping.elapsed() <= Duration::from_secs(5));
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
}

/// A stop that begins while the CONNECT is still being written keeps one deadline, counted from
/// the shutdown: the CONNACK wait that follows does not start a new one.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_connect_write_keeps_one_deadline_for_connack() {
    // 16 bytes of buffer: the CONNECT write waits for the broker to read.
    let mut harness = start_with(settings(), DuplexConnector::with_buffer(16));
    let mut broker = harness.broker().await;
    let stopping = Instant::now();
    harness.shutdown.cancel();
    // The broker takes the CONNECT 3 seconds into the stop and never sends CONNACK.
    let read_after = Duration::from_secs(3);
    tokio::time::sleep(read_after).await;
    let Some(Packet::Connect(_)) = bounded("CONNECT", broker.read()).await else {
        panic!("expected CONNECT")
    };
    assert_eq!(bounded("the close", broker.read()).await, None);
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    // The stop's 5 seconds from the shutdown, not 5 more from the end of the write.
    let took = stopping.elapsed();
    assert!(
        took >= Duration::from_secs(5) && took < read_after + Duration::from_secs(5),
        "the stop took {took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn reason_strings_from_the_broker_lose_their_control_characters() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    broker
        .send(&support::disconnect_with_reason_string(
            0x98,
            "line one\r\nline two\tend\u{85}\u{1b}[31m",
        ))
        .await;
    let LinkState::Disconnected { error, .. } = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await
    else {
        unreachable!()
    };
    assert!(!error.chars().any(char::is_control), "{error:?}");
    assert!(error.contains("line one  line two end  [31m"), "{error:?}");

    // The error (and so the log line) keeps at most 200 characters of it.
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    broker
        .send(&support::disconnect_with_reason_string(
            0x98,
            &"y".repeat(300),
        ))
        .await;
    let LinkState::Disconnected { error, .. } = harness
        .wait_state(
            "disconnected with the long reason",
            |state| matches!(state, LinkState::Disconnected { error, .. } if error.contains("yyy")),
        )
        .await
    else {
        unreachable!()
    };
    assert!(
        error.contains(&"y".repeat(200)) && !error.contains(&"y".repeat(201)),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_puback_that_never_comes_ends_the_connection_after_its_deadline() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(_)) = bounded("the PUBLISH", broker.read()).await else {
        panic!("expected the PUBLISH")
    };
    let sent = Instant::now();
    // The broker answers every PINGREQ but never the PUBLISH. The agent gives up with
    // DISCONNECT 0x04, so the broker publishes the will.
    assert_eq!(
        bounded("the DISCONNECT", broker.read_answering_pings()).await,
        Some(Packet::Disconnect { reason: 0x04 })
    );
    let waited = sent.elapsed();
    assert!(
        (Duration::from_secs(120)..Duration::from_secs(121)).contains(&waited),
        "the deadline is twice the keepalive: {waited:?}"
    );
    assert_eq!(bounded("the close", broker.read()).await, None);
    assert_eq!(
        bounded("the publisher", publisher).await.unwrap(),
        Err(PublishError::ConnectionLost)
    );
    let state = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    assert!(format!("{state:?}").contains("PUBACK"), "{state:?}");
    harness.broker().await.accept().await;
}

#[tokio::test(start_paused = true)]
async fn qos_0_publishes_pass_a_full_inflight_window() {
    let mut harness = start(LinkSettings {
        inflight_limit: 2,
        ..settings()
    });
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let mut publishers: Vec<_> = (0..5)
        .map(|_| spawn_publish(&harness.link, Channel::Telemetry, vec![1]))
        .collect();
    for _ in 0..2 {
        let Some(Packet::Publish(publish)) = bounded("a PUBLISH", broker.read()).await else {
            panic!("expected a PUBLISH")
        };
        assert_eq!(publish.qos, 1);
    }
    // The window is full and three QoS 1 publishes wait; a heartbeat (QoS 0) still goes out.
    publishers.push(spawn_publish(&harness.link, Channel::Heartbeat, vec![2]));
    let Some(Packet::Publish(heartbeat)) = bounded("the heartbeat", broker.read()).await else {
        panic!("the heartbeat waited behind the full window")
    };
    assert_eq!(
        (heartbeat.topic, heartbeat.qos),
        (Channel::Heartbeat.topic(&gateway()), 0)
    );
    assert_eq!(harness.link.in_flight(), 2);
    drop(publishers);
}

/// While the incoming queue is full the connection does not read, so it cannot see a PUBACK or
/// a PINGRESP: their deadlines wait however long the application takes, and count again from
/// the moment it reads. It still sends one PINGREQ, so the broker gets a packet within the
/// keepalive.
#[tokio::test(start_paused = true)]
async fn waiting_for_the_application_suspends_the_puback_and_pingresp_deadlines() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let (_, session) = harness.connected(&mut broker).await;
    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(sent)) = broker.read().await else {
        panic!("expected the PUBLISH")
    };
    // Fill the incoming queue; the last message waits for room, unacknowledged.
    let requests = Channel::LogRequest.topic(&gateway());
    let count = u16::try_from(INCOMING_QUEUE).unwrap() + 1;
    for id in 1..=count {
        broker
            .send(&publish(&requests, &id.to_be_bytes(), 1, false, Some(id)))
            .await;
    }
    for id in 1..count {
        assert_eq!(broker.read().await, Some(Packet::PubAck { id, reason: 0 }));
    }
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    // Ten minutes, far beyond the PINGRESP (30 s) and PUBACK (120 s) deadlines: the connection
    // stays up and sends nothing else.
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(
        *harness.link.state().borrow(),
        LinkState::Connected {
            session: session.clone()
        }
    );
    assert_eq!(harness.link.in_flight(), 1);
    assert!(broker.read().now_or_never().is_none(), "nothing else sent");

    assert_eq!(harness.recv().await.payload, 1_u16.to_be_bytes());
    let resumed = Instant::now();
    assert_eq!(
        broker.read().await,
        Some(Packet::PubAck {
            id: count,
            reason: 0
        })
    );
    // The broker answers late, but within the deadlines counted from the resumed reading.
    tokio::time::sleep(Duration::from_secs(20)).await;
    broker.send(&pingresp()).await;
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    broker.send(&pingresp()).await;
    tokio::time::sleep_until(resumed + Duration::from_secs(100)).await;
    broker.send(&puback(sent.id.unwrap(), 0)).await;
    assert_eq!(
        bounded("the publisher", publisher).await.unwrap(),
        Ok(Published::Acknowledged(ReasonCode(0)))
    );
    assert_eq!(
        *harness.link.state().borrow(),
        LinkState::Connected { session }
    );
}

/// The `online` status refused (the gateway's role lacks `up/status`): the cloud would never
/// see the gateway online, so the connection ends, without DISCONNECT (the broker publishes
/// the will), and the link reconnects after the backoff.
#[tokio::test(start_paused = true)]
async fn a_refused_online_status_ends_the_connection_and_the_link_tries_again() {
    let mut harness = start(LinkSettings {
        jitter: longest,
        ..settings()
    });
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
        panic!("expected SUBSCRIBE")
    };
    broker.send(&puback(online.id.unwrap(), 0x87)).await;
    broker.send(&suback(id, &vec![1; filters.len()])).await;
    assert_eq!(
        broker.read().await,
        None,
        "closed without DISCONNECT, which publishes the will"
    );
    let ended = Instant::now();
    let LinkState::Disconnected { error, .. } = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await
    else {
        unreachable!()
    };
    assert!(
        error.contains("refused the online status: 0x87 (not authorized)"),
        "{error}"
    );
    let (_next, _) = accept_next(&mut harness).await;
    assert_longest("the reconnect", ended.elapsed(), Duration::from_secs(1));
}

/// A SUBACK that does not come within twice the keepalive ends the connection like a missing
/// PUBACK: DISCONNECT 0x04 (the broker publishes the will), never Connected.
#[tokio::test(start_paused = true)]
async fn a_suback_that_never_comes_ends_the_connection_with_the_will() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { .. }) = broker.read().await else {
        panic!("expected SUBSCRIBE")
    };
    let subscribed = Instant::now();
    // The broker answers every PINGREQ but never the SUBSCRIBE.
    assert_eq!(
        bounded("the DISCONNECT", broker.read_answering_pings()).await,
        Some(Packet::Disconnect { reason: 0x04 })
    );
    let waited = subscribed.elapsed();
    assert!(
        (Duration::from_secs(120)..Duration::from_secs(121)).contains(&waited),
        "the deadline is twice the keepalive: {waited:?}"
    );
    assert_eq!(broker.read().await, None);
    let LinkState::Disconnected { error, .. } = harness
        .wait_state("disconnected", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await
    else {
        unreachable!()
    };
    assert!(error.contains("a PUBACK or SUBACK"), "{error}");
}

/// A subscription refused while the link stops does not matter any more: the stop still ends
/// with `offline`/`shutdown` and DISCONNECT 0x00.
#[tokio::test(start_paused = true)]
async fn a_subscription_refused_during_a_stop_still_ends_with_a_normal_disconnect() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
        panic!("expected SUBSCRIBE")
    };
    harness.shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read().await else {
        panic!("expected offline/shutdown")
    };
    assert!(matches!(
        Status::decode(&offline.payload).unwrap(),
        Status::Offline(_)
    ));
    let mut reasons = vec![1; filters.len()];
    reasons[0] = 0x87;
    broker.send(&suback(id, &reasons)).await;
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    assert_eq!(broker.read().await, None);
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(*harness.link.state().borrow(), LinkState::Stopped);
}

/// A write the broker does not take for 1.5 keepalives (at least 10 s) means the connection is
/// gone; the keepalive is the broker's Server Keep Alive when CONNACK sets one. Part of a
/// packet is on the stream, so it ends without DISCONNECT (the broker publishes the will), and
/// the message being written may or may not have arrived.
#[tokio::test(start_paused = true)]
async fn a_write_the_broker_does_not_take_ends_the_connection() {
    for (keep_alive, connack, limit) in [
        (
            Duration::from_secs(60),
            connack(0, &[]),
            Duration::from_secs(90),
        ),
        (
            Duration::from_secs(4),
            connack(0, &[]),
            Duration::from_secs(10),
        ),
        // Server Keep Alive 20 s replaces the agent's 60 s.
        (
            Duration::from_secs(60),
            connack(0, &[0x13, 0, 20]),
            Duration::from_secs(30),
        ),
    ] {
        let mut harness = start_with(
            LinkSettings {
                keep_alive,
                ..settings()
            },
            DuplexConnector::with_buffer(64),
        );
        let mut broker = harness.broker().await;
        harness.connected_with(&mut broker, &connack).await;
        // The broker stops reading; the message does not fit the stream's 64 bytes.
        let started = Instant::now();
        let stuck = spawn_publish(&harness.link, Channel::Live, vec![0xAB; 4096]);
        let LinkState::Disconnected { error, .. } = harness
            .wait_state("disconnected", |state| {
                matches!(state, LinkState::Disconnected { .. })
            })
            .await
        else {
            unreachable!()
        };
        let took = started.elapsed();
        assert!(
            (limit..limit + Duration::from_secs(1)).contains(&took),
            "keepalive {keep_alive:?}, CONNACK {connack:02X?}: the write gave up after {took:?}, expected {limit:?}"
        );
        assert!(error.contains("a write to the broker"), "{error}");
        assert_eq!(
            bounded("the stuck publish", stuck).await.unwrap(),
            Err(PublishError::ConnectionLost)
        );
        let rest = bounded("the rest of the stream", broker.read_raw_to_end()).await;
        assert!(
            rest.len() < 4096 && rest.first() == Some(&0x30),
            "only part of the PUBLISH, nothing after it: {} bytes",
            rest.len()
        );
        assert!(
            rest.ends_with(&[0xAB]),
            "the stream ends inside the payload"
        );
    }
}

/// [`Incoming::recv`] hands out what was received, and ends once the supervisor has stopped.
#[tokio::test(start_paused = true)]
async fn incoming_ends_once_the_supervisor_has_stopped_and_every_message_was_taken() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    broker
        .send(&publish(
            &Channel::Config.topic(&gateway()),
            b"cfg",
            1,
            true,
            Some(1),
        ))
        .await;
    assert_eq!(
        broker.read().await,
        Some(Packet::PubAck { id: 1, reason: 0 })
    );
    harness.shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read().await else {
        panic!("expected offline/shutdown")
    };
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(harness.recv().await.payload, b"cfg");
    assert_eq!(
        bounded("the end of the messages", harness.incoming.recv()).await,
        None
    );
}

/// A connector that never opens a stream: TCP, a TLS handshake or the WebSocket upgrade stuck.
struct HangingConnector;

impl Connector for HangingConnector {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        std::future::pending().await
    }
}

/// `connect_timeout` bounds opening the stream and, separately, waiting for CONNACK; a stream
/// that closes without DISCONNECT is reported as such.
#[tokio::test(start_paused = true)]
async fn connection_attempts_and_connack_are_bounded_by_the_connect_timeout() {
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = link::link(settings(), HangingConnector, shutdown.clone());
    let started = Instant::now();
    let supervisor = tokio::spawn(supervisor.run());
    let mut state = link.state();
    let disconnected = bounded(
        "the attempt to time out",
        state.wait_for(|state| matches!(state, LinkState::Disconnected { .. })),
    )
    .await
    .unwrap()
    .clone();
    let took = started.elapsed();
    assert!(
        (Duration::from_secs(30)..Duration::from_secs(31)).contains(&took),
        "{took:?}"
    );
    assert!(
        format!("{disconnected:?}").contains("waiting for the connection to the cloud"),
        "{disconnected:?}"
    );
    shutdown.cancel();
    bounded("the supervisor to stop", supervisor).await.unwrap();

    // A broker that takes the CONNECT and never answers.
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    let sent = Instant::now();
    assert_eq!(broker.read().await, None, "closed without DISCONNECT");
    let waited = sent.elapsed();
    assert!(
        (Duration::from_secs(30)..Duration::from_secs(31)).contains(&waited),
        "{waited:?}"
    );
    harness
        .wait_state("the CONNACK timeout", |state| {
            matches!(state, LinkState::Disconnected { error, .. } if error.contains("waiting for CONNACK"))
        })
        .await;

    // A broker that closes the stream after CONNECT.
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    drop(broker);
    harness
        .wait_state("the closed connection", |state| {
            matches!(state, LinkState::Disconnected { error, .. } if error.contains("closed without a DISCONNECT"))
        })
        .await;
}

/// Pings start when either direction has been quiet for the keepalive: QoS 0 messages that the
/// agent keeps sending do not hide a broker that went silent.
#[tokio::test(start_paused = true)]
async fn pings_start_when_the_broker_is_quiet_even_while_the_agent_keeps_sending() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let connected = Instant::now();
    for round in 1..=2_u8 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(
            harness.link.publish(Channel::Live, vec![round]).await,
            Ok(Published::Sent)
        );
        let Some(Packet::Publish(live)) = broker.read().await else {
            panic!("expected the live message")
        };
        assert_eq!(live.payload, [round]);
    }
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    let waited = connected.elapsed();
    assert!(
        (Duration::from_secs(60)..Duration::from_secs(61)).contains(&waited),
        "a keepalive after the broker's last packet: {waited:?}"
    );
}

/// PUBACK 0x10 (no matching subscribers) is a success too; a refusal carries the broker's
/// explanation, made printable, and its message shows at most 200 characters of it. Neither
/// ends the connection.
#[tokio::test(start_paused = true)]
async fn puback_codes_decide_the_outcome_and_a_refusal_carries_the_brokers_reason() {
    let mut harness = start(settings());
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![1]);
    let Some(Packet::Publish(sent)) = broker.read().await else {
        panic!("expected the PUBLISH")
    };
    broker.send(&puback(sent.id.unwrap(), 0x10)).await;
    assert_eq!(
        bounded("the publisher", publisher).await.unwrap(),
        Ok(Published::Acknowledged(ReasonCode(0x10)))
    );

    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![2]);
    let Some(Packet::Publish(sent)) = broker.read().await else {
        panic!("expected the PUBLISH")
    };
    let explanation = format!("role\nlacks\tit {}", "y".repeat(300));
    broker
        .send(&support::puback_with_reason_string(
            sent.id.unwrap(),
            0x87,
            &explanation,
        ))
        .await;
    let refused = bounded("the publisher", publisher).await.unwrap();
    let Err(PublishError::Refused {
        reason,
        reason_string: Some(text),
    }) = &refused
    else {
        panic!("expected a refusal with its reason string: {refused:?}")
    };
    assert_eq!(*reason, ReasonCode(0x87));
    assert_eq!(text, &format!("role lacks it {}", "y".repeat(300)));
    let message = refused.unwrap_err().to_string();
    assert!(
        message.contains("0x87 (not authorized): role lacks it yyy")
            && message.contains(&"y".repeat(186))
            && !message.contains(&"y".repeat(187)),
        "{message}"
    );
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
}

/// A heartbeat that finds the queue full is skipped, not delayed: the heartbeats keep their
/// interval, and the skipped one never queues behind the messages that filled the queue.
#[tokio::test(start_paused = true)]
async fn a_heartbeat_that_finds_the_queue_full_is_skipped_not_delayed() {
    let mut harness = start(settings());
    let heartbeats = tokio::spawn(agent::heartbeat::run(
        harness.link.clone(),
        agent::heartbeat::HeartbeatSettings {
            interval: Duration::from_secs(30),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: tokio::sync::watch::channel(protocol::UnixMillis::new(
                1_900_000_000_000,
            ))
            .1,
            buffer: agent::upload::Stats::default(),
            clock: agent::clock::GatewayClock::system(),
            root_crl: tokio::sync::watch::channel(None).1,
            health: agent::heartbeat::HealthSources::none(),
        },
        Instant::now(),
        harness.shutdown.clone(),
    ));
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let connected = Instant::now();
    let heartbeat_topic = Channel::Heartbeat.topic(&gateway());
    let Some(Packet::Publish(first)) = broker.read().await else {
        panic!("expected the first heartbeat")
    };
    assert_eq!(first.topic, heartbeat_topic);
    // A message larger than the stream's buffer (256 KiB), which the broker does not read for
    // now, holds the connection in its write; eight live messages fill the QoS 0 queue.
    let large = spawn_publish(&harness.link, Channel::Heartbeat, vec![0; 1024 * 1024]);
    tokio::time::sleep(Duration::from_millis(1)).await;
    let lives: Vec<_> = (0..REQUEST_QUEUE)
        .map(|index| {
            spawn_publish(
                &harness.link,
                Channel::Live,
                vec![u8::try_from(index).unwrap()],
            )
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(harness.link.queued(), REQUEST_QUEUE);
    // The heartbeat due 30 s after the first finds the queue full; the broker reads again 10 s
    // later.
    tokio::time::sleep_until(connected + Duration::from_secs(40)).await;
    assert_eq!(
        harness.link.queued(),
        REQUEST_QUEUE,
        "the heartbeat queued nothing"
    );
    let mut topics = Vec::new();
    loop {
        let Some(Packet::Publish(message)) = broker.read_answering_pings().await else {
            panic!("expected a PUBLISH")
        };
        if message.topic == heartbeat_topic && message.payload.len() < 1024 {
            break;
        }
        topics.push(message.topic);
    }
    let waited = connected.elapsed();
    assert!(
        (Duration::from_secs(60)..Duration::from_secs(61)).contains(&waited),
        "the next heartbeat comes an interval after the skipped one: {waited:?}"
    );
    assert_eq!(
        topics.len(),
        1 + REQUEST_QUEUE,
        "the stuck message and the queue, then the heartbeat: {topics:?}"
    );
    for publisher in std::iter::once(large).chain(lives) {
        assert_eq!(
            bounded("a publisher", publisher).await.unwrap(),
            Ok(Published::Sent)
        );
    }
    harness.shutdown.cancel();
    bounded("the heartbeats to stop", heartbeats).await.unwrap();
}

/// The PUBACK deadline (twice the keepalive) counts from the moment the PUBLISH is written: a
/// write that takes 80 s on a slow uplink leaves the broker its whole 120 s to answer.
#[tokio::test(start_paused = true)]
async fn a_slow_write_does_not_use_up_the_time_for_its_puback() {
    let mut harness = start_with(settings(), DuplexConnector::with_buffer(64));
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let started = Instant::now();
    // The broker does not read for 80 s; the message does not fit the stream's 64 bytes.
    let publisher = spawn_publish(&harness.link, Channel::Telemetry, vec![0xAB; 4096]);
    tokio::time::sleep(Duration::from_secs(80)).await;
    let Some(Packet::Publish(sent)) = bounded("the PUBLISH", broker.read()).await else {
        panic!("expected the PUBLISH")
    };
    // 140 s after the write began, 60 s after it ended: no DISCONNECT yet.
    let quiet = tokio::time::timeout_at(
        started + Duration::from_secs(140),
        broker.read_answering_pings(),
    )
    .await;
    assert!(quiet.is_err(), "the agent gave up early: {quiet:?}");
    broker.send(&puback(sent.id.unwrap(), 0)).await;
    assert_eq!(
        bounded("the publisher", publisher).await.unwrap(),
        Ok(Published::Acknowledged(ReasonCode(0)))
    );
}

/// The same for the `online` status and the SUBSCRIBE: each acknowledgement's deadline counts
/// from the end of its own write.
#[tokio::test(start_paused = true)]
async fn slow_writes_during_setup_do_not_use_up_the_time_for_their_acknowledgements() {
    for slow_subscribe in [false, true] {
        let mut harness = start_with(settings(), DuplexConnector::with_buffer(16));
        let mut broker = harness.broker().await;
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        broker.send(&connack(0, &[])).await;
        let started = Instant::now();
        let mut online_id = None;
        if slow_subscribe {
            let Some(Packet::Publish(online)) = broker.read().await else {
                panic!("expected online")
            };
            broker.send(&puback(online.id.unwrap(), 0)).await;
        }
        // The broker does not read for 80 s: the online status or the SUBSCRIBE (neither fits
        // the stream's 16 bytes) waits in its write.
        tokio::time::sleep(Duration::from_secs(80)).await;
        if !slow_subscribe {
            let Some(Packet::Publish(online)) = broker.read().await else {
                panic!("expected online")
            };
            online_id = online.id;
        }
        let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
            panic!("expected SUBSCRIBE")
        };
        let quiet = tokio::time::timeout_at(
            started + Duration::from_secs(140),
            broker.read_answering_pings(),
        )
        .await;
        assert!(
            quiet.is_err(),
            "slow SUBSCRIBE {slow_subscribe}: the agent gave up early: {quiet:?}"
        );
        if let Some(id) = online_id {
            broker.send(&puback(id, 0)).await;
        }
        broker.send(&suback(id, &vec![1; filters.len()])).await;
        harness
            .wait_state("connected", |state| {
                matches!(state, LinkState::Connected { .. })
            })
            .await;
    }
}

/// Settings whose credentials change when the returned sender raises the version, as
/// certificate renewal does (Laysi/trustiot-v2#83).
fn renewable(settings: LinkSettings) -> (LinkSettings, tokio::sync::watch::Sender<u64>) {
    let (version, credentials) = tokio::sync::watch::channel(0);
    (
        LinkSettings {
            credentials: Some(credentials),
            ..settings
        },
        version,
    )
}

/// Renewed credentials rotate the connection: it takes no new publishes, writes what it had
/// already accepted, waits for every PUBACK, ends its session like a clean stop and reconnects
/// at once with the new credentials. Nothing it accepted is lost.
#[tokio::test(start_paused = true)]
async fn renewed_credentials_rotate_the_connection_without_losing_what_it_accepted() {
    let (settings, version) = renewable(LinkSettings {
        inflight_limit: 2,
        ..settings()
    });
    let mut harness = start(settings);
    let mut broker = harness.broker().await;
    let (_, first) = harness.connected(&mut broker).await;
    // Two publishes in flight, two more waiting in the queue behind the full window.
    let mut publishers = Vec::new();
    for index in 0..4_u8 {
        publishers.push(spawn_publish(
            &harness.link,
            Channel::Telemetry,
            vec![index],
        ));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let mut written = Vec::new();
    for _ in 0..2 {
        let Some(Packet::Publish(publish)) = broker.read().await else {
            panic!("expected a PUBLISH")
        };
        written.push(publish);
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!((harness.link.in_flight(), harness.link.queued()), (2, 2));

    version.send_replace(1);
    tokio::time::sleep(Duration::from_millis(1)).await;
    // From now on the connection takes nothing new.
    refused_at_once(&harness.link, &PublishError::NotConnected, "rotating");
    // What it accepted is still written, as the window frees up, and acknowledged.
    for _ in 0..2 {
        let done = written.remove(0);
        broker.send(&puback(done.id.unwrap(), 0)).await;
        let Some(Packet::Publish(publish)) = broker.read().await else {
            panic!("expected a queued PUBLISH")
        };
        written.push(publish);
    }
    let payloads: Vec<Vec<u8>> = written
        .iter()
        .map(|publish| publish.payload.clone())
        .collect();
    assert_eq!(payloads, [vec![2], vec![3]]);
    for publish in written {
        broker.send(&puback(publish.id.unwrap(), 0)).await;
    }
    // Then the session ends cleanly: offline/shutdown, its PUBACK, DISCONNECT 0x00, so the
    // broker discards the will.
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected offline/shutdown")
    };
    let Status::Offline(status) = Status::decode(&offline.payload).unwrap() else {
        panic!("expected offline")
    };
    assert_eq!(
        (status.session, status.reason),
        (first.clone(), OfflineReason::Shutdown)
    );
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    assert_eq!(broker.read().await, None);
    let ended = Instant::now();
    for publisher in publishers {
        assert_eq!(
            bounded("a publisher", publisher).await.unwrap(),
            Ok(Published::Acknowledged(ReasonCode(0))),
            "every publish the connection accepted is acknowledged"
        );
    }

    // The next connection follows at once, without a backoff delay.
    let mut broker = harness.broker().await;
    assert!(
        Instant::now() - ended < MIN_DELAY,
        "reconnected after a backoff"
    );
    let (_, second) = harness.connected(&mut broker).await;
    assert_ne!(first, second);
    next_publish_is_a_fresh_one(&harness, &mut broker).await;
    assert_eq!(*harness.link.certificate_refused().borrow(), None);

    // A version that changes again rotates again.
    version.send_replace(2);
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected offline/shutdown")
    };
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
}

/// A caller that holds a place in the queue cannot hold a rotation up for longer than its
/// deadline (two keepalive periods): the connection then ends with the will, the place's
/// publish is refused, and the link reconnects at once.
#[tokio::test(start_paused = true)]
async fn a_held_permit_cannot_hold_up_a_rotation_past_its_deadline() {
    let (settings, version) = renewable(settings());
    let mut harness = start(settings);
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let permit = bounded("a place", harness.link.reserve(Channel::Telemetry))
        .await
        .unwrap();
    let rotated = Instant::now();
    version.send_replace(1);
    assert_eq!(
        broker.read_answering_pings().await,
        Some(Packet::Disconnect { reason: 0x04 })
    );
    let waited = Instant::now() - rotated;
    assert!(
        waited >= 2 * link::KEEP_ALIVE && waited < 2 * link::KEEP_ALIVE + MIN_DELAY,
        "the rotation ended after {waited:?}"
    );
    assert_eq!(
        bounded("the held place", permit.publish(vec![1])).await,
        Err(PublishError::NotConnected)
    );
    let ended = Instant::now();
    let mut broker = harness.broker().await;
    assert!(
        Instant::now() - ended < MIN_DELAY,
        "reconnected after a backoff"
    );
    harness.connected(&mut broker).await;
}

/// A shutdown during a rotation is a plain stop: the link stops instead of reconnecting.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_a_rotation_stops_the_link() {
    let (settings, version) = renewable(settings());
    let mut harness = start(settings);
    let mut broker = harness.broker().await;
    harness.connected(&mut broker).await;
    let permit = bounded("a place", harness.link.reserve(Channel::Telemetry))
        .await
        .unwrap();
    version.send_replace(1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    harness.shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected offline/shutdown")
    };
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    bounded("the supervisor to stop", &mut harness.supervisor)
        .await
        .unwrap();
    assert_eq!(*harness.link.state().borrow(), LinkState::Stopped);
    drop(permit);
}

/// Fails every connection attempt, with the scripted errors first.
struct Failing {
    failures: std::sync::Mutex<VecDeque<ConnectError>>,
    then: fn() -> ConnectError,
}

impl Failing {
    fn new(failures: impl IntoIterator<Item = ConnectError>, then: fn() -> ConnectError) -> Self {
        Self {
            failures: std::sync::Mutex::new(failures.into_iter().collect()),
            then,
        }
    }
}

impl Connector for Failing {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        let next = self.failures.lock().unwrap().pop_front();
        Err(next.unwrap_or_else(self.then))
    }
}

fn unreachable_error() -> ConnectError {
    ConnectError::new(ConnectStage::Tcp, "connection refused")
}

fn silent_inner_error() -> ConnectError {
    ConnectError::new(ConnectStage::InnerTls, "handshake cut short")
}

/// A TLS alert in the outer TLS handshake, as tokio-rustls hands it over. Anyone on the path
/// can send one before the handshake is done.
fn outer_alert(description: rustls::AlertDescription) -> ConnectError {
    ConnectError::new(
        ConnectStage::OuterTls,
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(description),
        ),
    )
}

/// A TLS alert from the broker as tokio-rustls hands it over: a rustls error inside an
/// `io::Error`.
fn alert(description: rustls::AlertDescription) -> ConnectError {
    ConnectError::new(
        ConnectStage::InnerTls,
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(description),
        ),
    )
}

/// The broker refusing the client certificate with an alert in the inner mTLS is reported at
/// once, with the version of the credentials the attempt used; the network, the outer TLS (a
/// certificate alert there included: it is not authenticated) and other alerts are not.
#[tokio::test(start_paused = true)]
async fn a_certificate_alert_is_reported_as_a_refusal_of_the_credentials_in_use() {
    let (settings, version) = renewable(settings());
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = link::link(
        settings,
        Failing::new(
            [
                ConnectError::new(ConnectStage::Tcp, "connection refused"),
                ConnectError::new(ConnectStage::OuterTls, "bad certificate"),
                outer_alert(rustls::AlertDescription::BadCertificate),
                outer_alert(rustls::AlertDescription::CertificateRevoked),
                alert(rustls::AlertDescription::HandshakeFailure),
                alert(rustls::AlertDescription::CertificateExpired),
            ],
            unreachable_error,
        ),
        shutdown.clone(),
    );
    let supervisor = tokio::spawn(supervisor.run());
    let mut refused = link.certificate_refused();
    let mut state = link.state();
    let mut failures = 0;
    while failures < 5 {
        bounded("a failed attempt", state.changed()).await.unwrap();
        if matches!(*state.borrow_and_update(), LinkState::Disconnected { .. }) {
            failures += 1;
        }
    }
    assert_eq!(*refused.borrow(), None, "not the certificate's fault");
    bounded("a refusal", refused.wait_for(Option::is_some))
        .await
        .unwrap();
    assert_eq!(
        *refused.borrow(),
        Some(Refusal {
            credentials: 0,
            explicit: true
        })
    );
    shutdown.cancel();
    bounded("the supervisor to stop", supervisor).await.unwrap();
    drop(version);

    // A refusal of renewed credentials carries their version.
    let (settings, version) = renewable(crate::settings());
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = link::link(
        settings,
        Failing::new([], || alert(rustls::AlertDescription::CertificateRevoked)),
        shutdown.clone(),
    );
    let supervisor = tokio::spawn(supervisor.run());
    let mut refused = link.certificate_refused();
    bounded(
        "a refusal",
        refused.wait_for(|refused| refused.is_some_and(|refused| refused.credentials == 0)),
    )
    .await
    .unwrap();
    version.send_replace(5);
    bounded(
        "a refusal of version 5",
        refused.wait_for(|refused| refused.is_some_and(|refused| refused.credentials == 5)),
    )
    .await
    .unwrap();
    shutdown.cancel();
    bounded("the supervisor to stop", supervisor).await.unwrap();
}

/// CONNACK 0x87 (the broker no longer knows the gateway) is a refusal at once; failures at the
/// inner mTLS that say nothing are one (not explicit) only once they have lasted REFUSED_AFTER
/// and a random part of REFUSED_JITTER, and new
/// credentials start that time over.
#[tokio::test(start_paused = true)]
async fn connack_refusals_count_at_once_and_silent_inner_failures_only_after_a_while() {
    let (settings, _version) = renewable(settings());
    let mut harness = start(settings);
    let mut refused = harness.link.certificate_refused();
    let mut broker = harness.broker().await;
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    broker.send(&connack(0x87, &[])).await;
    bounded("a refusal", refused.wait_for(Option::is_some))
        .await
        .unwrap();
    assert!(refused.borrow().is_some_and(|refused| refused.explicit));
    harness.shutdown.cancel();

    // The longest draw: the run of failures lasts REFUSED_AFTER and (almost) all of
    // REFUSED_JITTER before it counts.
    let (settings, version) = renewable(LinkSettings {
        jitter: longest,
        ..crate::settings()
    });
    let wanted = link::REFUSED_AFTER + link::REFUSED_JITTER - Duration::from_secs(1);
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = link::link(
        settings,
        Failing::new([], silent_inner_error),
        shutdown.clone(),
    );
    let supervisor = tokio::spawn(supervisor.run());
    let started = Instant::now();
    let mut refused = link.certificate_refused();
    // Bounded on the paused clock: REFUSED_AFTER, its jitter and at most one backoff delay (60 s)
    // more.
    let limit = link::REFUSED_AFTER + link::REFUSED_JITTER + Duration::from_secs(120);
    tokio::time::timeout(limit, refused.wait_for(Option::is_some))
        .await
        .expect("a refusal after REFUSED_AFTER")
        .unwrap();
    let after = Instant::now() - started;
    assert!(after >= wanted, "reported after {after:?}");
    assert!(refused.borrow().is_some_and(|refused| !refused.explicit));
    // New credentials get their own REFUSED_AFTER before they are blamed.
    version.send_replace(1);
    let renewed = Instant::now();
    tokio::time::timeout(
        limit,
        refused.wait_for(|refused| refused.is_some_and(|refused| refused.credentials == 1)),
    )
    .await
    .expect("a refusal of version 1")
    .unwrap();
    let after = Instant::now() - renewed;
    assert!(after >= wanted, "reported after {after:?}");
    shutdown.cancel();
    bounded("the supervisor to stop", supervisor).await.unwrap();
}
