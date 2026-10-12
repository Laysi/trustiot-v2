//! Throwaway Postgres containers for the tests. A test binary starts one container, which the
//! tests of that binary that run at the same time share; the last test to drop its handle
//! removes it.
//!
//! - [`test_database`]: a plain Postgres (or the server named by `TEST_DATABASE_URL`) with a
//!   database of its own for each test.
//! - [`tls_postgres`]: a Postgres that requires nothing but serves TLS with a certificate from a
//!   CA made for this test process, so tests can connect with `sslmode=verify-full` as services
//!   do.
//!
//! Both start their container with [`start_postgres`], which says what it waited for when it
//! gives up.

use std::future::Future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use super::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The image of every Postgres container of these tests. The version the platform runs is
/// decided with the database deployment; advisory locks behave the same in every supported
/// release.
pub const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// The password of the superuser and of every role the tests create.
pub const TEST_PASSWORD: &str = "service-kit-test";

/// The server certificate names only this address, which is where the container is published.
pub const SERVER_HOST: &str = "127.0.0.1";

/// Moves the generated certificate and key into place for the postgres user (the key must be
/// readable only by it) and starts the image's entrypoint with TLS on.
const START_WITH_TLS: &str = "set -eu
mkdir -p /etc/postgres-tls
printf '%s' \"$TEST_TLS_CERT\" > /etc/postgres-tls/server.crt
(umask 077; printf '%s' \"$TEST_TLS_KEY\" > /etc/postgres-tls/server.key)
chown -R postgres:postgres /etc/postgres-tls
exec docker-entrypoint.sh postgres -c ssl=on \
  -c ssl_cert_file=/etc/postgres-tls/server.crt -c ssl_key_file=/etc/postgres-tls/server.key";

/// A CA certificate in PEM, made for one test process.
pub struct TestCa {
    pub pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    pub fn new(name: &str) -> Self {
        let key = KeyPair::generate().expect("CA key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let pem = params.self_signed(&key).expect("CA certificate").pem();
        Self {
            pem,
            issuer: Issuer::new(params, key),
        }
    }

    /// A server certificate for `names` (host names or IP addresses) and its key, in PEM.
    pub fn server_certificate(&self, names: &[&str]) -> (String, String) {
        let key = KeyPair::generate().expect("server key");
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let params = CertificateParams::new(names).expect("server params");
        let certificate = params
            .signed_by(&key, &self.issuer)
            .expect("server certificate");
        (certificate.pem(), key.serialize_pem())
    }
}

/// A running Postgres with TLS.
pub struct TlsPostgres {
    pub address: SocketAddr,
    /// The CA that signed the server certificate.
    pub ca_pem: String,
    _container: Container,
}

/// Options to connect to the TLS Postgres published on `port`, verifying the server with the CA
/// in `ca_pem`.
fn tls_options(ca_pem: &str, port: u16, role: &str, database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host(SERVER_HOST)
        .port(port)
        .username(role)
        .password(TEST_PASSWORD)
        .database(database)
        .ssl_mode(PgSslMode::VerifyFull)
        .ssl_root_cert_from_pem(ca_pem.to_owned().into_bytes())
}

impl TlsPostgres {
    /// Options to connect as `role` to `database`, verifying the server with the test CA.
    pub fn options(&self, role: &str, database: &str) -> PgConnectOptions {
        tls_options(&self.ca_pem, self.address.port(), role, database)
    }

    /// The superuser, on `database`.
    pub fn superuser(&self, database: &str) -> PgConnectOptions {
        self.options("postgres", database)
    }
}

static SHARED: tokio::sync::Mutex<Weak<TlsPostgres>> = tokio::sync::Mutex::const_new(Weak::new());

/// The shared Postgres, started on first use with `init` (static SQL) run by the superuser on
/// the `postgres` database. `None` (after printing why) when no container runtime is available
/// outside CI; in CI that fails the test instead. Every caller of a binary must pass the same
/// `init`.
pub async fn tls_postgres(init: &'static str) -> Option<Arc<TlsPostgres>> {
    let mut shared = SHARED.lock().await;
    if let Some(running) = shared.upgrade() {
        return Some(running);
    }
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    let ca = TestCa::new("service-kit test CA");
    let (certificate, key) = ca.server_certificate(&[SERVER_HOST]);
    let cert_env = format!("TEST_TLS_CERT={certificate}");
    let key_env = format!("TEST_TLS_KEY={key}");
    let password_env = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
    let started = start_postgres(
        runtime,
        "postgres-tls",
        "Postgres with TLS",
        |container| {
            runtime_output(
                runtime,
                &[
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
                    "--memory",
                    "512m",
                    "--env",
                    &password_env,
                    "--env",
                    &cert_env,
                    "--env",
                    &key_env,
                    "--publish",
                    "127.0.0.1::5432",
                    "--entrypoint",
                    "sh",
                    POSTGRES_IMAGE,
                    "-c",
                    START_WITH_TLS,
                ],
            );
        },
        |address| log_in_once(tls_options(&ca.pem, address.port(), "postgres", "postgres")),
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    let postgres = TlsPostgres {
        address: started.address,
        ca_pem: ca.pem,
        _container: started.container,
    };
    let mut connection = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect to the started Postgres");
    sqlx::raw_sql(init)
        .execute(&mut connection)
        .await
        .expect("initialize the test cluster");
    connection.close().await.expect("close");
    let postgres = Arc::new(postgres);
    *shared = Arc::downgrade(&postgres);
    Some(postgres)
}

/// The limits of one start; [`START_LIMITS`] is what the tests use.
#[derive(Clone, Copy)]
pub struct StartLimits {
    /// How long a started container gets to accept its first login from the host.
    pub timeout: Duration,
    /// How long the server inside may have been accepting connections while the host still
    /// cannot log in, before the published port is taken for dead.
    pub host_reach_grace: Duration,
    /// Containers started for one server at most; each one after the first replaces a container
    /// whose published port was dead.
    pub containers: u32,
}

/// The limits of every start in these tests, from the constants below.
pub const START_LIMITS: StartLimits = StartLimits {
    timeout: POSTGRES_START_TIMEOUT,
    host_reach_grace: HOST_REACH_GRACE,
    containers: POSTGRES_START_ATTEMPTS,
};

/// How long a started Postgres container gets to accept its first login from the host.
///
/// What it waits for: the image runs `initdb`, starts a temporary server with networking off for
/// its init scripts, stops it, and starts the real server. The container is "running" from the
/// first moment, so the only proof that the server is there is a login that succeeds.
///
/// The basis is measured in `docs/test-reports/2026-10-12-364-job-lock-postgres-start.md`: from
/// `docker run` to the first successful login it took 1.5 to 1.7 s alone, 1.9 to 3.0 s for ten
/// containers started at once (up to 4.7 s with 24 busy processes on 20 cores), and 5 to 16.5 s
/// for sixty started at once, up to 20.5 s with the busy processes and a load average of 42.
/// Sixty is what six parallel runs of the old `job_lock` test file started, a container per
/// test; one container per binary is far below that. 60 s is three times the worst measured.
const POSTGRES_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the server inside a container may have been accepting connections (by `pg_isready`
/// run inside it) while the host still cannot log in, before the container's published port is
/// taken for dead. In the measurements the first login from the host followed the server's
/// "ready" log line by 0.03 to 1.1 s even with sixty containers starting at once; 10 s is nine
/// times that. A start that is only slow never reaches this: the server is not ready yet then.
const HOST_REACH_GRACE: Duration = Duration::from_secs(10);

/// Containers started for one server at most. The reason for more than one: Docker Desktop did
/// not set up the host side of a published port for 6 of 1,800 containers started in groups of
/// sixty, however long the test waited, while the server inside accepted connections. That, not
/// a slow start, was the failure behind `Postgres did not start in time` (see the report).
/// Starting another container gets past it.
const POSTGRES_START_ATTEMPTS: u32 = 3;

/// Pause between two attempts to log in to a starting server.
const POSTGRES_START_POLL: Duration = Duration::from_millis(250);

/// How often the container is asked whether its server accepts connections.
const READINESS_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// A started container and the host address its port 5432 is published on.
pub struct Started {
    pub container: Container,
    pub address: SocketAddr,
}

/// Why waiting for one container ended without a login.
enum WaitFailure {
    /// [`POSTGRES_START_TIMEOUT`] passed.
    TimedOut(String),
    /// The server inside accepts connections, but the host cannot reach it.
    HostCannotReach(String),
}

/// Whether the Postgres server inside `container` accepts connections, asked from inside the
/// container (`pg_isready` on its loopback address), so it does not depend on how the port is
/// published to the host.
fn server_accepts_connections(container: &Container) -> bool {
    Command::new(container.runtime)
        .args(["exec", &container.name, "pg_isready", "--host", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// What a failing test needs to see about `container`: its state and the tail of its log. Never
/// panics, so it is safe to call while reporting another failure. It prints no environment
/// (the TLS key and the passwords are passed that way), and the images do not log it.
fn container_diagnostics(container: &Container) -> String {
    let run = |args: &[&str]| {
        Command::new(container.runtime)
            .args(args)
            .output()
            .map(|output| {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                text.trim().to_owned()
            })
            .unwrap_or_else(|error| format!("{} {args:?}: {error}", container.runtime))
    };
    let state = run(&[
        "inspect",
        "--format",
        "status={{.State.Status}} exit_code={{.State.ExitCode}} oom_killed={{.State.OOMKilled}} started_at={{.State.StartedAt}}",
        &container.name,
    ]);
    let log = run(&["logs", "--tail", "20", &container.name]);
    format!(
        "container {}: {state}\nlast 20 log lines:\n{log}",
        container.name
    )
}

/// One login with `options` and `SELECT 1`; the error is for a failure message.
pub async fn log_in_once(options: PgConnectOptions) -> Result<(), String> {
    let mut connection = PgConnection::connect_with(&options)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query("SELECT 1")
        .execute(&mut connection)
        .await
        .map_err(|error| error.to_string())?;
    connection.close().await.map_err(|error| error.to_string())
}

/// Starts a Postgres container with `run` (which receives the container, named from `kind`, and
/// starts it with the container runtime), and waits until `connect` has logged in to the address
/// where the container publishes port 5432. Retries every [`POSTGRES_START_POLL`] for at most
/// [`POSTGRES_START_TIMEOUT`]; an attempt still running at the deadline is cut off. When the
/// server inside accepts connections but the host cannot reach the published port for
/// [`HOST_REACH_GRACE`], the container is removed and another one is started, up to
/// [`POSTGRES_START_ATTEMPTS`] containers.
///
/// The error says how long it waited, for what, after how many attempts, the last error, and the
/// state and log tail of the container. `what` names the server in it.
pub async fn start_postgres<Fut>(
    runtime: &'static str,
    kind: &str,
    what: &str,
    run: impl Fn(&Container),
    connect: impl Fn(SocketAddr) -> Fut,
) -> Result<Started, String>
where
    Fut: Future<Output = Result<(), String>>,
{
    start_postgres_within(START_LIMITS, runtime, kind, what, run, connect).await
}

/// [`start_postgres`] with other limits, for the tests of the start itself.
pub async fn start_postgres_within<Fut>(
    limits: StartLimits,
    runtime: &'static str,
    kind: &str,
    what: &str,
    run: impl Fn(&Container),
    connect: impl Fn(SocketAddr) -> Fut,
) -> Result<Started, String>
where
    Fut: Future<Output = Result<(), String>>,
{
    let mut unreachable = Vec::new();
    for container_number in 1..=limits.containers {
        let container = Container {
            runtime,
            name: unique_name(kind),
        };
        run(&container);
        let address = container.host_address("5432/tcp");
        match wait_until_accepting(limits, &container, what, || connect(address)).await {
            Ok(()) => return Ok(Started { container, address }),
            Err(WaitFailure::TimedOut(message)) => {
                unreachable.push(format!("container {container_number}: {message}"));
                return Err(unreachable.join("\n"));
            }
            Err(WaitFailure::HostCannotReach(message)) => {
                unreachable.push(format!("container {container_number}: {message}"));
            }
        }
    }
    Err(unreachable.join("\n"))
}

async fn wait_until_accepting<Fut>(
    limits: StartLimits,
    container: &Container,
    what: &str,
    mut connect: impl FnMut() -> Fut,
) -> Result<(), WaitFailure>
where
    Fut: Future<Output = Result<(), String>>,
{
    let started = Instant::now();
    let mut attempts = 0_u32;
    let mut server_ready_since: Option<Instant> = None;
    let mut last_probe: Option<Instant> = None;
    loop {
        attempts += 1;
        let remaining = limits.timeout.saturating_sub(started.elapsed());
        let last_error = match tokio::time::timeout(remaining, connect()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(_elapsed) => format!("attempt {attempts} was still running at the deadline"),
        };
        if server_ready_since.is_none()
            && last_probe.is_none_or(|probed| probed.elapsed() >= READINESS_PROBE_INTERVAL)
        {
            last_probe = Some(Instant::now());
            if server_accepts_connections(container) {
                server_ready_since = Some(Instant::now());
            }
        }
        if let Some(ready_since) = server_ready_since
            && ready_since.elapsed() >= limits.host_reach_grace
        {
            return Err(WaitFailure::HostCannotReach(format!(
                "the server in the container accepts connections (pg_isready, inside the \
                 container) but {what} could not be logged in to from the host for {:?} (waited \
                 {:?} in all, {attempts} attempts), so its published port is taken for dead; \
                 last error: {last_error}\n{}",
                ready_since.elapsed(),
                started.elapsed(),
                container_diagnostics(container)
            )));
        }
        if started.elapsed() >= limits.timeout {
            return Err(WaitFailure::TimedOut(format!(
                "{what} did not accept a login within {:?} (waited {:?}, \
                 {attempts} attempts; waiting for the image's initdb, its restart and the first \
                 successful login); last error: {last_error}\n{}",
                limits.timeout,
                started.elapsed(),
                container_diagnostics(container)
            )));
        }
        tokio::time::sleep(POSTGRES_START_POLL).await;
    }
}

/// The password of the superuser of the plain Postgres.
const PLAIN_PASSWORD: &str = "service-kit-test";

/// Connections the shared plain Postgres accepts. Every test of a binary opens a few pools of up
/// to four connections each at the same time; the default of 100 is not enough for ten of them.
const PLAIN_MAX_CONNECTIONS: &str = "400";

/// A running plain Postgres, shared by the tests of a binary.
struct PlainPostgres {
    address: SocketAddr,
    /// Index in [`TEST_DATABASES`] of the next database to hand out.
    next_database: AtomicUsize,
    _container: Container,
}

impl PlainPostgres {
    fn options(&self, database: &str) -> PgConnectOptions {
        plain_options(self.address, database)
    }

    fn url_of(&self, database: &str) -> String {
        format!(
            "postgres://postgres:{PLAIN_PASSWORD}@{}/{database}",
            self.address
        )
    }
}

pub fn plain_options(address: SocketAddr, database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host(&address.ip().to_string())
        .port(address.port())
        .username("postgres")
        .password(PLAIN_PASSWORD)
        .database(database)
}

/// The shared plain Postgres, or why starting it failed. The failure is kept so that the other
/// tests of the binary fail at once with the same message, instead of each waiting out the
/// start timeout again.
enum PlainState {
    Idle,
    Running(Weak<PlainPostgres>),
    Failed(String),
}

static PLAIN: tokio::sync::Mutex<PlainState> = tokio::sync::Mutex::const_new(PlainState::Idle);

/// A database for one test. The server (and with it every test's database) goes away when the
/// last test of the binary drops its handle.
pub struct TestDatabase {
    /// `postgres://` URL of the database, with credentials.
    pub url: String,
    _server: Option<Arc<PlainPostgres>>,
}

/// A database for one test, or `None` (after printing why) when there is no `TEST_DATABASE_URL`
/// and no container runtime outside CI; in CI that fails the test instead.
///
/// With `TEST_DATABASE_URL` set, that URL is used as it is. Otherwise a Postgres container is
/// started on first use and shared by the tests of the binary, and each test gets a database of
/// its own in it, so tests cannot see each other's advisory locks, which belong to a database.
/// When the tests of a binary do not overlap (`--test-threads=1`), the container is removed after
/// each test and started again for the next.
pub async fn test_database() -> Option<TestDatabase> {
    if let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return Some(TestDatabase { url, _server: None });
    }
    let server = plain_postgres().await?;
    let &(name, create) = TEST_DATABASES
        .get(server.next_database.fetch_add(1, Ordering::Relaxed))
        .unwrap_or_else(|| {
            panic!(
                "more tests share the server than the {} databases of TEST_DATABASES",
                TEST_DATABASES.len()
            )
        });
    let mut connection = PgConnection::connect_with(&server.options("postgres"))
        .await
        .expect("connect to the shared Postgres");
    sqlx::raw_sql(create)
        .execute(&mut connection)
        .await
        .expect("create the test's database");
    connection.close().await.expect("close");
    Some(TestDatabase {
        url: server.url_of(name),
        _server: Some(server),
    })
}

/// The databases the tests of a binary can take, each with its `CREATE DATABASE`. The statements
/// are literals because the workspace's SQL check (`tools/sql-check`) accepts no SQL built from
/// strings, and a database name cannot be a bind parameter. Add entries when a binary has more
/// tests that use a database than there are here.
const TEST_DATABASES: [(&str, &str); 32] = [
    ("test_db_00", "CREATE DATABASE test_db_00"),
    ("test_db_01", "CREATE DATABASE test_db_01"),
    ("test_db_02", "CREATE DATABASE test_db_02"),
    ("test_db_03", "CREATE DATABASE test_db_03"),
    ("test_db_04", "CREATE DATABASE test_db_04"),
    ("test_db_05", "CREATE DATABASE test_db_05"),
    ("test_db_06", "CREATE DATABASE test_db_06"),
    ("test_db_07", "CREATE DATABASE test_db_07"),
    ("test_db_08", "CREATE DATABASE test_db_08"),
    ("test_db_09", "CREATE DATABASE test_db_09"),
    ("test_db_10", "CREATE DATABASE test_db_10"),
    ("test_db_11", "CREATE DATABASE test_db_11"),
    ("test_db_12", "CREATE DATABASE test_db_12"),
    ("test_db_13", "CREATE DATABASE test_db_13"),
    ("test_db_14", "CREATE DATABASE test_db_14"),
    ("test_db_15", "CREATE DATABASE test_db_15"),
    ("test_db_16", "CREATE DATABASE test_db_16"),
    ("test_db_17", "CREATE DATABASE test_db_17"),
    ("test_db_18", "CREATE DATABASE test_db_18"),
    ("test_db_19", "CREATE DATABASE test_db_19"),
    ("test_db_20", "CREATE DATABASE test_db_20"),
    ("test_db_21", "CREATE DATABASE test_db_21"),
    ("test_db_22", "CREATE DATABASE test_db_22"),
    ("test_db_23", "CREATE DATABASE test_db_23"),
    ("test_db_24", "CREATE DATABASE test_db_24"),
    ("test_db_25", "CREATE DATABASE test_db_25"),
    ("test_db_26", "CREATE DATABASE test_db_26"),
    ("test_db_27", "CREATE DATABASE test_db_27"),
    ("test_db_28", "CREATE DATABASE test_db_28"),
    ("test_db_29", "CREATE DATABASE test_db_29"),
    ("test_db_30", "CREATE DATABASE test_db_30"),
    ("test_db_31", "CREATE DATABASE test_db_31"),
];

async fn plain_postgres() -> Option<Arc<PlainPostgres>> {
    let mut state = PLAIN.lock().await;
    match &*state {
        PlainState::Running(weak) => {
            if let Some(running) = weak.upgrade() {
                return Some(running);
            }
        }
        PlainState::Failed(message) => panic!("{message}"),
        PlainState::Idle => {}
    }
    let Some(runtime) = container_runtime() else {
        unavailable("TEST_DATABASE_URL, podman, or docker");
        return None;
    };
    let password_env = format!("POSTGRES_PASSWORD={PLAIN_PASSWORD}");
    let max_connections = format!("max_connections={PLAIN_MAX_CONNECTIONS}");
    let started = start_postgres(
        runtime,
        "postgres",
        "Postgres",
        |container| {
            runtime_output(
                runtime,
                &[
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
                    "--memory",
                    "1g",
                    "--env",
                    &password_env,
                    "--publish",
                    "127.0.0.1::5432",
                    POSTGRES_IMAGE,
                    "postgres",
                    "-c",
                    &max_connections,
                ],
            );
        },
        |address| log_in_once(plain_options(address, "postgres")),
    )
    .await;
    let started = match started {
        Ok(started) => started,
        Err(message) => {
            *state = PlainState::Failed(message.clone());
            panic!("{message}");
        }
    };
    let postgres = Arc::new(PlainPostgres {
        address: started.address,
        next_database: AtomicUsize::new(0),
        _container: started.container,
    });
    *state = PlainState::Running(Arc::downgrade(&postgres));
    Some(postgres)
}
