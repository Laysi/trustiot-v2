//! Helpers for the integration tests: a test CA on disk (an offline root made in memory, issuing
//! CA keys as the init container stores them, issuing CA certificates signed by the root), the
//! certs binary as a real process on the product's schema (`testkit::product`) with the public key
//! of a test identity issuer (`testkit::identity`), its JSON log, and plain HTTP calls to it.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ca_keys::{CaKey, CaKeys, Request};
use certs::ceremony;
use contract::{Actor, InternalClaims, ServiceName, TenantGrant};
use rcgen::KeyPair;
use serde_json::Value;
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::ProductPostgres;

/// The certs binary, built by Cargo for these tests.
pub const CERTS: &str = env!("CARGO_BIN_EXE_certs");

/// The ceremony tool, built by Cargo for these tests.
pub const CERTS_CA: &str = env!("CARGO_BIN_EXE_certs-ca");

/// The longest a started service may take to listen and to become ready, and the longest any
/// awaited condition may take. Generous: test hosts are often loaded (CLAUDE.md, "Test time
/// limits").
pub const LIMIT: Duration = Duration::from_secs(120);

/// Unix seconds now.
pub fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs(),
    )
    .expect("seconds fit")
}

/// A fresh directory under Cargo's temporary directory for these tests.
pub fn temp_dir(prefix: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// A test CA: a root, issuing CA keys and the root's certificates for them, in files as the chart
/// mounts them.
pub struct TestCa {
    pub root_key: KeyPair,
    pub root_pem: String,
    /// The key set's document (`issuing-ca-keys.json`), as the init container writes it.
    pub keys_json: String,
    /// The issuing CA certificates, PEM, in generation order.
    pub issuing_pems: Vec<String>,
    pub keys_file: PathBuf,
    pub root_file: PathBuf,
    pub certificates_file: PathBuf,
}

impl TestCa {
    /// A root and one issuing CA key per generation in `generations`, each with a certificate
    /// named `TrustIoT test issuing CA <generation> <tag>`, written to a fresh directory.
    pub fn new(generations: &[u64]) -> Self {
        let root_key = ceremony::new_root_key().expect("a root key");
        let root_pem =
            ceremony::root_certificate(&root_key, "TrustIoT test root", 3650, now()).expect("root");
        let mut keys = CaKeys::default();
        for &generation in generations {
            keys = keys
                .plan(
                    Request {
                        generation,
                        remove_before: 0,
                    },
                    now(),
                    CaKey::generate,
                )
                .expect("plan")
                .keys;
        }
        let tag = std::process::id();
        let issuing_pems = keys
            .keys()
            .iter()
            .map(|key| {
                ceremony::issuing_certificate(
                    &root_key,
                    &root_pem,
                    key.request_pem(),
                    &format!("TrustIoT test issuing CA {} {tag}", key.generation()),
                    365,
                    now(),
                )
                .expect("an issuing CA certificate")
            })
            .collect();
        let keys_json = keys.to_json().to_string();
        let dir = temp_dir("certs-ca");
        let ca = Self {
            root_key,
            root_pem,
            keys_json,
            issuing_pems,
            keys_file: dir.join("issuing-ca-keys.json"),
            root_file: dir.join("root.pem"),
            certificates_file: dir.join("issuing.pem"),
        };
        ca.write_all();
        ca
    }

    /// Writes the three files.
    pub fn write_all(&self) {
        std::fs::write(&self.keys_file, &self.keys_json).expect("write the keys file");
        std::fs::write(&self.root_file, &self.root_pem).expect("write the root file");
        std::fs::write(&self.certificates_file, self.issuing_pems.concat())
            .expect("write the certificates file");
    }

    /// The key set, parsed again.
    pub fn keys(&self) -> CaKeys {
        CaKeys::parse(&self.keys_json).expect("the test key set")
    }

    /// The environment that points the service at these files.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "TRUSTIOT__CA__KEYS_FILE",
                self.keys_file.display().to_string(),
            ),
            (
                "TRUSTIOT__CA__ROOT_CERTIFICATE_FILE",
                self.root_file.display().to_string(),
            ),
            (
                "TRUSTIOT__CA__CERTIFICATES_FILE",
                self.certificates_file.display().to_string(),
            ),
        ]
    }

    /// The root certificate, DER.
    pub fn root_der(&self) -> Vec<u8> {
        certs::pem::decode_all(&self.root_pem, certs::pem::CERTIFICATE)
            .expect("the root PEM")
            .remove(0)
    }

    /// The issuing CA certificates, DER, in generation order.
    pub fn issuing_ders(&self) -> Vec<Vec<u8>> {
        self.issuing_pems
            .iter()
            .map(|pem| {
                certs::pem::decode_all(pem, certs::pem::CERTIFICATE)
                    .expect("an issuing CA PEM")
                    .remove(0)
            })
            .collect()
    }

    /// The authority the service would build from these files now.
    pub fn authority(&self) -> certs::authority::Authority {
        certs::authority::Authority::from_parts(
            self.keys(),
            &self.root_pem,
            &self.issuing_pems.concat(),
            now(),
        )
        .expect("the test authority")
    }
}

