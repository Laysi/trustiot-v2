//! The root CRL updater (Laysi/trustiot-v2#563) with the agent wired as the binary wires it
//! (`Setup`), the whole way to a stand-in broker (`support::wss`): a newer root CRL is taken and
//! the next connection is a full handshake that checks the broker's chain against it; anything
//! not newer, or not signed by the root, is refused and the one held stays; a restart reuses the
//! root CRL saved; deleting or rolling back the file withholds every connection until a root CRL
//! at least as new is fetched; the heartbeat reports the number and whether it expired, and an
//! expired one still connects; and the `GET` on the gateway host itself
//! (`renewal::HttpsEndpoint`).
//!
//! The root CRL comes from a stand-in source (`Served`) except in the last test. Real sockets
//! need real time; every wait is bounded by `testkit::clock::bounded`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::convert::Infallible;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::backoff::BackoffSettings;
use agent::heartbeat::HeartbeatSettings;
use agent::link::{LinkState, PublishError, Published};
use agent::renewal::HttpsEndpoint;
use agent::revocation::{Fetched, Marker, Schedule, Source, Update, Updater};
use agent::tls::{RootCrl, RootKey};
use agent::transport::CloudUrl;
use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{ACCEPT, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use protocol::health::{Heartbeat, RootCrlStatus};
use protocol::topic::Channel;
use protocol::{AgentVersion, UnixMillis};
use rustls::HandshakeKind;
use support::wss::{Agent, Files, InnerBroker, front};
use support::{FakeBroker, Packet, gateway, puback};
use testkit::clock::bounded;
use testkit::mqtt::front::WsFront;
use testkit::mqtt::{
    Authority, BROKER_NAME, FRONT_HOST, ScratchDir, TestPki, mtls_server_with_chain,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// A root CRL source that answers what the test last put in it.
#[derive(Clone)]
struct Served(Arc<Mutex<Fetched>>);

impl Served {
    fn new(answer: Fetched) -> Self {
        Self(Arc::new(Mutex::new(answer)))
    }

    fn set(&self, answer: Fetched) {
        *self.0.lock().unwrap() = answer;
    }

    fn crl(&self, pem: &str) {
        self.set(Fetched::Crl(pem.as_bytes().to_vec()));
    }
}

impl Source for Served {
    async fn fetch(&self) -> io::Result<Fetched> {
        let answer = self.0.lock().unwrap().clone();
        Ok(answer)
    }

    fn describe(&self) -> String {
        "the stand-in".to_owned()
    }
}

/// A broker whose certificate `issuing` signed and which sends `issuing` with it, its front, and
/// a gateway configured for them, whose certificate `issuing` signed too.
async fn with_issuing_ca(pki: &TestPki, issuing: &Authority) -> (InnerBroker, WsFront, Files) {
    let leaf = issuing.server(&[BROKER_NAME]);
    let broker =
        InnerBroker::start(mtls_server_with_chain(&leaf, &[issuing], &pki.broker_ca)).await;
    let front = front(pki, broker.address, |_| {}).await;
    let files = Files::new(pki, front.address().port());
    files.use_issuing_ca(issuing);
    (broker, front, files)
}

/// The agent of `files` and its root CRL updater, asking `served`.
fn start(files: &Files, served: &Served) -> (Agent, Updater<Served>) {
    let mut setup = files.setup().unwrap();
    let updater = setup
        .root_crl_updater
        .take()
        .expect("broker.crl is set, so there is an updater")
        .with_source(served.clone());
    (Agent::from_setup(setup), updater)
}

/// The next connection the broker accepts, answered: its handshake kind and the broker's side.
async fn connected(
    broker: &mut InnerBroker,
    agent: &Agent,
) -> (HandshakeKind, FakeBroker<TlsStream<TcpStream>>) {
    let (mut mqtt, kind) = broker.next_broker().await;
    let (_, session) = mqtt.accept().await;
    agent
        .wait_state(
            "connected",
            |state| matches!(state, LinkState::Connected { session: now } if *now == session),
        )
        .await;
    (kind, mqtt)
}

/// The number of the root CRL a restarted agent would load.
fn loaded_number(files: &Files) -> Option<String> {
    match files.setup().unwrap().root_crl {
        RootCrl::Loaded { number, .. } => number,
        other => panic!("no root CRL loaded: {other:?}"),
    }
}

#[tokio::test]
async fn a_newer_root_crl_is_taken_and_the_next_connection_is_a_full_handshake_checked_against_it()
{
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing).await;
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(1, &[&retired], (2099, 1, 1)),
    );
    let served = Served::new(Fetched::NotPublished);
    let (agent, updater) = start(&files, &served);

    // A full handshake, then a resumed one: the resumption cache is in use.
    let (kind, first) = connected(&mut broker, &agent).await;
    assert_eq!(kind, HandshakeKind::Full);
    drop(first);
    let (kind, _second) = connected(&mut broker, &agent).await;
    assert_eq!(
        kind,
        HandshakeKind::Resumed,
        "the inner TLS resumes its session"
    );

    // Root CRL 2: taken, and the connection starts over with a full handshake, not resumed.
    served.crl(&pki.broker_ca.crl(2, &[&retired], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("2".to_owned())
        })
    );
    let (kind, _third) = connected(&mut broker, &agent).await;
    assert_eq!(
        kind,
        HandshakeKind::Full,
        "after a new root CRL the inner handshake is a full one"
    );
    assert_eq!(loaded_number(&files).as_deref(), Some("2"), "saved");
    let marker = std::fs::read_to_string(files.path("root-crl.pem.highest")).unwrap();
    assert!(marker.contains("number = \"2\""), "{marker}");

    // Root CRL 3 revokes the broker's issuing CA: the connection starts over and is refused.
    served.crl(&pki.broker_ca.crl(3, &[&retired, &issuing], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("3".to_owned())
        })
    );
    let refused = agent
        .wait_state("an attempt refused for revocation", |state| {
            matches!(state, LinkState::Disconnected { error, .. } if error.contains("Revoked"))
        })
        .await;
    let LinkState::Disconnected { error, .. } = refused else {
        unreachable!()
    };
    assert!(error.contains("inner mTLS"), "{error}");

    // Not newer, or not signed by the root: refused, root CRL 3 stays in use and on disk.
    for (what, answer, reason) in [
        (
            "lower",
            pki.broker_ca.crl(2, &[&retired], (2099, 1, 1)),
            "is lower than 3",
        ),
        (
            "a key with the root's name",
            Authority::new("testkit broker CA").crl(9, &[], (2099, 1, 1)),
            "does not verify",
        ),
        (
            "another root",
            Authority::new("testkit other root CA").crl(9, &[], (2099, 1, 1)),
            "is not a trust anchor",
        ),
        ("not a CRL", "not a CRL\n".to_owned(), "holds 0 PEM blocks"),
    ] {
        served.crl(&answer);
        match updater.update().await {
            Ok(Update::Refused(why)) => assert!(why.contains(reason), "{what}: {why}"),
            other => panic!("{what}: {other:?}"),
        }
        assert_eq!(loaded_number(&files).as_deref(), Some("3"), "{what}");
        assert!(matches!(
            updater.inner().root_crl(),
            RootCrl::Loaded { number: Some(number), .. } if number == "3"
        ));
    }
    // Anything but 200 or 404 is a failure to retry; 404 keeps the one held too.
    served.set(Fetched::Status(503));
    let error = updater.update().await.unwrap_err();
    assert!(error.contains("HTTP 503"), "{error}");
    served.set(Fetched::NotPublished);
    assert_eq!(updater.update().await, Ok(Update::NotPublished));
    agent.stop().await;

    // A restart reuses the root CRL saved, and still refuses the broker.
    assert_eq!(loaded_number(&files).as_deref(), Some("3"));
    let (agent, _updater) = start(&files, &served);
    let error = agent.first_error().await;
    assert!(error.contains("Revoked"), "{error}");
    agent.stop().await;
}

