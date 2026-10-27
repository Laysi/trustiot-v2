//! Test support: scratch directories, a test CA that signs workstation certificates, made-up
//! secrets built at run time, a stand-in for the platform's login, logout and workstation
//! certificate endpoints on loopback (axum), and a runner for the binary.

#![allow(
    dead_code,
    reason = "each test file uses a different part of the support module"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CustomExtension, DnType,
    IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData, SerialNumber,
};
use time::{Duration, OffsetDateTime};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;

pub mod registry;

/// The binary under test.
pub const BINARY: &str = env!("CARGO_BIN_EXE_trustiot-workstation");

/// The extendedKeyUsage extension's OID.
const EXTENDED_KEY_USAGE: [u64; 4] = [2, 5, 29, 37];

/// The DER of the clientAuth purpose OID, 1.3.6.1.5.5.7.3.2.
const CLIENT_AUTH_OID: [u8; 10] = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];

/// A fresh directory under the system's temporary directory; removed by [`Scratch::drop`].
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "workstation-{name}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        set_mode(&path, 0o700);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

pub fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

/// Made-up values that stand for secrets in a test, different on every run.
pub struct FakeSecrets {
    /// A login code: eight digits.
    pub code: String,
    /// A session cookie value: 43 base64url characters.
    pub cookie: String,
    /// The Cloudflare Access client id and secret of `--headers-file`.
    pub access_id: String,
    pub access_secret: String,
}

impl FakeSecrets {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_nanos();
        let mut state = (u64::try_from(nanos % u128::from(u64::MAX)).expect("fits")
            ^ u64::from(std::process::id()).rotate_left(32)
            ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(16))
            | 1;
        let mut next = move || {
            // xorshift64: enough to make values that never repeat across runs.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut text = |length: usize| -> String {
            (0..length)
                .map(|_| char::from(ALPHABET[usize::try_from(next() % 64).expect("fits")]))
                .collect()
        };
        let cookie = text(43);
        let access_id = format!("{}.access", text(32));
        let access_secret = text(64);
        let code = format!("{:08}", next() % 100_000_000);
        Self {
            code,
            cookie,
            access_id,
            access_secret,
        }
    }

    /// The `--headers-file` text for these values.
    pub fn headers_file(&self) -> String {
        format!(
            "# Cloudflare Access service token\ncf-access-client-id: {}\ncf-access-client-secret: {}\n",
            self.access_id, self.access_secret
        )
    }
}

/// What a test certificate's extended key usage holds.
#[derive(Clone, Copy, Debug)]
pub enum Purpose {
    Workstation,
    WorkstationAndClientAuth,
    ClientAuthOnly,
    None,
}

/// A workstation certificate to issue.
#[derive(Clone, Debug)]
pub struct Profile {
    pub purpose: Purpose,
    pub common_name: String,
    pub not_before: OffsetDateTime,
    pub not_after: OffsetDateTime,
    pub serial: [u8; 16],
}

impl Profile {
    /// What the certs service issues for `name`: valid from five minutes ago for 180 days.
    pub fn workstation(name: &str) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            purpose: Purpose::Workstation,
            common_name: format!("workstation:{name}"),
            not_before: now - Duration::minutes(5),
            not_after: now + Duration::days(180),
            serial: [
                0x4f, 0x3a, 0x9c, 0x1e, 0x2b, 0x7d, 0x60, 0x58, 0xa1, 0xc3, 0xe5, 0xf7, 0x08, 0x19,
                0x2a, 0x3b,
            ],
        }
    }

    pub fn serial_hex(&self) -> String {
        self.serial
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// A test issuing CA.
pub struct TestCa {
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    pub fn new() -> Self {
        let key = KeyPair::generate().expect("a CA key");
        let mut params = CertificateParams::new(Vec::new()).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, "TrustIoT test issuing CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        Self {
            issuer: Issuer::new(params, key),
        }
    }

    /// A certificate for `public_key` with `profile`, PEM.
    pub fn issue(&self, public_key: &impl PublicKeyData, profile: &Profile) -> String {
        let mut params = CertificateParams::new(Vec::new()).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, profile.common_name.as_str());
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.not_before = profile.not_before;
        params.not_after = profile.not_after;
        params.serial_number = Some(SerialNumber::from_slice(&profile.serial));
        let mut oids: Vec<u8> = Vec::new();
        if matches!(
            profile.purpose,
            Purpose::Workstation | Purpose::WorkstationAndClientAuth
        ) {
            oids.extend([0x06, 20]);
            oids.extend(contract::certs::WORKSTATION_PURPOSE_DER);
        }
        if matches!(
            profile.purpose,
            Purpose::WorkstationAndClientAuth | Purpose::ClientAuthOnly
        ) {
            oids.extend(CLIENT_AUTH_OID);
        }
        if !matches!(profile.purpose, Purpose::None) {
            let mut sequence = vec![0x30, u8::try_from(oids.len()).expect("short")];
            sequence.extend(oids);
            params
                .custom_extensions
                .push(CustomExtension::from_oid_content(
                    &EXTENDED_KEY_USAGE,
                    sequence,
                ));
        }
        params
            .signed_by(public_key, &self.issuer)
            .expect("a certificate")
            .pem()
    }
}