/// The lines a child process printed, shared with the thread that reads them.
#[derive(Clone, Default)]
pub struct Output(Arc<Mutex<Vec<String>>>);

impl Output {
    fn collect(&self, stream: impl Read + Send + 'static) {
        let lines = Arc::clone(&self.0);
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                lines.lock().expect("output lock").push(line);
            }
        });
    }

    /// Every line so far.
    pub fn text(&self) -> String {
        self.0.lock().expect("output lock").join("\n")
    }

    /// The JSON lines whose `message` starts with `message`.
    pub fn messages(&self, message: &str) -> Vec<Value> {
        self.0
            .lock()
            .expect("output lock")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| {
                value["message"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(message))
            })
            .collect()
    }
}

/// The certs service running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    /// Where the API listens, as a base URL.
    pub base_url: String,
    health: SocketAddr,
    /// The stand-in for the identity service whose public key the service trusts.
    pub issuer: TestIssuer,
}

/// What a POST answered.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// The `cache-control` header, if any.
    pub cache_control: Option<String>,
}

impl Reply {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("a JSON body ({error}): {}", self.body))
    }
}

impl Service {
    /// Starts the service on free local ports against `product` with the CA files of `ca`, the
    /// public key of a new [`TestIssuer`], and `env` (the CRL schedule, telemetry), with the role
    /// the chart gives it, and returns once it listens, ready or not. The environment is
    /// otherwise empty, so the developer's own variables cannot leak in.
    pub async fn spawn(product: &ProductPostgres, ca: &TestCa, env: &[(&str, &str)]) -> Self {
        let issuer = TestIssuer::new();
        let identity_keys = temp_dir("certs-identity").join("jwks.json");
        issuer
            .write_jwks(&identity_keys)
            .expect("write the identity public keys");
        let mut command = Command::new(CERTS);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT__DATABASE__USERNAME", "certs_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT__DATABASE__PLATFORM_USERNAME", "certs_platform")
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .envs(product.platform_env())
            .envs(ca.env())
            .env(
                "TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE",
                identity_keys.display().to_string(),
            )
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the certs service");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + LIMIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            assert!(
                Instant::now() < deadline,
                "the certs service did not log `listening` within {LIMIT:?}\nstdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let address = |field: &str| -> SocketAddr {
            listening["fields"][field]
                .as_str()
                .and_then(|address| address.parse().ok())
                .unwrap_or_else(|| panic!("no {field} in {listening}"))
        };
        Self {
            child,
            base_url: format!("http://{}", address("address")),
            health: address("health_address"),
            stdout,
            stderr,
            issuer,
        }
    }

    /// A token for this service: a new user, a platform administrator or not, acting in
    /// `tenant`, exchanged by `actor` (none: the ingress's own request).
    pub fn token(
        &self,
        platform_admin: bool,
        tenant: Option<TenantGrant>,
        actor: Option<ServiceName>,
    ) -> String {
        let claims = InternalClaims {
            platform_admin,
            act: actor.map(|sub| Actor { sub }),
            ..TestIssuer::claims(ServiceName::Certs, tenant)
        };
        self.issuer.token(&claims)
    }

    /// A platform administrator's token, as the ingress's cookie check mints it.
    pub fn admin_token(&self) -> String {
        self.token(true, None, None)
    }

    /// `POST path` on the API port with `token` in the internal identity header (none: no
    /// header), `content_type` (none: no header) and `body`.
    pub async fn post(
        &self,
        path: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        body: impl Into<reqwest::Body>,
    ) -> Reply {
        let mut request = reqwest::Client::new()
            .post(format!("{}{path}", self.base_url))
            .timeout(Duration::from_secs(30))
            .body(body);
        if let Some(token) = token {
            request = request.header(contract::INTERNAL_IDENTITY_HEADER, token);
        }
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let response = request.send().await.expect("call the certs service");
        let status = response.status().as_u16();
        let cache_control = response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Reply {
            status,
            body: response.text().await.expect("read the body"),
            cache_control,
        }
    }

    /// `POST path` with a JSON `body` and `token`.
    pub async fn post_json(&self, path: &str, token: Option<&str>, body: &Value) -> Reply {
        self.post(path, token, Some("application/json"), body.to_string())
            .await
    }

    /// Like [`Service::spawn`], and waits until it is ready.
    pub async fn start(product: &ProductPostgres, ca: &TestCa, env: &[(&str, &str)]) -> Self {
        let service = Self::spawn(product, ca, env).await;
        service.wait_readiness(200).await;
        service
    }

    /// Waits until `/readyz` answers `status`.
    pub async fn wait_readiness(&self, status: u16) {
        let url = format!("http://{}/readyz", self.health);
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Ok(response) = reqwest::get(&url).await
                && response.status() == status
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "/readyz of the certs service did not answer {status} within {LIMIT:?}\nstdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `GET path` on the API port (or the health port with `health`); the status and the body.
    pub async fn get(&self, path: &str) -> (u16, String) {
        get(&format!("{}{path}", self.base_url)).await
    }

    /// `GET path` on the health port.
    pub async fn get_health(&self, path: &str) -> (u16, String) {
        get(&format!("http://{}{path}", self.health)).await
    }

    /// Everything the process printed so far.
    pub fn output(&self) -> String {
        format!("{}\n{}", self.stdout.text(), self.stderr.text())
    }
}

