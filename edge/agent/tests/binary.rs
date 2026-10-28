//! The `trustiot-agent` executable as systemd runs it (Laysi/trustiot-v2#86): `--check`, the
//! `sd_notify` messages (`READY=1` before the first connection, `STATUS=`, `WATCHDOG=1` at half
//! of `WATCHDOG_USEC`, `STOPPING=1`), the device configuration (subscribed, applied, reported,
//! saved, and polled after a restart without the broker; Laysi/trustiot-v2#94), and SIGTERM
//! ending with `offline`/`shutdown`, a normal DISCONNECT and exit code 0. The test plays systemd's notification socket (an abstract Unix
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

use protocol::config::{
    ByteOrder, ConfigReport, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode,
    Host, Transport,
};
use protocol::health::{OfflineReason, Status};
use protocol::topic::Channel;
use protocol::{ConfigVersion, DeviceId, ModelName, PointKey, V1};
use support::wss::{Files, InnerBroker, front};
use support::{GATEWAY, Packet, gateway, puback, publish};
use testkit::clock::bounded;
use testkit::mqtt::{Authority, TestPki, mtls_server};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines};
use tokio::net::UnixDatagram;
use tokio::process::ChildStderr;
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
        // As systemd passes the unit's StateDirectory.
        .env("STATE_DIRECTORY", files.path("state"))
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

/// Version 1 of a configuration with one device at 127.0.0.1, an address the agent's address
/// guard refuses (Laysi/trustiot-v2#653): polling it fails without opening a socket, so the test
/// sends no packet to any device.
fn desired_config() -> Vec<u8> {
    let model = DeviceModel {
        name: ModelName::new("meter").unwrap(),
        points: vec![DataPoint {
            key: PointKey::new("power").unwrap(),
            function_code: FunctionCode::ReadHoldingRegisters,
            address: 0,
            data_type: DataType::U16,
            byte_order: ByteOrder::Abcd,
            length: None,
            scale: None,
            offset: None,
        }],
    };
    DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(1).unwrap(),
        devices: vec![Device {
            id: DeviceId::new("meter-1").unwrap(),
            model: model.name.clone(),
            transport: Transport::ModbusTcp,
            host: Host::new("127.0.0.1").unwrap(),
            port: 502,
            unit_id: 1,
            poll_interval_ms: 1000,
        }],
        models: vec![model],
    }
    .encode()
    .unwrap()
}

/// The agent's log, read line by line while it runs.
struct Log {
    lines: Lines<BufReader<ChildStderr>>,
    seen: Vec<String>,
}

impl Log {
    fn of(child: &mut Child) -> Self {
        Self {
            lines: BufReader::new(child.stderr.take().unwrap()).lines(),
            seen: Vec::new(),
        }
    }

    /// Waits for a line that contains every one of `parts`.
    async fn wait_for(&mut self, parts: &[&str]) -> String {
        let lines = &mut self.lines;
        let seen = &mut self.seen;
        bounded(format!("a log line with {parts:?}"), async {
            loop {
                let Some(line) = lines.next_line().await.unwrap() else {
                    panic!("the log ended without {parts:?}:\n{}", seen.join("\n"))
                };
                seen.push(line.clone());
                if parts.iter().all(|part| line.contains(part)) {
                    return line;
                }
            }
        })
        .await
    }

    /// Waits for the process to exit; returns whether it succeeded and the rest of the log.
    async fn exit(self, mut child: Child) -> (bool, String) {
        let mut rest = String::new();
        let mut stderr = self.lines.into_inner();
        bounded("the rest of the log", stderr.read_to_string(&mut rest))
            .await
            .unwrap();
        let status = bounded("the agent to exit", child.wait()).await.unwrap();
        (status.success(), rest)
    }
}

#[tokio::test]
async fn connected_the_agent_applies_its_configuration_reports_it_and_polls_it_after_a_restart() {
    let pki = TestPki::new();
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let mut notify = NotifySocket::new();
    let child = agent(&files, &notify);
    notify.wait_for("READY=1", |line| line == "READY=1").await;
    let (mqtt, _) = broker.next_broker().await;
    // The agent handles down/config, so its link subscribes to it after `online`, and only to
    // it; `accept` checks the SUBSCRIBE.
    let mut mqtt = mqtt.subscribing(&[Channel::Config]);
    let (_, session) = bounded("the connection", mqtt.accept()).await;
    // The broker delivers the retained configuration right after the SUBACK.
    let config = desired_config();
    mqtt.send(&publish(
        &Channel::Config.topic(&gateway()),
        &config,
        1,
        true,
        Some(1),
    ))
    .await;
    // The first heartbeat (sent once Connected), the PUBACK of the delivery and the report, in
    // any order.
    let (mut heartbeat, mut acked, mut report) = (false, false, None);
    while !(heartbeat && acked && report.is_some()) {
        match bounded(
            "the heartbeat, PUBACK and report",
            mqtt.read_answering_pings(),
        )
        .await
        {
            Some(Packet::Publish(publish))
                if publish.topic == Channel::Heartbeat.topic(&gateway()) =>
            {
                heartbeat = true;
            }
            Some(Packet::Publish(publish))
                if publish.topic == Channel::ConfigReport.topic(&gateway()) =>
            {
                mqtt.send(&puback(publish.id.unwrap(), 0)).await;
                report = Some(ConfigReport::decode(&publish.payload).unwrap());
            }
            Some(Packet::PubAck { id: 1, reason: 0 }) => acked = true,
            other => panic!("expected the heartbeat, the PUBACK and the report, got {other:?}"),
        }
    }
    assert_eq!(
        report,
        Some(ConfigReport::applied(ConfigVersion::new(1).unwrap()))
    );
    // Saved on the data partition before it took effect.
    assert_eq!(
        std::fs::read(files.path("state/desired-config.cbor")).unwrap(),
        config
    );
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
    assert!(log.contains("applied the configuration"), "{log}");

    // Started again with no way to the broker, it polls the saved configuration at once. The
    // device's address is refused by the address guard, so each poll fails to connect.
    let closed = ClosedPort::new();
    files.write_config(closed.port(), "public-ca.pem");
    let notify = NotifySocket::new();
    let mut child = agent(&files, &notify);
    let mut log = Log::of(&mut child);
    log.wait_for(&["polling with the saved configuration", "version=1"])
        .await;
    log.wait_for(&["polled", "device=meter-1", "failed=1"])
        .await;
    terminate(&child);
    let (success, rest) = log.exit(child).await;
    assert!(success, "{rest}");
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
