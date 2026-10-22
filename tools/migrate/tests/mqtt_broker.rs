//! The chart's MQTT broker configuration (deploy/chart/files/mosquitto/mosquitto.conf) in the
//! pinned Mosquitto image, with the files its init containers and sidecar write
//! (Laysi/trustiot-v2#84): the dynamic-security file from `mqtt_accounts::converge`, the CA and CRL
//! files from `mqtt_trust::install`, and the server certificate mounted the way the kubelet mounts
//! a Secret (symlinks through `..data`). The root filesystem is read-only, as in the chart.
//!
//! - Gateways: a certificate of our CA connects; no certificate, a certificate of another CA, an
//!   expired one, one without clientAuth and a revoked one are refused. The placeholder CA trusts
//!   no gateway. A new CRL and a new server certificate take effect on SIGHUP (what the sidecar
//!   sends), and a connected session is not dropped by the reload.
//! - Services: each logs in on the in-mesh listener with its own password, receives its channels
//!   through a shared subscription, may send only its channels, and only certs may use dynamic
//!   security's control topic. 1 MiB passes, a packet above the limit does not.
//! - A retained message, a session and a queued message survive SIGKILL.
//! - The probe leaves no error or warning in the log.
//!
//! The broker runs in a throwaway container (podman or docker) that publishes no port; the MQTT
//! clients are the image's own `mosquitto_pub`, `mosquitto_sub` and `mosquitto_rr`, run with
//! `exec`. Every certificate and key is made for the test. Without a container runtime the tests
//! skip, unless `CI` is set, where they fail. `cargo test -p migrate --test mqtt_broker --
//! --nocapture` prints the evidence lines the test report quotes.

mod support;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use migrate::Report;
use migrate::mqtt_accounts::{converge, reconcile};
use migrate::mqtt_trust::{TrustSet, install, placeholder, validate};
use protocol::GatewayId;
use protocol::acl::GATEWAY_ROLE;
use protocol::topic::{Channel, MAX_LARGE_BYTES, MIN_MAX_PACKET_BYTES};
use serde_json::{Value, json};
use support::pki::{self, Ca, Kind, Leaf};
use support::secrets::FakeSecrets;
use support::{Container, container_runtime, random_hex, scratch_dir, unavailable, unique_name};

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest: the chart's
/// `mqttBroker.image`, crates/protocol/tests/broker.rs and scripts/lib/stack.ts pin the same one
/// (`bun test deploy/chart` checks that they agree).
const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408";

/// The chart's configuration file; the chart's template carries the same text.
const CONFIG: &str = include_str!("../../../deploy/chart/files/mosquitto/mosquitto.conf");

/// The container's memory limit, and the line the chart appends to [`CONFIG`]: three quarters of
/// the container's limit.
const CONTAINER_MEMORY: &str = "256m";
const MEMORY_LIMIT: &str = "memory_limit 201326592";

/// The chart's readiness and liveness probe (`bun test deploy/chart` checks that the chart runs
/// exactly this command).
const PROBE: &str =
    "mosquitto_pub --unix /run/mosquitto/probe.sock -V 5 -I probe: -t trustiot/probe -n -q 0";

/// The container waits for the test to write the dynamic-security file before Mosquitto starts,
/// as the init container does in the pod; after a restart the file is there already.
const ENTRYPOINT: &str = "until [ -e /mosquitto/data/.start ]; do sleep 0.2; done; \
                          exec /usr/sbin/mosquitto -c /mosquitto/config/mosquitto.conf";

/// The probe socket's connection limit, `max_connections` of its listener in the configuration.
const PROBE_CONNECTIONS: usize = 8;

/// The Secret that holds the services' passwords.
const SECRET: &str = "trustiot-mqtt-broker-accounts";

/// Generous bounds for a loaded machine; each names what it waits for when it fails.
const START_WAIT: Duration = Duration::from_secs(120);
const CHANGE_WAIT: Duration = Duration::from_secs(60);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// PUBACK and SUBACK reason codes.
const SUCCESS: u8 = 0x00;
const NO_MATCHING_SUBSCRIBERS: u8 = 0x10;
const NOT_AUTHORIZED: u8 = 0x87;

#[allow(
    clippy::print_stderr,
    reason = "the test report quotes these lines (cargo test -- --nocapture)"
)]
fn evidence(line: impl Display) {
    eprintln!("evidence: {line}");
}

