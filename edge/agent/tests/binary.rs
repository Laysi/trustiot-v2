//! The `trustiot-agent` executable as systemd runs it (Laysi/trustiot-v2#86): `--check`, the
//! `sd_notify` messages (`READY=1` before the first connection, `STATUS=`, `WATCHDOG=1` at half
//! of `WATCHDOG_USEC`, `STOPPING=1`), and SIGTERM ending with `offline`/`shutdown`, a normal
//! DISCONNECT and exit code 0. The test plays systemd's notification socket (an abstract Unix
//! socket, as `NOTIFY_SOCKET=@...`) and the broker (`support::wss::InnerBroker` behind the
//! in-process front).

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::os::linux::net::SocketAddrExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

use protocol::health::{OfflineReason, Status};
use protocol::topic::Channel;
use support::wss::{Files, InnerBroker, front};
use support::{GATEWAY, Packet, gateway, puback};
use testkit::clock::bounded;
use testkit::mqtt::{Authority, TestPki, mtls_server};
use tokio::net::UnixDatagram;
use tokio::process::{Child, Command};

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent");

/// systemd's side of `sd_notify`.
struct NotifySocket {
    name: String,
    socket: UnixDatagram,
    received: Vec<String>,
}

impl NotifySocket {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "trustiot-agent-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let socket = std::os::unix::net::UnixDatagram::bind_addr(&address).unwrap();
        socket.set_nonblocking(true).unwrap();
        Self {
            name,
            socket: UnixDatagram::from_std(socket).unwrap(),
            received: Vec::new(),
        }
    }

    /// The `NOTIFY_SOCKET` value.
    fn env(&self) -> String {
        format!("@{}", self.name)
    }

    /// Waits for a datagram with a line that `wanted` accepts; returns the line.
    async fn wait_for(&mut self, what: &str, wanted: impl Fn(&str) -> bool) -> String {
        let socket = &self.socket;
        let received = &mut self.received;
        bounded(format!("{what} on the notification socket"), async {
            let mut buffer = vec![0_u8; 4096];
            loop {
                let len = socket.recv(&mut buffer).await.unwrap();
                let datagram = String::from_utf8(buffer[..len].to_vec()).unwrap();
                let found = datagram
                    .lines()
                    .find(|line| wanted(line))
                    .map(str::to_owned);
                received.push(datagram);
                if let Some(line) = found {
                    return line;
                }
            }
        })
        .await
    }
}

