//! The agent against Mosquitto 2.1 in a container (Laysi/trustiot-v2#86, acceptance criteria 1
//! and 2): outer TLS and the WebSocket to the in-process front, the inner mTLS straight to the
//! broker's listener with the production settings (`require_certificate`,
//! `use_identity_as_username`, TLS 1.3, dynamic security with the gateway role). The telemetry
//! pipeline's round trip through the same broker (Laysi/trustiot-v2#90, acceptance criterion 7):
//! a batch on `up/telemetry`, the cloud's acknowledgement on `down/telemetry-ack`.
//!
//! Needs podman or docker: without one the tests skip, with `CI` set they fail. The broker
//! helpers block (they run the container runtime), so the tests run on a multi-thread runtime
//! and call them through `blocking`, which keeps the agent's tasks running meanwhile.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent::ReasonCode;
use agent::clock::GatewayClock;
use agent::dispatch::{self, Handlers};
use agent::link::{LinkState, PublishError, Published, Subscriptions};
use agent::upload::{self, Settings};
use protocol::acl::{GATEWAY_ROLE, gateway_channel_acl_topic};
use protocol::health::{OfflineReason, Status};
use protocol::telemetry::{AckOutcome, Reading, Sample, TelemetryAck, TelemetryBatch};
use protocol::topic::{Channel, MAX_LARGE_BYTES};
use protocol::{DeviceId, PointKey, SessionId, V1, Value};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use serde_json::json;
use support::wss::{Agent, Files, front};
use support::{GATEWAY, connect_with_will, disconnect, gateway};
use testkit::clock::bounded;
use testkit::mqtt::mosquitto::{Delivered, Mosquitto, Subscription};
use testkit::mqtt::{BROKER_NAME, TestPki};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

/// A generous bound for anything that crosses the container runtime (CLAUDE.md: test time
/// limits; CI runners are often loaded).
const LIMIT: Duration = Duration::from_secs(120);

/// Runs blocking broker helpers without stalling the agent's tasks.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

fn status_topic() -> String {
    Channel::Status.topic(&gateway())
}

fn status(message: &Delivered) -> Status {
    Status::decode(&message.payload).unwrap()
}

/// Waits for the status message of `session` that `wanted` accepts.
fn wait_status(
    subscription: &Subscription,
    what: &str,
    wanted: impl Fn(&Status) -> bool,
) -> Delivered {
    let topic = status_topic();
    subscription.wait_for(what, LIMIT, |message| {
        message.topic == topic && wanted(&status(message))
    })
}

fn is_online(session: &SessionId) -> impl Fn(&Status) -> bool + '_ {
    move |status| matches!(status, Status::Online(online) if online.session == *session)
}

async fn connected(agent: &Agent) -> SessionId {
    match agent
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await
    {
        LinkState::Connected { session } => session,
        other => unreachable!("{other:?}"),
    }
}

/// Waits for a connection other than `previous`.
async fn reconnected(agent: &Agent, previous: &SessionId) -> SessionId {
    match agent
        .wait_state(
            "a new connection",
            |state| matches!(state, LinkState::Connected { session } if session != previous),
        )
        .await
    {
        LinkState::Connected { session } => session,
        other => unreachable!("{other:?}"),
    }
}

/// How the raw client of [`which_ends_make_mosquitto_publish_the_will`] ends its connection.
#[derive(Clone, Copy, Debug)]
enum End {
    /// DISCONNECT with this reason code.
    Disconnect(u8),
    /// TLS close_notify and TCP FIN, no DISCONNECT (as when the agent cannot write any more).
    CloseWithoutDisconnect,
}

impl std::fmt::Display for End {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnect(code) => write!(f, "DISCONNECT 0x{code:02X}"),
            Self::CloseWithoutDisconnect => f.write_str("close without DISCONNECT"),
        }
    }
}