#[tokio::test]
async fn deleting_or_rolling_back_the_root_crl_never_turns_revocation_checking_off() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing).await;
    let older = pki.broker_ca.crl(2, &[&retired], (2099, 1, 1));
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(3, &[&retired], (2099, 1, 1)),
    );
    // The updater writes the marker of the root CRL accepted.
    let served = Served::new(Fetched::NotPublished);
    let (agent, updater) = start(&files, &served);
    assert_eq!(updater.settle().await, Ok(Update::Unchanged));
    let (_, _mqtt) = connected(&mut broker, &agent).await;
    agent.stop().await;
    drop(updater);

    for (what, change) in [
        ("deleted", None),
        ("replaced by an older one", Some(older.as_str())),
    ] {
        match change {
            None => std::fs::remove_file(files.path("root-crl.pem")).unwrap(),
            Some(contents) => files.write("root-crl.pem", contents),
        }
        let root_crl = files.setup().unwrap().root_crl;
        assert!(
            matches!(&root_crl, RootCrl::Withheld { number: Some(number), .. } if number == "3"),
            "{what}: {root_crl:?}"
        );
        let (agent, _updater) = start(&files, &served);
        let error = agent.first_error().await;
        assert!(
            error.contains("connecting failed at root CRL: ")
                && error.contains("no connection until"),
            "{what}: {error}"
        );
        agent.stop().await;
    }

    // Running, the updater asks at once while withheld (not after `first_within`), retries with
    // backoff while the cloud has nothing newer, and the agent connects once it gets one at
    // least as new as the one accepted.
    let (agent, updater) = start(&files, &served);
    let updater = updater.with_schedule(Schedule {
        first_within: Duration::from_secs(3600),
        interval: Duration::from_secs(3600),
        retry: BackoffSettings {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(1),
        },
    });
    let inner = updater.inner().clone();
    let running = tokio::spawn(updater.run(agent.shutdown.clone()));
    served.crl(&older);
    let error = agent.first_error().await;
    assert!(error.contains("connecting failed at root CRL: "), "{error}");
    served.crl(&pki.broker_ca.crl(4, &[&retired], (2099, 1, 1)));
    let (kind, _mqtt) = connected(&mut broker, &agent).await;
    assert_eq!(kind, HandshakeKind::Full);
    assert!(matches!(
        inner.root_crl(),
        RootCrl::Loaded { number: Some(number), .. } if number == "4"
    ));
    agent.stop().await;
    bounded("the updater to stop", running).await.unwrap();
    assert_eq!(loaded_number(&files).as_deref(), Some("4"));
}

