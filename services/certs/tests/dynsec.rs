//! The MQTT broker's gateway permissions end to end (Laysi/trustiot-v2#633; `README.md`, "broker
//! 權限（dynamic security）"): Mosquitto with the chart's configuration
//! (`deploy/chart/files/mosquitto/mosquitto.conf` and its gateway listener) and the
//! dynamic-security file the broker's init container writes (`migrate::mqtt_accounts::reconcile`),
//! the real certs service syncing it on the broker's service port as `service:certs`, and
//! gateways connecting on the TLS port with certificates the service signed at their factory
//! registration:
//!
//! - A registered gateway that is not bound connects, and every publish gets PUBACK 0x87. Once it
//!   is bound, the same connection publishes (acceptance 2).
//! - A registered, bound gateway publishes to its own `up/` channel and subscribes to its own
//!   `down/`; publishing to and subscribing under another gateway get 0x87 (acceptance 1).
//! - With nothing to change, whole comparisons send no change: the broker's dynamic-security file
//!   stays byte for byte the same (acceptance 4).
//! - The broker restarts with a file holding only the chart's entries and one gateway client that
//!   should not exist: after one comparison the gateways are back, the stray one is gone, and the
//!   service accounts, roles and groups are what the init container wrote (acceptance 3).
//! - The service's log at `trace` holds no piece of the broker password (acceptance 5).
//! - A renewal asks for its gateway's comparison once it has committed (Laysi/trustiot-v2#81):
//!   with whole comparisons an hour apart, a gateway whose only certificate was revoked loses its
//!   client in the comparison on connecting, gets it back right after its renewal with no whole
//!   comparison in between, and publishes with the renewed certificate.
//! - A platform administrator's revocation (Laysi/trustiot-v2#636; `README.md`, "閘道器憑證的撤
//!   銷"), with two replicas and whole comparisons an hour apart, sent to the second: the
//!   gateway's open connection ends (N, measured), the second replica's log shows the ledger,
//!   the new CRL and the client's removal in that order, and the first replica changes nothing.
//!   Before the broker loads the new CRL the old certificate connects and gets PUBACK 0x87; after
//!   the CRL from the service passes the trust sidecar's check and the broker reloads, the TLS
//!   handshake fails with `CertificateRevoked`. A renewal brings the client back. Before that,
//!   which `mosquitto_ctrl dynsec` commands end a connected gateway's session is recorded.
//!
//! Starts a throwaway Postgres with TLS and a Mosquitto container (podman or docker); skipped
//! without them unless `CI` is set. `-- --nocapture` prints the evidence lines of the test report.

mod support;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use certs::gateways::{LOCK_GATEWAY_CERTIFICATES, REVOKE_PATH};
use certs::http::CRL_PATH;
use certs::pem;
use certs::registrations::REGISTRATIONS_PATH;
use certs::workstations::CERTIFICATES_PATH;
use contract::certs::{
    GatewayRegistration, WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER,
    WORKSTATION_SIGNATURE_HEADER, WORKSTATION_TIMESTAMP_HEADER, WorkstationCertificate,
    workstation_signing_input,
};
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{GatewayBindingId, ServiceName, TenantId};
use migrate::mqtt_accounts::{PASSWORD_ITERATIONS, SERVICES, encode_password, reconcile};
use migrate::mqtt_trust::GATEWAY_LISTENER_FILE;
use outbox::Aggregate;
use protocol::GatewayId;
use protocol::acl::GATEWAY_ROLE;
use protocol::renewal::{Challenge, RENEWAL_PATH, RenewedCertificate};
use protocol::topic::Channel;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData as _,
    SigningKey as _,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sha2::{Digest, Sha256};
use sqlx::Connection as _;
use sqlx::postgres::PgPoolOptions;
use support::{LIMIT, Reply, Service, TestCa, now, temp_dir, wait_for};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::mqtt::mosquitto::MOSQUITTO_IMAGE;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use x509_parser::prelude::FromDer as _;
use x509_parser::revocation_list::CertificateRevocationList;

/// The roles the test logs in with: the certs service's two, and the assets service's, which
/// writes the bindings.
const LOGINS: &[&str] = &[
    "assets_app",
    "assets_platform",
    "certs_app",
    "certs_platform",
];

/// The tests of this file share one Postgres ([`product_postgres`]) and with it the services'
/// job locks and every registered gateway, so they run one after the other.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The chart's broker configuration and its gateway listener.
const CONFIG: &str = include_str!("../../../deploy/chart/files/mosquitto/mosquitto.conf");
const GATEWAY_LISTENER: &str =
    include_str!("../../../deploy/chart/files/mosquitto/gateway-listener.conf");

/// Where the test keeps its files in the container: the configuration, a dynamic-security file
/// to install at the next start, and the marker that starts the broker. On the container's own
/// filesystem, so they outlive a restart.
const STAGE_DIR: &str = "/trustiot";

/// Where the broker's pod mounts what the chart's configuration reads.
const TLS_DIR: &str = "/etc/trustiot/mqtt-broker-tls";
const TRUST_DIR: &str = "/mosquitto/data/trust/current";
const LISTENERS_DIR: &str = "/mosquitto/listeners";
const DYNSEC_FILE: &str = "/mosquitto/data/dynamic-security.json";

/// Waits for the marker, installs a staged dynamic-security file (what the accounts init
/// container does before every start), and runs the broker as PID 1 (it drops to its own user),
/// so `restart` restarts it.
const START_SCRIPT: &str = r#"set -eu
while [ ! -f /trustiot/ready ]; do sleep 0.2; done
if [ -f /trustiot/dynamic-security.json ]; then
  mv /trustiot/dynamic-security.json /mosquitto/data/dynamic-security.json
fi
mkdir -p /run/mosquitto
chown -R mosquitto:mosquitto /trustiot /run/mosquitto /etc/trustiot /mosquitto/data /mosquitto/listeners
exec mosquitto -c /trustiot/mosquitto.conf
"#;

/// The DNS name of the test server certificate.
const SERVER_NAME: &str = "localhost";

/// The gateways of the test, and one that only the reset file has.
const GATEWAY_A: &str = "gw-633-a";
const GATEWAY_B: &str = "gw-633-b";
const GHOST: &str = "gw-633-ghost";
/// The gateway the administrator revokes, and one that keeps its certificate.
const REVOKED_GATEWAY: &str = "gw-636-revoked";
const PROBE: &str = "gw-636-probe";

/// N of `README.md`, "閘道器憑證的撤銷": a revocation ends the gateway's open connection within
/// this long. It covers the new CRL's 10 s at most (`certs::crl::REISSUE_TIMEOUT`), the ledger,
/// the job lock and the broker; the test bounds the time against it, not a shorter one.
const DISCONNECT_WITHIN: Duration = Duration::from_secs(30);

/// PUBACK and SUBACK reason codes.
const SUCCESS: u8 = 0x00;
const NO_MATCHING_SUBSCRIBERS: u8 = 0x10;
const GRANTED_QOS_1: u8 = 0x01;
const NOT_AUTHORIZED: u8 = 0x87;

/// The service's log lines this test waits for (`src/dynsec.rs`).
const IN_SYNC: &str = "the broker's gateway permissions match postgres";
const CHANGED: &str = "changed a gateway's broker permissions";
/// What a whole comparison that changed something logs; a single gateway's comparison does not.
const IN_LINE: &str = "brought the broker's gateway permissions in line with postgres";
/// What a renewal logs once its transaction has committed (`src/renewal.rs`).
const RENEWED: &str = "renewed a gateway certificate";

/// The CRL schedule of `tests/registration.rs`: the first CRL is out within seconds, so an
/// issuing CA can sign.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