/// One request the stand-in received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    pub fn step(&self) -> Option<String> {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()?
            .get("step")?
            .as_str()
            .map(str::to_owned)
    }
}

/// How the stand-in answers.
pub struct Behaviour {
    /// The status of `send_code`: 202, or an error status with an M1 body.
    pub send_code_status: StatusCode,
    /// Answer an accepted `send_code` with a body that is not a `send_code` answer.
    pub unreadable_send_code_answer: bool,
    /// A directory made read-only (0500) right before `verify_code` is answered, so the tool
    /// cannot store the session it gets.
    pub lock_on_verify: Option<PathBuf>,
    /// The issue endpoint answers the platform's own 401 (code `unauthenticated`) while the
    /// session is still live, as the certs service does when it cannot check the internal token.
    pub issue_rejects_session: bool,
    /// A path a proxy in front of the platform answers with a bare 401 (no M1 body).
    pub proxy_401_on: Option<&'static str>,
    /// The serial the issue answer gives instead of the certificate's.
    pub issued_serial: Option<String>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            send_code_status: StatusCode::ACCEPTED,
            unreadable_send_code_answer: false,
            lock_on_verify: None,
            issue_rejects_session: false,
            proxy_401_on: None,
            issued_serial: None,
        }
    }
}

struct Shared {
    secrets: FakeSecrets,
    behaviour: Behaviour,
    ca: TestCa,
    requests: Mutex<Vec<Recorded>>,
    revoked: Mutex<bool>,
}

/// A stand-in for the platform on `http://127.0.0.1:<port>`.
pub struct StandIn {
    pub url: String,
    shared: Arc<Shared>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn m1(status: StatusCode, code: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"code":"{code}","message_key":"error.{code}"}}"#),
    )
        .into_response()
}

impl StandIn {
    pub async fn start(secrets: FakeSecrets, behaviour: Behaviour) -> Self {
        let shared = Arc::new(Shared {
            secrets,
            behaviour,
            ca: TestCa::new(),
            requests: Mutex::new(Vec::new()),
            revoked: Mutex::new(false),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let router = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            url,
            shared,
            server,
        }
    }

    pub fn secrets(&self) -> &FakeSecrets {
        &self.shared.secrets
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.requests.lock().expect("unpoisoned").clone()
    }

    pub fn revoked(&self) -> bool {
        *self.shared.revoked.lock().expect("unpoisoned")
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 64 * 1024).await.unwrap_or_default().to_vec();
    let recorded = Recorded {
        method: parts.method.clone(),
        path: parts.uri.path().to_owned(),
        headers: parts.headers.clone(),
        body: body.clone(),
    };
    shared
        .requests
        .lock()
        .expect("unpoisoned")
        .push(recorded.clone());
    let secrets = &shared.secrets;
    let session_cookie = format!("__Host-trustiot-session={}", secrets.cookie);
    let has_cookie = recorded.header("cookie") == Some(session_cookie.as_str());
    match (parts.method.as_str(), parts.uri.path()) {
        (_, path) if shared.behaviour.proxy_401_on == Some(path) => (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/html")],
            "<html>401 Authorization Required</html>",
        )
            .into_response(),
        ("POST", "/auth/login") => {
            let json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(json) => json,
                Err(_) => return m1(StatusCode::BAD_REQUEST, "invalid_request"),
            };
            match json.get("step").and_then(serde_json::Value::as_str) {
                Some("send_code") => {
                    if shared.behaviour.send_code_status != StatusCode::ACCEPTED {
                        return m1(shared.behaviour.send_code_status, "unavailable");
                    }
                    if shared.behaviour.unreadable_send_code_answer {
                        return (StatusCode::ACCEPTED, "<html>accepted</html>").into_response();
                    }
                    (
                        StatusCode::ACCEPTED,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"code_length":8,"expires_in_secs":600,"resend_after_secs":60}"#,
                    )
                        .into_response()
                }
                Some("verify_code") => {
                    if json.get("code").and_then(serde_json::Value::as_str)
                        != Some(secrets.code.as_str())
                    {
                        return m1(StatusCode::UNAUTHORIZED, "unauthenticated");
                    }
                    if let Some(directory) = &shared.behaviour.lock_on_verify {
                        set_mode(directory, 0o500);
                    }
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::CACHE_CONTROL, "no-store")
                        .header(header::SET_COOKIE, "tracking=1; Path=/")
                        .header(
                            header::SET_COOKIE,
                            format!(
                                "__Host-trustiot-session={}; Max-Age=604800; Path=/; Secure; \
                                 HttpOnly; SameSite=Lax",
                                secrets.cookie
                            ),
                        )
                        .body(Body::from(r#"{"redirect_to":"/"}"#))
                        .expect("a response")
                }
                _ => m1(StatusCode::BAD_REQUEST, "invalid_request"),
            }
        }
        ("POST", "/api/certs/v1/workstation-certificates") => {
            if !has_cookie
                || *shared.revoked.lock().expect("unpoisoned")
                || shared.behaviour.issue_rejects_session
            {
                return m1(StatusCode::UNAUTHORIZED, "unauthenticated");
            }
            let json: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");
            let name = json["workstation"].as_str().expect("a name").to_owned();
            let csr = json["certificate_request"].as_str().expect("a CSR");
            let request = CertificateSigningRequestParams::from_pem(csr).expect("a CSR");
            let profile = Profile::workstation(&name);
            let certificate = shared.ca.issue(&request.public_key, &profile);
            let answer = serde_json::json!({
                "serial": shared
                    .behaviour
                    .issued_serial
                    .clone()
                    .unwrap_or_else(|| profile.serial_hex()),
                "workstation": name,
                "subject": profile.common_name,
                "ca_key_id": "0".repeat(64),
                "not_before": "2026-10-23T00:00:00Z",
                "not_after": "2027-04-21T00:00:00Z",
                "certificate": certificate,
            });
            (
                StatusCode::CREATED,
                [(header::CONTENT_TYPE, "application/json")],
                answer.to_string(),
            )
                .into_response()
        }
        ("DELETE", "/api/identity/v1/sessions/current") => {
            let mut revoked = shared.revoked.lock().expect("unpoisoned");
            if !has_cookie || *revoked {
                return m1(StatusCode::UNAUTHORIZED, "unauthenticated");
            }
            *revoked = true;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => m1(StatusCode::NOT_FOUND, "not_found"),
    }
}

/// A listener that counts connections and never answers: `Drop` closes each connection at once,
/// `Hold` keeps it open until the listener is dropped.
pub struct Silent {
    pub url: String,
    accepted: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy)]