fn agent(files: &Files, notify: &NotifySocket) -> Child {
    Command::new(EXECUTABLE)
        .args(["--config", &files.path("agent.toml")])
        .env("NOTIFY_SOCKET", notify.env())
        .env("WATCHDOG_USEC", "2000000")
        .env_remove("WATCHDOG_PID")
        .env("TRUSTIOT_AGENT_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

fn terminate(child: &Child) {
    let pid = child.id().unwrap().to_string();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Waits for the process to exit; returns whether it succeeded and what it logged.
async fn exit(child: Child) -> (bool, String) {
    let output = bounded("the agent to exit", child.wait_with_output())
        .await
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A loopback port that refuses connections for as long as this lives: a socket bound to it
/// without listening, so the kernel answers every connection attempt with a reset and no other
/// socket can bind the port meanwhile (the socket does not set `SO_REUSEADDR`). A port that is
/// only closed again could be taken by another test before the agent connects.
struct ClosedPort(socket2::Socket);

impl ClosedPort {
    fn new() -> Self {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket
            .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        Self(socket)
    }

    fn port(&self) -> u16 {
        self.0.local_addr().unwrap().as_socket().unwrap().port()
    }
}

#[test]
fn a_closed_port_refuses_connections_and_cannot_be_taken() {
    let closed = ClosedPort::new();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], closed.port()));
    let refused = std::net::TcpStream::connect(address).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
    // Not even a listener with SO_REUSEADDR (as tokio's) can take it.
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket.set_reuse_address(true).unwrap();
    let taken = socket.bind(&address.into()).unwrap_err();
    assert_eq!(taken.kind(), std::io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn check_accepts_a_working_configuration_and_refuses_a_key_of_another_certificate() {
    let pki = TestPki::new();
    let closed = ClosedPort::new();
    let files = Files::new(&pki, closed.port());
    let output = Command::new(EXECUTABLE)
        .args(["--config", &files.path("agent.toml"), "--check"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.starts_with(&format!(
            "ok: gateway {GATEWAY}, certificate valid until 4096-01-01T00:00:00Z, configuration "
        )),
        "{stdout}"
    );

    files.write(
        "gateway.key",
        &Authority::new("unrelated").client(GATEWAY).key_pem,
    );
    let output = Command::new(EXECUTABLE)
        .args(["--config", &files.path("agent.toml"), "--check"])
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("refusing to start"), "{stderr}");
}

#[tokio::test]
async fn without_a_broker_the_agent_is_ready_feeds_the_watchdog_and_stops_on_sigterm() {
    let pki = TestPki::new();
    let closed = ClosedPort::new();
    let files = Files::new(&pki, closed.port());
    let mut notify = NotifySocket::new();
    let child = agent(&files, &notify);
    // READY=1 does not wait for a connection: a gateway without network still runs.
    notify.wait_for("READY=1", |line| line == "READY=1").await;
    let status = notify
        .wait_for("a disconnected status", |line| {
            line.starts_with("STATUS=") && line.contains("disconnected")
        })
        .await;
    // The status names the step that failed: here TCP, against a closed port.
    assert!(
        status.contains(&format!(
            "gateway {GATEWAY}: disconnected (connecting failed at TCP: "
        )),
        "{status}"
    );
    notify
        .wait_for("WATCHDOG=1", |line| line == "WATCHDOG=1")
        .await;
    notify
        .wait_for("another WATCHDOG=1", |line| line == "WATCHDOG=1")
        .await;
    terminate(&child);
    notify
        .wait_for("STOPPING=1", |line| line == "STOPPING=1")
        .await;
    let (success, log) = exit(child).await;
    assert!(success, "{log}");
    assert!(log.contains("stopped"), "{log}");
}

#[tokio::test]
async fn connected_the_agent_subscribes_to_nothing_reports_its_session_and_stops_on_sigterm() {
    let pki = TestPki::new();
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let mut notify = NotifySocket::new();
    let child = agent(&files, &notify);
    notify.wait_for("READY=1", |line| line == "READY=1").await;
    let (mut mqtt, _) = broker.next_broker().await;
    // Nothing in the agent handles a down channel yet, so its link subscribes to none: the
    // broker expects no SUBSCRIBE, and right after `online` comes the first heartbeat, which
    // the agent sends once Connected.
    let (_, session) = bounded("the connection", mqtt.accept()).await;
    match bounded("the first heartbeat", mqtt.read_answering_pings()).await {
        Some(Packet::Publish(publish)) if publish.topic == Channel::Heartbeat.topic(&gateway()) => {
        }
        other => panic!("expected the first heartbeat right after online, got {other:?}"),
    }
    let connected = format!("STATUS=gateway {GATEWAY}: connected, session {session}");
    notify
        .wait_for("the connected status", |line| line == connected)
        .await;

    terminate(&child);
    notify
        .wait_for("STOPPING=1", |line| line == "STOPPING=1")
        .await;
    // The next heartbeat is an hour away.
    let offline = match bounded("the offline status", mqtt.read_answering_pings()).await {
        Some(Packet::Publish(publish)) if publish.topic == Channel::Status.topic(&gateway()) => {
            publish
        }
        other => panic!("expected offline, got {other:?}"),
    };
    let Status::Offline(status) = Status::decode(&offline.payload).unwrap() else {
        panic!("expected offline")
    };
    assert_eq!(
        (status.session, status.reason),
        (session, OfflineReason::Shutdown)
    );
    mqtt.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(
        bounded("the DISCONNECT", mqtt.read()).await,
        Some(Packet::Disconnect { reason: 0 })
    );
    let (success, log) = exit(child).await;
    assert!(success, "{log}");
}

async fn check(files: &Files) -> std::process::Output {
    Command::new(EXECUTABLE)
        .args(["--config", &files.path("agent.toml"), "--check"])
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn check_refuses_a_certificate_the_broker_ca_did_not_issue() {
    let pki = TestPki::new();
    let closed = ClosedPort::new();
    let files = Files::new(&pki, closed.port());
    // A well-formed client certificate with its own key, from a CA the broker does not trust.
    let other = Authority::new("another CA").client(GATEWAY);
    files.write("gateway.pem", &other.cert_pem);
    files.write("gateway.key", &other.key_pem);
    let output = check(&files).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("refusing to start") && stderr.contains("broker.ca"),
        "{stderr}"
    );
}

#[tokio::test]
async fn an_expired_certificate_only_warns_because_the_clock_may_be_wrong() {
    let pki = TestPki::new();
    let closed = ClosedPort::new();
    let files = Files::new(&pki, closed.port());
    let expired = pki
        .broker_ca
        .client_valid(GATEWAY, (2020, 1, 1), (2021, 1, 1));
    files.write("gateway.pem", &expired.cert_pem);
    files.write("gateway.key", &expired.key_pem);
    let output = check(&files).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("certificate valid until 2021-01-01T00:00:00Z"),
        "{stdout}"
    );
    assert!(
        stderr.contains("WARN")
            && stderr.contains("2020-01-01T00:00:00Z")
            && stderr.contains("2021-01-01T00:00:00Z"),
        "{stderr}"
    );
}

#[tokio::test]
async fn check_reports_the_root_crl_and_refuses_one_the_root_did_not_sign() {
    let pki = TestPki::new();
    let closed = ClosedPort::new();
    let files = Files::new(&pki, closed.port());
    // Configured, not received yet.
    let output = check(&files).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("no root CRL yet"), "{stderr}");

    // Received and expired: used anyway, with a warning.
    files.write("root-crl.pem", &pki.broker_ca.crl(3, &[], (2021, 1, 1)));
    let output = check(&files).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("root CRL loaded")
            && stderr.contains("number=\"3\"")
            && stderr.contains("2021-01-01T00:00:00Z")
            && stderr.contains("WARN"),
        "{stderr}"
    );

    // Signed by another key under our root's name.
    let forged = Authority::new("testkit broker CA").crl(4, &[], (2099, 1, 1));
    files.write("root-crl.pem", &forged);
    let output = check(&files).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("refusing to start") && stderr.contains("broker.crl"),
        "{stderr}"
    );
}
