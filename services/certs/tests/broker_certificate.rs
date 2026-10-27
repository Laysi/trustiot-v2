//! The MQTT broker's server certificate end to end (Laysi/trustiot-v2#535; deploy/chart/README.md,
//! "MQTT broker", 「伺服器憑證」): the CronJob's issuer (`migrate mqtt-broker-certificate`,
//! tools/migrate/src/mqtt_certificate.rs) against this service's real broker port and a stand-in
//! for the Kubernetes Secret API, and Mosquitto serving what the Secret holds to a TLS client that
//! trusts the offline root alone, as a gateway does (README.md, "broker 伺服器憑證"):
//!
//! - The first run fills the Secret the chart declares empty, bound to its uid and recording the
//!   generation. Mosquitto, with the chart's gateway listener (MQTT 5, TLS 1.3), serves the
//!   certificate and its issuing CA: the client verifies it for
//!   `mqtt-broker.trustiot.internal` under the root and gets a CONNACK with a gateway certificate;
//!   it refuses the same broker under another name or another trust anchor, and a certificate for
//!   the broker's name from a CA that is not ours.
//! - Rotation, once, with the shortest validity the service allows (one hour): a run before half
//!   of the validity keeps the certificate and writes nothing; a run at half of the validity (the
//!   issuer's clock; the service signs at the real time) writes a new certificate with a new key.
//!   Mosquitto loads it on SIGHUP, as the trust sidecar sends it: a new connection verifies the new
//!   certificate, and the connection opened before the reload is still served (PINGRESP).
//! - Neither the service's log nor the broker's holds the private key.
//!
//! The issuer's own rules (a spoilt Secret, conflicts, a service that cannot sign) are tested in
//! tools/migrate/tests/mqtt_certificate.rs, the endpoint's in broker.rs, and the chart's Mosquitto
//! configuration with the trust sidecar in tools/migrate/tests/mqtt_broker.rs. This test starts a
//! throwaway Postgres with TLS and a Mosquitto container (podman or docker); it is skipped without
//! them unless `CI` is set. `-- --nocapture` prints the evidence lines of the test report.

mod support;

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use certs::http::{CHAIN_PATH, CRL_PATH};
use certs::issue::{self, BROKER_SERVER_NAME, CLOCK_SKEW_SECS};
use certs::pem;
use migrate::Report;
use migrate::config::MqttCertificateSettings;
use migrate::mqtt_certificate::{Current, GENERATION_ANNOTATION, Outcome, converge_at};
use migrate::passwords::UID_ANNOTATION;
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::{Value, json};
use support::{LIMIT, Service, TestCa, now, temp_dir, wait_for};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::product::product_postgres;

/// The release namespace and the Secret the chart declares for release `trustiot`.
const NAMESPACE: &str = "trustiot";
const TLS_SECRET: &str = "trustiot-mqtt-broker-tls";

/// The broker Service's name the chart passes to both the service and the issuer.
const SERVICE_NAME: &str = "trustiot-mqtt-broker.trustiot.svc";

/// The shortest `broker.validity` the service accepts (services/certs/src/config.rs).
const VALIDITY_SECS: i64 = 3600;

/// A CRL schedule that gives the issuing CA its first CRL within a second, valid for an hour.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// A name that is not in the broker's certificate.
const WRONG_NAME: &str = "wrong-name.trustiot.internal";

/// The gateways the clients present certificates of: one for the connection kept across the
/// reload, one for every other connection. The broker takes the client id from the certificate,
/// so a second connection with the same certificate would take over the first one's session.
const GATEWAY_KEPT: &str = "gw-535-kept";
const GATEWAY_PROBE: &str = "gw-535-probe";

// ---- a stand-in for the Kubernetes API: one Secret ----

/// What the stand-in keeps: the Secret as the API returns it, and how many patches it applied.
struct ApiState {
    secret: Value,
    version: u64,
    patches: usize,
}

/// Answers the two requests the issuer makes, `GET` and JSON merge `PATCH` of the Secret with a
/// `resourceVersion` precondition, over plain HTTP on 127.0.0.1 (which the issuer accepts for
/// tests only). A connection at a time, closed after each answer.
struct FakeApi {
    url: String,
    token: String,
    state: Arc<Mutex<ApiState>>,
}

