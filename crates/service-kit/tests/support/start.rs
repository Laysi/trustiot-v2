//! The start of a throwaway Postgres container, shared by every crate whose tests start one
//! (`crates/service-kit`, `crates/outbox`, `crates/testkit` and `services/identity` include this
//! file with `#[path]`): [`start_postgres`] starts the container and waits until the host can log
//! in, replaces a container whose published port is dead, and says what it waited for when it
//! gives up.
//!
//! The including module provides `Container` and `unique_name` (a container that is removed when
//! dropped, and a name for it that is unique to the test process).

use std::future::Future;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};

use super::{Container, unique_name};

/// The image of every Postgres container of these tests. The version the platform runs is
/// decided with the database deployment; advisory locks behave the same in every supported
/// release.
pub const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// Moves the generated certificate and key into place for the postgres user (the key must be
/// readable only by it) and starts the image's entrypoint with TLS on.
pub const START_WITH_TLS: &str = "set -eu
mkdir -p /etc/postgres-tls
printf '%s' \"$TEST_TLS_CERT\" > /etc/postgres-tls/server.crt
(umask 077; printf '%s' \"$TEST_TLS_KEY\" > /etc/postgres-tls/server.key)
chown -R postgres:postgres /etc/postgres-tls
exec docker-entrypoint.sh postgres -c ssl=on \
  -c ssl_cert_file=/etc/postgres-tls/server.crt -c ssl_key_file=/etc/postgres-tls/server.key";

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
