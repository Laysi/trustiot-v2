//! The assistant binary as a child process on the product's schema (`testkit::product`), as the
//! chart starts it: its JSON log, and plain HTTP calls to its API with signed internal identity
//! tokens (`testkit::identity`). Used by the API's tests.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contract::identity::{TenantGrant, TenantRole};
use contract::{ServiceName, TenantId, UserId};
use serde_json::Value;
use service_kit::identity::UserIdentity;
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::ProductPostgres;

/// The assistant binary, built by Cargo for these tests.
pub const ASSISTANT: &str = env!("CARGO_BIN_EXE_assistant");

/// The longest a started service may take to listen, to become ready, or to log what a test
/// waits for. Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
pub const START_LIMIT: Duration = Duration::from_secs(60);

/// The longest a log line may take to reach a test after the response it belongs to: a reader
/// thread collects the service's stdout, so a line can arrive after its response does.
pub const LOG_LIMIT: Duration = Duration::from_secs(30);

/// One request's limit. Generous for a loaded host; no request here waits on anything slow.
const REQUEST_LIMIT: Duration = Duration::from_secs(30);

/// An origin where nothing listens: a port the OS handed out and that was closed again.
pub fn closed_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let address = listener.local_addr().expect("local address");
    drop(listener);
    format!("http://{address}")
}

/// Writes `jwks` to a file of its own under Cargo's temporary directory for these tests and
/// returns its path.
pub fn keys_file(jwks: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "assistant-jwks-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, jwks).expect("write the keys file");
    path
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

    /// The JSON lines whose `message` is `message`.
    pub fn messages(&self, message: &str) -> Vec<Value> {
        self.0
            .lock()
            .expect("output lock")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["message"] == message)
            .collect()
    }
}

/// A user of a tenant, who gets a fresh token for every request (tokens live a minute).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct User {
    pub tenant: TenantId,
    pub id: UserId,
    pub role: TenantRole,
}

impl User {
    /// A new user with `role` in `tenant`.
    pub fn new(tenant: TenantId, role: TenantRole) -> Self {
        Self {
            tenant,
            id: UserId::generate(),
            role,
        }
    }

    /// A signed token of this user for the assistant, issued now.
    pub fn token(&self, issuer: &TestIssuer) -> String {
        issuer.token(&self.claims(ServiceName::Assistant))
    }

    /// The claims of this user for `audience`, issued now.
    pub fn claims(&self, audience: ServiceName) -> contract::identity::InternalClaims {
        let mut claims = TestIssuer::claims(
            audience,
            Some(TenantGrant {
                id: self.tenant,
                role: self.role,
            }),
        );
        claims.sub = self.id;
        claims
    }

    /// The verified identity a handler of the assistant gets for this user, for calling the
    /// store directly.
    pub fn identity(&self, issuer: &TestIssuer) -> UserIdentity {
        issuer.user(ServiceName::Assistant, &self.claims(ServiceName::Assistant))
    }

    /// The store's principal for this user.
    pub fn principal(&self) -> assistant::history::Principal {
        assistant::history::Principal {
            tenant_id: self.tenant,
            user_id: self.id,
        }
    }
}

/// The assistant running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    /// Where the API listens, as a base URL.
    pub base_url: String,
    health: SocketAddr,
}

impl Service {
    /// Starts the service on free local ports against `product`, trusting the internal identity
    /// keys `jwks` (written to a keys file, as the chart's ConfigMap volume holds them), with the
    /// roles the chart gives it (`assistant_app`, and `assistant_platform` for the purge) and
    /// `env` on top, and waits until it is ready. The AI gateway's
    /// address points where nothing listens: no test here asks a question. The environment is
    /// otherwise empty, so the developer's own variables cannot leak in.
    pub async fn start(product: &ProductPostgres, jwks: &str, env: &[(&str, &str)]) -> Self {
        let service = Self::spawn(product, jwks, env).await;
        service.wait_readiness(200).await;
        service
    }

    /// Like [`Service::start`], but returns once the service listens, ready or not.
    pub async fn spawn(product: &ProductPostgres, jwks: &str, env: &[(&str, &str)]) -> Self {
        let keys_file = keys_file(jwks);
        let mut command = Command::new(ASSISTANT);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE", &keys_file)
            .env("TRUSTIOT__DATABASE__USERNAME", "assistant_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env(
                "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                "assistant_platform",
            )
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT__AI_GATEWAY__URL", closed_origin())
            .envs(product.platform_env())
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the assistant");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + START_LIMIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!(
                    "the assistant exited with {status}\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the assistant did not log `listening` within {START_LIMIT:?}\nstdout:\n{}\nstderr:\n{}",
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
        }
    }

    /// The status `/readyz` answers now, if it answers. Readiness runs its checks only when it
    /// is asked, as the kubelet does.
    pub async fn readiness(&self) -> Option<u16> {
        reqwest::get(format!("http://{}/readyz", self.health))
            .await
            .ok()
            .map(|response| response.status().as_u16())
    }

    /// Asks `/readyz` until it answers `status`.
    pub async fn wait_readiness(&self, status: u16) {
        let what = format!("/readyz of the assistant answering {status}");
        wait_for(&what, START_LIMIT, || async {
            (self.readiness().await == Some(status)).then_some(())
        })
        .await;
    }

    /// Waits until the service logged `message`, and returns that line.
    pub async fn wait_logged(&self, message: &str) -> Value {
        let what = format!("the assistant logging `{message}`");
        wait_for(&what, LOG_LIMIT, || async {
            self.stdout.messages(message).into_iter().next()
        })
        .await
    }

    /// `method path` with an `x-internal-identity` token (none: without the header), and a body
    /// with its `Content-Type` (none: no body and no header).
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<(&str, Vec<u8>)>,
    ) -> Reply {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base_url))
            .timeout(REQUEST_LIMIT);
        if let Some(token) = token {
            request = request.header("x-internal-identity", token);
        }
        if let Some((content_type, body)) = body {
            request = request.header("content-type", content_type).body(body);
        }
        let response = request.send().await.expect("call the assistant");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.expect("read the body");
        Reply {
            status,
            headers,
            body,
        }
    }

    /// `method path` as `user`, with a JSON body (none: no body and no header).
    pub async fn as_user(
        &self,
        issuer: &TestIssuer,
        user: &User,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Reply {
        let token = user.token(issuer);
        let body = body.map(|body| {
            (
                "application/json",
                serde_json::to_vec(body).expect("serialize the body"),
            )
        });
        self.call(method, path, Some(&token), body).await
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

/// A response of the service.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}

impl Reply {
    /// The body as JSON; fails naming the status and the body when it is not.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("{} {}: not JSON: {error}", self.status, self.body))
    }

    /// The value of a response header, if it is there.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Fails unless the status is `status`, naming the body.
    #[track_caller]
    pub fn expect_status(&self, status: u16) -> &Self {
        assert_eq!(self.status, status, "{}", self.body);
        self
    }

    /// Fails unless this is an M1 error with `code` and `message_key`.
    #[track_caller]
    pub fn expect_error(&self, status: u16, code: &str, message_key: &str) {
        self.expect_status(status);
        let body = self.json();
        assert_eq!(body["code"], code, "{}", self.body);
        assert_eq!(body["message_key"], message_key, "{}", self.body);
    }
}

/// Polls `check` every 100 ms until it returns `Some`, for at most `limit`; fails naming `what`
/// was awaited.
pub async fn wait_for<T, F, Fut>(what: &str, limit: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(Instant::now() < deadline, "{what}: not within {limit:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
