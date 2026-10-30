//! The `trustiot-agent` executable as systemd runs it (Laysi/trustiot-v2#86): `--check`, the
//! `sd_notify` messages (`READY=1` before the first connection, `STATUS=`, `WATCHDOG=1` at half
//! of `WATCHDOG_USEC`, `STOPPING=1`), the device configuration (subscribed, applied, reported,
//! saved, and polled after a restart without the broker; Laysi/trustiot-v2#94), the telemetry
//! upload (subscribed to the acknowledgements, the offline buffer under `STATE_DIRECTORY`, a
//! buffered batch sent and released by the cloud's acknowledgement; Laysi/trustiot-v2#90), and
//! SIGTERM ending with `offline`/`shutdown`, a normal DISCONNECT and exit code 0; and the
//! fail-closed start without a valid identity (Laysi/trustiot-v2#812). The test plays
//! systemd's notification socket (an abstract Unix socket, as `NOTIFY_SOCKET=@...`) and the
//! broker (`support::wss::InnerBroker` behind the in-process front). The `provision` subcommand
//! (Laysi/trustiot-v2#613) is run up to where it would open the TPM: its usage errors, no TPM
//! device, and devices other than `/dev/tpmrm<N>`; the runs against a software TPM are
//! `tests/provision.rs`. So is `provision install` (Laysi/trustiot-v2#617): its usage errors and
//! help, and status 9 without an agent installation; the runs against a software TPM are
//! `tests/provision_install.rs`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent::clock::{GatewayClock, SystemWall};
use agent::link::{ConnectError, Connector, LinkSettings};
use agent::upload;
use protocol::config::{
    ByteOrder, ConfigReport, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode,
    Host, Transport,
};
use protocol::health::{OfflineReason, Status};
use protocol::telemetry::{AckOutcome, Reading, Sample, TelemetryAck, TelemetryBatch};
use protocol::topic::Channel;
use protocol::{AgentVersion, ConfigVersion, DeviceId, ModelName, PointKey, UnixMillis, V1, Value};
use support::wss::{Files, InnerBroker, front};
use support::{GATEWAY, Packet, gateway, puback, publish};
use testkit::clock::bounded;
use testkit::mqtt::{Authority, TestPki, mtls_server};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines};
use tokio::net::UnixDatagram;
use tokio::process::ChildStderr;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

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

    /// Every datagram that has arrived and not been read yet.
    fn drain(&mut self) -> Vec<String> {
        let mut buffer = vec![0_u8; 4096];
        let mut datagrams = Vec::new();
        loop {
            match self.socket.try_recv(&mut buffer) {
                Ok(len) => datagrams.push(String::from_utf8_lossy(&buffer[..len]).into_owned()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return datagrams,
                Err(error) => panic!("reading the notification socket: {error}"),
            }
        }
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
    // The agent handles down/config (the configuration side), down/telemetry-ack (the
    // telemetry pipeline) and down/live-control (the live leases), so its link subscribes to
    // them after `online`, and only to them, in one SUBSCRIBE; `accept` checks it.
    let mut mqtt =
        mqtt.subscribing(&[Channel::Config, Channel::TelemetryAck, Channel::LiveControl]);
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
    // The offline buffer's directory, in the state directory systemd passes, only the agent's.
    let buffer = std::fs::metadata(files.path("state/buffer")).unwrap();
    assert!(buffer.is_dir());
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&buffer.permissions()) & 0o777,
        0o700
    );
    assert!(!log.contains("cannot open the buffer directory"), "{log}");

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

/// A connection that never comes up.
struct Nowhere;

impl Connector for Nowhere {
    type Stream = tokio::io::DuplexStream;

    async fn connect(&self) -> Result<Self::Stream, ConnectError> {
        std::future::pending().await
    }
}

/// Leaves one batch of three readings in the offline buffer of the state directory `state`, as
/// an agent does that stops before it could send them: the library's pipeline on a link that
/// never connects, stopped, which writes the batch in memory to disk. Returns the readings.
async fn leave_a_batch(state: &Path) -> Vec<Reading> {
    let shutdown = CancellationToken::new();
    let (link, _incoming, supervisor) = agent::link::link(
        LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap()),
        Nowhere,
        shutdown.clone(),
    );
    drop(supervisor);
    let clock = GatewayClock::new(SystemWall, state.join("no-timesync"));
    let stop = CancellationToken::new();
    let (handles, pipeline) = upload::pipeline(
        upload::Settings::new(state.join(upload::BUFFER_DIRECTORY)),
        link,
        clock.clone(),
        stop.clone(),
    )
    .unwrap();
    let pipeline = tokio::spawn(pipeline.run());
    let at = clock.wall();
    let readings: Vec<Reading> = (0..3)
        .map(|index| Reading {
            device: DeviceId::new("meter-1").unwrap(),
            point: PointKey::new(format!("p{index}")).unwrap(),
            sample: Sample {
                at,
                value: Value::Int(index),
            },
            clock_synced: false,
        })
        .collect();
    handles.readings.push(readings.clone());
    stop.cancel();
    bounded("the pipeline to write its batch to disk", pipeline)
        .await
        .unwrap();
    shutdown.cancel();
    readings
}

