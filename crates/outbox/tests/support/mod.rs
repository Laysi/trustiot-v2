//! Helpers shared by the integration tests: a Postgres to test against, fresh producer and
//! consumer schemas per test, a test event, and running `outbox-example` as a real process.
//!
//! The Postgres and process helpers follow `crates/service-kit/tests/support` (same image, same
//! `TEST_DATABASE_URL` and `CI` rules), until a shared test crate exists.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contract::{Event, ServiceName};
use outbox::{Consumer, Handler, Outcome};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};

/// Same image and digest as the kit's tests.
const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// The example service's binary, built by Cargo for these tests.
pub const EXAMPLE: &str = env!("CARGO_BIN_EXE_outbox-example");

/// The event the tests write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Counted {
    pub n: i64,
}

impl Event for Counted {
    const TYPE: &'static str = "example.counted";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
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
fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn runtime_output(runtime: &str, args: &[&str]) -> String {
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

/// A name unique within this test process, usable as a Postgres identifier.
pub fn unique(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{kind}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A container that is removed when dropped.
struct Container {
    runtime: &'static str,
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A database for one test; the container, if any, is removed when dropped.
pub struct Database {
    pub url: String,
    _container: Option<Container>,
}

/// `TEST_DATABASE_URL` (a superuser connection; each test makes its own schemas and roles), or
/// a throwaway Postgres container. `None` when neither is available outside CI.
pub async fn database() -> Option<Database> {
    if let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return Some(Database {
            url,
            _container: None,
        });
    }
    let Some(runtime) = container_runtime() else {
        unavailable("TEST_DATABASE_URL, podman, or docker");
        return None;
    };
    let container = Container {
        runtime,
        name: format!("outbox-test-{}", unique("postgres").replace('_', "-")),
    };
    runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--name",
            &container.name,
            "--env",
            "POSTGRES_PASSWORD=outbox-test",
            "--publish",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ],
    );
    let mapping = runtime_output(runtime, &["port", &container.name, "5432/tcp"]);
    let address: SocketAddr = mapping
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .unwrap_or_else(|| panic!("no host address for 5432: {mapping}"));
    let url = format!("postgres://postgres:outbox-test@{address}/postgres");
    // The image runs initdb with networking off and then restarts; wait for the real server.
    let started = Instant::now();
    loop {
        if let Ok(pool) = PgPool::connect(&url).await
            && sqlx::query("SELECT 1").execute(&pool).await.is_ok()
        {
            pool.close().await;
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "Postgres did not start in time"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Some(Database {
        url,
        _container: Some(container),
    })
}

impl Database {
    /// A pool as the database's own user.
    pub async fn admin(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&self.url)
            .await
            .expect("connect as admin")
    }

    /// A pool whose `search_path` is `schema`, as the database's own user.
    pub async fn pool(&self, schema: &str) -> PgPool {
        self.pool_as(schema, None).await
    }

    /// A pool whose `search_path` is `schema`, optionally as another user (password = name).
    pub async fn pool_as(&self, schema: &str, user: Option<&str>) -> PgPool {
        let mut options = PgConnectOptions::from_str(&self.url)
            .expect("parse the database URL")
            .options([("search_path", schema)]);
        if let Some(user) = user {
            options = options.username(user).password(user);
        }
        PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options)
            .await
            .expect("connect a pool")
    }

    /// Creates an empty schema.
    pub async fn create_schema(&self, schema: &str) {
        let admin = self.admin().await;
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create a schema");
        admin.close().await;
    }

    /// A fresh producer schema with the outbox tables and a `widget` table, and a fresh
    /// consumer schema with the consumer tables and a `seen` table for [`Recorder`].
    pub async fn setup(&self) -> Setup {
        let producer_schema = unique("producer");
        let consumer_schema = unique("consumer");
        self.create_schema(&producer_schema).await;
        self.create_schema(&consumer_schema).await;
        let producer = self.pool(&producer_schema).await;
        let consumer = self.pool(&consumer_schema).await;
        let mut conn = producer.acquire().await.expect("producer connection");
        outbox::schema::install_producer(&mut conn)
            .await
            .expect("install the producer tables");
        // Installing twice is harmless: the migration job runs it on every deployment.
        outbox::schema::install_producer(&mut conn)
            .await
            .expect("install the producer tables again");
        sqlx::raw_sql(
            "CREATE TABLE widget (id text PRIMARY KEY, name text NOT NULL); \
             CREATE TABLE scratch (n integer)",
        )
        .execute(&mut *conn)
        .await
        .expect("producer business tables");
        drop(conn);
        let mut conn = consumer.acquire().await.expect("consumer connection");
        outbox::schema::install_consumer(&mut conn)
            .await
            .expect("install the consumer tables");
        outbox::schema::install_consumer(&mut conn)
            .await
            .expect("install the consumer tables again");
        sqlx::raw_sql(SEEN_DDL)
            .execute(&mut *conn)
            .await
            .expect("consumer business tables");
        drop(conn);
        Setup {
            producer_schema,
            consumer_schema,
            producer,
            consumer,
        }
    }
}

/// What [`Recorder`] writes: one row per processed event, plus a per-aggregate high-water
/// mark, and `violations` when an aggregate's events arrive out of order.
const SEEN_DDL: &str = "\
    CREATE TABLE seen (event_id uuid PRIMARY KEY, subscription text NOT NULL, \
        aggregate_type text NOT NULL, aggregate_id text NOT NULL, seq bigint NOT NULL, \
        n bigint NOT NULL, handled_at timestamptz NOT NULL DEFAULT clock_timestamp()); \
    CREATE TABLE aggregate_state (aggregate_id text PRIMARY KEY, last_seq bigint NOT NULL, \
        total bigint NOT NULL, digest text NOT NULL); \
    CREATE TABLE violations (event_id uuid NOT NULL, expected bigint, found bigint NOT NULL)";

/// The two schemas and pools of one test.
pub struct Setup {
    pub producer_schema: String,
    pub consumer_schema: String,
    /// `search_path` = the producer schema.
    pub producer: PgPool,
    /// `search_path` = the consumer schema.
    pub consumer: PgPool,
}

impl Setup {
    /// Writes `Counted { n }` for `aggregate` in its own committed transaction.
    pub async fn publish(&self, aggregate: &str, n: i64) -> outbox::Published {
        let mut tx = self.producer.begin().await.expect("begin");
        let published = outbox::publish(
            &mut tx,
            &outbox::Aggregate::new("widget", aggregate).expect("aggregate"),
            &Counted { n },
            None,
        )
        .await
        .expect("publish");
        tx.commit().await.expect("commit");
        published
    }

