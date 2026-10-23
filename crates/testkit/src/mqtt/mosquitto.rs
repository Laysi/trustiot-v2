//! Mosquitto 2.1 in a throwaway container, set up like the product's broker for gateways: an
//! mTLS listener (TLS 1.3, `require_certificate`, `use_identity_as_username`,
//! `set_tcp_nodelay`), dynamic security with the protocol's gateway role
//! ([`protocol::acl::gateway_role_acls`]), anonymous access off.
//!
//! The listener lists ChaCha20-Poly1305 first in `ciphers_tls1.3`: Mosquitto picks the cipher
//! suite by its own order, not the client's, and without the setting it negotiates AES-256-GCM
//! even with a gateway that offers ChaCha20 first (measured 2026-10-21, Laysi/trustiot-v2#86;
//! the gateway's CPU has no AES instructions, decision 6).
//!
//! The mTLS listener is published on the host's loopback interface; a second, password-only
//! listener stays inside the container for the dynamic-security commands and for a cloud
//! account whose `mosquitto_sub`/`mosquitto_pub` run with `exec`. The certificates come from
//! the test's [`super::TestPki`] through environment variables, never the command line.
//! Without podman or docker the start returns `None` (the test skips), unless `CI` is set.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use protocol::acl::{GATEWAY_ROLE, gateway_role_acls};
use serde_json::{Value as Json, json};

use super::{TestPki, stop_listening};
use crate::container::{Container, container_runtime, unavailable, unique_name};

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest; the same as
/// `crates/protocol/tests/broker.rs` and `scripts/lib/stack.ts`.
pub const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408";

/// The cloud account's user name.
pub const CLOUD_USER: &str = "test-cloud";

/// How long to wait for the broker to answer after a start or restart. Generous: CI runners
/// are often loaded.
const READY_LIMIT: Duration = Duration::from_secs(120);

/// Writes the files from the environment (every start, so a restart finds them), initializes
/// dynamic security once, and runs the broker. Idempotent, because `restart` runs it again.
const START_SCRIPT: &str = r#"set -eu
mkdir -p /run/trustiot
printf '%s\n' "$CA_PEM" > /run/trustiot/ca.pem
printf '%s\n' "$SERVER_CERT_PEM" > /run/trustiot/server.pem
printf '%s\n' "$SERVER_KEY_PEM" > /run/trustiot/server.key
cat > /run/trustiot/mosquitto.conf <<CONF
per_listener_settings false
allow_anonymous false
set_tcp_nodelay true
plugin /usr/lib/mosquitto_dynamic_security.so
plugin_opt_config_file /run/trustiot/dynamic-security.json
log_dest stdout
log_type error
log_type warning
log_type notice
log_type information
connection_messages true
listener 1883 127.0.0.1
listener 8883 0.0.0.0
cafile /run/trustiot/ca.pem
certfile /run/trustiot/server.pem
keyfile /run/trustiot/server.key
require_certificate true
use_identity_as_username true
tls_version tlsv1.3
ciphers_tls1.3 TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256
CONF
if [ ! -f /run/trustiot/dynamic-security.json ]; then
  mosquitto_ctrl dynsec init /run/trustiot/dynamic-security.json admin "$ADMIN_PASSWORD" > /dev/null
fi
chown -R mosquitto:mosquitto /run/trustiot
chmod 0600 /run/trustiot/server.key
exec mosquitto -c /run/trustiot/mosquitto.conf
"#;

/// Runs `mosquitto_sub` with the arguments after the pid file, writing its pid first.
const SUBSCRIBE_SCRIPT: &str = r#"echo $$ > "$0"; exec mosquitto_sub "$@""#;

fn random_hex() -> String {
    use std::hash::{BuildHasher, RandomState};
    format!(
        "{:016x}{:016x}",
        RandomState::new().hash_one(1_u8),
        RandomState::new().hash_one(2_u8)
    )
}

