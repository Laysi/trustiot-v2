//! Helpers for the integration tests: the identity binary as a real process, a minimal HTTP/1.1
//! client, a throwaway TLS Postgres, and the session test tooling.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

/// The TLS Postgres of crates/service-kit's tests, shared so there is one implementation of the
/// throwaway CA and container (a shared test kit is Laysi/trustiot-v2#56). It needs the
/// `Container`, `container_runtime`, `runtime_output`, `unavailable` and `unique_name` below.
#[path = "../../../../crates/service-kit/tests/support/postgres.rs"]
pub mod postgres;

/// The start of that Postgres container (`postgres` above uses it): one logic for every crate's
/// test containers.
#[path = "../../../../crates/service-kit/tests/support/start.rs"]
pub mod start;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use identity::session::{SESSION_COOKIE, SESSION_SECRET_BYTES, SessionSecret};
use serde_json::json;

/// The identity binary, built by Cargo for these tests.
pub const IDENTITY: &str = env!("CARGO_BIN_EXE_identity");

/// A scratch directory for files the service reads (key, CA), under Cargo's target directory.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "identity-{name}-{}-{}",
        std::process::id(),
        next_id()
    ));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A throwaway Ed25519 key derived from a one-byte seed: no key material is stored in the
/// repository, and nothing outside the tests trusts it.
pub struct TestKey(SigningKey);

impl TestKey {
    pub fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }

    /// The private key as `openssl genpkey -algorithm ed25519` writes it.
    pub fn pem(&self) -> String {
        self.0
            .to_pkcs8_pem(LineEnding::LF)
            .expect("PKCS#8 PEM")
            .to_string()
    }

    /// Every encoding of the private key a leak could show: the PEM body, and the 32-byte seed in
    /// base64url, base64 and hex.
    pub fn secret_spellings(&self) -> Vec<String> {
        let seed = self.0.to_bytes();
        let body: String = self
            .pem()
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        vec![
            body,
            URL_SAFE_NO_PAD.encode(seed),
            base64::engine::general_purpose::STANDARD.encode(seed),
            seed.iter().map(|byte| format!("{byte:02x}")).collect(),
        ]
    }

    /// The public JWK under `kid`.
    pub fn jwk(&self, kid: &str) -> serde_json::Value {
        json!({
            "kty": "OKP", "crv": "Ed25519", "kid": kid,
            "x": URL_SAFE_NO_PAD.encode(self.0.verifying_key().as_bytes())
        })
    }
}

/// A JWK Set of these keys, as `TRUSTIOT_INTERNAL_IDENTITY_KEYS` holds it.
pub fn jwks(keys: &[(&TestKey, &str)]) -> String {
    let keys: Vec<_> = keys.iter().map(|(key, kid)| key.jwk(kid)).collect();
    json!({ "keys": keys }).to_string()
}

/// A new session secret from the operating system's CSPRNG, as the login flow will create them.
pub fn new_secret() -> SessionSecret {
    let mut bytes = [0_u8; SESSION_SECRET_BYTES];
    getrandom::fill(&mut bytes).expect("operating system randomness");
    SessionSecret::from_bytes(bytes)
}

/// The `Cookie` header value for a session.
pub fn cookie(secret: &SessionSecret) -> String {
    format!("{SESSION_COOKIE}={}", secret.to_cookie_value())
}

/// Lines a child process printed, shared with the threads that read them.
#[derive(Clone, Default)]
pub struct Output {
    lines: Arc<(Mutex<Vec<String>>, Condvar)>,
    closed: Arc<(Mutex<bool>, Condvar)>,
}

impl Output {
    fn collect(self, stream: impl Read + Send + 'static) {
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let (lines, changed) = &*self.lines;
                lines.lock().expect("output lock").push(line);
                changed.notify_all();
            }
            let (closed, changed) = &*self.closed;
            *closed.lock().expect("output lock") = true;
            changed.notify_all();
        });
    }

    /// Waits until the reader thread has read the stream to its end.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (closed, changed) = &*self.closed;
        let mut guard = closed.lock().expect("output lock");
        while !*guard {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = changed
                .wait_timeout(guard, deadline - now)
                .expect("output lock")
                .0;
        }
        true
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.0.lock().expect("output lock").clone()
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// JSON lines whose `message` is `message`.
    pub fn count_messages(&self, message: &str) -> usize {
        self.lines()
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value["message"] == message)
            .count()
    }

    /// Waits until at least `count` JSON lines have `message` as their `message`.
    pub fn wait_for_messages(&self, message: &str, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (lines, changed) = &*self.lines;
        let mut guard = lines.lock().expect("output lock");
        loop {
            let found = guard
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|value| value["message"] == message)
                .count();
            if found >= count {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = changed
                .wait_timeout(guard, deadline - now)
                .expect("output lock")
                .0;
        }
    }

    /// Waits for the JSON line whose `message` is `message`.
    pub fn wait_for_message(&self, message: &str, timeout: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        let (lines, changed) = &*self.lines;
        let mut guard = lines.lock().expect("output lock");
        loop {
            if let Some(found) = guard
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|value| value["message"] == message)
            {
                return Some(found);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            guard = changed
                .wait_timeout(guard, deadline - now)
                .expect("output lock")
                .0;
        }
    }
}