fn describe(output: &Output) -> String {
    format!(
        "status {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn topic(channel: Channel, gateway: &str) -> String {
    channel.topic(&GatewayId::new(gateway).expect("a gateway id"))
}

/// Our gateway CA as the certs service runs it, a CA we do not run, and the CA of the broker's
/// server certificate (Laysi/trustiot-v2#535 decides which one that is).
struct Pki {
    root: Ca,
    issuing: Ca,
    server_ca: Ca,
}

/// The gateway certificates the tests use: name (the certificate's CN unless noted), serial,
/// kind, and whether our issuing CA signed it.
const GATEWAYS: [(&str, u64, Kind); 6] = [
    ("gw-a", 101, Kind::Valid),
    ("gw-b", 102, Kind::Valid),
    ("gw-c", 103, Kind::Valid),
    ("gw-expired", 104, Kind::Expired),
    ("gw-server-only", 105, Kind::ServerAuthOnly),
    ("gw-revoked", 106, Kind::Valid),
];

impl Pki {
    fn new() -> Self {
        let root = pki::root("Test gateway root CA");
        let issuing = pki::intermediate("Test gateway issuing CA", &root);
        Self {
            root,
            issuing,
            server_ca: pki::root("Test broker server CA"),
        }
    }

    /// The broker's trust files with a CRL that revokes `serials`.
    fn trust(&self, crl_number: u64, serials: &[u64]) -> TrustSet {
        validate(
            &format!("{}{}", self.root.pem, self.issuing.pem),
            &pki::crl(&self.issuing, crl_number, serials),
        )
        .expect("valid trust files")
    }

    /// Writes every gateway's certificate and key, a gateway certificate of another CA with the
    /// CN `gw-a` (`foreign`), and the server CA (`server-ca.pem`) into `dir`.
    fn write(&self, dir: &Path) {
        for (name, serial, kind) in GATEWAYS {
            pki::write(dir, name, &pki::gateway(name, serial, kind, &self.issuing));
        }
        let foreign = pki::root("Test gateway root CA");
        pki::write(
            dir,
            "foreign",
            &pki::gateway("gw-a", 101, Kind::Valid, &foreign),
        );
        std::fs::write(dir.join("server-ca.pem"), &self.server_ca.pem).expect("server-ca.pem");
    }
}

/// How a client logs in.
enum Login<'a> {
    /// On the gateway listener with `<name>.pem` and `<name>.key`, trusting `server_ca`.
    Gateway { name: &'a str, server_ca: &'a str },
    /// On the gateway listener without a client certificate.
    NoCertificate,
    /// On the in-mesh listener as `service:<name>`, with its password.
    Service(&'a str),
    /// On the in-mesh listener with this username and password (`None`: without one).
    Password(&'a str, Option<&'a str>),
    /// On the in-mesh listener without a username.
    Anonymous,
}

/// What a `mosquitto_pub` run saw.
struct Attempt {
    connected: bool,
    puback: Option<u8>,
    output: Output,
}

impl Attempt {
    /// The client's last error line, for the evidence.
    fn error_line(&self) -> String {
        let stderr = String::from_utf8_lossy(&self.output.stderr);
        let stdout = String::from_utf8_lossy(&self.output.stdout);
        stderr
            .lines()
            .chain(stdout.lines())
            .rfind(|line| line.contains("rror") || line.contains("CONNACK"))
            .unwrap_or("(no error line)")
            .to_owned()
    }
}

/// One delivered message: topic, retain flag, QoS and payload length.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Delivered {
    topic: String,
    retain: bool,
    qos: u8,
    len: usize,
}

/// A background `mosquitto_sub`; killed when dropped.
struct Subscription {
    child: Child,
    lines: Receiver<String>,
    granted: Vec<u8>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Subscription {
    fn next_line(&self, wanted: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + RECEIVE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if wanted(&line) => return Some(line),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn next_message(&self) -> Option<Delivered> {
        let line = self.next_line(|line| line.starts_with("MSG "))?;
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields.len(), 5, "{line}");
        Some(Delivered {
            topic: fields[1].to_owned(),
            retain: fields[2] == "1",
            qos: fields[3].parse().expect("a QoS"),
            len: fields[4].parse().expect("a length"),
        })
    }

    /// Every message received before one on `marker` arrives.
    fn until(&self, marker: &str) -> Vec<Delivered> {
        let mut received = Vec::new();
        loop {
            let message = self
                .next_message()
                .unwrap_or_else(|| panic!("marker {marker} not received; got {received:?}"));
            if message.topic == marker {
                return received;
            }
            received.push(message);
        }
    }
}

/// A running broker and the host directory its files come from.
struct Broker {
    container: Container,
    dir: PathBuf,
    passwords: BTreeMap<String, String>,
}

impl Broker {
    /// Starts the broker with `trust` and the server certificate `server`; the services ingest,
    /// gateway-ops and certs have accounts, and every gateway of [`GATEWAYS`] is registered with
    /// the gateway role, as Laysi/trustiot-v2#82 will register them. `None` when no container
    /// runtime is available outside CI.
    async fn start(pki: &Pki, trust: &TrustSet, server: &Leaf) -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let dir = scratch_dir("mqtt-broker");
        for sub in ["pki", "trust", "tls"] {
            std::fs::create_dir(dir.join(sub)).expect("a directory");
        }
        pki.write(&dir.join("pki"));
        write_payload(&dir.join("pki/large.bin"), MAX_LARGE_BYTES);
        write_payload(&dir.join("pki/oversize.bin"), MIN_MAX_PACKET_BYTES);
        std::fs::write(
            dir.join("mosquitto.conf"),
            format!("{CONFIG}{MEMORY_LIMIT}\n"),
        )
        .expect("the configuration");
        install(&dir.join("trust"), trust).expect("the trust files");
        pki::mount_secret(&dir.join("tls"), 1, server);