impl FakeApi {
    /// Starts it with the Secret as the chart renders it: no data, no annotations.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake API");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("the fake API's address")
        );
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos();
        let token = format!("fake-api-{}-{nanos}", std::process::id());
        let state = Arc::new(Mutex::new(ApiState {
            secret: json!({
                "kind": "Secret",
                "apiVersion": "v1",
                "metadata": {
                    "name": TLS_SECRET,
                    "namespace": NAMESPACE,
                    "uid": format!("uid-{nanos}"),
                    "resourceVersion": "1",
                },
                "type": "Opaque",
            }),
            version: 1,
            patches: 0,
        }));
        let serving = Arc::clone(&state);
        let bearer = format!("Bearer {token}");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                let Some((head, body)) = read_request(&mut stream) else {
                    continue;
                };
                let (status, body) = answer(&serving, &bearer, &head, &body);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { url, token, state }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ApiState> {
        self.state.lock().expect("the fake API's state")
    }

    /// The issuer's settings for this stand-in and the service's broker port at `certs_url`.
    fn settings(&self, certs_url: &str) -> MqttCertificateSettings {
        let dir = temp_dir("broker-certificate-kube");
        let token_file = dir.join("token");
        std::fs::write(&token_file, format!("{}\n", self.token)).expect("write the token");
        MqttCertificateSettings {
            api_url: self.url.clone(),
            namespace: NAMESPACE.to_owned(),
            token_file,
            ca_file: dir.join("unused-ca.crt"),
            secret: TLS_SECRET.to_owned(),
            certs_url: certs_url.to_owned(),
            service_name: SERVICE_NAME.to_owned(),
            generation: "1".to_owned(),
            wait: Duration::from_secs(60),
        }
    }

    /// A key of the Secret's `data`, decoded.
    fn data(&self, key: &str) -> Option<Vec<u8>> {
        self.state().secret["data"][key]
            .as_str()
            .map(|value| STANDARD.decode(value).expect("base64"))
    }

    fn metadata(&self, field: &str) -> Value {
        self.state().secret["metadata"][field].clone()
    }

    fn patches(&self) -> usize {
        self.state().patches
    }
}

/// The head (request line and headers) and the body of one request.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream.set_read_timeout(Some(LIMIT)).ok()?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length = head
        .lines()
        .skip(1)
        .filter_map(|header| header.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = request[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Some((head, body))
}

fn status(code: u16) -> (u16, String) {
    (
        code,
        json!({"kind": "Status", "status": "Failure", "code": code}).to_string(),
    )
}

fn answer(state: &Mutex<ApiState>, bearer: &str, head: &str, body: &[u8]) -> (u16, String) {
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default();
    let headers: Vec<(String, String)> = lines
        .filter_map(|header| header.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    if header("authorization") != bearer {
        return status(401);
    }
    let path = format!("/api/v1/namespaces/{NAMESPACE}/secrets/{TLS_SECRET}");
    let mut state = state.lock().expect("the fake API's state");
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target == path => (200, state.secret.to_string()),
        (Some("PATCH"), Some(target))
            if target == format!("{path}?fieldManager=trustiot-migrate") =>
        {
            if header("content-type") != "application/merge-patch+json" {
                return status(415);
            }
            let Ok(patch) = serde_json::from_slice::<Value>(body) else {
                return status(400);
            };
            if patch["metadata"]["resourceVersion"] != state.secret["metadata"]["resourceVersion"] {
                return status(409);
            }
            merge(&mut state.secret, &patch);
            state.version += 1;
            state.secret["metadata"]["resourceVersion"] = json!(state.version.to_string());
            state.patches += 1;
            (200, state.secret.to_string())
        }
        _ => status(404),
    }
}

/// RFC 7386 JSON merge patch.
fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = json!({});
    }
    let object = target.as_object_mut().expect("an object");
    for (key, value) in patch {
        if value.is_null() {
            object.remove(key);
        } else {
            merge(object.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// A run of the issuer at `now` (the issuer's clock).
async fn run_at(settings: &MqttCertificateSettings, now: i64) -> Outcome {
    let mut report = Report::default();
    converge_at(settings, &mut report, now)
        .await
        .unwrap_or_else(|error| panic!("the issuer's run at {now}: {error}"))
}

fn issued(outcome: Outcome) -> (Current, String) {
    match outcome {
        Outcome::Issued {
            certificate,
            reason,
        } => (certificate, reason),
        other => panic!("expected a new certificate, got {other:?}"),
    }
}

// ---- certificates of the test ----

/// A plain CSR with `common_name` for a new P-256 key.
fn plain_request(common_name: &str) -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

/// A throwaway CA that is not ours and a server certificate under it for the broker's name
/// (server authentication only): (CA certificate DER, server certificate DER, server key).
fn foreign_broker_certificate() -> (Vec<u8>, Vec<u8>, KeyPair) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a CA key");
    let mut ca_params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "another CA");
    ca_params.distinguished_name = name;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_certificate = ca_params.self_signed(&ca_key).expect("another CA");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a server key");
    let mut params = CertificateParams::new(vec![BROKER_SERVER_NAME.to_owned()]).expect("params");
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, BROKER_SERVER_NAME);
    params.distinguished_name = subject;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let certificate = params
        .signed_by(&key, &issuer)
        .expect("the server certificate");
    (
        ca_certificate.der().to_vec(),
        certificate.der().to_vec(),
        key,
    )
}