/// The identity service running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    pub address: SocketAddr,
    /// The health port, which serves only `/healthz` and `/readyz` (the kubelet probes it).
    pub health_address: SocketAddr,
}

impl Service {
    /// Starts the service on a free local port with `env` and waits until it listens. The
    /// environment is otherwise empty, so the developer's own variables cannot leak in.
    pub fn start(env: &[(&str, &str)]) -> Self {
        let (child, stdout, stderr) = spawn(env);
        let listening = stdout
            .wait_for_message("listening", Duration::from_secs(30))
            .unwrap_or_else(|| {
                panic!(
                    "the service did not start listening\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                )
            });
        let parse = |field: &str| -> SocketAddr {
            listening["fields"][field]
                .as_str()
                .and_then(|address| address.parse().ok())
                .unwrap_or_else(|| panic!("no {field} in {listening}"))
        };
        Self {
            child,
            stdout,
            stderr,
            address: parse("address"),
            health_address: parse("health_address"),
        }
    }

    /// Waits for `/readyz` on the health port to answer 200.
    pub fn wait_ready(&self, timeout: Duration) {
        self.wait_readiness(200, timeout);
    }

    /// Waits until `/readyz` on the health port answers `status`.
    pub fn wait_readiness(&self, status: u16, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if request(self.health_address, "GET", "/readyz", &[]).is_ok_and(|r| r.status == status)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "readiness did not answer {status} within {timeout:?}\nstdout:\n{}",
                self.stdout.text()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stops the service with SIGTERM and returns everything it printed.
    pub fn stop(mut self) -> String {
        let _ = Command::new("kill")
            .args(["-s", "TERM", &self.child.id().to_string()])
            .status();
        let status = wait_exit(&mut self.child, Duration::from_secs(30));
        assert!(status.is_some(), "the service did not stop");
        for output in [&self.stdout, &self.stderr] {
            assert!(output.wait_closed(Duration::from_secs(10)));
        }
        format!("{}\n{}", self.stdout.text(), self.stderr.text())
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Spawns the service without waiting for it, with a short drain delay plus `env`.
pub fn spawn(env: &[(&str, &str)]) -> (Child, Output, Output) {
    let mut command = Command::new(IDENTITY);
    command
        .env_clear()
        .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
        .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
        .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
        .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start the identity service");
    let stdout = Output::default();
    let stderr = Output::default();
    stdout
        .clone()
        .collect(child.stdout.take().expect("piped stdout"));
    stderr
        .clone()
        .collect(child.stderr.take().expect("piped stderr"));
    (child, stdout, stderr)
}

/// Runs the service until it exits on its own (a refused startup) and returns the exit status
/// and everything it printed.
pub fn run_to_exit(env: &[(&str, &str)]) -> (ExitStatus, String) {
    let (mut child, stdout, stderr) = spawn(env);
    let status = wait_exit(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("the service kept running\nstdout:\n{}", stdout.text())
    });
    for output in [&stdout, &stderr] {
        assert!(output.wait_closed(Duration::from_secs(10)));
    }
    (status, format!("{}\n{}", stdout.text(), stderr.text()))
}

pub fn wait_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A parsed HTTP response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {}", self.body))
    }

    /// Every value of a header.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The whole response as text, for leak checks.
    pub fn raw(&self) -> String {
        let head: Vec<String> = self
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        format!("{}\n{}\n{}", self.status, head.join("\n"), self.body)
    }
}

/// Sends one HTTP/1.1 request on a fresh connection. `headers` may include `Host`; otherwise
/// the service's address is the host.
pub fn request(
    address: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> std::io::Result<Response> {
    request_with_body(address, method, target, headers, b"")
}

/// [`request`] with a body, sent with its `Content-Length`.
pub fn request_with_body(
    address: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut head = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        head.push_str(&format!("Host: {address}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("no end of headers"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("no status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    });
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_owned()
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut rest: &str) -> String {
    let mut body = String::new();
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let Some(chunk) = after.get(..size) else {
            break;
        };
        body.push_str(chunk);
        rest = after.get(size + 2..).unwrap_or("");
    }
    body
}

/// Fails the test in CI, where every prerequisite must be present; skips it elsewhere.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
pub fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever works.
pub fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs a container runtime command and returns its trimmed stdout.
pub fn runtime_output(runtime: &str, args: &[&str]) -> String {
    let output = Command::new(runtime)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{runtime} {args:?}: {error}"));
    assert!(
        output.status.success(),
        "{runtime} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A unique name for a container created by this test process.
pub fn unique_name(kind: &str) -> String {
    format!("identity-test-{kind}-{}-{}", std::process::id(), next_id())
}

/// A container that is removed when dropped.
pub struct Container {
    pub runtime: &'static str,
    pub name: String,
}

impl Container {
    /// The host address mapped to `port` inside the container.
    pub fn host_address(&self, port: &str) -> SocketAddr {
        let mapping = runtime_output(self.runtime, &["port", &self.name, port]);
        mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for {port}: {mapping}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            // `--volumes`: the image's anonymous volumes go with the container (Laysi/trustiot-v2#334).
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A TCP proxy in front of Postgres that holds every new connection for `delay` before it
/// reaches the server, so opening a database connection (TCP, TLS, SCRAM, service-kit's session
/// check) takes longer than one ext_authz check may (finding F1 of
/// docs/test-reports/2026-10-11-61-verify.md) without depending on machine load. It can also cut
/// every connection it carries, as a database failover does, and hold new connections for as long
/// as the test needs ([`SlowProxy::pause`]), so a state the service passes through while it
/// reconnects lasts until the test has seen it.
pub struct SlowProxy {
    pub port: u16,
    carried: Arc<Mutex<Vec<TcpStream>>>,
    accepted: Arc<AtomicU32>,
    paused: Arc<(Mutex<bool>, Condvar)>,
}

impl SlowProxy {
    /// Listens on a free local port and forwards to `upstream` after `delay`.
    pub fn start(upstream: SocketAddr, delay: Duration) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let carried = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicU32::new(0));
        let paused = Arc::new((Mutex::new(false), Condvar::new()));
        let (all, count, gate) = (
            Arc::clone(&carried),
            Arc::clone(&accepted),
            Arc::clone(&paused),
        );
        thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let all = Arc::clone(&all);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    let (lock, resumed) = &*gate;
                    drop(
                        resumed
                            .wait_while(lock.lock().expect("proxy gate"), |paused| *paused)
                            .expect("proxy gate"),
                    );
                    thread::sleep(delay);
                    let Ok(server) = TcpStream::connect(upstream) else {
                        return;
                    };
                    if let (Ok(c), Ok(s)) = (client.try_clone(), server.try_clone()) {
                        all.lock().expect("proxy lock").extend([c, s]);
                    }
                    forward(client, server);
                });
            }
        });
        Self {
            port,
            carried,
            accepted,
            paused,
        }
    }

    /// Holds every new connection until [`SlowProxy::resume`]; the proxy still accepts them.
    pub fn pause(&self) {
        *self.paused.0.lock().expect("proxy gate") = true;
    }

    /// Lets held and new connections through again, each after the proxy's delay.
    pub fn resume(&self) {
        *self.paused.0.lock().expect("proxy gate") = false;
        self.paused.1.notify_all();
    }

    /// Connections the proxy has accepted so far.
    pub fn accepted(&self) -> u32 {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Closes every connection the proxy carries, as a database failover would.
    pub fn cut(&self) {
        for stream in self.carried.lock().expect("proxy lock").drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Copies both directions until either side closes.
fn forward(client: TcpStream, server: TcpStream) {
    let (Ok(mut client_read), Ok(mut server_write)) = (client.try_clone(), server.try_clone())
    else {
        return;
    };
    let upstream = thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut server_write);
        let _ = server_write.shutdown(std::net::Shutdown::Both);
    });
    let (mut server_read, mut client_write) = (server, client);
    let _ = std::io::copy(&mut server_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Both);
    let _ = upstream.join();
}

