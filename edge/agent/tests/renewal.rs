//! Certificate renewal end to end (Laysi/trustiot-v2#83, rebuild checklist D2): the agent as the
//! binary wires it, renewing over HTTPS against a stand-in of the certs service's endpoint
//! (`support::renewal`), with a software key in place of the TPM (the TPM signs the same way;
//! `tests/tpm_swtpm.rs` covers it).
//!
//! - In process: a broker that refuses the certificate (as when it was revoked) makes the agent
//!   renew at once and reconnect with the new one; 429 and 503 from the endpoint are waited out.
//! - Against Mosquitto in a container (podman or docker; without one these skip, with `CI` set
//!   they fail): a short-lived certificate is renewed before it expires while messages keep
//!   flowing, none lost, and an expired one is renewed and used.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use agent::heartbeat::{self, HeartbeatSettings};
use agent::link::{LinkState, PublishError, Published, Subscriptions};
use protocol::health::{Heartbeat, OfflineReason, Status};
use protocol::topic::Channel;
use protocol::{AgentVersion, SessionId, UnixMillis};
use rcgen::KeyPair;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use support::renewal::{RenewalServer, Renewing};
use support::wss::{Files, InnerBroker, front};
use support::{FakeBroker, GATEWAY, gateway};
use testkit::clock::bounded;
use testkit::mqtt::mosquitto::Mosquitto;
use testkit::mqtt::{Authority, TestPki, mtls_server};
use tokio_util::sync::CancellationToken;

/// A generous bound for anything that crosses the container runtime (CLAUDE.md: test time
/// limits; CI runners are often loaded).
const LIMIT: Duration = Duration::from_secs(120);

/// Runs blocking broker helpers without stalling the agent's tasks.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