/// The base64 lines of a PEM key, to look for in logs.
fn pem_body_lines(pem: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(pem)
        .lines()
        .filter(|line| !line.starts_with("-----") && !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

// ---- the broker ----

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest, as in broker.rs,
/// `crates/protocol/tests/broker.rs`, the chart and the local dev environment; Renovate updates
/// every copy in one pull request.
const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408";

/// Where the test puts its own files inside the container: the configuration, and the marker
/// that starts the broker.
const BROKER_DIR: &str = "/run/trustiot";

/// Where the chart's gateway listener reads the server certificate Secret (its mount in the
/// broker's pod) and the trust files the trust init container installs.
const TLS_DIR: &str = "/etc/trustiot/mqtt-broker-tls";
const TRUST_DIR: &str = "/mosquitto/data/trust/current";

/// Waits until the test has put the files in place, then runs the broker as PID 1 (it drops to
/// its own user), so that a signal to the container reaches it.
const START_SCRIPT: &str = r#"set -eu
while [ ! -f /run/trustiot/ready ]; do sleep 1; done
chown -R mosquitto:mosquitto /run/trustiot /etc/trustiot /mosquitto/data/trust
exec mosquitto -c /run/trustiot/mosquitto.conf
"#;

/// The chart's gateway listener: MQTT 5 over TLS 1.3 on every interface (so the test can publish
/// it), the Secret's `tls.crt` (the server certificate, then its issuing CA) and `tls.key`, client
/// certificates under the certs service's chain checked against its CRL, the certificate's common
/// name as the username and the client id.
const GATEWAY_LISTENER: &str =
    include_str!("../../../deploy/chart/files/mosquitto/gateway-listener.conf");

/// The broker's configuration: the chart's gateway listener alone, logging to stdout.
fn mosquitto_conf() -> String {
    format!("{GATEWAY_LISTENER}\nlog_dest stdout\nlog_type all\n")
}

/// The Mosquitto container, its gateway port published on the host's loopback interface.
struct Broker {
    container: Container,
}

impl Broker {
    fn start(runtime: &'static str) -> Self {
        let container = Container {
            runtime,
            name: unique_name("certs-broker-rotation"),
        };
        // The container's id; `container` removes it, with its volumes, when dropped.
        let _id = runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--memory",
                "128m",
                "--publish",
                "127.0.0.1::8883",
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ],
        );
        Self { container }
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

    /// Puts a server certificate chain and key where the listener reads them, as the kubelet
    /// updates the mounted Secret.
    fn push_server(&self, chain: &[u8], key: &[u8]) {
        self.push(&format!("{TLS_DIR}/tls.crt"), chain);
        self.push(&format!("{TLS_DIR}/tls.key"), key);
    }

    /// Sends SIGHUP to the broker, as the trust sidecar does after the Secret changed.
    fn reload(&self) {
        let _ = runtime_output(
            self.container.runtime,
            &["kill", "--signal", "HUP", &self.container.name],
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

    /// Waits until the broker logs that it runs.
    async fn wait_running(&self) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let logs = self.logs();
            if logs.contains(" running") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Mosquitto did not log that it runs within {LIMIT:?}:\n{logs}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

// ---- the gateway's side: TLS and MQTT ----

/// A client that trusts `anchor` alone and presents `chain` with `key`.
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

/// An MQTT 5 CONNECT with a clean start, a keepalive of 600 seconds, no properties and
/// `client_id` (the broker uses the certificate's common name instead).
fn connect_packet(client_id: &str) -> Vec<u8> {
    let id = client_id.as_bytes();
    let remaining = u8::try_from(13 + id.len()).expect("a short packet");
    let mut packet = vec![
        0x10, remaining, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x02, 0x58, 0x00,
    ];
    packet.extend_from_slice(&u16::try_from(id.len()).expect("a short id").to_be_bytes());
    packet.extend_from_slice(id);
    packet
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

/// A connection that verified the broker and got a successful CONNACK, and the serial number (hex) of the
/// server certificate it verified.
struct Session {
    tls: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    serial: String,
}

impl Session {
    /// PINGREQ, and whether PINGRESP came back.
    fn ping(&mut self) -> Result<(), String> {
        self.tls
            .write_all(&[0xc0, 0x00])
            .map_err(|error| format!("PINGREQ: {error}"))?;
        match read_packet(&mut self.tls) {
            Ok((0xd0, rest)) if rest.is_empty() => Ok(()),
            Ok((kind, rest)) => Err(format!("expected PINGRESP, got {kind:02x} {rest:02x?}")),
            Err(error) => Err(format!("PINGRESP: {error}")),
        }
    }
}

/// Connects to the broker at `address` for the server name `name` with `config`, and sends
/// CONNECT. The error is the reason: rustls's when the client refused the server certificate.
fn connect(
    address: SocketAddr,
    config: &Arc<rustls::ClientConfig>,
    name: &str,
    client_id: &str,
) -> Result<Session, String> {
    let socket = TcpStream::connect_timeout(&address, LIMIT)
        .map_err(|error| format!("connect to {address}: {error}"))?;
    socket
        .set_read_timeout(Some(LIMIT))
        .and_then(|()| socket.set_write_timeout(Some(LIMIT)))
        .map_err(|error| format!("socket timeouts: {error}"))?;
    let server_name = ServerName::try_from(name.to_owned()).expect("a server name");
    let connection =
        rustls::ClientConnection::new(Arc::clone(config), server_name).expect("a TLS client");
    let mut tls = rustls::StreamOwned::new(connection, socket);
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock).map_err(|error| {
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .map_or_else(|| format!("TLS: {error}"), |tls| format!("{tls:?}"))
        })?;
    }
    let leaf = tls
        .conn
        .peer_certificates()
        .and_then(|chain| chain.first())
        .expect("the server's certificate")
        .clone();
    let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).expect("a certificate");
    let serial = issue::hex(parsed.raw_serial());
    tls.write_all(&connect_packet(client_id))
        .map_err(|error| format!("CONNECT: {error}"))?;
    let (kind, connack) = read_packet(&mut tls).map_err(|error| format!("CONNACK: {error}"))?;
    if kind != 0x20 || connack.get(1) != Some(&0x00) {
        return Err(format!(
            "expected a successful CONNACK, got {kind:02x} {connack:02x?}"
        ));
    }
    Ok(Session { tls, serial })
}

/// Why a connection was refused; fails when it was accepted.
fn refused(result: Result<Session, String>, what: &str) -> String {
    match result {
        Ok(session) => panic!("{what}: accepted, serving serial {}", session.serial),
        Err(reason) => reason,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_issuer_fills_the_secret_mosquitto_serves_it_and_loads_a_renewal() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let mut env = FAST_CRLS.to_vec();
    env.push(("TRUSTIOT__BROKER__SERVICE_NAME", SERVICE_NAME));
    env.push(("TRUSTIOT__BROKER__VALIDITY", "1h"));
    let service = Service::start(&product, &ca, &env).await;
    let crls = wait_for("the published CRLs", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;
    let (status, chain) = service.get(CHAIN_PATH).await;
    assert_eq!(status, 200, "{chain}");
    let mut names = vec![BROKER_SERVER_NAME.to_owned(), SERVICE_NAME.to_owned()];
    names.sort();

    // 1. The first run fills the Secret the chart declares empty.
    let api = FakeApi::start();
    let settings = api.settings(&service.broker_url);
    let started = now();
    let (first, reason) = issued(run_at(&settings, started).await);
    println!(
        "evidence: first run at {started}: {reason}; serial {} for {}, notBefore {}, notAfter {}, renewed from {}",
        first.serial,
        first.names.join(", "),
        first.not_before,
        first.not_after,
        first.renew_from()
    );
    assert_eq!(first.names, names);
    assert_eq!(
        first.not_after - first.not_before,
        VALIDITY_SECS + CLOCK_SKEW_SECS
    );
    assert_eq!(api.patches(), 1);
    let uid = api.metadata("uid");
    assert_eq!(
        api.metadata("annotations")[UID_ANNOTATION],
        uid,
        "bound to its uid"
    );
    assert_eq!(api.metadata("annotations")[GENERATION_ANNOTATION], "1");
    let first_chain = api.data("tls.crt").expect("tls.crt");
    let first_key = api.data("tls.key").expect("tls.key");
    let first_ders = pem::decode_all(
        std::str::from_utf8(&first_chain).expect("PEM"),
        pem::CERTIFICATE,
    )
    .expect("tls.crt is PEM");
    assert_eq!(first_ders.len(), 2, "the certificate, then its issuing CA");
    assert_eq!(first_ders[1], issuing.certificate_der());

    // 2. Mosquitto serves it; a gateway's client verifies it with the root and the name.
    let gateway = |id: &str| {
        let (key, request) = plain_request(id);
        let certificate = issue::gateway_certificate(
            issuing,
            &request,
            &GatewayId::new(id).expect("a gateway id"),
            Duration::from_secs(86_400),
            now(),
        )
        .expect("a gateway certificate");
        let chain = vec![
            CertificateDer::from(certificate.der.clone()),
            CertificateDer::from(issuing.certificate_der().to_vec()),
        ];
        (chain, key)
    };
    let (kept_chain, kept_key) = gateway(GATEWAY_KEPT);
    let (probe_chain, probe_key) = gateway(GATEWAY_PROBE);
    let (foreign_root, foreign_leaf, foreign_key) = foreign_broker_certificate();
    let kept_client = client_config(&ca.root_der(), kept_chain, &kept_key);
    let ours = client_config(&ca.root_der(), probe_chain.clone(), &probe_key);
    let theirs = client_config(&foreign_root, probe_chain, &probe_key);

    let broker = Broker::start(runtime);
    broker.push(&format!("{TRUST_DIR}/ca.pem"), chain.as_bytes());
    broker.push(&format!("{TRUST_DIR}/crl.pem"), crls.as_bytes());
    broker.push_server(&first_chain, &first_key);
    broker.push(
        &format!("{BROKER_DIR}/mosquitto.conf"),
        mosquitto_conf().as_bytes(),
    );
    broker.push(&format!("{BROKER_DIR}/ready"), b"");
    broker.wait_running().await;
    let address = broker.container.host_address("8883/tcp");

    let mut before = connect(address, &kept_client, BROKER_SERVER_NAME, "gw-535-before")
        .unwrap_or_else(|reason| panic!("the root and the broker's name: {reason}"));
    assert_eq!(before.serial, first.serial);
    println!(
        "evidence: trusting the root, {BROKER_SERVER_NAME}: verified serial {}, CONNACK success",
        before.serial
    );
    let wrong_name = refused(
        connect(address, &ours, WRONG_NAME, "gw-535-name"),
        "another name",
    );
    println!("evidence: trusting the root, {WRONG_NAME}: refused, {wrong_name}");
    assert!(wrong_name.contains("NotValidForName"), "{wrong_name}");
    let wrong_anchor = refused(
        connect(address, &theirs, BROKER_SERVER_NAME, "gw-535-anchor"),
        "another trust anchor",
    );
    println!("evidence: trusting another CA, {BROKER_SERVER_NAME}: refused, {wrong_anchor}");
    assert!(wrong_anchor.contains("UnknownIssuer"), "{wrong_anchor}");

    // 3. Before half of its validity a run keeps it and writes nothing.
    let kept = run_at(&settings, first.renew_from() - 60).await;
    assert_eq!(kept, Outcome::Kept(first.clone()));
    assert_eq!(
        api.patches(),
        1,
        "a run that keeps the certificate writes nothing"
    );
    println!(
        "evidence: run at {} (renewal from {}): kept serial {}",
        first.renew_from() - 60,
        first.renew_from(),
        first.serial
    );

    // 4. At half of its validity a run writes a new certificate with a new key.
    let (second, reason) = issued(run_at(&settings, first.renew_from()).await);
    println!(
        "evidence: run at {}: {reason}; serial {}, notBefore {}, notAfter {}",
        first.renew_from(),
        second.serial,
        second.not_before,
        second.not_after
    );
    assert_ne!(second.serial, first.serial);
    assert_eq!(second.names, names);
    assert_eq!(api.patches(), 2);
    let second_chain = api.data("tls.crt").expect("tls.crt");
    let second_key = api.data("tls.key").expect("tls.key");
    assert_ne!(second_key, first_key, "every renewal makes a new key");
    assert_eq!(api.metadata("annotations")[UID_ANNOTATION], uid);

    // 5. The broker loads it on SIGHUP: a new connection verifies the new certificate, and the
    // connection from before the reload is still served.
    broker.push_server(&second_chain, &second_key);
    broker.reload();
    let attempts = AtomicU32::new(0);
    let renewed = wait_for("a new connection to verify the renewed certificate", || {
        let attempt = attempts.fetch_add(1, Ordering::Relaxed);
        let renewed = connect(
            address,
            &ours,
            BROKER_SERVER_NAME,
            &format!("gw-535-after-{attempt}"),
        )
        .ok()
        .filter(|session| session.serial == second.serial);
        async move { renewed }
    })
    .await;
    println!(
        "evidence: after SIGHUP, a new connection trusting the root verified serial {} ({} attempt(s))",
        renewed.serial,
        attempts.load(Ordering::Relaxed)
    );
    before
        .ping()
        .unwrap_or_else(|problem| panic!("the connection from before the reload: {problem}"));
    println!("evidence: the connection opened before the reload answered PINGRESP");

    // 6. A later run keeps the new certificate.
    assert_eq!(
        run_at(&settings, now()).await,
        Outcome::Kept(second.clone())
    );
    assert_eq!(api.patches(), 2);

    // 7. A certificate for the broker's name from a CA that is not ours is refused by a client
    // that trusts the root.
    let foreign_chain = pem::encode(pem::CERTIFICATE, &foreign_leaf);
    broker.push_server(
        foreign_chain.as_bytes(),
        foreign_key.serialize_pem().as_bytes(),
    );
    broker.reload();
    let attempts = AtomicU32::new(0);
    let foreign = wait_for("the client to refuse the other CA's certificate", || {
        let attempt = attempts.fetch_add(1, Ordering::Relaxed);
        let refusal = connect(
            address,
            &ours,
            BROKER_SERVER_NAME,
            &format!("gw-535-foreign-{attempt}"),
        )
        .err()
        .filter(|reason| reason.contains("UnknownIssuer"));
        async move { refusal }
    })
    .await;
    println!(
        "evidence: serving another CA's certificate for {BROKER_SERVER_NAME}: refused, {foreign}"
    );

    // 8. No log holds the private key.
    let logs = broker.logs();
    println!("== broker log\n{logs}");
    let output = service.output();
    for (what, text) in [("the certs service", &output), ("Mosquitto", &logs)] {
        assert!(!text.contains("PRIVATE KEY"), "{what}'s log has a PEM key");
        for key in [&first_key, &second_key] {
            for line in pem_body_lines(key) {
                assert!(!text.contains(&line), "{what}'s log has a line of the key");
            }
        }
    }
}
