//! The start of the outbox tests' TLS Postgres (`TlsPostgres::start_within`, which `start` and
//! `start_restartable` use): what it reports when the server does not come up, and that it
//! replaces a container whose published port the host cannot reach, also when the test chose the
//! host port. The start logic is one file shared with service-kit's, testkit's and identity's
//! tests (`crates/service-kit/tests/postgres_start.rs` tests it in detail); these tests show that
//! outbox's container, TLS setup and login are wired to it. Needs podman or docker; skipped
//! without one unless `CI` is set.

mod support;

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use support::postgres::{TEST_PASSWORD, TlsPostgres};
use support::start::{START_LIMITS, StartLimits, log_in_once};
use support::{container_runtime, runtime_output, unavailable};

/// Names of the containers of this process whose name starts with `kind`, still known to the
/// runtime (running or not).
fn containers_of(runtime: &str, kind: &str) -> Vec<String> {
    let filter = format!("name=outbox-test-{kind}-{}-", std::process::id());
    runtime_output(
        runtime,
        &["ps", "--all", "--filter", &filter, "--format", "{{.Names}}"],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_does_not_come_up_is_reported_with_what_was_awaited_and_the_container() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    // The image needs more than a second for initdb alone, so no login succeeds within a
    // millisecond however fast the machine is; the container itself is running by then.
    let limits = StartLimits {
        timeout: Duration::from_millis(1),
        ..START_LIMITS
    };
    let error =
        TlsPostgres::start_within(limits, runtime, "start-timeout", false, |_address| false)
            .await
            .err()
            .expect("a server cannot start within a millisecond");
    for expected in [
        "Postgres with TLS did not accept a login within 1ms",
        "attempts",
        "initdb",
        "last error:",
        "status=running",
        "last 20 log lines:",
    ] {
        assert!(error.contains(expected), "no {expected:?} in:\n{error}");
    }
    // The TLS key and the password are passed to the container in its environment; the message
    // must not repeat them.
    // (The password alone is no use as a needle: the container's name starts with the same text.)
    let password_setting = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
    for secret in ["TEST_TLS_KEY", "PRIVATE KEY", password_setting.as_str()] {
        assert!(!error.contains(secret), "{secret:?} in:\n{error}");
    }
    assert_eq!(
        containers_of(runtime, "start-timeout"),
        Vec::<String>::new(),
        "the container that did not start was removed"
    );
}

/// Starts with the first container's published port dead, and returns the Postgres that replaced
/// it and the first address.
async fn start_with_dead_first_port(
    runtime: &'static str,
    kind: &str,
    fixed_port: bool,
) -> (TlsPostgres, SocketAddr) {
    let limits = StartLimits {
        host_reach_grace: Duration::from_secs(2),
        containers: 2,
        ..START_LIMITS
    };
    // The host cannot log in to the first container's address, however long it waits, while the
    // server inside is fine: what a port that Docker did not publish looks like.
    let first = Mutex::new(None::<SocketAddr>);
    let postgres = TlsPostgres::start_within(limits, runtime, kind, fixed_port, |address| {
        *first.lock().expect("first address").get_or_insert(address) == address
    })
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    let first = first
        .lock()
        .expect("first address")
        .expect("a first container");
    (postgres, first)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_container_whose_published_port_stays_dead_is_replaced() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (postgres, first) = start_with_dead_first_port(runtime, "start-replaced", false).await;
    assert_ne!(postgres.address, first, "the second container was used");
    log_in_once(postgres.options("postgres"))
        .await
        .expect("log in to the replacement with verify-full");
    assert_eq!(
        containers_of(runtime, "start-replaced").len(),
        1,
        "the container with the dead port was removed and the replacement is the only one left"
    );
    drop(postgres);
    assert_eq!(
        containers_of(runtime, "start-replaced"),
        Vec::<String>::new(),
        "the replacement is removed when dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_on_a_port_the_test_chose_keeps_that_port_across_a_restart() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (postgres, first) = start_with_dead_first_port(runtime, "start-fixed", true).await;
    assert_ne!(postgres.address, first, "the second container was used");
    log_in_once(postgres.options("postgres"))
        .await
        .expect("log in to the replacement with verify-full");
    // `restart` fails unless the replacement is published on the port it was started with.
    postgres.restart();
    let after_restart = tokio::time::timeout(
        Duration::from_secs(120),
        wait_for_login(postgres.options("postgres")),
    )
    .await;
    assert!(
        after_restart.is_ok(),
        "the replacement accepted no login after the restart"
    );
    assert_eq!(
        containers_of(runtime, "start-fixed").len(),
        1,
        "the container with the dead port was removed"
    );
}

/// Logs in every 250 ms until one succeeds; the caller bounds the wait.
async fn wait_for_login(options: sqlx::postgres::PgConnectOptions) {
    while log_in_once(options.clone()).await.is_err() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