        // The accounts init container's output, plus the gateways' clients.
        let secrets = FakeSecrets::start().await;
        secrets.declare(SECRET);
        let dynsec_file = dir.join("dynamic-security.json");
        converge(
            &secrets.mqtt_accounts_settings(SECRET, "ingest,gateway-ops,certs", "1", &dynsec_file),
            &mut Report::default(),
        )
        .await
        .expect("the accounts");
        let mut dynsec: Value =
            serde_json::from_slice(&std::fs::read(&dynsec_file).expect("the file")).expect("JSON");
        let clients = dynsec["clients"].as_array_mut().expect("clients");
        for (name, _, _) in GATEWAYS {
            clients.push(json!({ "username": name, "roles": [{ "rolename": GATEWAY_ROLE }] }));
        }
        let passwords = ["ingest", "gateway-ops", "certs"]
            .into_iter()
            .map(|service| {
                let password = secrets.value(SECRET, service).expect("a password");
                (service.to_owned(), password)
            })
            .collect();

        let container = Container {
            runtime,
            name: unique_name("mosquitto"),
        };
        let mount = |host: &str, path: &str| format!("{}:{path}:ro,z", dir.join(host).display());
        let config = mount("mosquitto.conf", "/mosquitto/config/mosquitto.conf");
        let trust_dir = mount("trust", "/mosquitto/data/trust");
        let tls = mount("tls", "/etc/trustiot/mqtt-broker-tls");
        let pki_dir = mount("pki", "/pki");
        let output = Command::new(runtime)
            .args([
                "run",
                "--detach",
                "--name",
                &container.name,
                "--user",
                "1883:1883",
                "--memory",
                CONTAINER_MEMORY,
                "--read-only",
                "--tmpfs",
                "/tmp",
                "--tmpfs",
                "/run/mosquitto:rw,mode=1777",
                "--volume",
                &config,
                "--volume",
                &trust_dir,
                "--volume",
                &tls,
                "--volume",
                &pki_dir,
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                ENTRYPOINT,
            ])
            .output()
            .expect("the container runtime runs");
        assert!(
            output.status.success(),
            "start Mosquitto: {}",
            describe(&output)
        );
        let broker = Self {
            container,
            dir,
            passwords,
        };
        broker.write_file(
            "/mosquitto/data/dynamic-security.json",
            dynsec.to_string().as_bytes(),
        );
        broker.exec_ok(&["touch", "/mosquitto/data/.start"]);
        broker.wait_ready();
        Some(broker)
    }

    fn exec(&self, args: &[&str]) -> Output {
        Command::new(self.container.runtime)
            .args(["exec", &self.container.name])
            .args(args)
            .output()
            .expect("the container runtime runs")
    }

    fn exec_ok(&self, args: &[&str]) -> Output {
        let output = self.exec(args);
        assert!(output.status.success(), "{args:?}: {}", describe(&output));
        output
    }

