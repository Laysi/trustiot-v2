//! Helpers for the integration tests: the assets binary as a real process on the product's
//! schema (`testkit::product`), its JSON log, and plain HTTP calls to it.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::ProductPostgres;

/// The assets binary, built by Cargo for these tests.
pub const ASSETS: &str = env!("CARGO_BIN_EXE_assets");

/// The longest a started service may take to listen and to become ready. Generous: test hosts
/// are often loaded (CLAUDE.md, "Test time limits").
pub const START_LIMIT: Duration = Duration::from_secs(60);

/// The longest the consumer may take to apply an event once it is committed. It polls every
/// second when idle; the bound is for a loaded host.
pub const EVENT_LIMIT: Duration = Duration::from_secs(60);

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

/// The assets service running as a child process.
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
    /// keys `jwks`, with the roles the chart gives it, and waits until it is ready. The
    /// environment is otherwise empty, so the developer's own variables cannot leak in.
    pub async fn start(product: &ProductPostgres, jwks: &str) -> Self {
        let mut command = Command::new(ASSETS);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT_INTERNAL_IDENTITY_KEYS", jwks)
            .env("TRUSTIOT__DATABASE__USERNAME", "assets_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT__DATABASE__PLATFORM_USERNAME", "assets_platform")
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the assets service");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + START_LIMIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            assert!(
                Instant::now() < deadline,
                "the assets service did not log `listening` within {START_LIMIT:?}\nstdout:\n{}\nstderr:\n{}",
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
        let service = Self {
            child,
            base_url: format!("http://{}", address("address")),
            health: address("health_address"),
            stdout,
            stderr,
        };
        service.wait_ready().await;
        service
    }

    /// Waits until `/readyz` answers 200: both roles opened a session that passed service-kit's
    /// row-level security check.
    async fn wait_ready(&self) {
        let url = format!("http://{}/readyz", self.health);
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if let Ok(response) = reqwest::get(&url).await
                && response.status() == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the assets service was not ready within {START_LIMIT:?}\nstdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `GET path` with an `x-internal-identity` token (none: without the header); the status
    /// and the body.
    pub async fn get(&self, path: &str, token: Option<&str>) -> (u16, String) {
        let mut request = reqwest::Client::new()
            .get(format!("{}{path}", self.base_url))
            .timeout(Duration::from_secs(15));
        if let Some(token) = token {
            request = request.header("x-internal-identity", token);
        }
        let response = request.send().await.expect("call the assets service");
        let status = response.status().as_u16();
        (status, response.text().await.expect("read the body"))
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
