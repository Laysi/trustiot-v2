//! The whole way to the broker in-process (Laysi/trustiot-v2#86): the configuration file, the
//! outer TLS and the WebSocket to a stand-in front (`testkit::mqtt::front`), and the inner mTLS
//! to a stand-in broker that speaks MQTT through `support::FakeBroker`. The container test
//! (`tests/broker.rs`) runs the same path against Mosquitto; these tests cover what only a
//! scripted peer can show: what each layer negotiated, resumption, and every refusal, including
//! the inner TLS's trust in the broker's certificate (Laysi/trustiot-v2#556): root anchors, the
//! issuing CA the broker sends, the fixed name, serverAuth and the root CRL.
//!
//! Real sockets need real time; every wait is bounded by `testkit::clock::bounded`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use agent::link::{DownMessage, LinkState, Published, Subscriptions};
use agent::tls::RootCrl;
use protocol::topic::Channel;
use rustls::{CipherSuite, HandshakeKind, ProtocolVersion};
use support::wss::{Agent, Files, InnerBroker, front};
use support::{FakeBroker, Packet, gateway, publish};
use testkit::clock::bounded;
use testkit::mqtt::front::{FrontSession, WsFront};
use testkit::mqtt::{
    Authority, BROKER_NAME, FRONT_HOST, TestPki, front_tls, mtls_server, mtls_server_with_chain,
};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

fn assert_negotiated(session: &FrontSession) {
    assert_eq!(session.error, None);
    assert_eq!(session.tls_version, Some(ProtocolVersion::TLSv1_3));
    assert_eq!(
        session.cipher_suite,
        Some(CipherSuite::TLS13_CHACHA20_POLY1305_SHA256)
    );
    assert_eq!(session.alpn.as_deref(), Some(&b"http/1.1"[..]));
    assert_eq!(
        session.server_name.as_deref(),
        Some(FRONT_HOST),
        "SNI is the URL's host"
    );
    assert_eq!(session.path.as_deref(), Some("/mqtt"));
    assert_eq!(session.subprotocols.as_deref(), Some("mqtt"));
    assert!(session.upgraded);
}

#[tokio::test]
async fn the_agent_reaches_the_broker_through_tls_and_websocket_with_mtls_inside() {
    let pki = TestPki::new();
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let mut agent = Agent::start_with(&files, Subscriptions::new([Channel::Config]).unwrap());

    let (mqtt, kind) = broker.next_broker().await;
    let mut mqtt = mqtt.subscribing(&[Channel::Config]);
    assert_eq!(kind, HandshakeKind::Full);
    let peer = mqtt_peer_certificate(&mqtt);
    assert_eq!(
        peer,
        files.gateway_cert(),
        "the inner TLS presents the gateway certificate"
    );
    let (_, session) = mqtt.accept().await;
    let state = agent
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    assert_eq!(state, LinkState::Connected { session });
    let sessions = front.sessions();
    assert_eq!(sessions.len(), 1);
    assert_negotiated(&sessions[0]);
    assert!(!sessions[0].resumed);

    // A publish the size of the largest live message, and a down message, both ways through
    // every layer.
    let large: Vec<u8> = (0..64 * 1024)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    let link = agent.link.clone();
    let sent = {
        let large = large.clone();
        tokio::spawn(async move { link.publish(Channel::Live, large).await })
    };
    let Some(Packet::Publish(received)) =
        bounded("the live message", mqtt.read_answering_pings()).await
    else {
        panic!("expected the live message")
    };
    assert_eq!(
        (received.topic, received.payload == large),
        (Channel::Live.topic(&gateway()), true)
    );
    assert_eq!(
        bounded("the publisher", sent).await.unwrap(),
        Ok(Published::Sent)
    );
    let config = Channel::Config.topic(&gateway());
    mqtt.send(&publish(&config, b"{}", 1, true, Some(7))).await;
    assert_eq!(
        bounded("the config", agent.incoming.recv()).await,
        Some(DownMessage {
            channel: Channel::Config,
            payload: b"{}".to_vec(),
            retained: true
        })
    );
    assert_eq!(
        bounded("the PUBACK", mqtt.read_answering_pings()).await,
        Some(Packet::PubAck { id: 7, reason: 0 })
    );
    agent.stop().await;
}

fn mqtt_peer_certificate(broker: &FakeBroker<TlsStream<TcpStream>>) -> Vec<u8> {
    let (_, server) = broker.stream().get_ref();
    server.peer_certificates().unwrap()[0].to_vec()
}