/// Creates the function that runs service-kit's migration records template for schema
/// `identity` (`example_schema`) and the service role `identity_app` (`example_app`). The
/// template is a bound parameter and the names are fixed, so no SQL is built in Rust.
const RECORDS_FUNCTION: &str = "CREATE FUNCTION pg_temp.identity_migration_records(template text)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE replace(replace(template, 'example_schema', 'identity'), 'example_app', 'identity_app');
END
$$";
const RUN_RECORDS_FUNCTION: &str = "SELECT pg_temp.identity_migration_records($1)";
const RECORD: &str = "INSERT INTO identity.trustiot_migrate_applied (component, name, sha256)
VALUES ($1, $2, $3)";

/// Stands in for the migration Job's records in a database these tests set up by hand: creates
/// `identity.trustiot_migrate_applied` from service-kit's template and records every file the
/// identity image expects (`identity::schema::SCHEMA`), so the service's `migrations` readiness
/// check passes. These tests apply only `0001_identity.sql`, which is all the ext_authz check
/// needs; the readiness check itself runs against the real Job in tests/migrations.rs. Run it as
/// the schema's owner.
pub async fn record_release_migrations(owner: &mut sqlx::PgConnection) {
    sqlx::raw_sql(RECORDS_FUNCTION)
        .execute(&mut *owner)
        .await
        .expect("create the records helper");
    sqlx::query(RUN_RECORDS_FUNCTION)
        .bind(service_kit::migrations::RECORDS_TEMPLATE)
        .execute(&mut *owner)
        .await
        .expect("create the migration records");
    for record in identity::schema::SCHEMA.expected() {
        sqlx::query(RECORD)
            .bind(&record.component)
            .bind(&record.name)
            .bind(&record.sha256)
            .execute(&mut *owner)
            .await
            .unwrap_or_else(|error| panic!("record {record}: {error}"));
    }
}