pub enum Silence {
    Drop,
    Hold,
}

impl Silent {
    pub async fn start(silence: Silence) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let accepted = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                match silence {
                    Silence::Drop => {
                        let _ = stream.shutdown().await;
                    }
                    Silence::Hold => held.push(stream),
                }
            }
        });
        Self {
            url,
            accepted,
            task,
        }
    }

    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Silent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One run of the binary.
#[derive(Debug)]
pub struct Run {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn output(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// The environment of a run: only what the tool reads.
pub struct Environment {
    pub runtime: Option<PathBuf>,
    pub home: PathBuf,
    pub log: &'static str,
}

impl Environment {
    pub fn in_scratch(scratch: &Scratch) -> Self {
        let runtime = scratch.join("run");
        std::fs::create_dir_all(&runtime).expect("a runtime directory");
        set_mode(&runtime, 0o700);
        let home = scratch.join("home");
        std::fs::create_dir_all(&home).expect("a home directory");
        Self {
            runtime: Some(runtime),
            home,
            log: "trace",
        }
    }

    pub fn session_file(&self) -> PathBuf {
        self.runtime
            .as_ref()
            .expect("a runtime directory")
            .join("trustiot-workstation/session")
    }
}

/// Runs the binary with `arguments`, `stdin` on its standard input.
pub async fn run(environment: &Environment, arguments: &[&str], stdin: &str) -> Run {
    run_program(environment, BINARY, &[], arguments, stdin).await
}

/// Runs the binary with `arguments` under `ulimit -f <blocks>` (the shell's 512- or 1024-byte
/// blocks): a write past the limit kills it with SIGXFSZ, as a crash in the middle of a write.
pub async fn run_with_file_size_limit(
    environment: &Environment,
    blocks: u32,
    arguments: &[&str],
) -> Run {
    let script = format!("ulimit -f {blocks} && exec \"$0\" \"$@\"");
    run_program(
        environment,
        "/bin/sh",
        &["-c", script.as_str(), BINARY],
        arguments,
        "",
    )
    .await
}

async fn run_program(
    environment: &Environment,
    program: &str,
    prefix: &[&str],
    arguments: &[&str],
    stdin: &str,
) -> Run {
    let mut variables: HashMap<&str, &std::ffi::OsStr> = HashMap::new();
    variables.insert("HOME", environment.home.as_os_str());
    variables.insert(
        "TRUSTIOT_WORKSTATION_LOG",
        std::ffi::OsStr::new(environment.log),
    );
    if let Some(runtime) = &environment.runtime {
        variables.insert("XDG_RUNTIME_DIR", runtime.as_os_str());
    }
    let mut child = tokio::process::Command::new(program)
        .args(prefix)
        .args(arguments)
        .env_clear()
        .envs(variables)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the binary starts");
    let mut input = child.stdin.take().expect("a stdin pipe");
    let text = stdin.to_owned();
    // The tool may exit before reading everything; a broken pipe is fine.
    let writer = tokio::spawn(async move {
        let _ = input.write_all(text.as_bytes()).await;
        drop(input);
    });
    let output = child.wait_with_output().await.expect("the binary ran");
    let _ = writer.await;
    Run {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Every line of base64 in a PEM text: what must never show anywhere.
pub fn pem_body_lines(pem: &str) -> Vec<String> {
    pem.lines()
        .filter(|line| !line.starts_with("-----") && !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}