/// The gateway's mTLS client settings for a connection straight to the broker's listener.
fn gateway_tls(pki: &TestPki) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(pki.broker_ca.pem.as_bytes()).unwrap())
        .unwrap();
    let leaf = pki.gateway(GATEWAY);
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(agent::tls::provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![leaf.cert_der()], leaf.key_der())
            .unwrap(),
    )
}

/// Connects to the broker as the gateway with the retained will `will` on its status topic,
/// ends the connection as `end` says, and returns once the broker has closed it, that is once
/// it has published the will or dropped it.
async fn connect_and_end(
    broker: &Mosquitto,
    tls: &Arc<rustls::ClientConfig>,
    will: &[u8],
    end: End,
) {
    let tcp = TcpStream::connect(broker.address()).await.unwrap();
    let name = ServerName::try_from(BROKER_NAME).unwrap();
    let mut stream = bounded(
        "the TLS handshake with the broker",
        TlsConnector::from(Arc::clone(tls)).connect(name, tcp),
    )
    .await
    .unwrap();
    stream
        .write_all(&connect_with_will(GATEWAY, &status_topic(), will))
        .await
        .unwrap();
    let mut head = [0_u8; 2];
    bounded("CONNACK", stream.read_exact(&mut head))
        .await
        .unwrap();
    assert!(head[0] == 0x20 && head[1] < 0x80, "CONNACK header {head:?}");
    let mut connack = vec![0_u8; usize::from(head[1])];
    bounded("CONNACK", stream.read_exact(&mut connack))
        .await
        .unwrap();
    assert_eq!(connack[1], 0x00, "CONNACK reason code");
    match end {
        End::Disconnect(code) => stream.write_all(&disconnect(code)).await.unwrap(),
        End::CloseWithoutDisconnect => stream.shutdown().await.unwrap(),
    }
    stream.flush().await.unwrap();
    // The broker closes the connection after handling its end (with or without close_notify).
    let mut rest = Vec::new();
    let _ = bounded(
        "the broker to close the connection",
        stream.read_to_end(&mut rest),
    )
    .await;
}

/// Which ends of a connection make Mosquitto publish the will. MQTT 5 says every end other than
/// DISCONNECT 0x00 publishes it; Mosquitto 2.1 (`handle__disconnect`) keeps the will only for
/// DISCONNECT 0x04 and drops it for every other reason code. The agent relies on this table:
/// it ends every failed connection with 0x04 (or without DISCONNECT when it cannot write).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn which_ends_make_mosquitto_publish_the_will() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let tls = gateway_tls(&pki);
    let ends = [
        End::Disconnect(0x00),
        End::Disconnect(0x04),
        End::Disconnect(0x80),
        End::Disconnect(0x81),
        End::Disconnect(0x82),
        End::Disconnect(0x83),
        End::Disconnect(0x94),
        End::Disconnect(0x95),
        End::CloseWithoutDisconnect,
    ];
    let mut observed = Vec::new();
    for end in ends {
        // A retained baseline first: afterwards the retained status is either this connection's
        // will (published) or still the baseline (dropped), so nothing waits for an absence.
        let baseline = format!("baseline before {end}").into_bytes();
        let will = format!("will of {end}").into_bytes();
        let stored = blocking(|| broker.publish(&status_topic(), &baseline, 1, true));
        assert!(stored.is_some_and(|reason| reason < 0x80), "{stored:?}");
        connect_and_end(&broker, &tls, &will, end).await;
        let retained = blocking(|| {
            broker
                .subscribe(&status_topic())
                .next_message(LIMIT)
                .unwrap()
        });
        let published = if retained.payload == will {
            true
        } else {
            assert_eq!(
                retained.payload, baseline,
                "{end}: neither the will nor the baseline"
            );
            false
        };
        println!("{:<26} will published: {published}", end.to_string());
        observed.push((end, published));
    }
    let published: Vec<String> = observed
        .iter()
        .filter(|(_, published)| *published)
        .map(|(end, _)| end.to_string())
        .collect();
    assert_eq!(
        published,
        ["DISCONNECT 0x04", "close without DISCONNECT"],
        "{observed:?}"
    );
}

