//! Helpers shared by the integration tests: a Postgres to test against, fresh producer and
//! consumer schemas and roles per test, a test event, and running `outbox-example` as a real
//! process.
//!
//! The Postgres serves TLS and the consumer connects as services do (decision 29): it reads the
//! producer's `outbox_event` as its platform role, under the row-level security of
//! `crates/service-kit/sql/outbox-event.sql`, and processes events as its service role, through
//! service-kit pools that verify the server and check every session. The container and process
//! helpers follow `crates/service-kit/tests/support` (same image, same `CI` rule), until a
//! shared test crate exists.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

pub mod postgres;

/// The start of the Postgres container: one logic for every crate's tests (this file is in
/// crates/service-kit/tests, which outbox's tests include as identity's do). It needs the
/// `Container` and `unique_name` below.
#[path = "../../../service-kit/tests/support/start.rs"]
pub mod start;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contract::{Event, ServiceName, TenantId};
use outbox::{Consumer, ConsumerConfig, Handler, Outcome};
use serde::{Deserialize, Serialize};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};

use self::postgres::{TEST_PASSWORD, TlsPostgres};

/// The example service's binary, built by Cargo for these tests.
pub const EXAMPLE: &str = env!("CARGO_BIN_EXE_outbox-example");

/// Two tenants the tests write events for.
pub const TENANT_A: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
pub const TENANT_B: &str = "01928f6e-7a3c-7d4e-8f00-00000000000b";

/// [`TENANT_A`] or [`TENANT_B`].
pub fn tenant(text: &str) -> TenantId {
    text.parse().expect("a tenant ID")
}

/// The tenant the tests' events of `aggregate` belong to: [`TENANT_A`] or [`TENANT_B`], fixed
/// per aggregate, so a test with several aggregates delivers in both tenants.
pub fn tenant_of(aggregate: &str) -> TenantId {
    let odd = aggregate.bytes().map(u32::from).sum::<u32>() % 2 == 1;
    tenant(if odd { TENANT_B } else { TENANT_A })
}

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