    /// Writes `bytes` to `path` in the container as the broker's user, readable by it only.
    fn write_file(&self, path: &str, bytes: &[u8]) {
        let mut child = Command::new(self.container.runtime)
            .args([
                "exec",
                "--interactive",
                &self.container.name,
                "/bin/sh",
                "-c",
            ])
            .arg(r#"umask 077 && cat > "$0""#)
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the container runtime runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write to the container");
        let output = child.wait_with_output().expect("exec ends");
        assert!(
            output.status.success(),
            "write {path}: {}",
            describe(&output)
        );
    }

    fn probe(&self) -> Output {
        let args: Vec<&str> = PROBE.split(' ').collect();
        self.exec(&args)
    }

    fn wait_ready(&self) {
        let started = Instant::now();
        loop {
            let output = self.probe();
            if output.status.success() {
                return;
            }
            assert!(
                started.elapsed() < START_WAIT,
                "the broker did not answer its probe in time: {}\nlog:\n{}",
                describe(&output),
                self.log()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn log(&self) -> String {
        let output = Command::new(self.container.runtime)
            .args(["logs", &self.container.name])
            .output()
            .expect("the container runtime runs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn reloads(&self) -> usize {
        self.log()
            .lines()
            .filter(|line| line.contains("Reloading config."))
            .count()
    }

    /// What the trust sidecar does after it installed new files or noticed a new server
    /// certificate. Returns once Mosquitto logged the reload: it reloads in that same turn of its
    /// event loop, so every connection after this sees the new files.
    fn reload(&self) {
        let before = self.reloads();
        let output = Command::new(self.container.runtime)
            .args(["kill", "--signal", "HUP", &self.container.name])
            .output()
            .expect("the container runtime runs");
        assert!(output.status.success(), "SIGHUP: {}", describe(&output));
        let started = Instant::now();
        while self.reloads() == before {
            assert!(
                started.elapsed() < CHANGE_WAIT,
                "timed out waiting for the broker to log the reload:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn login_args(&self, login: &Login<'_>) -> Vec<String> {
        let tls = |server_ca: &str| {
            vec![
                "-h".to_owned(),
                "localhost".to_owned(),
                "-p".to_owned(),
                "8883".to_owned(),
                "--cafile".to_owned(),
                format!("/pki/{server_ca}"),
            ]
        };
        let internal = |username: Option<&str>, password: Option<&str>| {
            let mut args = vec![
                "-h".to_owned(),
                "127.0.0.1".to_owned(),
                "-p".to_owned(),
                "1883".to_owned(),
                "-i".to_owned(),
                format!("{}:test-{}", username.unwrap_or("anonymous"), random_hex()),
            ];
            if let Some(username) = username {
                args.extend(["-u".to_owned(), username.to_owned()]);
            }
            if let Some(password) = password {
                args.extend(["-P".to_owned(), password.to_owned()]);
            }
            args
        };
        match login {
            Login::Gateway { name, server_ca } => {
                let mut args = tls(server_ca);
                args.extend([
                    "--cert".to_owned(),
                    format!("/pki/{name}.pem"),
                    "--key".to_owned(),
                    format!("/pki/{name}.key"),
                    // The broker uses the certificate's CN as the client id anyway; a persistent
                    // session needs one on the client's side too.
                    "-i".to_owned(),
                    (*name).to_owned(),
                ]);
                args
            }
            Login::NoCertificate => tls("server-ca.pem"),
            Login::Service(service) => internal(
                Some(&format!("service:{service}")),
                Some(
                    self.passwords
                        .get(*service)
                        .expect("a service with an account"),
                ),
            ),
            Login::Password(username, password) => internal(Some(username), *password),
            Login::Anonymous => internal(None, None),
        }
    }

    /// Publishes one message with QoS 1; `payload` is `-m <text>`, `-f <file>` or `-n`.
    fn publish(&self, login: &Login<'_>, topic: &str, payload: &[&str], retain: bool) -> Attempt {
        let mut args: Vec<String> = ["mosquitto_pub", "-V", "5", "-d", "-q", "1", "-t", topic]
            .map(str::to_owned)
            .to_vec();
        args.extend(self.login_args(login));
        args.extend(payload.iter().map(|arg| (*arg).to_owned()));
        if retain {
            args.push("-r".to_owned());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.exec(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let connected = stdout.contains("received CONNACK (0)");
        let puback = stdout
            .lines()
            .find_map(|line| line.split_once("received PUBACK ")?.1.split_once("RC:"))
            .map(|(_, rest)| {
                rest.trim_end_matches(')')
                    .parse::<u8>()
                    .expect("a reason code")
            });
        Attempt {
            connected,
            puback,
            output,
        }
    }

    /// Publishes and requires the broker to accept the message (reason code 0, or 0x10 when
    /// nobody subscribed).
    fn publish_ok(&self, login: &Login<'_>, topic: &str, payload: &[&str], retain: bool) {
        let attempt = self.publish(login, topic, payload, retain);
        assert!(
            attempt.connected && matches!(attempt.puback, Some(SUCCESS | NO_MATCHING_SUBSCRIBERS)),
            "publish to {topic}: {}",
            describe(&attempt.output)
        );
    }

    /// Requires the broker to refuse the publish with 0x87, after a successful login.
    fn publish_refused(&self, login: &Login<'_>, topic: &str) {
        let attempt = self.publish(login, topic, &["-m", "x"], false);
        assert!(
            attempt.connected && attempt.puback == Some(NOT_AUTHORIZED),
            "publish to {topic} must be refused: {}",
            describe(&attempt.output)
        );
    }

    /// Requires the login to fail; returns the client's error line.
    fn refused(&self, what: &str, login: &Login<'_>) -> String {
        let attempt = self.publish(login, "trustiot/v1/gw/gw-a/up/status", &["-m", "x"], false);
        assert!(
            !attempt.connected && !attempt.output.status.success(),
            "{what} must not connect: {}",
            describe(&attempt.output)
        );
        let line = attempt.error_line();
        evidence(format!("refused, {what}: {line}"));
        line
    }

    /// Requires the login to succeed.
    fn accepted(&self, what: &str, login: &Login<'_>) {
        let attempt = self.publish(login, "trustiot/v1/gw/gw-a/up/status", &["-m", "x"], false);
        assert!(
            attempt.connected,
            "{what} must connect: {}\nlog:\n{}",
            describe(&attempt.output),
            self.log()
        );
        evidence(format!("accepted, {what}: {}", attempt.error_line()));
    }

    /// Subscribes to `filters` with QoS 1 in the background and waits for the SUBACK.
    fn subscribe(&self, login: &Login<'_>, filters: &[&str]) -> Subscription {
        let mut args: Vec<String> = [
            "exec",
            // A terminal makes mosquitto_sub flush every line; through a pipe it buffers them.
            "--tty",
            &self.container.name,
            "mosquitto_sub",
            "-V",
            "5",
            "-d",
            "-q",
            "1",
            "-F",
            "MSG %t %r %q %l",
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(self.login_args(login));
        for filter in filters {
            args.extend(["-t".to_owned(), (*filter).to_owned()]);
        }
        let mut child = Command::new(self.container.runtime)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the container runtime runs");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line.trim_end_matches('\r').to_owned()).is_err() {
                    break;
                }
            }
        });
        let mut subscription = Subscription {
            child,
            lines,
            granted: Vec::new(),
        };
        // `mosquitto_sub -d` prints `Subscribed (mid: 1): 1, 135` with one reason code per filter.
        let line = subscription
            .next_line(|line| line.starts_with("Subscribed (mid:"))
            .unwrap_or_else(|| panic!("no SUBACK for {filters:?}\nlog:\n{}", self.log()));
        subscription.granted = line
            .split_once("):")
            .expect("reason codes")
            .1
            .split(',')
            .map(|code| code.trim().parse::<u8>().expect("a reason code"))
            .collect();
        subscription
    }

    /// Runs `mosquitto_sub` until it got `count` messages or `RECEIVE_TIMEOUT` passed; returns
    /// one line per message: topic, retain flag, QoS and payload.
    fn receive(
        &self,
        login: &Login<'_>,
        filter: &str,
        count: usize,
        extra: &[&str],
    ) -> Vec<String> {
        let count = count.to_string();
        let timeout = RECEIVE_TIMEOUT.as_secs().to_string();
        let mut args: Vec<String> = [
            "mosquitto_sub",
            "-V",
            "5",
            "-q",
            "1",
            "-t",
            filter,
            "-C",
            &count,
            "-W",
            &timeout,
            "-F",
            "%t %r %q %p",
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(self.login_args(login));
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.exec(&args);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty() && *line != "Timed out")
            .map(str::to_owned)
            .collect()
    }
}

fn write_payload(path: &Path, len: usize) {
    std::fs::write(path, vec![b'x'; len]).expect("a payload file");
}

fn gateway<'a>(name: &'a str) -> Login<'a> {
    Login::Gateway {
        name,
        server_ca: "server-ca.pem",
    }
}

/// Lines that report a problem: Mosquitto prefixes them with `Error:` or `Warning:`, and OpenSSL
/// failures name OpenSSL.
fn problem_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("warning") || lower.contains("openssl")
        })
        .collect()
}

/// The configuration's packet limit is the protocol's (crates/protocol, `MIN_MAX_PACKET_BYTES`),
/// and the memory limit is left to the chart, which derives it from the container's limit.
#[test]
fn the_configuration_follows_the_protocol_limits() {
    let packet_limit = format!("max_packet_size {MIN_MAX_PACKET_BYTES}");
    assert_eq!(
        CONFIG
            .lines()
            .filter(|line| line.starts_with("max_packet_size"))
            .collect::<Vec<_>>(),
        [packet_limit.as_str()]
    );
    assert!(!CONFIG.lines().any(|line| line.starts_with("memory_limit")));
    assert!(
        !CONFIG
            .lines()
            .any(|line| line.starts_with("message_size_limit"))
    );
}

#[tokio::test]
async fn gateways_need_a_current_certificate_of_our_ca() {
    let pki = Pki::new();
    let placeholder = placeholder().expect("the placeholder");
    let Some(broker) = Broker::start(&pki, &placeholder, &pki::server(&pki.server_ca)).await else {
        return;
    };

    // The probe: a few more runs, and the log holds their connections and no problem at all.
    for _ in 0..3 {
        let output = broker.probe();
        assert!(output.status.success(), "probe: {}", describe(&output));
    }
    let log = broker.log();
    assert_eq!(problem_lines(&log), Vec::<&str>::new(), "log:\n{log}");
    let probes: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("probe.sock") && line.contains("as probe:"))
        .collect();
    assert!(probes.len() >= 4, "the probes connected: {log}");
    evidence(format!(
        "probe: {} runs, no error, warning or OpenSSL line; e.g. {}",
        probes.len(),
        probes[0]
    ));