async fn get(url: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("call the certs service");
    let status = response.status().as_u16();
    (status, response.text().await.expect("read the body"))
}

impl Drop for Service {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Verifies `leaf` (DER) as a TLS client certificate the way a rustls server would: chained to
/// `root` through `issuing`, for client authentication, at `at` (Unix seconds), and, when `crls`
/// is not empty, not revoked by them (the leaf only, as Mosquitto checks; an expired CRL fails).
///
/// # Errors
///
/// rustls's reason.
pub fn verify_client(
    root: &[u8],
    issuing: &[Vec<u8>],
    leaf: &[u8],
    crls: &[Vec<u8>],
    at: i64,
) -> Result<(), rustls::Error> {
    use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, UnixTime};
    use rustls::server::WebPkiClientVerifier;

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(root.to_vec()))
        .expect("the root as a trust anchor");
    let mut builder = WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    );
    if !crls.is_empty() {
        builder = builder
            .with_crls(
                crls.iter()
                    .map(|crl| CertificateRevocationListDer::from(crl.clone())),
            )
            .only_check_end_entity_revocation()
            .enforce_revocation_expiration();
    }
    let verifier = builder.build().expect("a client verifier");
    let intermediates: Vec<CertificateDer<'_>> = issuing
        .iter()
        .map(|der| CertificateDer::from(der.as_slice()))
        .collect();
    verifier
        .verify_client_cert(
            &CertificateDer::from(leaf),
            &intermediates,
            UnixTime::since_unix_epoch(Duration::from_secs(
                u64::try_from(at).expect("a time after 1970"),
            )),
        )
        .map(drop)
}

/// Polls `check` every 100 ms until it returns `Some`, for at most [`LIMIT`]; fails naming
/// `what` was awaited.
pub async fn wait_for<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(Instant::now() < deadline, "{what}: not within {LIMIT:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- a fake OTLP/HTTP receiver ----

/// The path and body of one request the fake collector received.
type Received = (String, Vec<u8>);

/// Accepts OTLP/HTTP requests and records their paths and bodies.
pub struct FakeCollector {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeCollector {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake collector");
        let address = listener.local_addr().expect("collector address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let sink = Arc::clone(&sink);
                thread::spawn(move || {
                    if let Some(request) = read_request(stream) {
                        sink.lock().expect("collector lock").push(request);
                    }
                });
            }
        });
        Self { address, received }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The bodies received on `path`, oldest first.
    pub fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.received
            .lock()
            .expect("collector lock")
            .iter()
            .filter(|(received, _)| received == path)
            .map(|(_, body)| body.clone())
            .collect()
    }
}

/// Reads one request (Content-Length or chunked body), answers 200, and closes.
fn read_request(stream: TcpStream) -> Option<Received> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_owned();
    let mut length = 0;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().ok()?;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") && value.trim() == "chunked" {
            chunked = true;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body).ok()?;
    }
    let mut stream = stream;
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n",
        )
        .ok()?;
    Some((path, body))
}

/// One exported gauge point of the certs service: the metric, its value, and the issuing CA it
/// is about.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub metric: String,
    pub value: f64,
    pub key_id: String,
}

/// Every gauge point the collector received, in the order of the exports.
pub fn points(collector: &FakeCollector) -> Vec<Point> {
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    use prost::Message as _;

    let mut points = Vec::new();
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(body.as_slice()).expect("OTLP metrics");
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                for metric in scope.metrics {
                    let Some(metric::Data::Gauge(gauge)) = metric.data else {
                        continue;
                    };
                    for point in gauge.data_points {
                        let key_id = point
                            .attributes
                            .iter()
                            .find(|attribute| attribute.key == "certs.ca.key_id")
                            .and_then(|attribute| attribute.value.as_ref())
                            .and_then(|value| match &value.value {
                                Some(any_value::Value::StringValue(text)) => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        #[allow(
                            clippy::cast_precision_loss,
                            reason = "the gauges are 0 or 1 and seconds"
                        )]
                        let value = match point.value {
                            Some(number_data_point::Value::AsInt(value)) => value as f64,
                            Some(number_data_point::Value::AsDouble(value)) => value,
                            None => continue,
                        };
                        points.push(Point {
                            metric: metric.name.clone(),
                            value,
                            key_id,
                        });
                    }
                }
            }
        }
    }
    points
}
