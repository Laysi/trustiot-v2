//! A throwaway Mosquitto 2.1 with dynamic security for the client tests, and helpers to wait for
//! the client's events.
//!
//! The broker runs in a container (podman or docker) with the chart's service listener settings
//! that matter to a client (MQTT 5 only, `max_packet_size`, `max_inflight_messages`,
//! `max_keepalive`), published on a loopback port of the host only. Dynamic security is
//! initialised with a random admin password; accounts are created with `mosquitto_rr` run inside
//! the container. The container is removed with its volumes when dropped. Without a container
//! runtime the tests skip, unless `CI` is set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]
#![allow(dead_code, reason = "each test file uses a part of the helpers")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use mqtt_client::{Config, Event, Events};
use serde_json::{Value as Json, json};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest: the chart's
/// `mqttBroker.image`, crates/protocol/tests/broker.rs, tools/migrate/tests/mqtt_broker.rs and
/// scripts/lib/stack.ts pin the same one; Renovate updates them in one pull request.
pub const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:860aeb5d63ce509508f6dcfb2bc8a1c1dd2bc60555b562b45ce90e52d8dabb0f";

/// The role of the test's service account: publish under `test/`, subscribe under `test/`
/// (directly or shared). Everything else is refused (dynamic security's default).
pub const ROLE: &str = "test-service";

/// The service account the tests log in with.
pub const USERNAME: &str = "service:test";

/// Generous bounds for a loaded machine; each names what it waits for when it fails.
pub const START_WAIT: Duration = Duration::from_secs(120);
pub const EVENT_WAIT: Duration = Duration::from_secs(90);

/// The broker's configuration and start, inside the container. Everything lives on the image's
/// data volume, so a restart keeps the accounts; dynamic security is initialised only once.
const START_SCRIPT: &str = r#"set -eu
cat > /mosquitto/data/mosquitto.conf <<CONF
log_dest stdout
listener 1883
protocol mqtt
accept_protocol_versions 5
allow_anonymous false
max_packet_size 1056768
max_inflight_messages 20
max_keepalive 120
plugin /usr/lib/mosquitto_dynamic_security.so
plugin_opt_config_file /mosquitto/data/dynamic-security.json
CONF
if [ ! -e /mosquitto/data/dynamic-security.json ]; then
  mosquitto_ctrl dynsec init /mosquitto/data/dynamic-security.json admin "$ADMIN_PASSWORD" > /dev/null
fi
chown -R mosquitto:mosquitto /mosquitto/data
exec mosquitto -c /mosquitto/data/mosquitto.conf
"#;

/// A random lowercase hex string.
pub fn random_hex() -> String {
    let mut bytes = [0_u8; 16];
    getrandom_fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A random 48-character password from letters and symbols that hex strings, numbers and the
/// client's log messages rarely contain, so any 5-character window of it in a log is a leak.
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"GHJKLMNPQRSTUVWXYZghjkmnpqrstuvwxyz!%&*=?@^~";
    let mut bytes = [0_u8; 48];
    getrandom_fill(&mut bytes);
    bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect()
}

fn getrandom_fill(bytes: &mut [u8]) {
    getrandom::fill(bytes).expect("operating system randomness");
}

/// A directory under cargo's temporary directory for this test, removed when dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mqtt-client-{}", random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `content` to `name` and returns its path.
    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A throwaway Mosquitto container.
pub struct Broker {
    container: Container,
    pub port: u16,
    admin_password: String,
}