/// The certificates of a PEM file.
fn chain_of(path: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn mode(path: &str) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The gateway's certificate from `issuing` for a new key, valid from `not_before` to
/// `not_after`, written with `issuing` after it, as production gateways have it.
fn give_certificate(
    files: &Files,
    issuing: &Authority,
    not_before: SystemTime,
    not_after: SystemTime,
) -> String {
    let key = KeyPair::generate().unwrap();
    let certificate = issuing.client_for_key(GATEWAY, &key, not_before, not_after);
    files.write("gateway.pem", &format!("{certificate}{}", issuing.pem));
    files.write("gateway.key", &key.serialize_pem());
    std::fs::set_permissions(
        files.path("gateway.pem"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    key.serialize_pem()
}

#[tokio::test]
async fn a_certificate_the_broker_refuses_is_renewed_over_https_and_the_agent_reconnects_with_it() {
    let pki = TestPki::new();
    let revoked = pki
        .broker_ca
        .issuing("issuing CA whose certificates the broker refuses");
    let current = pki.broker_ca.issuing("current issuing CA");
    // The broker takes only certificates of the current issuing CA: to it the gateway's
    // certificate is as good as revoked.
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &current)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let now = SystemTime::now();
    let key = give_certificate(
        &files,
        &revoked,
        now - Duration::from_secs(3600),
        now + Duration::from_secs(90 * 86_400),
    );
    let old = std::fs::read(files.path("gateway.pem")).unwrap();
    let server = RenewalServer::start(&pki, current, &key, Duration::from_secs(90 * 86_400)).await;
    let mut renewing = Renewing::start(&files, &server, Subscriptions::none());

    // Refused, renewed, and the next connection presents the new certificate.
    let first = broker.next().await.err().unwrap();
    assert!(first.contains("UnknownIssuer"), "{first}");
    let issued = server.wait_issued(1).await;
    let stream = loop {
        match broker.next().await {
            Ok(stream) => break stream,
            Err(refused) => assert!(refused.contains("UnknownIssuer"), "{refused}"),
        }
    };
    let presented = stream.get_ref().1.peer_certificates().unwrap().to_vec();
    assert_eq!(
        presented[0], issued,
        "the inner TLS presents the renewed certificate"
    );
    assert_eq!(presented.len(), 2, "with its issuing CA");
    let mut mqtt = FakeBroker::new(stream);
    let (_, session) = mqtt.accept().await;
    let state = renewing
        .agent
        .wait_state("connected", |state| {
            matches!(state, LinkState::Connected { .. })
        })
        .await;
    assert_eq!(state, LinkState::Connected { session });
    renewing.renewed(1).await;

    // Two requests on the gateway host's renewal path, a challenge and its redemption.
    let seen = server.seen();
    let steps: Vec<(&str, u16)> = seen.iter().map(|seen| (seen.step, seen.status)).collect();
    assert_eq!(steps, [("challenge", 200), ("renew", 201)]);
    let host = format!("localhost:{}", server.address().port());
    assert!(seen.iter().all(|seen| seen.host == host), "{seen:?}");
    // The file holds the new chain, with the old file's permissions.
    let written = chain_of(&files.path("gateway.pem"));
    assert_eq!(written, presented);
    assert_ne!(std::fs::read(files.path("gateway.pem")).unwrap(), old);
    assert_eq!(mode(&files.path("gateway.pem")), 0o640);
    // Heartbeats report the new expiry.
    let not_after = *renewing.not_after.borrow();
    let in_80_days = UnixMillis::from_system_time(now + Duration::from_secs(80 * 86_400)).unwrap();
    assert!(not_after > in_80_days, "{not_after:?}");
    renewing.stop().await;
}

#[tokio::test]
async fn the_endpoint_s_429_and_503_are_waited_out_and_each_retry_starts_with_a_challenge() {
    let pki = TestPki::new();
    let issuing = pki.broker_ca.issuing("issuing CA");
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    files.append_config("\n[renewal]\nretry_base_s = 1\nretry_cap_s = 4\n");
    // Expired: due for renewal at once, and the broker refuses it.
    let now = SystemTime::now();
    let key = give_certificate(
        &files,
        &issuing,
        now - Duration::from_secs(7200),
        now - Duration::from_secs(3600),
    );
    let server = RenewalServer::start(&pki, issuing, &key, Duration::from_secs(3600)).await;
    server.refuse_next(503, Some(2));
    server.refuse_next(429, Some(3));
    let mut renewing = Renewing::start(&files, &server, Subscriptions::none());
    server.wait_issued(1).await;
    let stream = loop {
        match broker.next().await {
            Ok(stream) => break stream,
            Err(refused) => assert!(refused.to_lowercase().contains("expired"), "{refused}"),
        }
    };
    let mut mqtt = FakeBroker::new(stream);
    mqtt.accept().await;
    renewing.renewed(1).await;

    let seen = server.seen();
    let steps: Vec<(&str, u16)> = seen.iter().map(|seen| (seen.step, seen.status)).collect();
    assert_eq!(
        steps,
        [
            ("challenge", 503),
            ("challenge", 429),
            ("challenge", 200),
            ("renew", 201)
        ]
    );
    // Each wait is at least the Retry-After the endpoint asked for.
    assert!(seen[1].at - seen[0].at >= Duration::from_secs(2));
    assert!(seen[2].at - seen[1].at >= Duration::from_secs(3));
    renewing.stop().await;
}

fn status_topic() -> String {
    Channel::Status.topic(&gateway())
}

async fn connected_other_than(renewing: &Renewing, previous: Option<&SessionId>) -> SessionId {
    match renewing
        .agent
        .wait_state(
            "a connection",
            |state| matches!(state, LinkState::Connected { session } if Some(session) != previous),
        )
        .await
    {
        LinkState::Connected { session } => session,
        other => unreachable!("{other:?}"),
    }
}

/// The certificate is renewed while the gateway publishes without a pause: the connection
/// finishes what it accepted before it reconnects with the new certificate, so no publish ends
/// in doubt (`ConnectionLost`) and every message reaches the broker exactly once. The old
/// session ends with `offline`/`shutdown`, and heartbeats report the new expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_lived_certificate_is_renewed_before_it_expires_without_losing_messages() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let issuing = pki.broker_ca.issuing("issuing CA");
    let front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    files.append_config("\n[renewal]\nremaining_percent = 50\nretry_base_s = 1\nretry_cap_s = 4\n");
    // Issued like the certs service issues them, starting five minutes back (the renewed one
    // must start later), and valid for under 11 minutes: half of it is left 20 s from now. The
    // endpoint holds its answers until the gateway publishes steadily, so the renewal cannot
    // finish before the first connection, however slow the host.
    let now = SystemTime::now();
    let key = give_certificate(
        &files,
        &issuing,
        now - Duration::from_secs(305),
        now + Duration::from_secs(345),
    );
    let server = RenewalServer::start(&pki, issuing, &key, Duration::from_secs(3600)).await;
    server.hold();
    let statuses = blocking(|| broker.subscribe(&status_topic()));
    let telemetry_topic = Channel::Telemetry.topic(&gateway());
    let telemetry = blocking(|| broker.subscribe(&telemetry_topic));
    let heartbeat_topic = Channel::Heartbeat.topic(&gateway());
    let heartbeats = blocking(|| broker.subscribe(&heartbeat_topic));
    let mut renewing = Renewing::start(&files, &server, Subscriptions::none());
    let first = connected_other_than(&renewing, None).await;
    let shutdown = CancellationToken::new();
    tokio::spawn(heartbeat::run(
        renewing.agent.link.clone(),
        HeartbeatSettings {
            interval: Duration::from_secs(1),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: renewing.not_after.clone(),
        },
        tokio::time::Instant::now(),
        shutdown.clone(),
    ));

    // A publisher that keeps going, retrying what the link did not take (NotConnected, Busy) as
    // the offline buffer will, and counting what ends in doubt.
    let acknowledged = Arc::new(AtomicU32::new(0));
    let publisher = {
        let link = renewing.agent.link.clone();
        let acknowledged = Arc::clone(&acknowledged);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let (mut in_doubt, mut retried) = (0_u32, 0_u32);
            while !shutdown.is_cancelled() {
                let next = acknowledged.load(Ordering::Relaxed);
                match link
                    .publish(Channel::Telemetry, next.to_be_bytes().to_vec())
                    .await
                {
                    Ok(Published::Acknowledged(_)) => {
                        acknowledged.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Published::Sent) => panic!("telemetry is QoS 1"),
                    Err(PublishError::NotConnected | PublishError::Busy) => retried += 1,
                    Err(PublishError::ConnectionLost) => in_doubt += 1,
                    Err(other) => panic!("publish {next}: {other}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (in_doubt, retried)
        })
    };

    bounded("messages before the renewal", async {
        while acknowledged.load(Ordering::Relaxed) < 20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(
        *renewing.version.borrow(),
        0,
        "renewed while the endpoint held its answers"
    );
    server.release();
    renewing.renewed(1).await;
    let second = connected_other_than(&renewing, Some(&first)).await;
    let at_reconnect = acknowledged.load(Ordering::Relaxed);
    bounded("more messages after the reconnect", async {
        while acknowledged.load(Ordering::Relaxed) < at_reconnect + 20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    shutdown.cancel();
    let (in_doubt, retried) = bounded("the publisher", publisher).await.unwrap();
    let sent = acknowledged.load(Ordering::Relaxed);
    assert_eq!(in_doubt, 0, "a publish ended in doubt during the rotation");
    assert!(
        sent > at_reconnect && at_reconnect > 0,
        "{at_reconnect} then {sent}"
    );

    // Every acknowledged message arrived, each once.
    let received = blocking(|| {
        let mut seen = Vec::new();
        while seen.len() < usize::try_from(sent).unwrap() {
            let message = telemetry
                .next_message(LIMIT)
                .unwrap_or_else(|| panic!("only {} of {sent} messages arrived", seen.len()));
            seen.push(u32::from_be_bytes(message.payload.try_into().unwrap()));
        }
        seen
    });
    let distinct: BTreeSet<u32> = received.iter().copied().collect();
    assert_eq!(distinct.len(), received.len(), "a message arrived twice");
    assert_eq!(distinct, (0..sent).collect::<BTreeSet<u32>>());
    eprintln!(
        "renewal under load: {sent} messages, {retried} retried while the link took none, none lost"
    );

    // Presence: the first session ended on purpose, the second came online.
    blocking(|| {
        let offline = statuses.wait_for("offline of the first session", LIMIT, |message| {
            matches!(Status::decode(&message.payload), Ok(Status::Offline(offline)) if offline.session == first)
        });
        let Ok(Status::Offline(offline)) = Status::decode(&offline.payload) else {
            unreachable!()
        };
        assert_eq!(offline.reason, OfflineReason::Shutdown);
        statuses.wait_for("online of the second session", LIMIT, |message| {
            matches!(Status::decode(&message.payload), Ok(Status::Online(online)) if online.session == second)
        });
    });
    // Heartbeats of the second session carry the new expiry.
    let renewed_not_after = *renewing.not_after.borrow();
    assert!(renewed_not_after.get() > 0);
    blocking(|| {
        heartbeats.wait_for("a heartbeat with the new expiry", LIMIT, |message| {
            Heartbeat::decode(&message.payload).is_ok_and(|heartbeat| {
                heartbeat.session == second && heartbeat.cert_not_after == renewed_not_after
            })
        });
    });
    assert_eq!(
        chain_of(&files.path("gateway.pem"))[0],
        CertificateDer::from_pem_slice(server.issued()[0].as_bytes()).unwrap()
    );
    renewing.stop().await;
}

/// An expired certificate (the broker refuses it) is renewed, and the agent connects with the
/// new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_certificate_is_renewed_and_the_broker_takes_the_new_one() {
    let pki = TestPki::new();
    let Some(broker) = blocking(|| Mosquitto::start(&pki)) else {
        return;
    };
    blocking(|| broker.register_gateway(GATEWAY));
    let issuing = pki.broker_ca.issuing("issuing CA");
    let front = front(&pki, broker.address(), |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let now = SystemTime::now();
    let key = give_certificate(
        &files,
        &issuing,
        now - Duration::from_secs(7200),
        now - Duration::from_secs(3600),
    );
    let server = RenewalServer::start(&pki, issuing, &key, Duration::from_secs(3600)).await;
    let statuses = blocking(|| broker.subscribe(&status_topic()));
    let mut renewing = Renewing::start(&files, &server, Subscriptions::none());
    let session = connected_other_than(&renewing, None).await;
    renewing.renewed(1).await;
    assert_eq!(server.issued().len(), 1);
    blocking(|| {
        statuses.wait_for("online", LIMIT, |message| {
            matches!(Status::decode(&message.payload), Ok(Status::Online(online)) if online.session == session)
        });
    });
    assert_eq!(
        chain_of(&files.path("gateway.pem"))[0],
        CertificateDer::from_pem_slice(server.issued()[0].as_bytes()).unwrap()
    );
    renewing.stop().await;
}