/// Laysi/trustiot-v2#926: `provision install` replaced the root without a root CRL while
/// broker.crl was gone, which leaves the marker of the old root. The root CRL the agent fetches
/// is saved (broker.ca, read again under the lock, still holds the roots in use, so the marker is
/// stale for certain) without running `provision install` again, and a restart connects with it.
#[tokio::test]
async fn a_stale_marker_of_another_root_without_broker_crl_does_not_keep_root_crls_from_being_saved()
 {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing).await;
    files.write(
        "root-crl.pem.highest",
        &Marker::text(&RootKey::of_spki(b"the old root"), "50"),
    );
    let served = Served::new(Fetched::NotPublished);
    let (agent, updater) = start(&files, &served);
    let (_, _first) = connected(&mut broker, &agent).await;

    served.crl(&pki.broker_ca.crl(2, &[&retired], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("2".to_owned())
        })
    );
    let (kind, _second) = connected(&mut broker, &agent).await;
    assert_eq!(kind, HandshakeKind::Full);
    assert_eq!(loaded_number(&files).as_deref(), Some("2"), "saved");
    let marker = std::fs::read_to_string(files.path("root-crl.pem.highest")).unwrap();
    assert!(marker.contains("number = \"2\""), "{marker}");
    agent.stop().await;
    drop(updater);

    // A restart loads it and connects; an older one fetched is refused.
    let (agent, updater) = start(&files, &served);
    let (kind, _third) = connected(&mut broker, &agent).await;
    assert_eq!(kind, HandshakeKind::Full);
    served.crl(&pki.broker_ca.crl(1, &[&retired], (2099, 1, 1)));
    match updater.update().await {
        Ok(Update::Refused(why)) => assert!(why.contains("is lower than 2"), "{why}"),
        other => panic!("{other:?}"),
    }
    agent.stop().await;
}

/// The next packet the agent sends on `mqtt`, `None` once it closed the connection.
async fn next_packet(mqtt: &mut FakeBroker<TlsStream<TcpStream>>, what: &str) -> Option<Packet> {
    bounded(what, mqtt.read_answering_pings()).await
}

/// A telemetry publish of the agent's link, written to `mqtt` and not acknowledged yet: the task
/// that waits for its outcome and the packet identifier.
async fn in_flight(
    agent: &Agent,
    mqtt: &mut FakeBroker<TlsStream<TcpStream>>,
    payload: Vec<u8>,
) -> (
    tokio::task::JoinHandle<Result<Published, PublishError>>,
    u16,
) {
    let link = agent.link.clone();
    let sent = payload.clone();
    let publishing = tokio::spawn(async move { link.publish(Channel::Telemetry, sent).await });
    match next_packet(mqtt, "the telemetry publish").await {
        Some(Packet::Publish(message)) if message.payload == payload => {
            (publishing, message.id.unwrap())
        }
        other => panic!("expected the telemetry publish, got {other:?}"),
    }
}