    // The placeholder trusts no gateway; the certs service's files take effect on SIGHUP.
    broker.refused("a valid certificate, placeholder CA", &gateway("gw-a"));
    let first = pki.trust(1, &[106]);
    install(&broker.dir.join("trust"), &first).expect("the trust files");
    broker.reload();
    broker.accepted("gw-a after the reload", &gateway("gw-a"));
    broker.publish_ok(
        &gateway("gw-a"),
        &topic(Channel::Status, "gw-a"),
        &["-m", "x"],
        false,
    );
    // The certificate's CN is the identity: gw-b cannot publish as gw-a.
    broker.publish_refused(&gateway("gw-b"), &topic(Channel::Status, "gw-a"));

    // Acceptance criterion 1.
    broker.refused("no client certificate", &Login::NoCertificate);
    broker.refused("a certificate of another CA, CN gw-a", &gateway("foreign"));
    broker.refused("an expired certificate", &gateway("gw-expired"));
    broker.refused(
        "a certificate without clientAuth",
        &gateway("gw-server-only"),
    );
    broker.refused("a revoked certificate", &gateway("gw-revoked"));

    // A new CRL takes effect on SIGHUP. The reload drops no connection: a gateway revoked while
    // connected stays connected (Laysi/trustiot-v2#82 has to disconnect it).
    let connected = broker.subscribe(&gateway("gw-c"), &[&topic(Channel::Config, "gw-c")]);
    assert_eq!(connected.granted, [1]);
    install(&broker.dir.join("trust"), &pki.trust(2, &[103, 106])).expect("the trust files");
    broker.reload();
    broker.refused("gw-c after its revocation", &gateway("gw-c"));
    broker.publish_ok(
        &Login::Service("gateway-ops"),
        &topic(Channel::Config, "gw-c"),
        &["-m", "after the reload"],
        false,
    );
    let delivered = connected
        .next_message()
        .expect("the open connection still receives");
    assert_eq!(delivered.topic, topic(Channel::Config, "gw-c"));
    evidence("gw-c's connection from before its revocation still receives after the reload");
    broker.accepted("gw-a after the new CRL", &gateway("gw-a"));

