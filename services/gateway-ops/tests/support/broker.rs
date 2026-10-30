//! A throwaway Mosquitto 2.1 for the delivery's tests (Laysi/trustiot-v2#720): the gateway-ops
//! service's account with the role and default access the migration Job gives it
//! (`migrate::mqtt_accounts`), and a stand-in gateway that reads every gateway's `down/config`
//! and publishes apply reports. Adapted from services/ingest/tests/support/broker.rs.
//!
//! The broker runs in a container (podman or docker) with the chart's service listener settings
//! that matter to a client (MQTT 5 only, `max_packet_size`, `max_inflight_messages`,
//! `max_keepalive`), published on a loopback port of the host only. It has no persistence, as
//! the chart's broker may lose its retained messages: a restart ([`Broker::stop`] then
//! [`Broker::resume`]) keeps the accounts (dynamic security's file is on the image's data volume)
//! and drops every retained message. Dynamic security starts with a random admin password; the
//! accounts are created with `mosquitto_rr` inside the container. The container is removed with
//! its volumes when dropped. Without a container runtime the tests skip, unless `CI` is set,
//! where they fail.
//!
//! The stand-in gateway is a service account (`service:test-gateway`) whose role may read every
//! gateway's `down/config` and publish on every `up/config-report`, `up/status` and
//! `up/heartbeat`: the client crate speaks only to the service port, which takes passwords, and
//! the gateway-ops service sees the same topics a gateway's certificate would give it (the
//! gateway comes from the topic). [`RawGateway`] is the same account over a bare MQTT 5
//! connection with a last will, which it can drop or leave half open.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use migrate::mqtt_accounts::{
    default_acl_access, service, service_role, service_role_acls, service_username,
};
use mqtt_client::{Client, Config, Event, Events, PublishOptions};
use protocol::config::DesiredConfig;
use protocol::topic::{Channel, parse_topic};
use protocol::{ConfigVersion, GatewayId};
use serde_json::{Value as Json, json};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The broker image, pinned once in testkit with the chart's `mqttBroker.image`.
pub use testkit::mqtt::mosquitto::MOSQUITTO_IMAGE;

/// The stand-in gateway's role and account.
const GATEWAY_ROLE: &str = "test-gateway";
const GATEWAY_USERNAME: &str = "service:test-gateway";

/// How long the broker may take to start, and the stand-in gateway to connect or to receive a
/// configuration. Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
pub const BROKER_WAIT: Duration = Duration::from_secs(120);
pub const MESSAGE_WAIT: Duration = Duration::from_secs(90);

/// How many times the broker's start is tried when its loopback port was taken before the
/// container runtime bound it.
const START_ATTEMPTS: u32 = 5;

/// The broker's configuration and start, inside the container. Everything lives on the image's
/// data volume, so a restart keeps the accounts; dynamic security is initialised only once. No
/// `persistence`: retained messages live in memory only and a restart drops them. The script
/// runs as root (`--user root` below, as testkit's and protocol's brokers do with the image's
/// default user): it writes the volume, hands it to `mosquitto`, and Mosquitto started as root
/// drops to that user, which then saves the dynamic-security file.
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
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gateway-ops-{}", random_hex()));
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

/// A throwaway Mosquitto container with the gateway-ops service's account and the stand-in
/// gateway's.
pub struct Broker {
    container: Container,
    pub port: u16,
    admin_password: String,
    service_password: String,
    gateway_password: String,
    scratch: Scratch,
}