fn describe(output: &Output) -> String {
    format!(
        "status {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A free TCP port on the loopback interface, for publishing the broker on a port that stays
/// the same across restarts. The port is free only when this returns: another process can take
/// it before the container runtime binds it, so [`Mosquitto::start`] retries with a new one.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a free port");
    let port = listener.local_addr().expect("local address").port();
    // Release the port even if a process spawned meanwhile holds a copy of the descriptor.
    stop_listening(&listener);
    port
}

/// How many ports `start` tries when the one it picked was taken in the meantime.
const START_ATTEMPTS: usize = 5;

/// Whether the container runtime failed because the host port was taken (podman's rootlessport
/// or pasta, rootful podman, docker).
fn port_taken(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    stderr.contains("address already in use") || stderr.contains("port is already allocated")
}

/// A running broker; the container is removed when dropped.
#[derive(Debug)]
pub struct Mosquitto {
    container: Container,
    admin_password: String,
    cloud_password: String,
    address: SocketAddr,
}

impl Mosquitto {
    /// Starts the broker with the certificates of `pki`, the gateway role, and the cloud
    /// account. `None` when no container runtime works (outside CI).
    ///
    /// # Panics
    ///
    /// When the container does not start or the broker does not answer in time, and without a
    /// container runtime when `CI` is set.
    #[must_use]
    pub fn start(pki: &TestPki) -> Option<Self> {
        Self::start_with(pki, free_port)
    }

    /// [`Self::start`] on the ports `next_port` gives, trying the next one while the runtime
    /// reports the previous one taken.
    fn start_with(pki: &TestPki, mut next_port: impl FnMut() -> u16) -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let mut attempt = 0;
        let broker = loop {
            attempt += 1;
            let port = next_port();
            let broker = Self {
                container: Container {
                    runtime,
                    name: unique_name("mosquitto"),
                },
                admin_password: random_hex(),
                cloud_password: random_hex(),
                address: SocketAddr::from(([127, 0, 0, 1], port)),
            };
            let output = broker.run(pki);
            if output.status.success() {
                break broker;
            }
            // Dropping `broker` removes the container the runtime may have created.
            assert!(
                port_taken(&output) && attempt < START_ATTEMPTS,
                "start Mosquitto (attempt {attempt}): {}",
                describe(&output)
            );
        };
        broker.wait_until_ready();
        broker.control(&json!([
            { "command": "createRole", "rolename": GATEWAY_ROLE, "acls": gateway_role_acls() },
            {
                "command": "createRole",
                "rolename": CLOUD_USER,
                "acls": [
                    { "acltype": "publishClientSend", "topic": "trustiot/#", "allow": true },
                    { "acltype": "publishClientReceive", "topic": "trustiot/#", "allow": true },
                    { "acltype": "subscribePattern", "topic": "trustiot/#", "allow": true },
                ]
            },
            {
                "command": "createClient",
                "username": CLOUD_USER,
                "password": broker.cloud_password,
                "roles": [{ "rolename": CLOUD_USER }]
            },
        ]));
        Some(broker)
    }

    /// `run` of the container, publishing the mTLS listener on this broker's address.
    fn run(&self, pki: &TestPki) -> Output {
        let runtime = self.container.runtime;
        let publish = format!("127.0.0.1:{}:8883", self.address.port());
        Command::new(runtime)
            .args([
                "run",
                "--detach",
                "--name",
                &self.container.name,
                "--memory",
                "128m",
                "--publish",
                &publish,
                "--env",
                "ADMIN_PASSWORD",
                "--env",
                "CA_PEM",
                "--env",
                "SERVER_CERT_PEM",
                "--env",
                "SERVER_KEY_PEM",
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ])
            .env("ADMIN_PASSWORD", &self.admin_password)
            .env("CA_PEM", &pki.broker_ca.pem)
            .env("SERVER_CERT_PEM", &pki.broker.cert_pem)
            .env("SERVER_KEY_PEM", &pki.broker.key_pem)
            .output()
            .expect("run the container runtime")
    }

    /// The mTLS listener on the host.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    fn exec(&self, args: &[&str]) -> Output {
        Command::new(self.container.runtime)
            .args(["exec", &self.container.name])
            .args(args)
            .output()
            .expect("run the container runtime")
    }

    fn wait_until_ready(&self) {
        let started = Instant::now();
        while self
            .try_control(&json!([{ "command": "getDefaultACLAccess" }]))
            .is_none()
        {
            assert!(
                started.elapsed() < READY_LIMIT,
                "Mosquitto did not answer dynamic-security commands within {READY_LIMIT:?}; logs:\n{}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Sends dynamic-security commands; `None` when the broker did not answer.
    fn try_control(&self, commands: &Json) -> Option<Vec<Json>> {
        let request = json!({ "commands": commands }).to_string();
        let output = self.exec(&[
            "mosquitto_rr",
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-u",
            "admin",
            "-P",
            &self.admin_password,
            "-t",
            "$CONTROL/dynamic-security/v1",
            "-e",
            "$CONTROL/dynamic-security/v1/response",
            "-W",
            "10",
            "-m",
            &request,
        ]);
        if !output.status.success() {
            return None;
        }
        let response: Json = serde_json::from_slice(&output.stdout).ok()?;
        response["responses"].as_array().cloned()
    }

    /// Sends dynamic-security commands and requires every one to succeed.
    ///
    /// # Panics
    ///
    /// When the broker does not answer or refuses a command.
    pub fn control(&self, commands: &Json) -> Vec<Json> {
        let responses = self
            .try_control(commands)
            .unwrap_or_else(|| panic!("no answer to {commands}"));
        for response in &responses {
            assert!(response.get("error").is_none(), "{response}");
        }
        responses
    }

    /// Registers a gateway: a dynamic-security client named after its certificate's CN, bound
    /// to the gateway role, as provisioning does (Laysi/trustiot-v2#82).
    pub fn register_gateway(&self, gateway: &str) {
        self.control(&json!([{
            "command": "createClient",
            "username": gateway,
            "roles": [{ "rolename": GATEWAY_ROLE }]
        }]));
    }

    /// Removes one rule from the gateway role, for example to see a refused publish.
    pub fn remove_gateway_rule(&self, acltype: &str, topic: &str) {
        self.control(&json!([{
            "command": "removeRoleACL",
            "rolename": GATEWAY_ROLE,
            "acltype": acltype,
            "topic": topic
        }]));
    }

    /// Restarts the container (SIGTERM, then a fresh broker on the same port) and waits until
    /// it answers again. Retained messages are lost (no persistence), the roles and clients
    /// stay.
    ///
    /// # Panics
    ///
    /// When the restart fails or the broker does not answer in time.
    pub fn restart(&self) {
        let output = Command::new(self.container.runtime)
            .args(["restart", "--time", "2", &self.container.name])
            .output()
            .expect("run the container runtime");
        assert!(
            output.status.success(),
            "restart Mosquitto: {}",
            describe(&output)
        );
        self.wait_until_ready();
    }

    /// The broker's log so far.
    #[must_use]
    pub fn logs(&self) -> String {
        let output = Command::new(self.container.runtime)
            .args(["logs", &self.container.name])
            .output()
            .expect("run the container runtime");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Waits until a log line contains every one of `parts`; returns the line.
    ///
    /// # Panics
    ///
    /// When no such line appears within `limit`.
    pub fn wait_for_log(&self, parts: &[&str], limit: Duration) -> String {
        let started = Instant::now();
        loop {
            let logs = self.logs();
            if let Some(line) = logs
                .lines()
                .find(|line| parts.iter().all(|part| line.contains(part)))
            {
                return line.to_owned();
            }
            assert!(
                started.elapsed() < limit,
                "no log line with {parts:?} within {limit:?}; logs:\n{logs}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Publishes `payload` as the cloud account; returns the PUBACK reason code (`None` for
    /// QoS 0).
    ///
    /// # Panics
    ///
    /// When the client cannot connect or gets no PUBACK.
    pub fn publish(&self, topic: &str, payload: &[u8], qos: u8, retain: bool) -> Option<u8> {
        let client_id = format!("{CLOUD_USER}-pub-{}", random_hex());
        let qos_text = qos.to_string();
        let mut args = vec![
            "exec",
            "--interactive",
            &self.container.name,
            "mosquitto_pub",
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-d",
            "-u",
            CLOUD_USER,
            "-P",
            &self.cloud_password,
            "-i",
            &client_id,
            "-q",
            &qos_text,
            "-t",
            topic,
            "-s",
        ];
        if retain {
            args.push("-r");
        }
        let mut child = Command::new(self.container.runtime)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the container runtime");
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload).expect("payload to mosquitto_pub");
        }
        let output = child.wait_with_output().expect("mosquitto_pub");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("received CONNACK (0)"),
            "publish to {topic}: {}",
            describe(&output)
        );
        if qos == 0 {
            return None;
        }
        let code = stdout
            .lines()
            .find_map(|line| line.split_once("received PUBACK ")?.1.split_once("RC:"))
            .and_then(|(_, rest)| rest.trim_end_matches(')').parse::<u8>().ok());
        assert!(
            code.is_some(),
            "no PUBACK for {topic}: {}",
            describe(&output)
        );
        code
    }

    /// Subscribes the cloud account to `filter` in the background (QoS 1, retain flags as
    /// published).
    ///
    /// # Panics
    ///
    /// When the subscription is not acknowledged.
    #[must_use]
    pub fn subscribe(&self, filter: &str) -> Subscription {
        let client_id = format!("{CLOUD_USER}-sub-{}", random_hex());
        let pid_file = format!("/tmp/{client_id}.pid");
        let args = [
            "exec",
            // A terminal makes mosquitto_sub flush every line; through a pipe it buffers them.
            "--tty",
            &self.container.name,
            "/bin/sh",
            "-c",
            SUBSCRIBE_SCRIPT,
            &pid_file,
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-d",
            "-u",
            CLOUD_USER,
            "-P",
            &self.cloud_password,
            "-i",
            &client_id,
            "-q",
            "1",
            "--retain-as-published",
            "-F",
            "MSG %t %r %q %x",
            "-t",
            filter,
        ];
        let mut child = Command::new(self.container.runtime)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the container runtime");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line.trim_end_matches('\r').to_owned()).is_err() {
                    break;
                }
            }
        });
        let subscription = Subscription {
            child,
            lines,
            runtime: self.container.runtime,
            container: self.container.name.clone(),
            pid_file,
        };
        subscription
            .next_line(|line| line.starts_with("Subscribed (mid:"), READY_LIMIT)
            .unwrap_or_else(|| panic!("no SUBACK for {filter}"));
        subscription
    }
}

