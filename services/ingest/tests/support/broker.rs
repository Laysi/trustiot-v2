//! A throwaway Mosquitto 2.1 for the telemetry path's tests: the ingest service's account with
//! the role and default access the migration Job gives it (`migrate::mqtt_accounts`), and a
//! stand-in gateway that publishes batches and reads the acknowledgements.
//!
//! The broker runs in a container (podman or docker) with the chart's service listener settings
//! that matter to a client (MQTT 5 only, `max_packet_size`, `max_inflight_messages`,
//! `max_keepalive`), published on a loopback port of the host only, as in
//! crates/mqtt-client/tests/support. Dynamic security starts with a random admin password; the
//! accounts are created with `mosquitto_rr` inside the container. The container is removed with
//! its volumes when dropped. Without a container runtime the tests skip, unless `CI` is set,
//! where they fail.
//!
//! The stand-in gateway is a service account (`service:test-gateway`) whose role may publish on
//! every gateway's `up/telemetry` and read every `down/telemetry-ack`: the client crate speaks
//! only to the service port, which takes passwords, and the ingest service sees the same topics
//! a gateway's certificate would give it (the gateway comes from the topic).

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use migrate::mqtt_accounts::{
    default_acl_access, service, service_role, service_role_acls, service_username,
};
use mqtt_client::{Client, Config, Event, Events, PublishOptions};
use protocol::telemetry::{Sample, Series, TelemetryAck, TelemetryBatch};
use protocol::topic::{Channel, parse_topic};
use protocol::{DeviceId, GatewayId, PointKey, Seq, StreamId, UnixMillis, V1, Value};
use serde_json::{Value as Json, json};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The broker image, pinned once in testkit with the chart's `mqttBroker.image`.
pub use testkit::mqtt::mosquitto::MOSQUITTO_IMAGE;

/// The stand-in gateway's role and account.
const GATEWAY_ROLE: &str = "test-gateway";
const GATEWAY_USERNAME: &str = "service:test-gateway";

/// How long the broker may take to start, and the stand-in gateway to connect or to receive an
/// acknowledgement. Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
pub const BROKER_WAIT: Duration = Duration::from_secs(120);
pub const ACK_WAIT: Duration = Duration::from_secs(90);

/// The broker's configuration and start, inside the container. Everything lives on the image's
/// data volume, so a restart keeps the accounts; dynamic security is initialised only once.
/// The script runs as root (`--user root` below, as testkit's and protocol's brokers do with the
/// image's default user): it writes the volume, hands it to `mosquitto`, and Mosquitto started as
/// root drops to that user, which then saves the dynamic-security file.
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