#[tokio::test]
async fn after_the_broker_or_the_relay_goes_away_the_agent_reconnects_and_resumes_tls() {
    let pki = TestPki::new();
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let mut front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let (mut mqtt, _) = broker.next_broker().await;
    let (_, first) = mqtt.accept().await;

    // The broker goes away: the relay closes the WebSocket, the agent connects again.
    drop(mqtt);
    let (mut mqtt, kind) = broker.next_broker().await;
    assert_eq!(
        kind,
        HandshakeKind::Resumed,
        "the inner TLS resumes its session"
    );
    let (_, second) = mqtt.accept().await;
    assert_ne!(first, second);
    let sessions = front.sessions();
    assert_eq!(sessions.len(), 2);
    assert_negotiated(&sessions[1]);
    assert!(sessions[1].resumed, "the outer TLS resumes its session");

    // The relay crashes (every connection cut without a WebSocket close) and comes back.
    front.stop().await;
    let state = agent
        .wait_state("the agent to notice the relay is gone", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    drop(mqtt);
    let LinkState::Disconnected { error, .. } = state else {
        unreachable!()
    };
    assert!(!error.is_empty());
    front.restart().await.unwrap();
    let (mut mqtt, _) = broker.next_broker().await;
    let (_, third) = mqtt.accept().await;
    assert_ne!(second, third);
    let state = agent
        .wait_state("connected again", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    assert_eq!(state, LinkState::Connected { session: third });
    agent.stop().await;
}

#[tokio::test]
async fn an_outer_certificate_from_another_ca_is_refused() {
    let pki = TestPki::new();
    let broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    files.write("other-ca.pem", &Authority::new("another public CA").pem);
    files.write_config(front.address().port(), "other-ca.pem");
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("outer TLS") && error.contains("UnknownIssuer"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_front_without_tls_1_3_is_refused() {
    let pki = TestPki::new();
    let broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let tls12 = front_tls(&pki.front, &[&rustls::version::TLS12]);
    let front = front(&pki, broker.address, |options| options.tls = tls12).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(error.contains("outer TLS"), "{error}");
    let sessions = front.sessions();
    assert!(
        sessions[0]
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("TLS")),
        "{sessions:?}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_front_that_does_not_answer_the_mqtt_subprotocol_is_refused() {
    let pki = TestPki::new();
    let broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |options| {
        options.answer_subprotocol = false
    })
    .await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(error.contains("WebSocket upgrade"), "{error}");
    agent.stop().await;
}

#[tokio::test]
async fn a_broker_certificate_for_another_name_is_refused() {
    let pki = TestPki::new();
    let leaf = pki.broker_ca.server(&["other.trustiot.test"]);
    let broker = InnerBroker::start(mtls_server(&leaf, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS")
            && error.contains(&format!("not valid for name \"{BROKER_NAME}\"")),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_broker_certificate_from_another_ca_is_refused() {
    let pki = TestPki::new();
    let impostor = Authority::new("impostor CA");
    let leaf = impostor.server(&[BROKER_NAME]);
    let broker = InnerBroker::start(mtls_server(&leaf, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS") && error.contains("UnknownIssuer"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_broker_that_does_not_trust_the_gateway_certificate_never_sees_mqtt() {
    let pki = TestPki::new();
    let other = Authority::new("another client CA");
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &other)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let refused = broker.next().await.err().unwrap();
    assert!(refused.contains("UnknownIssuer"), "{refused}");
    // TLS 1.3 finishes the client's side of the handshake before the server checks the client
    // certificate, so the agent learns of the refusal from the alert, before any CONNACK.
    let error = agent.first_error().await;
    assert!(error.contains("alert"), "{error}");
    agent.stop().await;
}

// The two layers trust different CAs (decision 6): the outer trusts only `cloud.trust`, the inner
// only `broker.ca`. A certificate that is valid in one layer is refused in the other.

#[tokio::test]
async fn a_broker_certificate_from_the_outer_ca_is_refused() {
    let pki = TestPki::new();
    let leaf = pki.public_ca.server(&[BROKER_NAME]);
    let broker = InnerBroker::start(mtls_server(&leaf, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS") && error.contains("UnknownIssuer"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_front_certificate_from_the_broker_ca_is_refused() {
    let pki = TestPki::new();
    let broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let leaf = pki.broker_ca.server(&[FRONT_HOST]);
    let tls = front_tls(&leaf, &[&rustls::version::TLS13]);
    let front = front(&pki, broker.address, |options| options.tls = tls).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("outer TLS") && error.contains("UnknownIssuer"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn an_outer_certificate_for_another_host_is_refused() {
    let pki = TestPki::new();
    let broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let leaf = pki.public_ca.server(&["elsewhere.trustiot.test"]);
    let tls = front_tls(&leaf, &[&rustls::version::TLS13]);
    let front = front(&pki, broker.address, |options| options.tls = tls).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("outer TLS") && error.contains("not valid for name"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_broker_without_tls_1_3_is_refused() {
    let pki = TestPki::new();
    let tls12 = testkit::mqtt::mtls_server_with_versions(
        &pki.broker,
        &pki.broker_ca,
        &[&rustls::version::TLS12],
    );
    let broker = InnerBroker::start(tls12).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(error.contains("inner mTLS"), "{error}");
    agent.stop().await;
}

// The broker's chain as production has it (services/certs/README.md, 「閘道器對 broker 伺服器憑證的
// 信任」): `broker.ca` holds only the root; the issuing CA signs the broker's certificate and the
// gateway's, and the broker sends its certificate with the issuing CA after it.

/// The stand-in broker with a certificate from `issuing`, sending `issuing` after it or not,
/// the front, and a gateway with a certificate from `issuing`.
async fn with_issuing_ca(
    pki: &TestPki,
    issuing: &Authority,
    sends_issuing_ca: bool,
) -> (InnerBroker, WsFront, Files) {
    let leaf = issuing.server(&[BROKER_NAME]);
    let tls = if sends_issuing_ca {
        mtls_server_with_chain(&leaf, &[issuing], &pki.broker_ca)
    } else {
        mtls_server(&leaf, &pki.broker_ca)
    };
    let broker = InnerBroker::start(tls).await;
    let front = front(pki, broker.address, |_| {}).await;
    let files = Files::new(pki, front.address().port());
    files.use_issuing_ca(issuing);
    (broker, front, files)
}

/// Waits for the agent to connect, answering as the broker; returns the broker's side.
async fn connected(broker: &mut InnerBroker, agent: &Agent) -> FakeBroker<TlsStream<TcpStream>> {
    let (mut mqtt, _) = broker.next_broker().await;
    let (_, session) = mqtt.accept().await;
    let state = agent
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    assert_eq!(state, LinkState::Connected { session });
    mqtt
}

#[tokio::test]
async fn the_broker_s_chain_through_the_issuing_ca_it_sends_is_accepted() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing, true).await;
    // The configuration names a root CRL that has not arrived: no revocation check.
    assert!(matches!(
        files.setup().unwrap().root_crl,
        RootCrl::Missing(_)
    ));
    let agent = Agent::start(&files);
    let mqtt = connected(&mut broker, &agent).await;
    let (_, server) = mqtt.stream().get_ref();
    assert_eq!(
        server.peer_certificates().unwrap().len(),
        2,
        "the gateway sends its certificate and the issuing CA"
    );
    assert_eq!(
        server.server_name(),
        Some(BROKER_NAME),
        "SNI is the fixed broker name"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_broker_that_does_not_send_its_issuing_ca_is_refused() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let (_broker, _front, files) = with_issuing_ca(&pki, &issuing, false).await;
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS") && error.contains("UnknownIssuer"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_certificate_without_server_auth_is_refused_as_the_broker_s() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    // A certificate for the broker's name that only allows client authentication, like every
    // gateway certificate from the same issuing CA.
    let leaf = issuing.leaf(
        &[BROKER_NAME],
        &[rcgen::ExtendedKeyUsagePurpose::ClientAuth],
    );
    let broker =
        InnerBroker::start(mtls_server_with_chain(&leaf, &[&issuing], &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS")
            && error.contains("does not allow extended key usage for server authentication"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_root_crl_that_revokes_the_issuing_ca_refuses_the_broker() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let (_broker, _front, files) = with_issuing_ca(&pki, &issuing, true).await;
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(2, &[&issuing], (2099, 1, 1)),
    );
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS") && error.contains("Revoked"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_root_crl_that_does_not_list_the_issuing_ca_lets_the_agent_connect() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing, true).await;
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(2, &[&retired], (2099, 1, 1)),
    );
    let root_crl = files.setup().unwrap().root_crl;
    assert!(
        matches!(&root_crl, RootCrl::Loaded { number: Some(number), expired: false, .. } if number == "2"),
        "{root_crl:?}"
    );
    // The broker's own certificate has no CRL here (its issuer is the issuing CA): it passes.
    let agent = Agent::start(&files);
    let _mqtt = connected(&mut broker, &agent).await;
    agent.stop().await;
}

#[tokio::test]
async fn an_expired_root_crl_still_counts_and_still_lets_the_agent_connect() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("testkit issuing CA");
    let retired = pki.broker_ca.issuing("testkit retired issuing CA");
    let (mut broker, _front, files) = with_issuing_ca(&pki, &issuing, true).await;
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(3, &[&retired], (2021, 1, 1)),
    );
    assert!(matches!(
        files.setup().unwrap().root_crl,
        RootCrl::Loaded { expired: true, .. }
    ));
    let agent = Agent::start(&files);
    let mqtt = connected(&mut broker, &agent).await;
    agent.stop().await;
    drop(mqtt);

    // Expired, it still revokes.
    files.write(
        "root-crl.pem",
        &pki.broker_ca.crl(4, &[&retired, &issuing], (2021, 1, 1)),
    );
    let agent = Agent::start(&files);
    let error = agent.first_error().await;
    assert!(
        error.contains("inner mTLS") && error.contains("Revoked"),
        "{error}"
    );
    agent.stop().await;
}

#[tokio::test]
async fn a_root_crl_the_root_did_not_sign_stops_the_agent_from_starting() {
    let pki = TestPki::new();
    let files = Files::new(&pki, 1);
    // Same issuer name as our root, another key.
    let forged = Authority::new("testkit broker CA");
    files.write("root-crl.pem", &forged.crl(5, &[], (2099, 1, 1)));
    let error = files.setup().err().unwrap();
    assert!(
        error.contains("broker.crl") && error.contains("does not verify"),
        "{error}"
    );
}