/// One message the cloud account received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    /// The topic.
    pub topic: String,
    /// The retain flag as published (or 1 for a retained message sent at subscribe time).
    pub retain: bool,
    /// The QoS of the delivery.
    pub qos: u8,
    /// The payload.
    pub payload: Vec<u8>,
}

/// A background `mosquitto_sub`; stopped when dropped.
#[derive(Debug)]
pub struct Subscription {
    child: Child,
    lines: Receiver<String>,
    runtime: &'static str,
    container: String,
    pid_file: String,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Killing the local `exec` client does not stop `mosquitto_sub` in the container, which
        // would stay subscribed; stop it by its pid first. Its exit ends the `exec` session, so
        // the wait below returns once it is gone. Best effort: after a restart it is gone already.
        let _ = Command::new(self.runtime)
            .args([
                "exec",
                &self.container,
                "/bin/sh",
                "-c",
                r#"kill "$(cat "$0")" 2>/dev/null; rm -f "$0""#,
                &self.pid_file,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Subscription {
    fn next_line(&self, wanted: impl Fn(&str) -> bool, limit: Duration) -> Option<String> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if wanted(&line) => return Some(line),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The next message, or `None` after `limit`.
    ///
    /// # Panics
    ///
    /// When `mosquitto_sub` prints a line that is not in the expected format.
    #[must_use]
    pub fn next_message(&self, limit: Duration) -> Option<Delivered> {
        let line = self.next_line(|line| line.starts_with("MSG "), limit)?;
        let mut fields = line.splitn(5, ' ');
        let _ = fields.next();
        let topic = fields.next().expect("topic").to_owned();
        let retain = fields.next().expect("retain") == "1";
        let qos = fields.next().expect("qos").parse().expect("qos number");
        let hex = fields.next().unwrap_or("").trim();
        let payload = hex
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("hex payload");
                u8::from_str_radix(pair, 16).expect("hex payload")
            })
            .collect();
        Some(Delivered {
            topic,
            retain,
            qos,
            payload,
        })
    }

    /// Waits for the first message that `wanted` accepts, skipping others.
    ///
    /// # Panics
    ///
    /// When none arrives within `limit`; the message names `what`.
    pub fn wait_for(
        &self,
        what: &str,
        limit: Duration,
        wanted: impl Fn(&Delivered) -> bool,
    ) -> Delivered {
        let deadline = Instant::now() + limit;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_message(left) {
                Some(message) if wanted(&message) => return message,
                Some(message) => seen.push(message.topic),
                None => {
                    panic!("timed out after {limit:?} waiting for {what}; other messages: {seen:?}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_taken_before_the_runtime_binds_it_is_retried_with_another() {
        // Another process holds the first port the broker would use.
        let taken = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let mut ports = [taken_port]
            .into_iter()
            .chain(std::iter::repeat_with(free_port));
        let Some(broker) = Mosquitto::start_with(&TestPki::new(), || ports.next().unwrap()) else {
            return;
        };
        assert_ne!(broker.address().port(), taken_port);
        // The broker answers on its port: it was started and set up.
        broker.register_gateway("gw-port-test");
    }

    #[test]
    fn a_dropped_subscription_stops_mosquitto_sub_in_the_container() {
        let Some(broker) = Mosquitto::start(&TestPki::new()) else {
            return;
        };
        let running = || broker.exec(&["pidof", "mosquitto_sub"]).status.success();
        let subscription = broker.subscribe("trustiot/#");
        assert!(running(), "the subscription runs in the container");
        drop(subscription);
        assert!(!running(), "mosquitto_sub outlived its Subscription");
    }
}