/// A random lowercase hex string of 32 digits.
pub fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A directory under cargo's temporary directory for this test, removed when dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ingest-{}", random_hex()));
        std::fs::create_dir_all(&dir).expect("create the test's directory");
        Self(dir)
    }

    /// Writes `content` to `name`, readable by the owner only, and returns its path.
    pub fn write_secret(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).expect("write the file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("make the file private");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A throwaway Mosquitto container with the ingest service's account and the stand-in gateway's.
pub struct Broker {
    container: Container,
    pub port: u16,
    admin_password: String,
    ingest_password: String,
    gateway_password: String,
    scratch: Scratch,
}

impl Broker {
    /// Starts the broker with the production's default access, the ingest service's role and
    /// account, and the stand-in gateway's; `None` (after [`unavailable`]) without a container
    /// runtime.
    pub fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        // A free loopback port, named explicitly so the mapping survives a restart.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind a free port")
            .local_addr()
            .expect("the bound address")
            .port();
        let broker = Self {
            container: Container {
                runtime,
                name: unique_name("ingest-mosquitto"),
            },
            port,
            admin_password: random_hex(),
            ingest_password: random_hex(),
            gateway_password: random_hex(),
            scratch: Scratch::new(),
        };
        let publish = format!("127.0.0.1:{port}:1883");
        let output = Command::new(runtime)
            .args([
                "run",
                "--detach",
                "--name",
                &broker.container.name,
                "--user",
                "root",
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
            .expect("run the container runtime");
        assert!(
            output.status.success(),
            "start Mosquitto: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        broker.wait_ready();

        let defaults: Vec<Json> = default_acl_access()
            .as_object()
            .expect("the default access is an object")
            .iter()
            .map(|(acltype, allow)| json!({ "acltype": acltype, "allow": allow }))
            .collect();
        let ingest = service("ingest").expect("ingest is one of the migration Job's services");
        broker.control(&json!([
            { "command": "setDefaultACLAccess", "acls": defaults },
            {
                "command": "createRole",
                "rolename": service_role("ingest"),
                "acls": service_role_acls(ingest),
            },
            {
                "command": "createClient",
                "username": service_username("ingest"),
                "password": broker.ingest_password,
                "roles": [{ "rolename": service_role("ingest") }],
            },
            {
                "command": "createRole",
                "rolename": GATEWAY_ROLE,
                "acls": [
                    {
                        "acltype": "publishClientSend",
                        "topic": Channel::Telemetry.filter(),
                        "allow": true,
                    },
                    {
                        "acltype": "subscribePattern",
                        "topic": Channel::TelemetryAck.filter(),
                        "allow": true,
                    },
                    {
                        "acltype": "publishClientReceive",
                        "topic": Channel::TelemetryAck.filter(),
                        "allow": true,
                    },
                ],
            },
            {
                "command": "createClient",
                "username": GATEWAY_USERNAME,
                "password": broker.gateway_password,
                "roles": [{ "rolename": GATEWAY_ROLE }],
            },
        ]));
        Some(broker)
    }

    /// Waits until dynamic security answers commands.
    fn wait_ready(&self) {
        let started = Instant::now();
        while self
            .try_control(&json!([{ "command": "getDefaultACLAccess" }]))
            .is_none()
        {
            assert!(
                started.elapsed() < BROKER_WAIT,
                "Mosquitto did not answer dynamic-security commands within {BROKER_WAIT:?}"
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
            .expect("run the container runtime");
        if !output.status.success() {
            return None;
        }
        let response: Json = serde_json::from_slice(&output.stdout).ok()?;
        response["responses"].as_array().cloned()
    }

    /// Sends dynamic-security commands and requires every one to succeed.
    fn control(&self, commands: &Json) {
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

    /// Stops the broker: connections drop and new ones are refused until [`Broker::resume`].
    pub fn stop(&self) {
        // Panics, naming the command, when the runtime fails.
        let _ = runtime_output(
            self.container.runtime,
            &["stop", "-t", "10", &self.container.name],
        );
    }

    /// Starts the stopped broker again, with the same accounts and port.
    pub fn resume(&self) {
        let _ = runtime_output(self.container.runtime, &["start", &self.container.name]);
        self.wait_ready();
    }

    /// The ingest service's broker settings, with its password in a private file.
    pub fn ingest_config(&self) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: self.port,
            username: service_username("ingest"),
            password_file: self
                .scratch
                .write_secret("ingest-password", &self.ingest_password),
        }
    }

    /// The stand-in gateway's broker settings.
    fn gateway_config(&self) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: self.port,
            username: GATEWAY_USERNAME.to_owned(),
            password_file: self
                .scratch
                .write_secret("gateway-password", &self.gateway_password),
        }
    }

    /// The broker's log (Mosquitto writes it to stderr), or why it could not be read.
    pub fn logs(&self) -> String {
        match Command::new(self.container.runtime)
            .args(["logs", &self.container.name])
            .output()
        {
            Ok(output) => format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => format!("reading the broker's log failed: {error}"),
        }
    }
}

/// The milliseconds since the Unix epoch now.
pub fn now_millis() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("the clock is after 1970");
    u64::try_from(since.as_millis()).expect("milliseconds fit in u64")
}

/// A batch of `stream` numbered `seq`: for each of `devices`, the points `power` (floats) and
/// `count` (integers) with `samples` readings each, one a second, the last at `end_ms`. Values
/// differ by device, point and sample, so a lost or doubled reading changes every sum.
pub fn batch(
    stream: &StreamId,
    seq: u64,
    devices: &[uuid::Uuid],
    samples: u64,
    end_ms: u64,
) -> TelemetryBatch {
    let mut series = Vec::new();
    for (index, device) in (0_u32..).zip(devices) {
        for point in ["power", "count"] {
            series.push(Series {
                device: DeviceId::new(device.hyphenated().to_string()).expect("a device ID"),
                point: PointKey::new(point).expect("a point key"),
                samples: (0..samples)
                    .map(|sample| {
                        let at = end_ms - (samples - 1 - sample) * 1_000;
                        let sample = u32::try_from(sample).expect("a few samples");
                        Sample {
                            at: UnixMillis::new(at),
                            value: if point == "power" {
                                Value::Float(f64::from(index) * 1_000.0 + 0.25 * f64::from(sample))
                            } else {
                                Value::Int(i64::from(index) * 1_000 + i64::from(sample))
                            },
                        }
                    })
                    .collect(),
            });
        }
    }
    TelemetryBatch {
        v: V1,
        stream: stream.clone(),
        seq: Seq::new(seq).expect("sequence numbers start at 1"),
        clock_synced: true,
        series,
    }
}

/// An acknowledgement and the gateway its topic names.
#[derive(Clone, Debug)]
pub struct Received {
    pub gateway: GatewayId,
    pub ack: TelemetryAck,
}

/// The stand-in gateway: publishes batches on any gateway's `up/telemetry` and reads every
/// `down/telemetry-ack`.
pub struct Gateway {
    client: Client,
    events: Events,
    /// Acknowledgements read while waiting for another one.
    unclaimed: Vec<Received>,
}

impl Gateway {
    /// Connects and subscribes; waits until the subscription is granted.
    pub async fn connect(broker: &Broker) -> Self {
        let (client, mut events) = Client::start(
            &broker.gateway_config(),
            vec![Channel::TelemetryAck.filter()],
        )
        .expect("start the stand-in gateway's client");
        let deadline = tokio::time::Instant::now() + BROKER_WAIT;
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(Event::Connected { subscriptions })) => {
                    assert!(
                        subscriptions.all_granted(),
                        "the broker refused the stand-in gateway's subscription"
                    );
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stand-in gateway's client ended"),
                Err(_) => panic!("the stand-in gateway did not connect within {BROKER_WAIT:?}"),
            }
        }
        Self {
            client,
            events,
            unclaimed: Vec::new(),
        }
    }

    /// Publishes `payload` as `gateway`'s telemetry with QoS 1, with the retain flag when
    /// `retain`; waits for the broker's PUBACK.
    pub async fn send(&self, gateway: &GatewayId, payload: Vec<u8>, retain: bool) {
        self.client
            .publish(
                &Channel::Telemetry.topic(gateway),
                payload,
                PublishOptions {
                    retain,
                    message_expiry: None,
                },
            )
            .await
            .expect("the broker accepts the batch");
    }

    /// Reads the connection's events until none is ready, keeping every acknowledgement.
    fn read(&mut self, event: Option<Event>) {
        match event {
            Some(Event::Message(message)) => {
                let parsed = parse_topic(&message.topic).expect("an acknowledgement topic");
                assert_eq!(parsed.channel, Channel::TelemetryAck, "{}", message.topic);
                let ack: TelemetryAck =
                    protocol::decode(&message.payload).expect("a valid acknowledgement");
                self.unclaimed.push(Received {
                    gateway: parsed.gateway,
                    ack,
                });
            }
            Some(_) => {}
            None => panic!("the stand-in gateway's client ended"),
        }
    }

    fn take(&mut self, gateway: &GatewayId, stream: &StreamId, seq: u64) -> Option<Received> {
        let at = self.unclaimed.iter().position(|received| {
            received.gateway == *gateway
                && received.ack.stream == *stream
                && received.ack.seq.get() == seq
        })?;
        Some(self.unclaimed.remove(at))
    }

    /// Waits up to [`ACK_WAIT`] for the acknowledgement of `gateway`'s batch `seq` of `stream`.
    pub async fn ack(&mut self, gateway: &GatewayId, stream: &StreamId, seq: u64) -> TelemetryAck {
        let deadline = tokio::time::Instant::now() + ACK_WAIT;
        loop {
            if let Some(received) = self.take(gateway, stream, seq) {
                return received.ack;
            }
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(event) => self.read(event),
                Err(_) => panic!(
                    "no acknowledgement of {gateway} {stream} {seq} within {ACK_WAIT:?}; \
                     others: {:?}",
                    self.unclaimed
                ),
            }
        }
    }

    /// The acknowledgements of `gateway`'s batch `seq` of `stream` that arrive within `window`.
    /// Absence can only be sampled: a test uses it where the code under test must not have
    /// answered yet, so a short window can only miss a wrong answer, never fail a right one.
    pub async fn acks_within(
        &mut self,
        gateway: &GatewayId,
        stream: &StreamId,
        seq: u64,
        window: Duration,
    ) -> Vec<TelemetryAck> {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(event) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            self.read(event);
        }
        let mut found = Vec::new();
        while let Some(received) = self.take(gateway, stream, seq) {
            found.push(received.ack);
        }
        found
    }
}