    // A new server certificate in the Secret takes effect on SIGHUP.
    let next_server_ca = pki::root("Test broker server CA 2");
    std::fs::write(broker.dir.join("pki/server-ca-2.pem"), &next_server_ca.pem)
        .expect("server-ca-2.pem");
    pki::mount_secret(&broker.dir.join("tls"), 2, &pki::server(&next_server_ca));
    broker.reload();
    let rotated = Login::Gateway {
        name: "gw-a",
        server_ca: "server-ca-2.pem",
    };
    broker.accepted("a client that trusts the new server CA", &rotated);
    broker.refused(
        "a client that trusts only the old server CA",
        &gateway("gw-a"),
    );

    assert_eq!(
        broker.reloads(),
        3,
        "one reload per SIGHUP:\n{}",
        broker.log()
    );
    let log = broker.log();
    let still_running = Command::new(broker.container.runtime)
        .args([
            "inspect",
            "--format",
            "{{.State.Running}}",
            &broker.container.name,
        ])
        .output()
        .expect("the container runtime runs");
    assert_eq!(
        String::from_utf8_lossy(&still_running.stdout).trim(),
        "true",
        "{log}"
    );
}

#[tokio::test]
async fn services_log_in_with_their_accounts_and_reach_only_their_channels() {
    let pki = Pki::new();
    let Some(broker) = Broker::start(&pki, &pki.trust(1, &[]), &pki::server(&pki.server_ca)).await
    else {
        return;
    };

    // Logins on the in-mesh listener.
    let ingest_password = broker.passwords["ingest"].clone();
    broker.refused(
        "service:ingest with another password",
        &Login::Password("service:ingest", Some(&random_hex())),
    );
    broker.refused(
        "service:ingest with gateway-ops' password",
        &Login::Password("service:ingest", Some(&broker.passwords["gateway-ops"])),
    );
    broker.refused("no username", &Login::Anonymous);
    broker.refused(
        "a gateway's name without a certificate",
        &Login::Password("gw-a", None),
    );
    broker.refused(
        "a gateway's name with a password",
        &Login::Password("gw-a", Some(&ingest_password)),
    );

    // ingest receives telemetry through its shared subscription; it may not subscribe to what
    // is not its channel.
    let shared = format!("$share/ingest/{}", Channel::Telemetry.filter());
    let ingest = broker.subscribe(&Login::Service("ingest"), &[&shared, "#"]);
    assert_eq!(ingest.granted, [1, NOT_AUTHORIZED], "telemetry yes, # no");
    broker.publish_ok(
        &gateway("gw-a"),
        &topic(Channel::Telemetry, "gw-a"),
        &["-m", "reading"],
        false,
    );
    let delivered = ingest.next_message().expect("telemetry reaches ingest");
    assert_eq!(delivered.topic, topic(Channel::Telemetry, "gw-a"));
    evidence(format!("shared subscription delivered {delivered:?}"));

    // The probe socket's connection limit is its own: more service connections than that are
    // open at once on the in-mesh listener, and the probe still connects.
    let limit = format!("max_connections {PROBE_CONNECTIONS}");
    assert_eq!(CONFIG.matches(&limit).count(), 1, "{limit} once");
    assert!(
        CONFIG.find(&limit) > CONFIG.find("listener 0 /run/mosquitto/probe.sock"),
        "{limit} belongs to the probe socket"
    );
    let many: Vec<Subscription> = (0..PROBE_CONNECTIONS + 2)
        .map(|_| broker.subscribe(&Login::Service("ingest"), &[&shared]))
        .collect();
    assert!(
        many.iter().all(|subscription| subscription.granted == [1]),
        "every connection subscribed"
    );
    let probe = broker.probe();
    assert!(probe.status.success(), "{}", describe(&probe));
    evidence(format!(
        "{} service connections open at once on 1883 (the probe socket allows {PROBE_CONNECTIONS}), \
         and the probe connects",
        many.len() + 1
    ));
    drop(many);

    // Each service sends only its own channels.
    broker.publish_ok(
        &Login::Service("ingest"),
        &topic(Channel::TelemetryAck, "gw-a"),
        &["-m", "ack"],
        false,
    );
    broker.publish_refused(&Login::Service("ingest"), &topic(Channel::Config, "gw-a"));
    broker.publish_refused(
        &Login::Service("ingest"),
        &topic(Channel::Telemetry, "gw-a"),
    );
    broker.publish_refused(
        &Login::Service("gateway-ops"),
        &topic(Channel::TelemetryAck, "gw-a"),
    );
    broker.publish_refused(&Login::Service("certs"), &topic(Channel::Config, "gw-a"));

    // Only certs administers dynamic security.
    let as_certs = |commands: Value| -> Value {
        let request = json!({ "commands": commands }).to_string();
        let mut args: Vec<String> = [
            "mosquitto_rr",
            "-V",
            "5",
            "-t",
            "$CONTROL/dynamic-security/v1",
            "-e",
            "$CONTROL/dynamic-security/v1/response",
            "-W",
            "10",
            "-m",
            &request,
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(broker.login_args(&Login::Service("certs")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = broker.exec_ok(&args);
        serde_json::from_slice(&output.stdout).expect("a JSON response")
    };
    let response = as_certs(json!([{ "command": "listClients" }]));
    let clients = &response["responses"][0]["data"]["clients"];
    assert!(
        clients
            .as_array()
            .is_some_and(|clients| clients.contains(&json!("gw-a"))
                && clients.contains(&json!("service:ingest"))),
        "{response}"
    );
    evidence("certs listed the broker's clients through dynamic security");
    for service in ["ingest", "gateway-ops"] {
        broker.publish_refused(&Login::Service(service), "$CONTROL/dynamic-security/v1");
    }

    // The file the broker saves after a change passes the accounts init container's checks, so
    // the next start keeps every entry the broker wrote.
    let response = as_certs(json!([
        {
            "command": "createRole",
            "rolename": "operator-notes",
            "textname": "Notes",
            "acls": [
                { "acltype": "subscribePattern", "topic": "notes/#", "priority": 1, "allow": true },
                { "acltype": "publishClientReceive", "topic": "notes/#", "allow": false },
            ],
        },
        {
            "command": "createClient",
            "username": "gw-new",
            "clientid": "gw-new",
            "textname": "registered later",
            "roles": [{ "rolename": GATEWAY_ROLE, "priority": 2 }],
        },
        { "command": "createGroup", "groupname": "fleet", "roles": [{ "rolename": "operator-notes" }] },
        { "command": "addGroupClient", "groupname": "fleet", "username": "gw-new", "priority": 1 },
        { "command": "disableClient", "username": "gw-new" },
    ]));
    let responses = response["responses"].as_array().expect("responses");
    assert!(
        responses.len() == 5 && responses.iter().all(|reply| reply.get("error").is_none()),
        "{response}"
    );
    let saved = broker.exec_ok(&["cat", "/mosquitto/data/dynamic-security.json"]);
    let saved = String::from_utf8(saved.stdout).expect("UTF-8");
    let reconciled: Value = serde_json::from_str(
        &reconcile(Some(&saved), &[]).expect("the file the broker saved is accepted"),
    )
    .expect("JSON");
    let kept = reconciled["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .find(|client| client["username"] == "gw-new")
        .expect("the new client is kept");
    assert_eq!(kept["disabled"], true, "{kept}");
    assert_eq!(kept["clientid"], "gw-new", "{kept}");
    assert!(
        reconciled["groups"].to_string().contains("fleet")
            && reconciled["roles"].to_string().contains("operator-notes"),
        "{reconciled}"
    );
    evidence("the file the broker saved after certs' changes passes the init container's checks");

    // A 1 MiB message passes; a packet above the broker's limit never arrives.
    let config = topic(Channel::Config, "gw-b");
    let receiver = broker.subscribe(
        &gateway("gw-b"),
        &[&config, &topic(Channel::LogRequest, "gw-b")],
    );
    broker.publish_ok(
        &Login::Service("gateway-ops"),
        &config,
        &["-f", "/pki/large.bin"],
        false,
    );
    let large = receiver.next_message().expect("the 1 MiB message arrives");
    assert_eq!(large.len, MAX_LARGE_BYTES);
    let oversize = broker.publish(
        &Login::Service("gateway-ops"),
        &config,
        &["-f", "/pki/oversize.bin"],
        false,
    );
    assert!(oversize.puback.is_none(), "{}", describe(&oversize.output));
    evidence(format!(
        "a {MIN_MAX_PACKET_BYTES}-byte payload: {}",
        oversize.error_line()
    ));
    let marker = topic(Channel::LogRequest, "gw-b");
    broker.publish_ok(
        &Login::Service("gateway-ops"),
        &marker,
        &["-m", "marker"],
        false,
    );
    assert_eq!(
        receiver.until(&marker),
        Vec::new(),
        "nothing before the marker"
    );
}

#[tokio::test]
async fn retained_messages_and_sessions_survive_sigkill() {
    let pki = Pki::new();
    let Some(broker) = Broker::start(&pki, &pki.trust(1, &[]), &pki::server(&pki.server_ca)).await
    else {
        return;
    };
    let status = topic(Channel::Status, "gw-a");
    let config = topic(Channel::Config, "gw-b");
    broker.publish_ok(&gateway("gw-a"), &status, &["-m", "online"], true);
    // gw-b subscribes with a session that outlives the connection, then goes offline.
    let session = ["-c", "-x", "86400", "-E"];
    let mut args: Vec<String> = ["mosquitto_sub", "-V", "5", "-q", "1", "-t", &config]
        .map(str::to_owned)
        .to_vec();
    args.extend(broker.login_args(&gateway("gw-b")));
    args.extend(session.map(str::to_owned));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    broker.exec_ok(&args);
    let attempt = broker.publish(
        &Login::Service("gateway-ops"),
        &config,
        &["-m", "queued"],
        false,
    );
    assert!(
        attempt.connected && attempt.puback == Some(SUCCESS),
        "queued for gw-b's session: {}",
        describe(&attempt.output)
    );

    // Acceptance criterion 2: SIGKILL right after the acknowledgements.
    let runtime = broker.container.runtime;
    let name = &broker.container.name;
    for command in [
        &["kill", "--signal", "KILL", name][..],
        &["wait", name][..],
        &["start", name][..],
    ] {
        let output = Command::new(runtime)
            .args(command)
            .output()
            .expect("the container runtime runs");
        assert!(
            output.status.success(),
            "{command:?}: {}",
            describe(&output)
        );
    }
    broker.wait_ready();
    let log = broker.log();
    for line in log.lines().filter(|line| line.contains("sqlite: Restored")) {
        evidence(line);
    }

    let retained = broker.receive(
        &Login::Service("gateway-ops"),
        &Channel::Status.filter(),
        1,
        &["--retained-only"],
    );
    assert_eq!(retained, [format!("{status} 1 1 online")]);
    let queued = broker.receive(&gateway("gw-b"), &config, 1, &["-c", "-x", "86400"]);
    assert_eq!(queued, [format!("{config} 0 1 queued")]);
    evidence(format!(
        "after SIGKILL: retained {retained:?}, queued {queued:?}"
    ));
}