    /// Rows of the producer's log.
    pub async fn event_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM outbox_event")
            .fetch_one(&self.producer)
            .await
            .expect("count events")
    }

    /// `(aggregate_id, last_seq, total, digest)` per aggregate, the [`Recorder`]'s state.
    pub async fn recorded_state(&self) -> Vec<(String, i64, i64, String)> {
        sqlx::query_as(
            "SELECT aggregate_id, last_seq, total, digest FROM aggregate_state ORDER BY 1",
        )
        .fetch_all(&self.consumer)
        .await
        .expect("read the recorded state")
    }

    /// How many events the [`Recorder`] processed.
    pub async fn seen_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM seen")
            .fetch_one(&self.consumer)
            .await
            .expect("count seen")
    }

    /// Out-of-order deliveries the [`Recorder`] noticed.
    pub async fn violations(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM violations")
            .fetch_one(&self.consumer)
            .await
            .expect("count violations")
    }
}

/// A handler that records every event and folds each aggregate's events into a running total
/// and an order-sensitive digest. A second processing of the same event fails on `seen`'s
/// primary key (and is retried forever, so a test would notice), and an event whose sequence
/// number does not follow the aggregate's last one is recorded in `violations`.
pub struct Recorder {
    pub subscription: String,
}

impl Handler for Recorder {
    async fn handle(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
        delivery: &outbox::Delivery,
    ) -> Result<(), outbox::HandlerError> {
        let event: Counted = delivery.envelope.decode()?;
        let last: Option<i64> = sqlx::query_scalar(
            "SELECT last_seq FROM aggregate_state WHERE aggregate_id = $1 FOR UPDATE",
        )
        .bind(delivery.aggregate.id())
        .fetch_optional(&mut **tx)
        .await?;
        let expected = last.unwrap_or(0) + 1;
        if delivery.aggregate_seq != expected {
            sqlx::query("INSERT INTO violations (event_id, expected, found) VALUES ($1, $2, $3)")
                .bind(delivery.envelope.id.as_uuid())
                .bind(expected)
                .bind(delivery.aggregate_seq)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO seen (event_id, subscription, aggregate_type, aggregate_id, seq, n) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(delivery.envelope.id.as_uuid())
        .bind(&self.subscription)
        .bind(delivery.aggregate.kind())
        .bind(delivery.aggregate.id())
        .bind(delivery.aggregate_seq)
        .bind(event.n)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO aggregate_state (aggregate_id, last_seq, total, digest) \
             VALUES ($1, $2, $3, md5($3::text)) \
             ON CONFLICT (aggregate_id) DO UPDATE SET last_seq = EXCLUDED.last_seq, \
             total = aggregate_state.total + EXCLUDED.total, \
             digest = md5(aggregate_state.digest || ':' || EXCLUDED.total::text)",
        )
        .bind(delivery.aggregate.id())
        .bind(delivery.aggregate_seq)
        .bind(event.n)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

/// Polls until every committed event is processed (the lag is 0); panics on a retry outcome or
/// after `timeout`.
///
/// Polling alone can report caught up while committed events wait: a consumer reads only up
/// to the oldest write transaction running anywhere in the cluster, and tests running in
/// parallel on one database hold such transactions.
pub async fn drain<H: Handler>(consumer: &Consumer<H>, timeout: Duration) -> outbox::Progress {
    drain_until(consumer, timeout, true).await
}

/// Polls until a poll reports caught up, even if committed events still wait behind a running
/// transaction.
pub async fn drain_readable<H: Handler>(
    consumer: &Consumer<H>,
    timeout: Duration,
) -> outbox::Progress {
    drain_until(consumer, timeout, false).await
}

async fn drain_until<H: Handler>(
    consumer: &Consumer<H>,
    timeout: Duration,
    until_no_lag: bool,
) -> outbox::Progress {
    let deadline = Instant::now() + timeout;
    let mut total = outbox::Progress::default();
    loop {
        let progress = consumer.poll_once().await.expect("poll");
        total.delivered += progress.delivered;
        total.duplicates += progress.duplicates;
        total.dead_lettered += progress.dead_lettered;
        match progress.outcome {
            Outcome::CaughtUp => {
                if !until_no_lag || consumer.measure_lag().await.expect("lag").events == 0 {
                    return total;
                }
            }
            Outcome::MoreAvailable | Outcome::Busy => {}
            Outcome::Retry { error, .. } => panic!("the handler asked for a retry: {error}"),
        }
        assert!(Instant::now() < deadline, "the consumer did not catch up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- outbox-example as a child process ----

/// Lines a child process printed, shared with the threads that read them.
#[derive(Clone, Default)]
pub struct Output {
    lines: Arc<(Mutex<Vec<String>>, Condvar)>,
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
        });
    }

    /// Everything so far, newline-separated.
    pub fn text(&self) -> String {
        self.lines.0.lock().expect("output lock").join("\n")
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
                .find(|line| line["message"] == message)
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

/// `outbox-example` running as a child process; killed when dropped.
pub struct Example {
    child: Child,
    pub stdout: Output,
    pub address: SocketAddr,
}

impl Example {
    /// Starts the example against `setup`'s schemas with `env` on top, and waits until it
    /// listens. The environment is otherwise empty.
    pub fn start(database: &Database, setup: &Setup, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(EXAMPLE);
        command
            .env_clear()
            .env("TRUSTIOT__DATABASE_URL", &database.url)
            .env("TRUSTIOT__PRODUCER_SCHEMA", &setup.producer_schema)
            .env("TRUSTIOT__CONSUMER_SCHEMA", &setup.consumer_schema)
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__CONSUMER__POLL_INTERVAL", "100ms")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start outbox-example");
        let stdout = Output::default();
        let stderr = Output::default();
        stdout
            .clone()
            .collect(child.stdout.take().expect("piped stdout"));
        stderr
            .clone()
            .collect(child.stderr.take().expect("piped stderr"));
        let listening = stdout
            .wait_for_message("listening", Duration::from_secs(30))
            .unwrap_or_else(|| {
                panic!(
                    "outbox-example did not start\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                )
            });
        let address = listening["fields"]["address"]
            .as_str()
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| panic!("no address in {listening}"));
        Self {
            child,
            stdout,
            address,
        }
    }

    /// Kills the process with SIGKILL, as a node failure or the OOM killer would, and reaps it.
    pub fn kill(mut self) {
        let status = Command::new("kill")
            .args(["-s", "KILL", &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -s KILL failed");
        let status = self.child.wait().expect("reap the child");
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(9),
            "the process was not killed by SIGKILL: {status}"
        );
    }

    /// `PUT /v1/widgets/{id}` with a new name; returns the status and body.
    pub fn rename(&self, id: &str, name: &str) -> (u16, serde_json::Value) {
        let body = serde_json::json!({ "name": name }).to_string();
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))
            .expect("connect to the example");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        let request = format!(
            "PUT /v1/widgets/{id} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.address,
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("send");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).expect("read the response");
        let (head, body) = raw.split_once("\r\n\r\n").expect("a complete response");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        (status, serde_json::from_str(body).unwrap_or_default())
    }
}

impl Drop for Example {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Polls `check` until it returns `Some`, or panics after `timeout`.
pub async fn eventually<T, F, Fut>(timeout: Duration, what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- a fake OTLP/HTTP receiver ----

/// The path and body of one request the fake collector received.
type Request = (String, Vec<u8>);

/// Accepts OTLP/HTTP requests and records their paths and bodies.
pub struct FakeCollector {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Request>>>,
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
fn read_request(stream: TcpStream) -> Option<Request> {
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