/// A name unique within this test process, usable as a Postgres identifier.
pub fn unique(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{kind}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A name unique to this test process, for a container.
pub fn unique_name(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "outbox-test-{kind}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
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

/// Runs once per Postgres, as decision 29's database template prescribes: no temporary tables,
/// which service-kit refuses.
const INIT: &str = include_str!("../../../service-kit/sql/database.sql");

/// A Postgres with TLS for one test; the container is removed when dropped.
pub struct Database {
    pub postgres: TlsPostgres,
    /// The superuser, without TLS, for the example service's migration connection.
    pub url: String,
}

/// A throwaway Postgres with TLS (podman or docker). `None` when there is no container runtime
/// outside CI. `TEST_DATABASE_URL` is not used: the tests create roles as the superuser and
/// need a server whose CA they know.
pub async fn database() -> Option<Database> {
    Some(with_url(TlsPostgres::start(INIT).await?))
}

/// Like [`database`], for a test that restarts the container (`Database::postgres.restart()`):
/// the host port is chosen by the test, so it is the same after the restart on podman and Docker.
pub async fn restartable_database() -> Option<Database> {
    Some(with_url(TlsPostgres::start_restartable(INIT).await?))
}

fn with_url(postgres: TlsPostgres) -> Database {
    let url = format!(
        "postgres://postgres:{TEST_PASSWORD}@127.0.0.1:{}/postgres",
        postgres.address.port()
    );
    Database { postgres, url }
}

impl Database {
    /// A pool as the superuser.
    pub async fn admin(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(self.postgres.options("postgres"))
            .await
            .expect("connect as admin")
    }

    /// A pool whose `search_path` is `schema`, as the superuser.
    pub async fn pool(&self, schema: &str) -> PgPool {
        self.pool_as(schema, "postgres").await
    }

    /// A plain pool (no session check) whose `search_path` is `schema`, as `user`.
    pub async fn pool_as(&self, schema: &str, user: &str) -> PgPool {
        let options = self
            .postgres
            .options(user)
            .options([("search_path", schema)]);
        PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options)
            .await
            .expect("connect a pool")
    }

    /// Runs `sql` as the superuser; identifiers come from [`unique`], never from input.
    pub async fn admin_sql(&self, sql: String) {
        let admin = self.admin().await;
        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .expect("admin statement");
        admin.close().await;
    }

    /// Lets `role` read the producer's `outbox_event` (schema usage and `SELECT`), as a
    /// migration mistake would; with `policy`, also a policy `service_read` that opens every
    /// row to it, like `consumer_read` does for the platform role.
    pub async fn grant_log_read(&self, setup: &Setup, role: &str, policy: bool) {
        let ps = &setup.producer_schema;
        let mut sql = format!(
            "GRANT USAGE ON SCHEMA {ps} TO {role}; GRANT SELECT ON {ps}.outbox_event TO {role};"
        );
        if policy {
            sql.push_str(&format!(
                " CREATE POLICY service_read ON {ps}.outbox_event FOR SELECT TO {role} USING (true);"
            ));
        }
        self.admin_sql(sql).await;
    }

    /// Replaces the template's `consumer_read` with a read policy for the consumer's platform
    /// role that opens only platform-wide events (`tenant_id IS NULL`), as a migration mistake
    /// would. Service-kit's session check accepts it, since the policy is not the tenant
    /// condition, but the reader cannot see every tenant's events.
    pub async fn narrow_log_read(&self, setup: &Setup) {
        let (ps, reader) = (&setup.producer_schema, &setup.consumer_platform);
        self.admin_sql(format!(
            "DROP POLICY consumer_read ON {ps}.outbox_event; \
             CREATE POLICY platform_wide_read ON {ps}.outbox_event FOR SELECT TO {reader} \
                 USING (tenant_id IS NULL);"
        ))
        .await;
    }

    /// Creates an empty schema.
    pub async fn create_schema(&self, schema: &str) {
        self.admin_sql(format!("CREATE SCHEMA {schema}")).await;
    }

    /// A consuming service's pools, as a replica makes them: `platform_role`'s and
    /// `service_role`'s, both checked by service-kit. A refused session fails a query after
    /// `acquire_timeout`.
    pub fn consumer_pools(
        &self,
        platform_role: &str,
        service_role: &str,
        acquire_timeout: Duration,
    ) -> (PlatformDb, BackgroundTenantDb) {
        let pool = || {
            PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(acquire_timeout)
        };
        let platform = PlatformDb::connect_lazy(self.postgres.options(platform_role), pool())
            .expect("verify-full options");
        let (_, work) = TenantDb::connect_lazy_with_background(
            self.postgres.options(service_role),
            pool(),
            ServiceIdentity::new(ServiceName::Assets),
        )
        .expect("verify-full options");
        (platform, work)
    }

    /// A fresh producer schema with the outbox tables and a `widget` table, and a fresh
    /// consumer schema with the consumer tables and a `seen` table for [`Recorder`], plus the
    /// roles of decision 29 with the grants in the outbox README:
    ///
    /// - The tables belong to the superuser, the migration role.
    /// - `<producer>_app` and `<producer>_platform` are the producer's service and platform
    ///   roles; `outbox_event` and `outbox_aggregate` have row-level security from service-kit's
    ///   templates.
    /// - `<consumer>_app` is the consumer's service role. Its `search_path` is the consumer
    ///   schema, and it may use every table created there (default privileges).
    /// - `<consumer>_platform` is the consumer's platform role, the only one the template's
    ///   `consumer_read` policy lets read every tenant's events.
    pub async fn setup(&self) -> Setup {
        let producer_schema = unique("producer");
        let consumer_schema = unique("consumer");
        let (ps, cs) = (&producer_schema, &consumer_schema);
        let producer_role = format!("{ps}_app");
        let producer_platform = format!("{ps}_platform");
        let consumer_role = format!("{cs}_app");
        let consumer_platform = format!("{cs}_platform");
        self.admin_sql(format!(
            "CREATE SCHEMA {ps}; CREATE SCHEMA {cs}; \
             CREATE ROLE {producer_role} LOGIN PASSWORD '{TEST_PASSWORD}'; \
             CREATE ROLE {producer_platform} LOGIN PASSWORD '{TEST_PASSWORD}'; \
             CREATE ROLE {consumer_role} LOGIN PASSWORD '{TEST_PASSWORD}'; \
             CREATE ROLE {consumer_platform} LOGIN PASSWORD '{TEST_PASSWORD}'; \
             ALTER ROLE {producer_role} SET search_path = {ps}; \
             ALTER ROLE {producer_platform} SET search_path = {ps}; \
             ALTER ROLE {consumer_role} SET search_path = {cs}; \
             ALTER DEFAULT PRIVILEGES IN SCHEMA {cs} \
                 GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {consumer_role};"
        ))
        .await;
        let producer = self.pool(ps).await;
        let consumer = self.pool(cs).await;
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
        // The producer's migration runs service-kit's template after install_producer.
        let template = include_str!("../../../service-kit/sql/outbox-event.sql")
            .replace("example_app", &producer_role)
            .replace("example_platform", &producer_platform)
            .replace("example_reader", &consumer_platform);
        sqlx::raw_sql(AssertSqlSafe(template))
            .execute(&mut *conn)
            .await
            .expect("row-level security on outbox_event");
        // And then the template for the aggregates' tenant column and row-level security
        // (Laysi/trustiot-v2#413), which grants both producer roles what `publish` needs.
        let template = include_str!("../../../service-kit/sql/outbox-aggregate.sql")
            .replace("example_app", &producer_role)
            .replace("example_platform", &producer_platform);
        sqlx::raw_sql(AssertSqlSafe(template))
            .execute(&mut *conn)
            .await
            .expect("row-level security on outbox_aggregate");
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
        // The grants of the outbox README (the templates grant INSERT on outbox_event and
        // SELECT, INSERT, UPDATE on outbox_aggregate to the producer's roles, and SELECT on
        // outbox_event to the consumer's platform role).
        self.admin_sql(format!(
            "GRANT USAGE ON SCHEMA {ps} TO {producer_role}, {producer_platform}; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON {ps}.widget TO {producer_role}; \
             GRANT USAGE ON SCHEMA {cs} TO {consumer_role}; \
             GRANT USAGE ON SCHEMA {ps} TO {consumer_platform};"
        ))
        .await;
        let (platform, work) =
            self.consumer_pools(&consumer_platform, &consumer_role, Duration::from_secs(30));
        Setup {
            producer_schema,
            consumer_schema,
            producer_role,
            producer_platform,
            consumer_role,
            consumer_platform,
            producer,
            consumer,
            platform,
            work,
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

/// The two schemas, roles, and pools of one test.
pub struct Setup {
    pub producer_schema: String,
    pub consumer_schema: String,
    /// The producer's service role and platform role.
    pub producer_role: String,
    pub producer_platform: String,
    /// The consumer's service role and platform role.
    pub consumer_role: String,
    pub consumer_platform: String,
    /// The superuser, `search_path` = the producer schema: writes events of any tenant.
    pub producer: PgPool,
    /// The superuser, `search_path` = the consumer schema: inspects what the consumer did.
    pub consumer: PgPool,
    /// The consumer's platform role pool.
    pub platform: PlatformDb,
    /// The consumer's service role pool, for background work.
    pub work: BackgroundTenantDb,
}

impl Setup {
    /// A consumer of the producer's log with the consuming service's pools.
    pub fn subscribe<H: Handler>(&self, config: ConsumerConfig, handler: H) -> Consumer<H> {
        Consumer::new(&self.platform, self.work.clone(), config, handler)
    }

    /// Writes `Counted { n }` for `aggregate`, in [`tenant_of`] the aggregate, in its own
    /// committed transaction.
    pub async fn publish(&self, aggregate: &str, n: i64) -> outbox::Published {
        let mut tx = self.producer.begin().await.expect("begin");
        let published = outbox::publish(
            &mut tx,
            &outbox::Aggregate::new("widget", aggregate).expect("aggregate"),
            &Counted { n },
            Some(tenant_of(aggregate)),
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
        let port = database.postgres.address.port().to_string();
        command
            .env_clear()
            .env("TRUSTIOT__DATABASE_URL", &database.url)
            .env("TRUSTIOT__PRODUCER_SCHEMA", &setup.producer_schema)
            .env("TRUSTIOT__CONSUMER_SCHEMA", &setup.consumer_schema)
            .env("TRUSTIOT_POSTGRES_HOST", postgres::SERVER_HOST)
            .env("TRUSTIOT_POSTGRES_PORT", &port)
            .env("TRUSTIOT_POSTGRES_DATABASE", "postgres")
            .env("TRUSTIOT_POSTGRES_SSLMODE", "verify-full")
            .env("TRUSTIOT_POSTGRES_CA_FILE", &database.postgres.ca_file)
            .env(
                "TRUSTIOT__CONSUMER_DATABASE__SERVICE_ROLE",
                &setup.consumer_role,
            )
            .env(
                "TRUSTIOT__CONSUMER_DATABASE__SERVICE_PASSWORD",
                TEST_PASSWORD,
            )
            .env(
                "TRUSTIOT__CONSUMER_DATABASE__PLATFORM_ROLE",
                &setup.consumer_platform,
            )
            .env(
                "TRUSTIOT__CONSUMER_DATABASE__PLATFORM_PASSWORD",
                TEST_PASSWORD,
            )
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            // Not the default 8081: tests run several examples at once.
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
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

    /// `PUT /v1/widgets/{id}` with a new name, as an event of `tenant` (platform-wide for
    /// `None`); returns the status and body.
    pub fn rename(&self, id: &str, name: &str, tenant: Option<&str>) -> (u16, serde_json::Value) {
        let body = serde_json::json!({ "name": name, "tenant_id": tenant }).to_string();
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