/// Whether a lock this process took on the file `path` is still held, by this process or by a
/// child's copy of the handle (`/proc/locks` names the process that took a lock, and the file
/// as `<major>:<minor>:<inode>`). Only the inode is compared: on btrfs the device there is the
/// file system's, not the subvolume's that `stat` reports.
fn locked(path: &Path) -> bool {
    let inode = format!(":{}", std::fs::metadata(path).unwrap().ino());
    let pid = std::process::id().to_string();
    std::fs::read_to_string("/proc/locks")
        .unwrap()
        .lines()
        .any(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.windows(2).any(|pair| {
                pair[0] == pid && pair[1].matches(':').count() == 2 && pair[1].ends_with(&inode)
            })
        })
}

/// Waits until no lock this process took on the directory `dir` is held any more, without
/// taking it.
///
/// Laysi/trustiot-v2#817: the test's own pipeline has held the buffer directory's lock, and a
/// child that another test of this executable spawned meanwhile keeps a copy of every
/// descriptor until it executes its program, so the lock can outlive the pipeline for as long
/// as a loaded machine holds that child back. The agent started next would then find its
/// buffer directory locked and try again only after `buffer::DISK_RETRY`. Waiting here takes no
/// lock (a lock taken to check would be copied the same way), and once no copy holds it, no
/// child created later can get one.
async fn unlocked(dir: &Path) {
    bounded(
        format!(
            "the lock of {} to be released (a child process keeps a copy until it executes \
             its program)",
            dir.display()
        ),
        async {
            while locked(dir) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;
}

/// The lock check that [`unlocked`] waits with sees a directory's `flock` while it is held, and
/// not after.
#[tokio::test]
async fn the_lock_check_sees_a_directorys_lock() {
    let scratch = testkit::mqtt::ScratchDir::new("agent-binary-lock");
    let handle = std::fs::File::open(scratch.path()).unwrap();
    assert!(!locked(scratch.path()));
    handle.try_lock().unwrap();
    assert!(locked(scratch.path()));
    drop(handle);
    unlocked(scratch.path()).await;
}

/// The sizes of the segment files in the offline buffer of the state directory `state`.
fn segments(state: &Path) -> Vec<u64> {
    std::fs::read_dir(state.join(upload::BUFFER_DIRECTORY))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
        .map(|entry| entry.metadata().unwrap().len())
        .collect()
}

/// The telemetry side of the executable (Laysi/trustiot-v2#90 with Laysi/trustiot-v2#94): a
/// batch an earlier run left in the offline buffer under `STATE_DIRECTORY` goes up on
/// `up/telemetry` once the agent is connected, unchanged, and the cloud's acknowledgement on
/// `down/telemetry-ack`, which the agent's dispatcher hands to the uploader, releases it: its
/// segment is gone when the agent has stopped. Readings the executable polls itself cannot
/// reach a device on the test machine (the address guard refuses loopback and the machine's
/// own addresses), so `desired_config::polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it`
/// follows polled readings through the same pieces in process.
#[tokio::test]
async fn a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it() {
    let pki = TestPki::new();
    let mut broker = InnerBroker::start(mtls_server(&pki.broker, &pki.broker_ca)).await;
    let front = front(&pki, broker.address, |_| {}).await;
    let files = Files::new(&pki, front.address().port());
    let state = PathBuf::from(files.path("state"));
    let readings = leave_a_batch(&state).await;
    unlocked(&state.join(upload::BUFFER_DIRECTORY)).await;
    assert!(
        segments(&state).iter().any(|len| *len > 16),
        "the batch is on disk: {:?}",
        segments(&state)
    );

    let notify = NotifySocket::new();
    let mut child = agent(&files, &notify);
    let mut log = Log::of(&mut child);
    let (mqtt, _) = broker.next_broker().await;
    let mut mqtt =
        mqtt.subscribing(&[Channel::Config, Channel::TelemetryAck, Channel::LiveControl]);
    let (_, session) = bounded("the connection", mqtt.accept()).await;
    let telemetry = Channel::Telemetry.topic(&gateway());
    // The first heartbeat (sent once Connected) and the batch, in either order; the next
    // heartbeat is an hour away.
    let (mut heartbeat, mut batch) = (false, None);
    while !(heartbeat && batch.is_some()) {
        match bounded(
            "the heartbeat and the telemetry batch",
            mqtt.read_answering_pings(),
        )
        .await
        {
            Some(Packet::Publish(publish)) if publish.topic == telemetry => {
                assert!(publish.qos == 1 && !publish.retain, "{publish:?}");
                assert!(batch.is_none(), "one batch, sent once");
                mqtt.send(&puback(publish.id.unwrap(), 0)).await;
                batch = Some(TelemetryBatch::decode(&publish.payload).unwrap());
            }
            Some(Packet::Publish(publish))
                if publish.topic == Channel::Heartbeat.topic(&gateway()) =>
            {
                heartbeat = true;
            }
            other => panic!("expected the heartbeat and the telemetry batch, got {other:?}"),
        }
    }
    let batch = batch.unwrap();
    assert_eq!((batch.seq.get(), batch.clock_synced), (1, false));
    let sent: Vec<Reading> = batch
        .series
        .iter()
        .flat_map(|series| {
            series.samples.iter().map(|sample| Reading {
                device: series.device.clone(),
                point: series.point.clone(),
                sample: sample.clone(),
                clock_synced: batch.clock_synced,
            })
        })
        .collect();
    assert_eq!(sent, readings, "the readings as they were buffered");

    let ack = TelemetryAck {
        v: V1,
        stream: batch.stream.clone(),
        seq: batch.seq,
        outcome: AckOutcome::Committed { records: 3 },
        server_time: UnixMillis::from_system_time(std::time::SystemTime::now()).unwrap(),
    };
    mqtt.send(&publish(
        &Channel::TelemetryAck.topic(&gateway()),
        &protocol::encode(&ack).unwrap(),
        1,
        false,
        Some(1),
    ))
    .await;
    log.wait_for(&["the cloud has the batch", "seq=1", "records=3"])
        .await;

    terminate(&child);
    // The PUBACK of the acknowledgement may still come before `offline`.
    let offline = loop {
        match bounded("the offline status", mqtt.read_answering_pings()).await {
            Some(Packet::PubAck { id: 1, reason: 0 }) => {}
            Some(Packet::Publish(publish))
                if publish.topic == Channel::Status.topic(&gateway()) =>
            {
                break publish;
            }
            other => panic!("expected offline, got {other:?}"),
        }
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
    let (success, rest) = log.exit(child).await;
    assert!(success, "{rest}");
    // Released: the uploader deleted the segment once its only batch was acknowledged, and the
    // stop had nothing in memory to write.
    assert_eq!(segments(&state), Vec::<u64>::new(), "{rest}");
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

/// A gateway without a finished factory registration or without a valid identity, each with
/// what makes it so: the agent must refuse to start, and the log must say what failed.
type Breakage = (&'static str, Box<dyn Fn(&Files)>, &'static str);

fn broken_identities() -> Vec<Breakage> {
    let remove = |name: &'static str| {
        Box::new(move |files: &Files| std::fs::remove_file(files.path(name)).unwrap())
            as Box<dyn Fn(&Files)>
    };
    vec![
        (
            "no configuration file",
            remove("agent.toml"),
            "agent.toml: No such file",
        ),
        (
            "no certificate",
            remove("gateway.pem"),
            "reading the certificate file",
        ),
        (
            "no key",
            remove("gateway.key"),
            "reading the private key file",
        ),
        (
            "a certificate the broker CA did not issue",
            Box::new(|files: &Files| {
                let other = Authority::new("another CA").client(GATEWAY);
                files.write("gateway.pem", &other.cert_pem);
                files.write("gateway.key", &other.key_pem);
            }),
            "broker.ca: the gateway certificate does not pass as a client certificate",
        ),
        (
            "a key of another certificate",
            Box::new(|files: &Files| {
                files.write(
                    "gateway.key",
                    &Authority::new("unrelated").client(GATEWAY).key_pem,
                );
            }),
            "the private key does not belong to the certificate",
        ),
    ]
}

/// The fail-closed start (Laysi/trustiot-v2#812, rebuild checklist D5): a gateway without its
/// configuration, certificate or key, or whose certificate and key the broker would refuse,
/// does not run. The state directory holds a configuration applied before, as on a gateway
/// whose identity was removed after it ran, and a port that accepts connections stands where
/// the broker's front is. In every case the agent exits with 1 before `READY=1` (nothing at
/// all reaches the notification socket), connects nowhere, does not poll the saved
/// configuration and leaves the state directory as it was. The same files with a valid
/// identity do all of that, so the absence is not an artefact of the test.
#[tokio::test]
async fn without_a_valid_identity_the_agent_never_gets_ready_connects_or_polls_a_saved_configuration()
 {
    let pki = TestPki::new();
    let front = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    front.set_nonblocking(true).unwrap();
    let port = front.local_addr().unwrap().port();
    let saved = desired_config();
    let gateway_with_saved_configuration = || {
        let files = Files::new(&pki, port);
        std::fs::write(files.path("state/desired-config.cbor"), &saved).unwrap();
        files
    };

    for (case, break_it, reason) in broken_identities() {
        let files = gateway_with_saved_configuration();
        break_it(&files);
        let mut notify = NotifySocket::new();
        let child = agent(&files, &notify);
        let output = bounded(
            format!("the agent to exit ({case})"),
            child.wait_with_output(),
        )
        .await
        .unwrap();
        let log = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{case}: {log}");
        assert!(
            log.contains("refusing to start") && log.contains(reason),
            "{case}: the log should say {reason:?}: {log}"
        );
        for started in [
            "polling with the saved configuration",
            "applied the configuration",
            "polled",
        ] {
            assert!(
                !log.contains(started),
                "{case}: the log shows {started:?}: {log}"
            );
        }
        assert_eq!(
            notify.drain(),
            Vec::<String>::new(),
            "{case}: nothing may reach systemd's notification socket"
        );
        match front.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("{case}: the agent connected to the broker's port: {other:?}"),
        }
        assert_eq!(
            std::fs::read(files.path("state/desired-config.cbor")).unwrap(),
            saved,
            "{case}"
        );
        assert!(
            !Path::new(&files.path("state/buffer")).exists(),
            "{case}: the agent opened its offline buffer"
        );
    }

    // The control: the same files with the identity intact.
    let files = gateway_with_saved_configuration();
    let mut notify = NotifySocket::new();
    let mut child = agent(&files, &notify);
    let mut log = Log::of(&mut child);
    notify.wait_for("READY=1", |line| line == "READY=1").await;
    log.wait_for(&["polling with the saved configuration", "version=1"])
        .await;
    bounded("a connection to the broker's port", async {
        loop {
            match front.accept() {
                Ok(_) => return,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("accepting the agent's connection: {error}"),
            }
        }
    })
    .await;
    terminate(&child);
    let (success, rest) = log.exit(child).await;
    assert!(success, "{rest}");
}

/// `trustiot-agent provision <args>` (Laysi/trustiot-v2#613), with its log at debug level.
fn provision(args: &[&str]) -> std::process::Output {
    std::process::Command::new(EXECUTABLE)
        .arg("provision")
        .args(args)
        .env("TRUSTIOT_AGENT_LOG", "debug")
        .output()
        .unwrap()
}

/// A resource manager device that cannot exist: the kernel numbers TPMs below 65536. Tests that
/// get as far as opening the device use it, so they never touch a TPM of the machine they run on.
const NO_TPM: &str = "/dev/tpmrm99999";

/// Fails unless `output` ended with `code` and printed nothing on standard output; returns its
/// standard error.
fn refused(output: &std::process::Output, code: i32) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(code), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}

#[test]
fn provision_without_a_tpm_device_exits_3_with_nothing_on_standard_output() {
    for extra in [&[][..], &["--inspect"], &["--handle", "0x81000200"]] {
        let mut args = vec!["--hardware-id", "tg-0001", "--device", NO_TPM];
        args.extend_from_slice(extra);
        let stderr = refused(&provision(&args), 3);
        assert!(
            stderr.contains("cannot open the TPM device") && stderr.contains(NO_TPM),
            "{stderr}"
        );
    }
}

#[test]
fn provision_refuses_invalid_hardware_ids_handles_and_arguments_with_status_2() {
    let long = "a".repeat(65);
    for args in [
        &[][..],
        &["--hardware-id"],
        &["--hardware-id", ""],
        &["--hardware-id", "tg/0001"],
        &["--hardware-id", "tg 0001"],
        &["--hardware-id", "tg-0001;reboot"],
        &["--hardware-id", "$(id)"],
        &["--hardware-id", &long],
        &["--hardware-id", "tg-0001", "--hardware-id", "tg-0002"],
        &["--hardware-id", "tg-0001", "--handle", "0x81000001"],
        &["--hardware-id", "tg-0001", "--handle", "0x81800000"],
        &["--hardware-id", "tg-0001", "--handle", "0x80000001"],
        &["--hardware-id", "tg-0001", "--handle", "0x01c0000a"],
        &["--hardware-id", "tg-0001", "--handle", "81000100"],
        &["--hardware-id", "tg-0001", "--handle", "0x8100010g"],
        &["--hardware-id", "tg-0001", "--handle"],
        &["--hardware-id", "tg-0001", "--verbose"],
        &["--inspect"],
        &["--hardware-id", "tg-0001", "--help"],
        &["--help", "--inspect"],
    ] {
        let mut args = args.to_vec();
        args.extend_from_slice(&["--device", NO_TPM]);
        let stderr = refused(&provision(&args), 2);
        assert!(
            stderr.contains("usage: trustiot-agent provision"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn provision_refuses_devices_other_than_the_resource_manager_with_status_4_unopened() {
    // /dev/null would be opened and answer the first command with nothing (status 1, a
    // malformed response): status 4 shows the device was refused before it was opened.
    let file = std::env::temp_dir().join(format!("tpmrm0-{}", std::process::id()));
    std::fs::write(&file, b"").unwrap();
    for device in [
        "/dev/tpm0",
        "/dev/null",
        "/dev/tpmrm0/../tpm0",
        file.to_str().unwrap(),
    ] {
        for extra in [&[][..], &["--inspect"]] {
            let mut args = vec!["--hardware-id", "tg-0001", "--device", device];
            args.extend_from_slice(extra);
            let stderr = refused(&provision(&args), 4);
            assert!(
                stderr.contains(device) && stderr.contains("resource manager"),
                "{stderr}"
            );
        }
    }
    let written = std::fs::read(&file).unwrap();
    std::fs::remove_file(file).unwrap();
    assert!(written.is_empty(), "the file was written");
}

/// `trustiot-agent provision install <args>` (Laysi/trustiot-v2#617) with empty standard input.
fn install(args: &[&str]) -> std::process::Output {
    std::process::Command::new(EXECUTABLE)
        .args(["provision", "install"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn provision_install_refuses_invalid_arguments_with_status_2_and_has_its_own_help() {
    for args in [
        &[][..],
        &["--hardware-id"],
        &["--hardware-id", "tg 0001"],
        &["--hardware-id", "$(id)"],
        &["--hardware-id", "tg-0001", "--hardware-id", "tg-0001"],
        &["--hardware-id", "tg-0001", "--config"],
        &["--hardware-id", "tg-0001", "--inspect"],
        &["--hardware-id", "tg-0001", "--help"],
    ] {
        let stderr = refused(&install(args), 2);
        assert!(
            stderr.contains("usage: trustiot-agent provision install"),
            "{args:?}: {stderr}"
        );
    }
    let output = install(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("provision install --hardware-id <id>") && help.contains("Exit status"),
        "{help}"
    );
    // The service's own help names the subcommand without --config, which it refuses.
    let top = std::process::Command::new(EXECUTABLE)
        .arg("--help")
        .output()
        .expect("the agent runs");
    assert_eq!(top.status.code(), Some(0));
    let top = String::from_utf8_lossy(&top.stdout);
    let line = top
        .lines()
        .find(|line| line.contains("provision install"))
        .expect("the subcommand is listed");
    assert!(!line.contains("--config"), "{line}");
}

#[test]
fn provision_install_without_an_agent_installation_exits_9_and_writes_nothing() {
    // Without the agent's account (this machine is not a gateway) or without its configuration
    // (always /etc/trustiot/agent.toml), the installation cannot be used: status 9 before
    // standard input is read.
    let stderr = refused(&install(&["--hardware-id", "tg-0001"]), 9);
    assert!(
        stderr.contains("trustiot-agent") || stderr.contains("/etc/trustiot/agent.toml"),
        "{stderr}"
    );
    // Whoever names the configuration decides where root writes: there is no --config.
    let dir = testkit::mqtt::ScratchDir::new("agent-install-binary");
    let config = dir.path().join("agent.toml");
    let stderr = refused(
        &install(&[
            "--hardware-id",
            "tg-0001",
            "--config",
            config.to_str().unwrap(),
        ]),
        2,
    );
    assert!(stderr.contains("--config is not accepted"), "{stderr}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn provision_help_and_the_agents_help_name_the_subcommand() {
    let output = provision(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("--hardware-id <id>") && help.contains("Exit status"),
        "{help}"
    );
    let output = std::process::Command::new(EXECUTABLE)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("trustiot-agent provision"));
}