impl Broker {
    /// Starts the broker and creates [`ROLE`]; `None` (after [`unavailable`]) without a
    /// container runtime.
    pub fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        // A free loopback port, named explicitly so the mapping survives a restart.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let broker = Self {
            container: Container {
                runtime,
                name: unique_name("mqtt-client-mosquitto"),
            },
            port,
            admin_password: random_hex(),
        };
        let publish = format!("127.0.0.1:{port}:1883");
        let output = Command::new(runtime)
            .args([
                "run",
                "--detach",
                "--name",
                &broker.container.name,
                "--memory",
                "128m",
                "--publish",
                &publish,
                "--env",
                "ADMIN_PASSWORD",
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ])
            .env("ADMIN_PASSWORD", &broker.admin_password)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "start Mosquitto: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        broker.wait_ready();
        broker.control(&json!([{
            "command": "createRole",
            "rolename": ROLE,
            "acls": [
                { "acltype": "publishClientSend", "topic": "test/#", "allow": true },
                { "acltype": "publishClientReceive", "topic": "test/#", "allow": true },
                { "acltype": "subscribePattern", "topic": "test/#", "allow": true },
                { "acltype": "subscribePattern", "topic": "$share/+/test/#", "allow": true },
            ]
        }]));
        Some(broker)
    }

    /// Waits until dynamic security answers commands.
    pub fn wait_ready(&self) {
        let started = Instant::now();
        while self
            .try_control(&json!([{ "command": "getDefaultACLAccess" }]))
            .is_none()
        {
            assert!(
                started.elapsed() < START_WAIT,
                "Mosquitto did not answer dynamic-security commands within {START_WAIT:?}"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Sends dynamic-security commands; `None` when the broker did not answer.
    fn try_control(&self, commands: &Json) -> Option<Vec<Json>> {
        let request = json!({ "commands": commands }).to_string();
        let output = Command::new(self.container.runtime)
            .args([
                "exec",
                &self.container.name,
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
                "5",
                "-m",
                &request,
            ])
            .output()
            .unwrap();
        if !output.status.success() {
            return None;
        }
        let response: Json = serde_json::from_slice(&output.stdout).ok()?;
        response["responses"].as_array().cloned()
    }

    /// Sends dynamic-security commands and requires every one to succeed.
    pub fn control(&self, commands: &Json) {
        let responses = self.try_control(commands).unwrap_or_else(|| {
            panic!(
                "no answer to dynamic-security command {}",
                commands[0]["command"]
            )
        });
        for response in &responses {
            assert!(response.get("error").is_none(), "{response}");
        }
    }

    /// Creates the service account [`USERNAME`] with [`ROLE`].
    pub fn create_account(&self, password: &str) {
        self.control(&json!([{
            "command": "createClient",
            "username": USERNAME,
            "password": password,
            "roles": [{ "rolename": ROLE }]
        }]));
    }

    /// Gives [`USERNAME`] a new password; the old one stops working at once for new logins.
    pub fn set_password(&self, password: &str) {
        self.control(&json!([{
            "command": "setClientPassword",
            "username": USERNAME,
            "password": password,
        }]));
    }

    /// Restarts the container (SIGTERM, then a new broker process with the same accounts).
    pub fn restart(&self) {
        let _ = runtime_output(
            self.container.runtime,
            &["restart", "-t", "10", &self.container.name],
        );
        self.wait_ready();
    }

    /// The client's settings for [`USERNAME`] with `password_file`.
    pub fn config(&self, password_file: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: self.port,
            username: USERNAME.to_owned(),
            password_file,
        }
    }
}

/// Waits up to [`EVENT_WAIT`] for an event that `wanted` accepts, skipping the others; `what`
/// names it in the failure message, with the events skipped.
pub async fn expect_event(
    events: &mut Events,
    what: &str,
    wanted: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let mut skipped = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) if wanted(&event) => return event,
            Ok(Some(event)) => skipped.push(format!("{event:?}")),
            Ok(None) => panic!("the client ended while waiting for {what}; skipped {skipped:#?}"),
            Err(_) => panic!("no {what} within {EVENT_WAIT:?}; skipped {skipped:#?}"),
        }
    }
}

/// Waits for [`Event::Connected`] and returns the SUBACK reason codes.
pub async fn expect_connected(events: &mut Events, what: &str) -> Vec<u8> {
    match expect_event(events, what, |event| {
        matches!(event, Event::Connected { .. })
    })
    .await
    {
        Event::Connected { subscriptions } => subscriptions.iter().map(|s| s.code).collect(),
        _ => unreachable!(),
    }
}

/// Prints a line the test report quotes (`cargo test -- --nocapture`).
#[allow(
    clippy::print_stderr,
    reason = "the test report quotes these lines (cargo test -- --nocapture)"
)]
pub fn evidence(line: impl std::fmt::Display) {
    eprintln!("evidence: {line}");
}