#[allow(
    clippy::print_stderr,
    reason = "the test report quotes these lines (cargo test -- --nocapture)"
)]
fn evidence(line: impl Display) {
    eprintln!("evidence: {line}");
}

fn gateway_id(value: &str) -> GatewayId {
    GatewayId::new(value).expect("a gateway id")
}

fn random_text(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    getrandom::fill(&mut bytes).expect("random");
    URL_SAFE_NO_PAD.encode(bytes).chars().take(len).collect()
}

/// A local port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port")
        .port()
}

// ---- the broker ----

/// The Mosquitto container, its service and gateway ports published on fixed loopback ports, so
/// they stay the same across a restart.
struct Broker {
    container: Container,
    service_port: u16,
    gateway_port: u16,
}

impl Broker {
    fn start(runtime: &'static str) -> Self {
        let container = Container {
            runtime,
            name: unique_name("certs-dynsec-broker"),
        };
        let (service_port, gateway_port) = (free_port(), free_port());
        let _id = runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--memory",
                "256m",
                "--publish",
                &format!("127.0.0.1:{service_port}:1883"),
                "--publish",
                &format!("127.0.0.1:{gateway_port}:8883"),
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ],
        );
        Self {
            container,
            service_port,
            gateway_port,
        }
    }

    /// Writes `contents` to `path` inside the container, through standard input.
    fn push(&self, path: &str, contents: &[u8]) {
        let mut child = Command::new(self.container.runtime)
            .args([
                "exec",
                "-i",
                &self.container.name,
                "sh",
                "-c",
                r#"mkdir -p "$(dirname "$0")" && cat > "$0""#,
                path,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start exec");
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(contents).expect("write the file");
        }
        let output = child.wait_with_output().expect("exec");
        assert!(
            output.status.success(),
            "push {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn logs(&self) -> String {
        let output = Command::new(self.container.runtime)
            .args(["logs", &self.container.name])
            .output()
            .expect("read the broker's logs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Waits until the broker has logged that it runs `starts` times.
    async fn wait_running(&self, starts: usize) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let logs = self.logs();
            if logs.matches(" running").count() >= starts {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Mosquitto did not log start {starts} within {LIMIT:?}:\n{logs}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Restarts the container: Mosquitto stops, the staged file replaces its dynamic-security
    /// file, and it starts again.
    fn restart(&self) {
        let _ = runtime_output(
            self.container.runtime,
            &["restart", "--time", "10", &self.container.name],
        );
    }

    /// The broker's dynamic-security file as it saved it.
    fn dynsec_text(&self) -> String {
        runtime_output(
            self.container.runtime,
            &["exec", &self.container.name, "cat", DYNSEC_FILE],
        )
    }

    fn dynsec(&self) -> Value {
        serde_json::from_str(&self.dynsec_text()).expect("the dynamic-security file is JSON")
    }

    /// Runs `mosquitto_ctrl dynsec <args>` in the container against the service port, as
    /// `service:certs` with `password`. The password is not echoed on failure.
    fn dynsec_command(&self, password: &str, args: &[&str]) {
        let output = Command::new(self.container.runtime)
            .args([
                "exec",
                &self.container.name,
                "mosquitto_ctrl",
                "-h",
                "127.0.0.1",
                "-p",
                "1883",
                "-u",
                "service:certs",
                "-P",
                password,
                "dynsec",
            ])
            .args(args)
            .output()
            .expect("run mosquitto_ctrl");
        assert!(
            output.status.success(),
            "mosquitto_ctrl dynsec {args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// How often the broker logged a reload.
    fn reloads(&self) -> usize {
        self.logs()
            .lines()
            .filter(|line| line.contains("Reloading config."))
            .count()
    }

    /// What the trust sidecar does after it installed a new CRL: SIGHUP, and returns once
    /// Mosquitto logged the reload, which it does in the same turn of its loop, so every
    /// connection after this sees the new files.
    async fn reload(&self) {
        let before = self.reloads();
        let _ = runtime_output(
            self.container.runtime,
            &["kill", "--signal", "HUP", &self.container.name],
        );
        let deadline = Instant::now() + LIMIT;
        while self.reloads() == before {
            assert!(
                Instant::now() < deadline,
                "Mosquitto did not log the reload within {LIMIT:?}:\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// The broker's clients by username: their role names (sorted), groups and password hash.
fn clients(dynsec: &Value) -> BTreeMap<String, Value> {
    dynsec["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .map(|client| {
            let mut roles: Vec<&str> = client["roles"]
                .as_array()
                .map(|roles| roles.iter().filter_map(|role| role["rolename"].as_str()).collect())
                .unwrap_or_default();
            roles.sort_unstable();
            (
                client["username"].as_str().expect("a username").to_owned(),
                json!({
                    "roles": roles,
                    "disabled": client.get("disabled").cloned().unwrap_or(Value::Bool(false)),
                    "encoded_password": client.get("encoded_password").cloned().unwrap_or(Value::Null),
                }),
            )
        })
        .collect()
}

/// The service accounts of [`clients`].
fn service_accounts(dynsec: &Value) -> BTreeMap<String, Value> {
    clients(dynsec)
        .into_iter()
        .filter(|(username, _)| username.starts_with("service:"))
        .collect()
}

/// The roles by name, with their rules as a sorted list, and the groups.
fn roles_and_groups(dynsec: &Value) -> (BTreeMap<String, Vec<String>>, Value) {
    let roles = dynsec["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .map(|role| {
            let mut rules: Vec<String> = role["acls"]
                .as_array()
                .map(|acls| {
                    acls.iter()
                        .map(|acl| {
                            format!(
                                "{} {} {} {}",
                                acl["acltype"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_ascii_lowercase(),
                                acl["topic"],
                                acl["priority"],
                                acl["allow"]
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            rules.sort_unstable();
            (
                role["rolename"].as_str().expect("a role name").to_owned(),
                rules,
            )
        })
        .collect();
    (roles, dynsec.get("groups").cloned().unwrap_or(json!([])))
}

/// A server certificate for [`SERVER_NAME`] from a CA of its own: the chain and key PEM for the
/// broker's Secret, and the CA (DER) the gateways trust.
fn server_certificate() -> (String, String, Vec<u8>) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut ca_params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "Test broker server CA");
    ca_params.distinguished_name = name;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_certificate = ca_params.self_signed(&ca_key).expect("a server CA");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut params = CertificateParams::new(vec![SERVER_NAME.to_owned()]).expect("params");
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, SERVER_NAME);
    params.distinguished_name = subject;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = params
        .signed_by(&key, &issuer)
        .expect("a server certificate");
    (
        format!("{}{}", leaf.pem(), ca_certificate.pem()),
        key.serialize_pem(),
        ca_certificate.der().to_vec(),
    )
}

// ---- the gateway's side: TLS and MQTT 5 ----

/// A client that trusts `anchor` and presents `chain` with `key`.
fn client_config(
    anchor: &[u8],
    chain: Vec<CertificateDer<'static>>,
    key: &KeyPair,
) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(anchor.to_vec()))
        .expect("a trust anchor");
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .expect("a client certificate"),
    )
}

/// An MQTT variable byte integer.
fn varint(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(value % 128).expect("below 128");
        value /= 128;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut packet = vec![first];
    varint(body.len(), &mut packet);
    packet.extend_from_slice(body);
    packet
}

fn string(value: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&u16::try_from(value.len()).expect("short").to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// Reads one MQTT packet: its first byte, and what follows the remaining length.
fn read_packet(stream: &mut impl std::io::Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut byte = [0_u8; 1];
    stream.read_exact(&mut byte)?;
    let kind = byte[0];
    let mut length = 0_usize;
    for shift in [0, 7, 14, 21] {
        stream.read_exact(&mut byte)?;
        length |= usize::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
    }
    let mut rest = vec![0_u8; length];
    stream.read_exact(&mut rest)?;
    Ok((kind, rest))
}

/// One gateway's MQTT 5 connection over TLS, as the gateway's agent keeps it open.
struct Gateway {
    tls: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    name: String,
    next_id: u16,
}

impl Gateway {
    /// Connects with the certificate the service signed for `name`, sends CONNECT (clean start,
    /// the broker's largest keepalive) and requires a successful CONNACK.
    fn connect(address: SocketAddr, config: &Arc<rustls::ClientConfig>, name: &str) -> Self {
        Self::try_connect(address, config, name)
            .unwrap_or_else(|problem| panic!("{name}: could not connect: {problem}"))
    }

    /// [`Gateway::connect`], or how the TLS handshake or the CONNECT failed: a certificate the
    /// broker refuses fails when the CONNACK is read, since under TLS 1.3 the server checks the
    /// client's certificate after the client has finished its side of the handshake.
    fn try_connect(
        address: SocketAddr,
        config: &Arc<rustls::ClientConfig>,
        name: &str,
    ) -> Result<Self, String> {
        let socket = TcpStream::connect_timeout(&address, LIMIT).expect("connect to the broker");
        socket
            .set_read_timeout(Some(LIMIT))
            .and_then(|()| socket.set_write_timeout(Some(LIMIT)))
            .expect("socket timeouts");
        let server_name = ServerName::try_from(SERVER_NAME).expect("a server name");
        let connection =
            rustls::ClientConnection::new(Arc::clone(config), server_name).expect("a TLS client");
        let mut tls = rustls::StreamOwned::new(connection, socket);
        let mut body = Vec::new();
        string("MQTT", &mut body);
        // Version 5, clean start, keepalive 120 s (the broker's max_keepalive), no properties.
        body.extend_from_slice(&[0x05, 0x02, 0x00, 0x78, 0x00]);
        string(name, &mut body);
        tls.write_all(&packet(0x10, &body))
            .map_err(|error| format!("CONNECT: {error}"))?;
        let (kind, connack) = read_packet(&mut tls).map_err(|error| format!("CONNACK: {error}"))?;
        if kind != 0x20 || connack.get(1) != Some(&0x00) {
            return Err(format!(
                "expected a successful CONNACK, got {kind:02x} {connack:02x?}"
            ));
        }
        Ok(Self {
            tls,
            name: name.to_owned(),
            next_id: 1,
        })
    }

    /// Publishes with QoS 1: the PUBACK reason code, or how the connection ended (the broker
    /// closed it or sent DISCONNECT) instead.
    fn try_publish(&mut self, topic: &str) -> Result<u8, String> {
        let id = self.packet_id();
        let mut body = Vec::new();
        string(topic, &mut body);
        body.extend_from_slice(&id.to_be_bytes());
        body.push(0x00);
        body.extend_from_slice(b"{}");
        self.tls
            .write_all(&packet(0x32, &body))
            .map_err(|error| format!("PUBLISH: {error}"))?;
        loop {
            let (kind, rest) = read_packet(&mut self.tls).map_err(|error| closed(&error))?;
            if kind & 0xf0 == 0xe0 {
                return Err(disconnect(&rest));
            }
            if kind == 0x40 && rest.get(..2) == Some(&id.to_be_bytes()[..]) {
                return Ok(rest.get(2).copied().unwrap_or(SUCCESS));
            }
        }
    }

    /// Waits until the broker ends the connection (up to the socket's read timeout,
    /// [`LIMIT`]) and says how.
    fn wait_closed(&mut self) -> String {
        loop {
            match read_packet(&mut self.tls) {
                Ok((kind, rest)) if kind & 0xf0 == 0xe0 => return disconnect(&rest),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!(
                        "{}: the broker kept the connection for {LIMIT:?}",
                        self.name
                    )
                }
                Err(error) => return closed(&error),
            }
        }
    }

    fn packet_id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// The next packet of `kind` with `id`; the connection must stay up.
    fn expect(&mut self, kind: u8, id: u16) -> Vec<u8> {
        loop {
            let (got, rest) = read_packet(&mut self.tls)
                .unwrap_or_else(|error| panic!("{}: the connection broke: {error}", self.name));
            assert_ne!(
                got & 0xf0,
                0xe0,
                "{}: the broker disconnected: {rest:02x?}",
                self.name
            );
            if got == kind && rest.get(..2) == Some(&id.to_be_bytes()[..]) {
                return rest;
            }
        }
    }

    /// Publishes with QoS 1 and returns the PUBACK reason code.
    fn publish(&mut self, topic: &str) -> u8 {
        let id = self.packet_id();
        let mut body = Vec::new();
        string(topic, &mut body);
        body.extend_from_slice(&id.to_be_bytes());
        body.push(0x00);
        body.extend_from_slice(b"{}");
        self.tls.write_all(&packet(0x32, &body)).expect("PUBLISH");
        let puback = self.expect(0x40, id);
        puback.get(2).copied().unwrap_or(SUCCESS)
    }

    /// Subscribes to `filter` with QoS 1 and returns the SUBACK reason code.
    fn subscribe(&mut self, filter: &str) -> u8 {
        let id = self.packet_id();
        let mut body = id.to_be_bytes().to_vec();
        body.push(0x00);
        string(filter, &mut body);
        body.push(0x01);
        self.tls.write_all(&packet(0x82, &body)).expect("SUBSCRIBE");
        let suback = self.expect(0x90, id);
        let mut properties = 0_usize;
        let mut at = 2;
        for shift in [0, 7, 14, 21] {
            let byte = suback[at];
            at += 1;
            properties |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        suback[at + properties]
    }
}

fn accepted(code: u8) -> bool {
    matches!(code, SUCCESS | NO_MATCHING_SUBSCRIBERS)
}

/// A DISCONNECT packet's reason code, as text.
fn disconnect(rest: &[u8]) -> String {
    format!(
        "DISCONNECT reason {:#04x}",
        rest.first().copied().unwrap_or(0)
    )
}

/// How reading from a connection the broker ended failed.
fn closed(error: &std::io::Error) -> String {
    format!("closed: {error}")
}

// ---- factory registration and binding ----

/// A new P-256 key and a CSR for it.
fn request() -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "ignored");
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

/// A production workstation: its key and certificate (DER) from the platform administrators'
/// API.
struct Workstation {
    key: KeyPair,
    certificate: Vec<u8>,
}

impl Workstation {
    /// A workstation certificate from the API, once an issuing CA can sign (its first CRL).
    async fn new(service: &Service) -> Self {
        let (key, csr) = request();
        let body = json!({
            "workstation": "line-633-ws",
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &csr),
        });
        let issued = wait_for("a workstation certificate", || async {
            let reply = service
                .post_json(CERTIFICATES_PATH, Some(&service.admin_token()), &body)
                .await;
            (reply.status == 201).then_some(reply)
        })
        .await;
        let issued: WorkstationCertificate = serde_json::from_str(&issued.body).expect("issued");
        Self {
            key,
            certificate: pem::decode_all(&issued.certificate, pem::CERTIFICATE)
                .expect("PEM")
                .remove(0),
        }
    }

    /// Registers `hardware_id` with a new TPM key; returns that key and the registration.
    async fn register(
        &self,
        service: &Service,
        hardware_id: &str,
    ) -> (KeyPair, GatewayRegistration) {
        let (key, csr) = request();
        let endorsement = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("an EK");
        let body = json!({
            "hardware_id": hardware_id,
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &csr),
            "endorsement_key": pem::encode(pem::PUBLIC_KEY, &endorsement.subject_public_key_info()),
        })
        .to_string()
        .into_bytes();
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).expect("random");
        let nonce = URL_SAFE_NO_PAD.encode(nonce);
        let timestamp = now();
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let input =
            workstation_signing_input("POST", REGISTRATIONS_PATH, timestamp, &nonce, &digest);
        let signature = self.key.sign(&input).expect("sign the request");
        let response = reqwest::Client::new()
            .post(format!("{}{REGISTRATIONS_PATH}", service.base_url))
            .timeout(LIMIT)
            .header("content-type", "application/json")
            .header(contract::INTERNAL_IDENTITY_HEADER, service.admin_token())
            .header(
                WORKSTATION_CERTIFICATE_HEADER,
                STANDARD.encode(&self.certificate),
            )
            .header(WORKSTATION_TIMESTAMP_HEADER, timestamp.to_string())
            .header(WORKSTATION_NONCE_HEADER, nonce)
            .header(WORKSTATION_SIGNATURE_HEADER, STANDARD.encode(signature))
            .body(body)
            .send()
            .await
            .expect("call the certs service");
        let status = response.status().as_u16();
        let text = response.text().await.expect("read the body");
        assert_eq!(status, 201, "register {hardware_id}: {text}");
        (key, serde_json::from_str(&text).expect("a registration"))
    }
}

/// The assets service's role as its binding uses it: a tenant transaction per binding.
fn assets_writer(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("assets_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("the assets service role's pool")
    .1
}

/// Writes `assets.gateway_bound` as the assets service's binding does.
async fn bind(assets: &BackgroundTenantDb, tenant: TenantId, hardware_id: &str) {
    let binding_id = GatewayBindingId::generate();
    let event = GatewayBound {
        binding_id,
        hardware_id: gateway_id(hardware_id),
    };
    let mut tx = assets
        .begin(tenant, "test-bind")
        .await
        .expect("a tenant transaction of the assets service");
    let aggregate =
        Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding_id.to_string()).expect("an aggregate");
    outbox::publish(tx.transaction(), &aggregate, &event, Some(tenant))
        .await
        .expect("write the event");
    tx.commit().await.expect("commit the binding");
}

/// The service's INFO lines of changes for `hardware_id`, as `action` words.
fn changes(service: &Service, hardware_id: &str) -> Vec<String> {
    service
        .stdout
        .messages(CHANGED)
        .iter()
        .filter(|line| line["fields"]["hardware_id"] == hardware_id)
        .map(|line| {
            line["fields"]["action"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// Renews `hardware_id`'s certificate as its gateway does: a challenge, then the challenge
/// signed with its TPM key `key`. `None` while the service answers 503 (the bindings' lag is
/// not known yet after a start) or 429 (these retries used up the hardware ID's limit).
async fn renew(service: &Service, key: &KeyPair, hardware_id: &str) -> Option<RenewedCertificate> {
    let reply = renewal_answer(service, key, hardware_id).await?;
    assert_eq!(reply.status, 201, "renew {hardware_id}: {}", reply.body);
    Some(serde_json::from_str(&reply.body).expect("a renewed certificate"))
}

/// The answer to [`renew`]'s second step, whatever it is; `None` on 503 or 429 as there.
async fn renewal_answer(service: &Service, key: &KeyPair, hardware_id: &str) -> Option<Reply> {
    let wait = |status: u16| status == 503 || status == 429;
    let reply = service
        .post_json(
            RENEWAL_PATH,
            None,
            &json!({ "step": "challenge", "hardware_id": hardware_id }),
        )
        .await;
    if wait(reply.status) {
        return None;
    }
    assert_eq!(
        reply.status, 200,
        "a challenge for {hardware_id}: {}",
        reply.body
    );
    let challenge: Challenge = serde_json::from_str(&reply.body).expect("a challenge");
    let signature = key
        .sign(&challenge.signing_input())
        .expect("sign the challenge");
    let reply = service
        .post_json(
            RENEWAL_PATH,
            None,
            &json!({
                "step": "renew",
                "hardware_id": hardware_id,
                "nonce": challenge.nonce,
                "signature": STANDARD.encode(signature),
            }),
        )
        .await;
    (!wait(reply.status)).then_some(reply)
}

/// How many gateway certificates the ledger has for `hardware_id`, read as the superuser.
async fn gateway_certificates(product: &ProductPostgres, hardware_id: &str) -> i64 {
    let mut admin = product.superuser().await;
    sqlx::query_scalar(
        "SELECT count(*) FROM certs.issued_certificates
         WHERE profile = 'gateway' AND subject = $1",
    )
    .bind(hardware_id)
    .fetch_one(&mut admin)
    .await
    .expect("count the gateway's certificates")
}

/// Where the first JSON line of `output` with `message` and these `fields` is.
fn position(output: &str, message: &str, fields: &[(&str, &str)]) -> Option<usize> {
    output.lines().position(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|line| {
            line["message"] == message
                && fields
                    .iter()
                    .all(|(key, value)| line["fields"][*key] == *value)
        })
    })
}

/// The broker as the chart runs it, before any gateway: the chart's configuration and gateway
/// listener, the dynamic-security file the accounts init container writes, the trust files the
/// trust containers install, and the broker password file the chart mounts for certs.
struct Stack {
    broker: Broker,
    /// Each service account's broker password.
    passwords: BTreeMap<&'static str, String>,
    /// The dynamic-security file with the chart's entries only.
    chart_only: String,
    /// The CA certificate the gateways trust for the broker's server certificate (DER).
    server_ca: Vec<u8>,
    /// The CA chain the broker trusts for gateways (`ca.pem`).
    chain: String,
    /// certs' broker password file.
    password_path: String,
}

impl Stack {
    async fn start(runtime: &'static str, ca: &TestCa) -> Self {
        let authority = ca.authority();

        // The broker's accounts, as the init container makes them: a password per service, hashed
        // into the dynamic-security file next to the chart's roles.
        let passwords: BTreeMap<&str, String> = SERVICES
            .iter()
            .map(|access| (access.service, random_text(32)))
            .collect();
        let accounts: Vec<_> = SERVICES
            .iter()
            .map(|access| {
                let mut salt = [0_u8; 64];
                getrandom::fill(&mut salt).expect("random");
                let encoded = encode_password(
                    passwords[access.service].as_bytes(),
                    &salt,
                    PASSWORD_ITERATIONS,
                );
                (access, encoded)
            })
            .collect();
        let chart_only = reconcile(None, &accounts).expect("the init container's file");

        // The trust files the broker's trust containers install: the CA chain and the issuing CA's
        // CRL, made by the service's own code.
        let chain = format!("{}{}", ca.root_pem, ca.issuing_pems.concat());
        let crl = certs::crl::build(
            &authority.issuing()[0],
            1,
            &[],
            now() - 300,
            Duration::from_secs(86_400),
        )
        .expect("a CRL");
        let (server_chain, server_key, server_ca) = server_certificate();

        let broker = Broker::start(runtime);
        broker.push(&format!("{STAGE_DIR}/mosquitto.conf"), CONFIG.as_bytes());
        broker.push(
            &format!("{LISTENERS_DIR}/{GATEWAY_LISTENER_FILE}"),
            GATEWAY_LISTENER.as_bytes(),
        );
        broker.push(&format!("{TLS_DIR}/tls.crt"), server_chain.as_bytes());
        broker.push(&format!("{TLS_DIR}/tls.key"), server_key.as_bytes());
        broker.push(&format!("{TRUST_DIR}/ca.pem"), chain.as_bytes());
        broker.push(
            &format!("{TRUST_DIR}/crl.pem"),
            pem::encode(pem::CRL, &crl).as_bytes(),
        );
        broker.push(
            &format!("{STAGE_DIR}/dynamic-security.json"),
            chart_only.as_bytes(),
        );
        broker.push(&format!("{STAGE_DIR}/ready"), b"");
        broker.wait_running(1).await;
        evidence(format!(
            "broker {MOSQUITTO_IMAGE} with the chart's mosquitto.conf and gateway listener, \
             dynamic-security file from migrate::mqtt_accounts::reconcile ({} clients, {} roles)",
            clients(&broker.dynsec()).len(),
            roles_and_groups(&broker.dynsec()).0.len()
        ));

        // The broker account the chart would mount for certs.
        let password_file = temp_dir("certs-mqtt").join("certs");
        std::fs::write(&password_file, format!("{}\n", passwords["certs"])).expect("password file");
        Self {
            broker,
            passwords,
            chart_only,
            server_ca,
            chain,
            password_path: password_file.display().to_string(),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_service_keeps_the_brokers_gateway_clients_in_line_with_postgres() {
    // The tests share the database and the job locks: one at a time.
    let _one_at_a_time = ONE_AT_A_TIME.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let ca = TestCa::new(&[1]);
    let Stack {
        broker,
        passwords,
        chart_only,
        server_ca,
        chain: _,
        password_path,
    } = Stack::start(runtime, &ca).await;
    let service_port = broker.service_port.to_string();
    let mut env: Vec<(&str, &str)> = FAST_CRLS.to_vec();
    env.extend([
        ("TRUSTIOT__TELEMETRY__LOG_FILTER", "trace"),
        ("TRUSTIOT__MQTT__HOST", "127.0.0.1"),
        ("TRUSTIOT__MQTT__PORT", service_port.as_str()),
        ("TRUSTIOT__MQTT__USERNAME", "service:certs"),
        ("TRUSTIOT__MQTT__PASSWORD_FILE", password_path.as_str()),
        ("TRUSTIOT__DYNSEC__INTERVAL", "2s"),
    ]);
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the first whole comparison", || async {
        (!service.stdout.messages(IN_SYNC).is_empty()).then_some(())
    })
    .await;
    evidence(
        "certs connected as service:certs and compared: none of this test's gateways registered",
    );

    // Factory registration of two gateways: certificates from the service.
    let workstation = Workstation::new(&service).await;
    let (key_a, registration_a) = workstation.register(&service, GATEWAY_A).await;
    let (key_b, registration_b) = workstation.register(&service, GATEWAY_B).await;
    let config = |key: &KeyPair, registration: &GatewayRegistration| {
        let chain = pem::decode_all(&registration.certificate_chain, pem::CERTIFICATE)
            .expect("the chain")
            .into_iter()
            .map(CertificateDer::from)
            .collect();
        client_config(&server_ca, chain, key)
    };
    let (config_a, config_b) = (
        config(&key_a, &registration_a),
        config(&key_b, &registration_b),
    );
    let gateway_address = SocketAddr::from(([127, 0, 0, 1], broker.gateway_port));
    let (a, b) = (gateway_id(GATEWAY_A), gateway_id(GATEWAY_B));

    // Acceptance 2: registered, not bound: connects, may publish nothing.
    let mut gateway_a = Gateway::connect(gateway_address, &config_a, GATEWAY_A);
    let unbound = gateway_a.publish(&Channel::Status.topic(&a));
    assert_eq!(unbound, NOT_AUTHORIZED, "an unbound gateway's publish");
    evidence(format!(
        "{GATEWAY_A} registered (serial {}), not bound: CONNACK 0x00, PUBACK {unbound:#04x} on {}",
        registration_a.serial,
        Channel::Status.topic(&a)
    ));

    let assets = assets_writer(&product);
    let tenant = TenantId::generate();
    let bound_at = Instant::now();
    bind(&assets, tenant, GATEWAY_A).await;
    let mut attempts = 0_u32;
    let code = loop {
        attempts += 1;
        let code = gateway_a.publish(&Channel::Status.topic(&a));
        if accepted(code) {
            break code;
        }
        assert_eq!(code, NOT_AUTHORIZED);
        assert!(
            bound_at.elapsed() < LIMIT,
            "{GATEWAY_A} could not publish within {LIMIT:?} of its binding"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(changes(&service, GATEWAY_A), ["added"]);
    evidence(format!(
        "{GATEWAY_A} bound: the same connection's publish {attempts} got PUBACK {code:#04x}; \
         service logged `{CHANGED}` hardware_id={GATEWAY_A} action=added"
    ));

    // Acceptance 1: both bound; each reaches its own tree only.
    bind(&assets, tenant, GATEWAY_B).await;
    wait_for("the binding of the second gateway", || async {
        (!changes(&service, GATEWAY_B).is_empty()).then_some(())
    })
    .await;
    let own_up = gateway_a.publish(&Channel::Telemetry.topic(&a));
    let own_down = gateway_a.subscribe(&Channel::Config.topic(&a));
    let other_up = gateway_a.publish(&Channel::Telemetry.topic(&b));
    let other_down = gateway_a.subscribe(&Channel::Config.topic(&b));
    assert!(accepted(own_up), "own up: {own_up:#04x}");
    assert_eq!(own_down, GRANTED_QOS_1, "own down");
    assert_eq!(other_up, NOT_AUTHORIZED, "another gateway's up");
    assert_eq!(other_down, NOT_AUTHORIZED, "another gateway's down");
    let mut gateway_b = Gateway::connect(gateway_address, &config_b, GATEWAY_B);
    let b_own = gateway_b.publish(&Channel::Status.topic(&b));
    let b_other = gateway_b.publish(&Channel::Status.topic(&a));
    assert!(accepted(b_own), "{GATEWAY_B} own up: {b_own:#04x}");
    assert_eq!(b_other, NOT_AUTHORIZED);
    evidence(format!(
        "{GATEWAY_A} bound: PUBLISH own up/telemetry -> {own_up:#04x}, SUBSCRIBE own \
         down/config -> {own_down:#04x}, PUBLISH {GATEWAY_B} up/telemetry -> {other_up:#04x}, \
         SUBSCRIBE {GATEWAY_B} down/config -> {other_down:#04x}; {GATEWAY_B}: own up/status -> \
         {b_own:#04x}, {GATEWAY_A} up/status -> {b_other:#04x}"
    ));
    drop(gateway_b);

    // Acceptance 4: whole comparisons with nothing to change send nothing.
    let file_before = broker.dynsec_text();
    let changed_before = service.stdout.messages(CHANGED).len();
    let rounds_before = service.stdout.messages(IN_SYNC).len();
    wait_for("three more whole comparisons", || async {
        (service.stdout.messages(IN_SYNC).len() >= rounds_before + 3).then_some(())
    })
    .await;
    let file_after = broker.dynsec_text();
    assert_eq!(file_before, file_after, "the dynamic-security file changed");
    assert_eq!(service.stdout.messages(CHANGED).len(), changed_before);
    evidence(format!(
        "no change: {} whole comparisons (`{IN_SYNC}`), no `{CHANGED}` line, \
         dynamic-security file sha256 {} before and after",
        service.stdout.messages(IN_SYNC).len() - rounds_before,
        hex(&Sha256::digest(file_after.as_bytes()))
    ));
    // The connection is still the one from before the binding.
    let still = gateway_a.publish(&Channel::Status.topic(&a));
    assert!(accepted(still));
    drop(gateway_a);

    // Acceptance 3: the broker restarts with the chart's entries only, and a gateway client
    // that should not exist.
    let mut reset: Value = serde_json::from_str(&chart_only).expect("JSON");
    reset["clients"]
        .as_array_mut()
        .expect("clients")
        .push(json!({ "username": GHOST, "roles": [{ "rolename": GATEWAY_ROLE, "priority": 0 }] }));
    let reset_text = reset.to_string();
    broker.push(
        &format!("{STAGE_DIR}/dynamic-security.json"),
        reset_text.as_bytes(),
    );
    broker.restart();
    broker.wait_running(2).await;
    let restarted = broker.dynsec();
    let in_line = wait_for("the gateway clients after the restart", || async {
        let dynsec = broker.dynsec();
        let clients = clients(&dynsec);
        let gateway = |name: &str| {
            clients
                .get(name)
                .is_some_and(|client| client["roles"] == json!([GATEWAY_ROLE]))
        };
        (gateway(GATEWAY_A) && gateway(GATEWAY_B) && !clients.contains_key(GHOST)).then_some(dynsec)
    })
    .await;
    assert_eq!(changes(&service, GHOST), ["removed"]);
    assert_eq!(changes(&service, GATEWAY_A), ["added", "added"]);
    assert_eq!(changes(&service, GATEWAY_B), ["added", "added"]);
    // The service accounts, the chart's roles and the groups are what the init container wrote.
    let written = serde_json::from_str::<Value>(&reset_text).expect("JSON");
    assert_eq!(service_accounts(&in_line), service_accounts(&written));
    assert_eq!(roles_and_groups(&in_line), roles_and_groups(&written));
    let gateway_clients: Vec<String> = clients(&in_line)
        .into_keys()
        // The other test's gateways share the database when it ran first.
        .filter(|name| !name.starts_with("service:") && !name.starts_with("gw-636-"))
        .collect();
    assert_eq!(gateway_clients, [GATEWAY_A, GATEWAY_B]);
    evidence(format!(
        "after the restart with the chart's entries and {GHOST}: gateway clients {gateway_clients:?} \
         (each with {GATEWAY_ROLE} only), {GHOST} removed; service accounts {:?} and {} roles \
         unchanged; file read right after the restart had {} clients",
        service_accounts(&in_line).keys().collect::<Vec<_>>(),
        roles_and_groups(&in_line).0.len(),
        clients(&restarted).len()
    ));
    let mut again = Gateway::connect(gateway_address, &config_a, GATEWAY_A);
    let after_restart = again.publish(&Channel::Status.topic(&a));
    assert!(accepted(after_restart));
    evidence(format!(
        "{GATEWAY_A} after the restart: PUBACK {after_restart:#04x}"
    ));
    drop(again);

    // Acceptance 5: no piece of the broker password in the service's output at trace.
    let output = service.output();
    let password: Vec<char> = passwords["certs"].chars().collect();
    let pieces: Vec<String> = password
        .windows(8)
        .map(|piece| piece.iter().collect())
        .collect();
    for piece in &pieces {
        assert!(
            !output.contains(piece.as_str()),
            "the service's output holds a piece of the password"
        );
    }
    let pieces = pieces.len();
    evidence(format!(
        "{} bytes of service output at trace: none of the {pieces} 8-character pieces of the \
         broker password",
        output.len()
    ));

    // Laysi/trustiot-v2#81: a renewal asks for its gateway's comparison once its transaction has
    // committed. The service starts again with whole comparisons an hour apart, so after the one
    // on connecting, only that request can bring a client back within the test.
    drop(service);
    let mut admin = product.superuser().await;
    // Superseded: a keyCompromise revocation would end the gateway's renewals.
    sqlx::query(
        "UPDATE certs.issued_certificates SET revoked_at = now(), revocation_reason = 4
         WHERE profile = 'gateway' AND subject = $1",
    )
    .bind(GATEWAY_A)
    .execute(&mut admin)
    .await
    .expect("revoke the first gateway's only certificate");
    let mut env: Vec<(&str, &str)> = FAST_CRLS.to_vec();
    env.extend([
        ("TRUSTIOT__MQTT__HOST", "127.0.0.1"),
        ("TRUSTIOT__MQTT__PORT", service_port.as_str()),
        ("TRUSTIOT__MQTT__USERNAME", "service:certs"),
        ("TRUSTIOT__MQTT__PASSWORD_FILE", password_path.as_str()),
        ("TRUSTIOT__DYNSEC__INTERVAL", "1h"),
        // The renewal retries until the bindings' lag is known; a limit it reaches only delays it.
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GATEWAY_INTERVAL", "1s"),
    ]);
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the whole comparison on connecting", || async {
        (changes(&service, GATEWAY_A) == ["removed"]
            && !service.stdout.messages(IN_LINE).is_empty())
        .then_some(())
    })
    .await;
    assert!(changes(&service, GATEWAY_B).is_empty());
    assert_eq!(service.stdout.messages(IN_LINE).len(), 1);
    // The broker's CRL in this test lists nothing, so the revoked certificate still connects.
    let mut revoked = Gateway::connect(gateway_address, &config_a, GATEWAY_A);
    let without_client = revoked.publish(&Channel::Status.topic(&a));
    assert_eq!(without_client, NOT_AUTHORIZED, "a gateway without a client");
    drop(revoked);
    evidence(format!(
        "{GATEWAY_A}'s only certificate revoked; the service restarted with dynsec.interval 1h: \
         its comparison on connecting logged `{CHANGED}` hardware_id={GATEWAY_A} action=removed \
         and one `{IN_LINE}`; the old certificate connects, PUBACK {without_client:#04x}"
    ));

    let renewed = wait_for("the renewal of the first gateway", || async {
        renew(&service, &key_a, GATEWAY_A).await
    })
    .await;
    wait_for("the renewed gateway's broker client", || async {
        (changes(&service, GATEWAY_A) == ["removed", "added"]).then_some(())
    })
    .await;
    // No whole comparison changed anything since the one on connecting: the client came from
    // the renewal's request, which follows its commit and its log line.
    assert_eq!(
        service.stdout.messages(IN_LINE).len(),
        1,
        "a whole comparison ran after the start"
    );
    let output = service.stdout.text();
    let renewed_at =
        position(&output, RENEWED, &[("hardware_id", GATEWAY_A)]).expect("the renewal's log line");
    let added_at = position(
        &output,
        CHANGED,
        &[("hardware_id", GATEWAY_A), ("action", "added")],
    )
    .expect("the broker client's log line");
    assert!(
        renewed_at < added_at,
        "the broker client was added before the renewal committed"
    );
    let chain = pem::decode_all(&renewed.certificate_chain, pem::CERTIFICATE)
        .expect("the renewed chain")
        .into_iter()
        .map(CertificateDer::from)
        .collect();
    let mut gateway = Gateway::connect(
        gateway_address,
        &client_config(&server_ca, chain, &key_a),
        GATEWAY_A,
    );
    let published = gateway.publish(&Channel::Status.topic(&a));
    assert!(
        accepted(published),
        "the renewed certificate's publish: {published:#04x}"
    );
    drop(gateway);
    evidence(format!(
        "{GATEWAY_A} renewed (serial {}): `{RENEWED}` at stdout line {renewed_at}, then \
         `{CHANGED}` action=added at line {added_at}, still one `{IN_LINE}` (no whole \
         comparison in between); the renewed certificate's publish: PUBACK {published:#04x}",
        renewed.serial
    ));
}

/// Whether one of the CRLs in `bundle` (PEM) lists `serial` (hex).
fn crl_lists(bundle: &str, serial: &str) -> bool {
    pem::decode_all(bundle, pem::CRL)
        .expect("PEM CRLs")
        .iter()
        .any(|der| {
            let (_, crl) = CertificateRevocationList::from_der(der).expect("a CRL");
            crl.iter_revoked_certificates()
                .any(|revoked| hex(revoked.raw_serial()) == serial)
        })
}

/// What a connected gateway saw after a dynamic-security command: two publishes, so a
/// disconnection the broker makes at the end of its loop turn is seen too.
fn after_command(session: &mut Option<Gateway>, topic: &str) -> String {
    let Some(gateway) = session.as_mut() else {
        return "no connection".to_owned();
    };
    let outcome = gateway
        .try_publish(topic)
        .and_then(|first| Ok((first, gateway.try_publish(topic)?)));
    match outcome {
        Ok((first, second)) => format!("connected, PUBACK {first:#04x} then {second:#04x}"),
        Err(how) => {
            *session = None;
            format!("disconnected ({how})")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_gateway_disconnects_it_and_its_crl_then_refuses_it() {
    // The tests share the database and the job locks: one at a time.
    let _one_at_a_time = ONE_AT_A_TIME.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let ca = TestCa::new(&[1]);
    let Stack {
        broker,
        passwords,
        chart_only: _,
        server_ca,
        chain,
        password_path,
    } = Stack::start(runtime, &ca).await;
    let service_port = broker.service_port.to_string();
    let gateway_address = SocketAddr::from(([127, 0, 0, 1], broker.gateway_port));
    let mqtt = [
        ("TRUSTIOT__MQTT__HOST", "127.0.0.1"),
        ("TRUSTIOT__MQTT__PORT", service_port.as_str()),
        ("TRUSTIOT__MQTT__USERNAME", "service:certs"),
        ("TRUSTIOT__MQTT__PASSWORD_FILE", password_path.as_str()),
    ];

    // Two gateways registered and bound, with their broker clients, by a first service.
    let mut env: Vec<(&str, &str)> = FAST_CRLS.to_vec();
    env.extend(mqtt);
    env.push(("TRUSTIOT__DYNSEC__INTERVAL", "2s"));
    let setup = Service::start(&product, &ca, &env).await;
    let workstation = Workstation::new(&setup).await;
    let (key_r, registration_r) = workstation.register(&setup, REVOKED_GATEWAY).await;
    let (key_p, registration_p) = workstation.register(&setup, PROBE).await;
    let assets = assets_writer(&product);
    let tenant = TenantId::generate();
    bind(&assets, tenant, REVOKED_GATEWAY).await;
    bind(&assets, tenant, PROBE).await;
    wait_for("both gateways' broker clients", || async {
        (changes(&setup, REVOKED_GATEWAY) == ["added"] && changes(&setup, PROBE) == ["added"])
            .then_some(())
    })
    .await;
    drop(setup);
    let config = |key: &KeyPair, chain_pem: &str| {
        let chain = pem::decode_all(chain_pem, pem::CERTIFICATE)
            .expect("the chain")
            .into_iter()
            .map(CertificateDer::from)
            .collect();
        client_config(&server_ca, chain, key)
    };
    let config_r = config(&key_r, &registration_r.certificate_chain);
    let config_p = config(&key_p, &registration_p.certificate_chain);
    let (r, p) = (gateway_id(REVOKED_GATEWAY), gateway_id(PROBE));
    let (topic_r, topic_p) = (Channel::Status.topic(&r), Channel::Status.topic(&p));

    // Which dynamic-security commands end a connected gateway's session (Mosquitto keeps a
    // session when only its permissions change). No service runs, so nothing puts the client
    // back in between.
    let password = passwords["certs"].as_str();
    let mut session = Some(Gateway::connect(gateway_address, &config_p, PROBE));
    let mut observed = BTreeMap::new();
    for args in [
        &["removeClientRole", PROBE, GATEWAY_ROLE][..],
        &["addClientRole", PROBE, GATEWAY_ROLE, "0"],
        &["disableClient", PROBE],
        &["enableClient", PROBE],
        &["deleteClient", PROBE],
    ] {
        if session.is_none() {
            session = Gateway::try_connect(gateway_address, &config_p, PROBE).ok();
        }
        let before = after_command(&mut session, &topic_p);
        broker.dynsec_command(password, args);
        let seen = after_command(&mut session, &topic_p);
        evidence(format!(
            "mosquitto_ctrl dynsec {}: {PROBE} before: {before}; after: {seen}",
            args.join(" ")
        ));
        observed.insert(args[0], (before, seen));
    }
    // Mosquitto 2.1 ends the session on every change to the client itself (DISCONNECT 0x98,
    // administrative action), not only on its deletion; enabling it again changes nothing for a
    // connection. A disabled client still connects with its certificate and publishes: dynsec
    // disables password logins only, so disabling is no way to keep a gateway out.
    for command in [
        "removeClientRole",
        "addClientRole",
        "disableClient",
        "deleteClient",
    ] {
        let (before, after) = &observed[command];
        assert!(before.starts_with("connected"), "{command}: {observed:?}");
        assert!(
            after.starts_with("disconnected (DISCONNECT reason 0x98)"),
            "{command}: {observed:?}"
        );
    }
    let (while_disabled, enabled) = &observed["enableClient"];
    assert!(
        while_disabled.starts_with("connected") && !while_disabled.contains("0x87"),
        "a disabled client's new connection: {observed:?}"
    );
    assert!(enabled.starts_with("connected"), "{observed:?}");
    drop(session);

    // Two replicas with whole comparisons an hour apart: after the one on connecting, only a
    // request's own sync changes the broker.
    let mut env: Vec<(&str, &str)> = FAST_CRLS.to_vec();
    env.extend(mqtt);
    env.extend([
        // A whole comparison without changes logs at DEBUG only.
        ("TRUSTIOT__TELEMETRY__LOG_FILTER", "debug"),
        ("TRUSTIOT__DYNSEC__INTERVAL", "1h"),
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GATEWAY_INTERVAL", "1s"),
    ]);
    let first = Service::start(&product, &ca, &env).await;
    let second = Service::start(&product, &ca, &env).await;
    let whole = |service: &Service| {
        service.stdout.messages(IN_SYNC).len() + service.stdout.messages(IN_LINE).len()
    };
    wait_for("both replicas' comparison on connecting", || async {
        (whole(&first) == 1 && whole(&second) == 1).then_some(())
    })
    .await;
    wait_for("the probe's broker client again", || async {
        clients(&broker.dynsec()).contains_key(PROBE).then_some(())
    })
    .await;

    // Acceptance 1: connected, then revoked through the replica that is not the first.
    let mut connected = Gateway::connect(gateway_address, &config_r, REVOKED_GATEWAY);
    let before = connected.publish(&topic_r);
    assert!(accepted(before), "{REVOKED_GATEWAY} before: {before:#04x}");
    let started = Instant::now();
    let watcher = tokio::task::spawn_blocking(move || {
        let how = connected.wait_closed();
        (started.elapsed(), how)
    });
    let path = REVOKE_PATH.replace("{hardware_id}", REVOKED_GATEWAY);
    let reply = second
        .post_json(&path, Some(&second.admin_token()), &json!({}))
        .await;
    let answered = started.elapsed();
    assert_eq!(reply.status, 200, "revoke: {}", reply.body);
    let revoked = reply.json();
    assert_eq!(revoked["revoked"].as_array().map(Vec::len), Some(1));
    assert_eq!(revoked["revoked"][0]["serial"], registration_r.serial);
    let (closed_after, how) = watcher.await.expect("the connection watcher");
    assert!(
        how.starts_with("DISCONNECT reason 0x98"),
        "how the connection ended: {how}"
    );
    assert!(
        closed_after < DISCONNECT_WITHIN,
        "N: the connection ended {closed_after:?} after the request, README says {DISCONNECT_WITHIN:?}"
    );
    evidence(format!(
        "{REVOKED_GATEWAY} revoked through the second replica: 200 after {answered:?}; its open \
         connection ended {closed_after:?} after the request ({how})"
    ));
    wait_for("the second replica's removal", || async {
        (changes(&second, REVOKED_GATEWAY) == ["removed"]).then_some(())
    })
    .await;
    assert!(changes(&first, REVOKED_GATEWAY).is_empty());
    assert_eq!(
        whole(&first),
        1,
        "a whole comparison ran on the first replica"
    );
    assert_eq!(
        whole(&second),
        1,
        "a whole comparison ran on the second replica"
    );
    let output = second.stdout.text();
    let revoked_at = position(
        &output,
        "revoked a gateway's certificates",
        &[("hardware_id", REVOKED_GATEWAY)],
    )
    .expect("the revocation's line");
    let crl_at = position(&output, "issued a new CRL", &[("cause", "revocation")])
        .expect("the new CRL's line");
    let removed_at = position(
        &output,
        CHANGED,
        &[("hardware_id", REVOKED_GATEWAY), ("action", "removed")],
    )
    .expect("the removal's line");
    assert!(
        revoked_at < crl_at && crl_at < removed_at,
        "the steps' order: revoked {revoked_at}, CRL {crl_at}, removed {removed_at}"
    );
    let (status, bundle) = second.get(CRL_PATH).await;
    assert_eq!(status, 200);
    assert!(crl_lists(&bundle, &registration_r.serial));
    assert!(!clients(&broker.dynsec()).contains_key(REVOKED_GATEWAY));
    evidence(format!(
        "second replica's stdout: `revoked a gateway's certificates` line {revoked_at}, `issued a \
         new CRL` cause=revocation line {crl_at}, `{CHANGED}` action=removed line {removed_at}; \
         the first replica changed nothing; one whole comparison each; {CRL_PATH} lists serial {} \
         when the answer arrives",
        registration_r.serial
    ));

    // Acceptance 2: before the broker loads the new CRL, the handshake passes, with nothing
    // allowed; after it, the handshake fails.
    let mut early = Gateway::connect(gateway_address, &config_r, REVOKED_GATEWAY);
    let early_code = early.publish(&topic_r);
    assert_eq!(early_code, NOT_AUTHORIZED, "before the new CRL");
    drop(early);
    let trust = migrate::mqtt_trust::validate(&chain, &bundle).expect("the sidecar's check");
    broker.push(&format!("{TRUST_DIR}/crl.pem"), trust.crl_pem.as_bytes());
    broker.reload().await;
    let refused = Gateway::try_connect(gateway_address, &config_r, REVOKED_GATEWAY)
        .err()
        .expect("the revoked certificate after the reload");
    assert!(refused.contains("CertificateRevoked"), "{refused}");
    let mut probe = Gateway::connect(gateway_address, &config_p, PROBE);
    let probe_code = probe.publish(&topic_p);
    assert!(
        accepted(probe_code),
        "{PROBE} after the reload: {probe_code:#04x}"
    );
    drop(probe);
    evidence(format!(
        "{REVOKED_GATEWAY} before the reload: CONNACK 0x00, PUBACK {early_code:#04x}; after the \
         CRL from {CRL_PATH} passed migrate::mqtt_trust::validate and the broker's SIGHUP \
         reload: {refused}; {PROBE} still publishes ({probe_code:#04x})"
    ));

    // Acceptance 3: a new certificate (renewal) brings the client back.
    let renewed = wait_for("the renewal", || async {
        renew(&second, &key_r, REVOKED_GATEWAY).await
    })
    .await;
    wait_for("the renewed gateway's broker client", || async {
        (changes(&second, REVOKED_GATEWAY) == ["removed", "added"]).then_some(())
    })
    .await;
    let mut gateway = Gateway::connect(
        gateway_address,
        &config(&key_r, &renewed.certificate_chain),
        REVOKED_GATEWAY,
    );
    let published = gateway.publish(&topic_r);
    assert!(accepted(published), "after the renewal: {published:#04x}");
    drop(gateway);
    evidence(format!(
        "{REVOKED_GATEWAY} renewed (serial {}): `{CHANGED}` action=added on the second replica, \
         the new certificate connects and publishes ({published:#04x})",
        renewed.serial
    ));

    // A keyCompromise revocation ends the gateway's renewals: its registered key is not trusted.
    let mut connected = Gateway::connect(
        gateway_address,
        &config(&key_r, &renewed.certificate_chain),
        REVOKED_GATEWAY,
    );
    let reply = second
        .post_json(
            &path,
            Some(&second.admin_token()),
            &json!({ "reason": "key_compromise" }),
        )
        .await;
    assert_eq!(
        reply.status, 200,
        "revoke for keyCompromise: {}",
        reply.body
    );
    assert_eq!(reply.json()["revoked"][0]["serial"], renewed.serial);
    let how = tokio::task::spawn_blocking(move || connected.wait_closed())
        .await
        .expect("the connection watcher");
    let certificates = gateway_certificates(&product, REVOKED_GATEWAY).await;
    let refused = wait_for("the renewal's answer", || async {
        renewal_answer(&second, &key_r, REVOKED_GATEWAY).await
    })
    .await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(
        refused.json()["message_key"],
        "certificate_renewal.key_compromised"
    );
    assert_eq!(
        gateway_certificates(&product, REVOKED_GATEWAY).await,
        certificates,
        "a certificate was signed"
    );
    assert_eq!(
        changes(&second, REVOKED_GATEWAY),
        ["removed", "added", "removed"]
    );
    assert!(!clients(&broker.dynsec()).contains_key(REVOKED_GATEWAY));
    evidence(format!(
        "{REVOKED_GATEWAY} revoked again for key_compromise: disconnected ({how}); the renewal \
         with its registered key: {} {}; {certificates} certificates in the ledger before and \
         after, no broker client",
        refused.status, refused.body
    ));

    // A keyCompromise revocation that commits while a renewal is past its read still ends it:
    // the renewal waits for the gateway's lock (held here, as a revocation holds it), then reads
    // again in its signing transaction.
    let mut holder = product.superuser().await;
    let mut held = holder.begin().await.expect("a transaction");
    sqlx::query(LOCK_GATEWAY_CERTIFICATES)
        .bind(PROBE)
        .execute(&mut *held)
        .await
        .expect("take the probe's lock");
    let before = gateway_certificates(&product, PROBE).await;
    let revoke_while_waiting = async {
        wait_for("the renewal waiting for the probe's lock", || async {
            let mut watcher = product.superuser().await;
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks
                 WHERE locktype = 'advisory' AND classid = 1735881334 AND NOT granted",
            )
            .fetch_one(&mut watcher)
            .await
            .expect("read the locks");
            (waiting > 0).then_some(())
        })
        .await;
        sqlx::query(
            "UPDATE certs.issued_certificates SET revoked_at = now(), revocation_reason = 1
             WHERE profile = 'gateway' AND subject = $1 AND revoked_at IS NULL",
        )
        .bind(PROBE)
        .execute(&mut *held)
        .await
        .expect("revoke the probe for keyCompromise");
        held.commit().await.expect("commit the revocation");
    };
    let (answer, ()) = tokio::join!(renewal_answer(&second, &key_p, PROBE), revoke_while_waiting);
    let answer = answer.expect("an answer, not 503 or 429");
    assert_eq!(answer.status, 403, "{}", answer.body);
    assert_eq!(
        answer.json()["message_key"],
        "certificate_renewal.key_compromised"
    );
    assert_eq!(gateway_certificates(&product, PROBE).await, before);
    evidence(format!(
        "{PROBE}: a renewal that passed its read waited for the gateway's lock; a keyCompromise \
         revocation committed meanwhile; the renewal answered {} {}; {before} certificates before \
         and after",
        answer.status, answer.body
    ));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