impl Broker {
    /// Starts the broker with the production's default access, the gateway-ops service's role
    /// and account, and the stand-in gateway's; `None` (after [`unavailable`]) without a
    /// container runtime.
    pub fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let admin_password = random_hex();
        // A free loopback port, named explicitly so the mapping survives a restart. The port is
        // released before the runtime binds it, so another process (a parallel test binary) may
        // take it in between: the runtime then refuses the mapping, and the start is tried again
        // on another port, with a new container (the refused one is removed when dropped).
        let mut attempt = 1;
        let (container, port) = loop {
            let port = TcpListener::bind("127.0.0.1:0")
                .expect("bind a free port")
                .local_addr()
                .expect("the bound address")
                .port();
            let container = Container {
                runtime,
                name: unique_name("gateway-ops-mosquitto"),
            };
            let publish = format!("127.0.0.1:{port}:1883");
            let output = Command::new(runtime)
                .args([
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
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
                .env("ADMIN_PASSWORD", &admin_password)
                .output()
                .expect("run the container runtime");
            if output.status.success() {
                break (container, port);
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            // podman (pasta, rootlessport) reports "address already in use", docker "port is
            // already allocated".
            let lower = stderr.to_ascii_lowercase();
            let port_taken = lower.contains("address already in use")
                || lower.contains("port is already allocated");
            assert!(
                port_taken && attempt < START_ATTEMPTS,
                "start Mosquitto (attempt {attempt} of {START_ATTEMPTS}): {stderr}"
            );
            attempt += 1;
        };
        let broker = Self {
            container,
            port,
            admin_password,
            service_password: random_hex(),
            gateway_password: random_hex(),
            scratch: Scratch::new(),
        };
        broker.wait_ready();

        let defaults: Vec<Json> = default_acl_access()
            .as_object()
            .expect("the default access is an object")
            .iter()
            .map(|(acltype, allow)| json!({ "acltype": acltype, "allow": allow }))
            .collect();
        let gateway_ops =
            service("gateway-ops").expect("gateway-ops is one of the migration Job's services");
        broker.control(&json!([
            { "command": "setDefaultACLAccess", "acls": defaults },
            {
                "command": "createRole",
                "rolename": service_role("gateway-ops"),
                "acls": service_role_acls(gateway_ops),
            },
            {
                "command": "createClient",
                "username": service_username("gateway-ops"),
                "password": broker.service_password,
                "roles": [{ "rolename": service_role("gateway-ops") }],
            },
            {
                "command": "createRole",
                "rolename": GATEWAY_ROLE,
                "acls": [
                    {
                        "acltype": "publishClientSend",
                        "topic": Channel::ConfigReport.filter(),
                        "allow": true,
                    },
                    {
                        "acltype": "subscribePattern",
                        "topic": Channel::Config.filter(),
                        "allow": true,
                    },
                    {
                        "acltype": "publishClientReceive",
                        "topic": Channel::Config.filter(),
                        "allow": true,
                    },
                    // The presence tests' stand-in gateway publishes statuses (its last will
                    // among them) and heartbeats.
                    {
                        "acltype": "publishClientSend",
                        "topic": Channel::Status.filter(),
                        "allow": true,
                    },
                    {
                        "acltype": "publishClientSend",
                        "topic": Channel::Heartbeat.filter(),
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

    /// Stops the broker: connections drop, new ones are refused until [`Broker::resume`], and
    /// every retained message is gone.
    pub fn stop(&self) {
        // Panics, naming the command, when the runtime fails.
        let _ = runtime_output(
            self.container.runtime,
            &["stop", "-t", "10", &self.container.name],
        );
    }

    /// Starts the stopped broker again, with the same accounts and port and no retained
    /// messages.
    pub fn resume(&self) {
        let _ = runtime_output(self.container.runtime, &["start", &self.container.name]);
        self.wait_ready();
    }

    /// The gateway-ops service's broker settings, with its password in a private file.
    pub fn service_config(&self) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: self.port,
            username: service_username("gateway-ops"),
            password_file: self
                .scratch
                .write_secret("gateway-ops-password", &self.service_password),
        }
    }

    /// The stand-in gateway's broker settings.
    pub fn gateway_config(&self) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: self.port,
            username: GATEWAY_USERNAME.to_owned(),
            password_file: self
                .scratch
                .write_secret("gateway-password", &self.gateway_password),
        }
    }

    /// The broker's log, or why it could not be read.
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

/// A configuration the stand-in gateway received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// The gateway its topic names.
    pub gateway: GatewayId,
    /// Its version, from the payload's header; `None` for an empty payload (a cleared retained
    /// message).
    pub version: Option<ConfigVersion>,
    /// The retain flag: set for a retained message the broker sent because of the subscription.
    pub retain: bool,
    /// The QoS it arrived with.
    pub qos: u8,
}

/// The stand-in gateway: reads every gateway's `down/config` and publishes on any gateway's
/// `up/config-report`. A new one receives the broker's retained configurations first.
pub struct Gateway {
    client: Client,
    events: Events,
    /// Every configuration read so far, in arrival order.
    pub received: Vec<Received>,
}

impl Gateway {
    /// Connects and subscribes; waits until the subscription is granted.
    pub async fn connect(broker: &Broker) -> Self {
        let (client, mut events) =
            Client::start(&broker.gateway_config(), vec![Channel::Config.filter()])
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
            received: Vec::new(),
        }
    }

    /// Publishes `payload` as `gateway`'s apply report with QoS 1; waits for the broker's PUBACK.
    pub async fn report(&self, gateway: &GatewayId, payload: Vec<u8>) {
        self.client
            .publish(
                &Channel::ConfigReport.topic(gateway),
                payload,
                PublishOptions::default(),
            )
            .await
            .expect("the broker accepts the report");
    }

    fn read(&mut self, event: Option<Event>) {
        match event {
            Some(Event::Message(message)) => {
                let parsed = parse_topic(&message.topic).expect("a configuration topic");
                assert_eq!(parsed.channel, Channel::Config, "{}", message.topic);
                let version = if message.payload.is_empty() {
                    None
                } else {
                    let config = DesiredConfig::decode(&message.payload)
                        .expect("the service publishes valid configurations");
                    Some(config.version)
                };
                self.received.push(Received {
                    gateway: parsed.gateway,
                    version,
                    retain: message.retain,
                    qos: message.qos,
                });
            }
            Some(_) => {}
            None => panic!("the stand-in gateway's client ended"),
        }
    }

    /// Waits up to [`MESSAGE_WAIT`] until a configuration of `gateway` matching `wanted` has
    /// arrived (also one read earlier), and returns the first such.
    pub async fn wait(
        &mut self,
        what: &str,
        gateway: &GatewayId,
        wanted: impl Fn(&Received) -> bool,
    ) -> Received {
        let deadline = tokio::time::Instant::now() + MESSAGE_WAIT;
        loop {
            if let Some(found) = self
                .received
                .iter()
                .find(|received| received.gateway == *gateway && wanted(received))
            {
                return found.clone();
            }
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(event) => self.read(event),
                Err(_) => panic!(
                    "{what}: no such configuration of {gateway} within {MESSAGE_WAIT:?}; \
                     received: {:?}",
                    self.received
                ),
            }
        }
    }

    /// Reads whatever arrives within `window`. Absence can only be sampled: a test uses it where
    /// the code under test must not publish again, so a short window can only miss a wrong
    /// publish, never fail a right one.
    pub async fn drain(&mut self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(event) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            self.read(event);
        }
    }

    /// The configurations of `gateway` read so far.
    pub fn of(&self, gateway: &GatewayId) -> Vec<Received> {
        self.received
            .iter()
            .filter(|received| received.gateway == *gateway)
            .cloned()
            .collect()
    }

    /// Disconnects.
    pub async fn close(self) {
        self.client.shutdown().await;
    }
}

/// A stand-in gateway on a bare MQTT 5 connection (`rumqttc`'s packet codec only), as the
/// agent connects: a clean start with its session's last will retained on its `up/status`
/// (`protocol::health::Status::last_will`). It sends nothing on its own, no PINGREQ either, so
/// between publishes the connection is silent: with a short keepalive it is a half-open
/// connection as the broker sees one. [`RawGateway::drop_connection`] closes the socket without
/// a DISCONNECT, as a lost connection does.
pub struct RawGateway {
    stream: tokio::net::TcpStream,
    buffer: bytes::BytesMut,
    next_packet_id: u16,
}

impl RawGateway {
    /// Connects as the stand-in account with a client ID of its own, `keep_alive` seconds and
    /// the last will of `session` on `gateway`'s status topic; waits for a successful CONNACK.
    pub async fn connect(
        broker: &Broker,
        gateway: &GatewayId,
        session: &protocol::SessionId,
        keep_alive: u16,
    ) -> Self {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::{Connect, ConnectReturnCode, LastWill, Login, Packet};
        use tokio::io::AsyncWriteExt as _;

        let will = protocol::health::Status::last_will(session.clone())
            .encode()
            .expect("encode the last will");
        let connect = Connect {
            keep_alive,
            client_id: format!("test-raw-{}", random_hex()),
            clean_start: true,
            properties: None,
        };
        let packet = Packet::Connect(
            connect,
            Some(LastWill::new(
                Channel::Status.topic(gateway),
                will,
                QoS::AtLeastOnce,
                true,
                None,
            )),
            Some(Login::new(
                GATEWAY_USERNAME,
                broker.gateway_password.clone(),
            )),
        );
        let stream = tokio::time::timeout(
            BROKER_WAIT,
            tokio::net::TcpStream::connect(("127.0.0.1", broker.port)),
        )
        .await
        .expect("connect to the broker in time")
        .expect("connect to the broker");
        let mut raw = Self {
            stream,
            buffer: bytes::BytesMut::new(),
            next_packet_id: 1,
        };
        let mut encoded = bytes::BytesMut::new();
        packet.write(&mut encoded, None).expect("encode CONNECT");
        raw.stream.write_all(&encoded).await.expect("send CONNECT");
        match raw.read().await {
            Packet::ConnAck(connack) => assert_eq!(
                connack.code,
                ConnectReturnCode::Success,
                "the broker refused the raw stand-in gateway"
            ),
            other => panic!("expected CONNACK, got {other:?}"),
        }
        raw
    }

    /// The next packet from the broker, within [`MESSAGE_WAIT`].
    async fn read(&mut self) -> rumqttc::v5::mqttbytes::v5::Packet {
        use rumqttc::v5::mqttbytes::Error;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use tokio::io::AsyncReadExt as _;

        let deadline = tokio::time::Instant::now() + MESSAGE_WAIT;
        loop {
            match Packet::read(&mut self.buffer, None) {
                Ok(packet) => return packet,
                Err(Error::InsufficientBytes(_)) => {}
                Err(error) => panic!("an invalid packet from the broker: {error}"),
            }
            let read = tokio::time::timeout_at(deadline, self.stream.read_buf(&mut self.buffer))
                .await
                .unwrap_or_else(|_| panic!("no packet from the broker within {MESSAGE_WAIT:?}"))
                .expect("read from the broker");
            assert!(read > 0, "the broker closed the raw stand-in's connection");
        }
    }

    /// Publishes `payload` to `topic`: with QoS 1 (waiting for the PUBACK) when `acknowledged`,
    /// else with QoS 0, as the agent sends heartbeats.
    pub async fn publish(
        &mut self,
        topic: &str,
        payload: Vec<u8>,
        retain: bool,
        acknowledged: bool,
    ) {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::{Packet, Publish};
        use tokio::io::AsyncWriteExt as _;

        let qos = if acknowledged {
            QoS::AtLeastOnce
        } else {
            QoS::AtMostOnce
        };
        let mut publish = Publish::new(topic, qos, payload, None);
        publish.retain = retain;
        let packet_id = self.next_packet_id;
        if acknowledged {
            publish.pkid = packet_id;
            self.next_packet_id = self.next_packet_id.wrapping_add(1).max(1);
        }
        let mut encoded = bytes::BytesMut::new();
        Packet::Publish(publish)
            .write(&mut encoded, None)
            .expect("encode PUBLISH");
        self.stream.write_all(&encoded).await.expect("send PUBLISH");
        if acknowledged {
            match self.read().await {
                Packet::PubAck(ack) => {
                    assert_eq!(ack.pkid, packet_id, "PUBACK of another packet");
                    assert!(
                        matches!(
                            ack.reason,
                            rumqttc::v5::mqttbytes::v5::PubAckReason::Success
                                | rumqttc::v5::mqttbytes::v5::PubAckReason::NoMatchingSubscribers
                        ),
                        "the broker refused the publish to {topic}: {:?}",
                        ack.reason
                    );
                }
                other => panic!("expected PUBACK, got {other:?}"),
            }
        }
    }

    /// Closes the socket without a DISCONNECT: the broker publishes the last will.
    pub fn drop_connection(self) {
        drop(self.stream);
    }
}