#[tokio::test]
async fn a_root_crl_revoking_the_brokers_issuing_ca_closes_the_open_connection_at_once() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing).await;
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(1, &[&retired], (2099, 1, 1)),
    );
    let served = Served::new(Fetched::NotPublished);
    let (agent, updater) = start(&files, &served);
    let (_, mut mqtt) = connected(&mut broker, &agent).await;

    // Root CRL 2 does not revoke this broker's issuing CA: checked again, the connection passes
    // and finishes what it accepted as before (the publish in flight is acknowledged), then
    // ends its session cleanly and reconnects.
    let (publishing, id) = in_flight(&agent, &mut mqtt, vec![1; 64]).await;
    served.crl(&pki.broker_ca.crl(2, &[&retired], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("2".to_owned())
        })
    );
    mqtt.send(&puback(id, 0)).await;
    let outcome = bounded("the publish in flight", publishing).await.unwrap();
    assert!(
        matches!(outcome, Ok(Published::Acknowledged(_))),
        "a broker that passes keeps its connection until what it accepted is acknowledged: {outcome:?}"
    );
    let status = Channel::Status.topic(&gateway());
    match next_packet(&mut mqtt, "offline/shutdown").await {
        Some(Packet::Publish(message)) if message.topic == status => {
            mqtt.send(&puback(message.id.unwrap(), 0)).await;
        }
        other => panic!("expected offline/shutdown, got {other:?}"),
    }
    assert_eq!(
        next_packet(&mut mqtt, "the DISCONNECT").await,
        Some(Packet::Disconnect { reason: 0 })
    );
    let (kind, mut mqtt) = connected(&mut broker, &agent).await;
    assert_eq!(kind, HandshakeKind::Full);

    // Root CRL 3 revokes it. The connection, with a publish the broker has not acknowledged,
    // ends at once: the broker sees the stream end before anything else (no offline/shutdown,
    // no DISCONNECT, no wait for the acknowledgement, which the rotation would wait for up to
    // twice the keepalive, 120 seconds, longer than this test waits), and the publish ends as
    // lost, so the offline buffer sends it again (tests/upload.rs).
    let (publishing, _) = in_flight(&agent, &mut mqtt, vec![2; 64]).await;
    served.crl(&pki.broker_ca.crl(3, &[&retired, &issuing], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("3".to_owned())
        })
    );
    assert_eq!(
        next_packet(&mut mqtt, "the end of the revoked broker's connection").await,
        None,
        "nothing more reaches a broker the root CRL revokes"
    );
    assert_eq!(
        bounded("the publish in flight", publishing).await.unwrap(),
        Err(PublishError::ConnectionLost)
    );
    // The next connection is a full handshake checked against root CRL 3: refused.
    let refused = agent
        .wait_state("an attempt refused for revocation", |state| {
            matches!(state, LinkState::Disconnected { error, .. } if error.contains("Revoked"))
        })
        .await;
    let LinkState::Disconnected { error, .. } = refused else {
        unreachable!()
    };
    assert!(error.contains("inner mTLS"), "{error}");
    agent.stop().await;
}

/// The next heartbeat the agent publishes on `mqtt`.
async fn next_heartbeat(mqtt: &mut FakeBroker<TlsStream<TcpStream>>) -> Heartbeat {
    let topic = Channel::Heartbeat.topic(&gateway());
    bounded("a heartbeat", async {
        loop {
            match mqtt.read_answering_pings().await {
                Some(Packet::Publish(message)) if message.topic == topic => {
                    return Heartbeat::decode(&message.payload).unwrap();
                }
                Some(Packet::Publish(message)) => {
                    if let Some(id) = message.id {
                        mqtt.send(&puback(id, 0)).await;
                    }
                }
                Some(_) => {}
                None => panic!("the agent closed the connection"),
            }
        }
    })
    .await
}