/// A refused subscription ends the connection with DISCONNECT 0x04, so Mosquitto publishes the
/// will and the retained status of that session does not stay `online` (after DISCONNECT 0x83
/// it would); the agent then tries again with a new session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_subscription_leaves_the_will_as_the_status_and_the_agent_tries_again() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    // The role may no longer subscribe to one down channel: its SUBACK code is 0x87.
    blocking(|| {
        broker.control(&json!([{
            "command": "addRoleACL",
            "rolename": GATEWAY_ROLE,
            "acltype": "subscribePattern",
            "topic": gateway_channel_acl_topic(Channel::LogRequest),
            "priority": 10,
            "allow": false
        }]))
    });
    let front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let statuses = blocking(|| broker.subscribe(&status_topic()));
    let agent = Agent::start_with(
        &files,
        Subscriptions::new([Channel::Config, Channel::LogRequest]).unwrap(),
    );

    let online = blocking(|| {
        wait_status(&statuses, "online", |status| {
            matches!(status, Status::Online(_))
        })
    });
    let Status::Online(first) = status(&online) else {
        unreachable!()
    };
    let error = agent.first_error().await;
    assert!(
        error.contains(&Channel::LogRequest.topic(&gateway())) && error.contains("0x87"),
        "{error}"
    );
    let will = blocking(|| {
        wait_status(
            &statuses,
            "the will of the refused connection",
            |status| matches!(status, Status::Offline(offline) if offline.session == first.session),
        )
    });
    assert_eq!(status(&will), Status::last_will(first.session.clone()));
    assert!(will.retain && will.qos == 1, "{will:?}");
    blocking(|| {
        wait_status(
            &statuses,
            "the next attempt's online",
            |status| matches!(status, Status::Online(online) if online.session != first.session),
        )
    });
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_talks_mqtt_5_with_mosquitto_through_the_websocket_with_mtls_inside() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let up = blocking(|| broker.subscribe(&format!("trustiot/v1/gw/{GATEWAY}/up/#")));
    let config = Channel::Config.topic(&gateway());
    // The largest configuration the protocol allows (1 MiB), so the whole way (Mosquitto, the
    // WebSocket front, the inner TLS, the agent's Maximum Packet Size) carries it intact
    // (crates/protocol/README.md: test with a 1 MiB configuration message).
    let large_config: Vec<u8> = (0..Channel::Config.max_payload_bytes())
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    // 0x10 (no matching subscribers) is a success code too.
    let stored = blocking(|| broker.publish(&config, &large_config, 1, true));
    assert!(stored.is_some_and(|reason| reason < 0x80), "{stored:?}");

    let mut agent = Agent::start_with(&files, Subscriptions::new([Channel::Config]).unwrap());
    let session = connected(&agent).await;
    // The broker took the certificate's CN as the user name, over MQTT 5.
    let line = blocking(|| broker.wait_for_log(&[&format!("u'{GATEWAY}'"), "p5"], LIMIT));
    assert!(
        line.contains(&format!(" as {GATEWAY} ")),
        "the client ID is the gateway ID: {line}"
    );
    // Mosquitto picks by its own order, so the listener lists ChaCha20 first (testkit's
    // configuration); the agent offers it first as well.
    blocking(|| {
        broker.wait_for_log(
            &[&format!(
                "Client {GATEWAY} negotiated TLSv1.3 cipher TLS_CHACHA20_POLY1305_SHA256"
            )],
            LIMIT,
        )
    });
    assert!(front.sessions()[0].upgraded);

    // online first, retained and QoS 1.
    let online = blocking(|| wait_status(&up, "online", is_online(&session)));
    assert!(online.retain && online.qos == 1, "{online:?}");

    // The retained configuration arrives when the agent subscribes.
    let down = bounded("the retained config", agent.incoming.recv())
        .await
        .unwrap();
    assert_eq!((down.channel, down.retained), (Channel::Config, true));
    assert_eq!(down.payload.len(), MAX_LARGE_BYTES);
    assert!(
        down.payload == large_config,
        "the configuration arrived changed"
    );

    // A QoS 1 publish is acknowledged; once the role no longer allows a channel, the PUBACK
    // refuses it with 0x87 and the publisher sees that.
    assert_eq!(
        agent.link.publish(Channel::Telemetry, b"t1".to_vec()).await,
        Ok(Published::Acknowledged(ReasonCode(0)))
    );
    let telemetry =
        blocking(|| up.wait_for("telemetry", LIMIT, |message| message.payload == b"t1"));
    assert_eq!(
        (telemetry.topic, telemetry.qos),
        (Channel::Telemetry.topic(&gateway()), 1)
    );
    blocking(|| {
        broker.remove_gateway_rule(
            "publishClientSend",
            &gateway_channel_acl_topic(Channel::Log),
        )
    });
    let refused = agent.link.publish(Channel::Log, b"l1".to_vec()).await;
    assert!(
        matches!(
            refused,
            Err(PublishError::Refused {
                reason: ReasonCode(0x87),
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(
        *agent.link.state().borrow(),
        LinkState::Connected {
            session: session.clone()
        },
        "a refused publish does not end the connection"
    );

    // A clean stop: offline with reason shutdown, then a normal DISCONNECT, so the broker
    // discards the will and the retained status stays offline/shutdown.
    agent.stop().await;
    let offline = blocking(|| {
        wait_status(
            &up,
            "offline",
            |status| matches!(status, Status::Offline(offline) if offline.session == session),
        )
    });
    let Status::Offline(offline_status) = status(&offline) else {
        unreachable!()
    };
    assert_eq!(offline_status.reason, OfflineReason::Shutdown);
    assert!(offline.retain);
    // "disconnected." without a reason: the broker received DISCONNECT 0x00.
    blocking(|| broker.wait_for_log(&[&format!("Client {GATEWAY} ["), "] disconnected."], LIMIT));
    let later = blocking(|| broker.subscribe(&status_topic()));
    let retained = blocking(|| later.next_message(LIMIT)).unwrap();
    assert_eq!(
        status(&retained),
        status(&offline),
        "the will was not published"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_reconnects_after_the_relay_or_the_broker_restarts() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let mut front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let statuses = blocking(|| broker.subscribe(&status_topic()));
    let agent = Agent::start(&files);
    let first = connected(&agent).await;
    blocking(|| wait_status(&statuses, "online", is_online(&first)));

    // The relay crashes: the broker loses the connection and publishes the will.
    front.stop().await;
    let will = blocking(|| {
        wait_status(
            &statuses,
            "the will",
            |status| matches!(status, Status::Offline(offline) if offline.session == first),
        )
    });
    assert_eq!(status(&will), Status::last_will(first.clone()));
    assert!(will.retain && will.qos == 1, "{will:?}");
    agent
        .wait_state("the agent to notice", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;

    // The relay comes back on the same address: a new connection with a new session.
    front.restart().await.unwrap();
    let second = reconnected(&agent, &first).await;
    blocking(|| wait_status(&statuses, "online again", is_online(&second)));

    // The broker restarts (SIGTERM, a fresh process, retained messages gone).
    drop(statuses);
    blocking(|| broker.restart());
    let third = reconnected(&agent, &second).await;
    let statuses = blocking(|| broker.subscribe(&status_topic()));
    let online = blocking(|| statuses.next_message(LIMIT)).unwrap();
    assert!(
        online.retain && is_online(&third)(&status(&online)),
        "the retained status after the restart is the new session's online: {online:?}"
    );
    agent.stop().await;
}

/// Acceptance criterion 7 of Laysi/trustiot-v2#90: the agent's link subscribes to
/// `down/telemetry-ack` (the gateway role allows it), the pipeline's batch reaches the cloud on
/// `up/telemetry` through Mosquitto, the cloud's `TelemetryAck` comes back through Mosquitto to
/// the dispatcher, and the batch leaves the buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_goes_up_through_mosquitto_and_the_clouds_acknowledgement_releases_it() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let telemetry = Channel::Telemetry.topic(&gateway());
    let up = blocking(|| broker.subscribe(&telemetry));
    let Agent {
        link,
        incoming,
        shutdown,
        supervisor,
    } = Agent::start_with(&files, dispatch::subscriptions());
    let mut state = link.state();
    bounded(
        "the link to connect",
        state.wait_for(|state| matches!(state, LinkState::Connected { .. })),
    )
    .await
    .unwrap();

    let clock = GatewayClock::system();
    let mut settings = Settings::new(PathBuf::from(files.path("state")).join("buffer"));
    settings.upload_interval = upload::MIN_UPLOAD_INTERVAL;
    let stop = CancellationToken::new();
    let (handles, pipeline) =
        upload::pipeline(settings, link.clone(), clock.clone(), stop.clone()).unwrap();
    let pipeline = tokio::spawn(pipeline.run());
    let dispatcher = tokio::spawn(dispatch::run(
        incoming,
        Handlers {
            // Nothing publishes a configuration in this test.
            config: agent::desired::inbox().0,
            telemetry_ack: handles.acks.clone(),
        },
    ));
    let now = agent::modbus::Clock::now(&clock);
    let readings: Vec<Reading> = (0..3)
        .map(|index| Reading {
            device: DeviceId::new("meter-1").unwrap(),
            point: PointKey::new(format!("p{index}")).unwrap(),
            sample: Sample {
                at: now.at,
                value: Value::Int(index),
            },
            clock_synced: now.synced,
        })
        .collect();
    handles.readings.push(readings);

    let delivered = blocking(|| {
        up.wait_for("the telemetry batch", LIMIT, |message| {
            message.topic == telemetry
        })
    });
    assert_eq!(
        (delivered.qos, delivered.retain),
        (1, false),
        "{delivered:?}"
    );
    let batch = TelemetryBatch::decode(&delivered.payload).unwrap();
    assert_eq!((batch.seq.get(), batch.record_count()), (1, 3));
    assert_eq!(batch.series.len(), 3);
    assert!(
        batch
            .series
            .iter()
            .all(|series| series.samples.iter().all(|sample| sample.at == now.at)),
        "the timestamps as read"
    );
    // Nothing releases the batch but the acknowledgement.
    assert_eq!(handles.stats.snapshot().pending_batches, 1);

    let ack = TelemetryAck {
        v: V1,
        stream: batch.stream.clone(),
        seq: batch.seq,
        outcome: AckOutcome::Committed { records: 3 },
        server_time: clock.wall(),
    };
    let payload = protocol::encode(&ack).unwrap();
    let topic = Channel::TelemetryAck.topic(&gateway());
    let sent = blocking(|| broker.publish(&topic, &payload, 1, false));
    assert!(sent.is_some_and(|reason| reason < 0x80), "{sent:?}");
    bounded("the acknowledged batch to leave the buffer", async {
        while handles.stats.snapshot().pending_batches != 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let stats = handles.stats.snapshot();
    assert_eq!(
        (stats.pending_records, stats.dropped_records),
        (0, 0),
        "{stats:?}"
    );
    assert!(
        clock.synced(),
        "an acknowledgement within the window counts as a synchronized clock"
    );

    stop.cancel();
    bounded("the pipeline to stop", pipeline).await.unwrap();
    shutdown.cancel();
    bounded("the link to stop", supervisor).await.unwrap();
    bounded("the dispatcher to stop", dispatcher).await.unwrap();
}
