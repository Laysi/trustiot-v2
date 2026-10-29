//! Helpers for the service's integration tests: the ingest binary as a real process on the
//! product's Postgres schema (`testkit::product`), its JSON log and its readiness, and waiting
//! for a condition. ClickHouse comes from `tests/common` (the schema tests' container), the
//! broker from [`broker`].
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

pub mod broker;

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::ProductPostgres;

/// The ingest binary, built by Cargo for these tests.
pub const INGEST: &str = env!("CARGO_BIN_EXE_ingest");

/// The longest a started service may take to listen, to become ready, or to log a readiness
/// failure. Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
pub const START_LIMIT: Duration = Duration::from_secs(60);

/// The longest the consumer may take to apply an event once it is committed. It polls every
/// second when idle; the bound is for a loaded host.
pub const EVENT_LIMIT: Duration = Duration::from_secs(60);

/// A local TCP port that nothing listens on: bound once to get a free port, then released.
pub fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("the bound address").port()
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

/// Where the service finds ClickHouse and which account it logs in with.
pub struct ClickHouseTarget {
    pub url: String,
    pub user: String,
    pub password: String,
}

/// The ingest service running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    health: SocketAddr,
}

impl Service {
    /// Starts the service on free local ports against `product` (with its Postgres port replaced
    /// by `postgres_port` when given), `clickhouse` and the broker of `mqtt`, with the roles the
    /// chart gives it, and waits until it listens. The environment is otherwise empty, so the
    /// developer's own variables cannot leak in.
    pub async fn start(
        product: &ProductPostgres,
        postgres_port: Option<u16>,
        clickhouse: &ClickHouseTarget,
        mqtt: &mqtt_client::Config,
    ) -> Self {
        let mut command = Command::new(INGEST);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT__DATABASE__USERNAME", "ingest_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT__DATABASE__PLATFORM_USERNAME", "ingest_platform")
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT_CLICKHOUSE_URL", &clickhouse.url)
            .env("TRUSTIOT__CLICKHOUSE__USERNAME", &clickhouse.user)
            .env("TRUSTIOT__CLICKHOUSE__PASSWORD", &clickhouse.password)
            .env("TRUSTIOT__MQTT__HOST", &mqtt.host)
            .env("TRUSTIOT__MQTT__PORT", mqtt.port.to_string())
            .env("TRUSTIOT__MQTT__USERNAME", &mqtt.username)
            .env("TRUSTIOT__MQTT__PASSWORD_FILE", &mqtt.password_file)
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(port) = postgres_port {
            command.env("TRUSTIOT_POSTGRES_PORT", port.to_string());
        }
        let mut child = command.spawn().expect("start the ingest service");
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
                "the ingest service did not log `listening` within {START_LIMIT:?}\n\
                 stdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let health = listening["fields"]["health_address"]
            .as_str()
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| panic!("no health_address in {listening}"));
        Self {
            child,
            stdout,
            stderr,
            health,
        }
    }

    /// `GET /readyz`: the status and the body.
    pub async fn readiness(&self) -> (u16, String) {
        let response = reqwest::Client::new()
            .get(format!("http://{}/readyz", self.health))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .expect("call /readyz");
        let status = response.status().as_u16();
        (status, response.text().await.expect("read the body"))
    }

    /// Waits until `/readyz` answers 200.
    pub async fn wait_ready(&self) {
        self.wait_ready_within(START_LIMIT).await;
    }

    /// Waits up to `limit` until `/readyz` answers 200.
    pub async fn wait_ready_within(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            if self.readiness().await.0 == 200 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "/readyz of the ingest service did not answer 200 within {limit:?}\n\
                 stdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits until the service logs that the readiness check `check` failed, and returns the
    /// log line.
    pub async fn wait_check_failed(&self, check: &str) -> Value {
        wait_for(
            &format!("the ingest service logging that its `{check}` readiness check failed"),
            START_LIMIT,
            || async {
                // Readiness runs its checks when it is asked.
                let _ = self.readiness().await;
                self.stdout
                    .messages("readiness check failed")
                    .into_iter()
                    .find(|line| line["fields"]["check"] == check)
            },
        )
        .await
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

/// A binding row as the consumer of `assets.gateway_bound` records it: tenant, binding, hardware
/// ID and the event's position in the assets service's log.
const INSERT_BINDING: &str = "INSERT INTO ingest.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id)
VALUES ($1::uuid, $2::uuid, $3, now(), $4, 0)";

/// Binds `gateway` to `tenant` in ingest's projection, written directly as the superuser (the
/// projection's consumer is `tests/gateway_bindings.rs`'s), and returns the binding ID. A later
/// binding of the same gateway needs a larger `position`.
pub async fn bind(
    product: &ProductPostgres,
    gateway: &protocol::GatewayId,
    tenant: uuid::Uuid,
    position: i64,
) -> uuid::Uuid {
    let binding = uuid::Uuid::new_v4();
    let mut admin = product.superuser().await;
    sqlx::query(INSERT_BINDING)
        .bind(tenant.to_string())
        .bind(binding.to_string())
        .bind(gateway.as_str())
        .bind(position)
        .execute(&mut admin)
        .await
        .expect("record the binding");
    binding
}

impl Service {
    /// Waits until the service logs `message`, and returns the first such line.
    pub async fn wait_logged(&self, message: &str) -> Value {
        wait_for(
            &format!("the ingest service logging `{message}`"),
            START_LIMIT,
            || async { self.stdout.messages(message).into_iter().next() },
        )
        .await
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