#[tokio::test]
async fn the_heartbeat_reports_the_root_crl_and_an_expired_one_still_connects() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing).await;
    // Expired since 2021-01-01: still used, and the agent still connects.
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(5, &[&retired], (2021, 1, 1)),
    );
    let served = Served::new(Fetched::NotPublished);
    let mut setup = files.setup().unwrap();
    let in_use = setup.inner.root_crl_in_use();
    let updater = setup
        .root_crl_updater
        .take()
        .unwrap()
        .with_source(served.clone());
    let agent = Agent::from_setup(setup);
    let heartbeats = tokio::spawn(agent::heartbeat::run(
        agent.link.clone(),
        HeartbeatSettings {
            interval: Duration::from_secs(3600),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: tokio::sync::watch::channel(UnixMillis::new(1_900_000_000_000))
                .1,
            buffer: agent::upload::Stats::default(),
            clock: agent::clock::GatewayClock::system(),
            root_crl: in_use,
        },
        tokio::time::Instant::now(),
        agent.shutdown.clone(),
    ));
    let (_, mut mqtt) = connected(&mut broker, &agent).await;
    assert_eq!(
        next_heartbeat(&mut mqtt).await.root_crl,
        Some(RootCrlStatus {
            number: Some("5".to_owned()),
            next_update: Some(UnixMillis::new(1_609_459_200_000)),
            expired: true,
        })
    );

    // A newer one, not expired: the heartbeats after the reconnect report it.
    served.crl(&pki.broker_ca.crl(6, &[&retired], (2099, 1, 1)));
    assert_eq!(
        updater.update().await,
        Ok(Update::Adopted {
            number: Some("6".to_owned())
        })
    );
    let (_, mut mqtt) = connected(&mut broker, &agent).await;
    assert_eq!(
        next_heartbeat(&mut mqtt).await.root_crl,
        Some(RootCrlStatus {
            number: Some("6".to_owned()),
            next_update: Some(UnixMillis::new(4_070_908_800_000)),
            expired: false,
        })
    );
    agent.stop().await;
    bounded("the heartbeats to stop", heartbeats).await.unwrap();
}

/// What the stand-in gateway host saw of one request: method, path, `Host`, `Accept`.
type Seen = (String, String, String, String);

#[tokio::test]
async fn the_root_crl_is_a_get_on_the_gateway_host_of_cloud_url() {
    let pki = TestPki::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let answer: Arc<Mutex<(u16, Vec<u8>)>> = Arc::new(Mutex::new((404, Vec::new())));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let acceptor = TlsAcceptor::from(pki.front_tls());
    let server = {
        let (answer, seen) = (Arc::clone(&answer), Arc::clone(&seen));
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let (answer, seen) = (Arc::clone(&answer), Arc::clone(&seen));
                let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                    let header = |name| {
                        request
                            .headers()
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned()
                    };
                    seen.lock().unwrap().push((
                        request.method().to_string(),
                        request.uri().to_string(),
                        header(HOST),
                        header(ACCEPT),
                    ));
                    let (status, body) = answer.lock().unwrap().clone();
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::from_u16(status).unwrap())
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            }
        })
    };
    let dir = ScratchDir::new("agent-root-crl-https");
    let outer =
        agent::tls::outer_config(&[dir.write("public-ca.pem", &pki.public_ca.pem)]).unwrap();
    let url = CloudUrl::parse(&format!("wss://{FRONT_HOST}:{port}/mqtt")).unwrap();
    let endpoint = HttpsEndpoint::new(&url, outer);
    assert_eq!(
        endpoint.describe(),
        format!("https://{FRONT_HOST}:{port}/certs/root-crl")
    );

    let crl = pki.broker_ca.crl(1, &[], (2099, 1, 1));
    *answer.lock().unwrap() = (200, crl.clone().into_bytes());
    assert_eq!(
        endpoint.fetch().await.unwrap(),
        Fetched::Crl(crl.into_bytes())
    );
    assert_eq!(
        seen.lock().unwrap().last().cloned().unwrap(),
        (
            "GET".to_owned(),
            "/certs/root-crl".to_owned(),
            format!("{FRONT_HOST}:{port}"),
            "application/x-pem-file".to_owned()
        )
    );
    *answer.lock().unwrap() = (404, b"no root CRL".to_vec());
    assert_eq!(endpoint.fetch().await.unwrap(), Fetched::NotPublished);
    *answer.lock().unwrap() = (503, Vec::new());
    assert_eq!(endpoint.fetch().await.unwrap(), Fetched::Status(503));
    // More than a root CRL may be: refused while reading, not kept.
    let limit = usize::try_from(root_crl::MAX_BYTES).unwrap();
    *answer.lock().unwrap() = (200, vec![b'A'; limit + 1]);
    let error = endpoint.fetch().await.unwrap_err();
    assert!(error.to_string().contains("larger than"), "{error}");
    assert_eq!(seen.lock().unwrap().len(), 4, "one request per fetch");
    server.abort();
}
