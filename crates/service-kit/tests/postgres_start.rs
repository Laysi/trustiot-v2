//! The start of a throwaway Postgres container (`support::start::start_postgres`): what it
//! reports when the server never comes up, and that it replaces a container whose published port
//! the host cannot reach. Needs podman or docker; skipped without one unless `CI` is set.

mod support;

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use support::postgres::{TEST_PASSWORD, plain_options};
use support::start::{
    POSTGRES_IMAGE, START_LIMITS, StartLimits, log_in_once, start_postgres_within,
};
use support::{Container, container_runtime, runtime_output, unavailable};

/// Names of the containers of this process whose name starts with `kind`, still known to the
/// runtime (running or not).
fn containers_of(runtime: &str, kind: &str) -> Vec<String> {
    let filter = format!("name=service-kit-test-{kind}-{}-", std::process::id());
    runtime_output(
        runtime,
        &["ps", "--all", "--filter", &filter, "--format", "{{.Names}}"],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn run_postgres(runtime: &'static str, container: &Container) {
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
            &format!("POSTGRES_PASSWORD={TEST_PASSWORD}"),
            "--publish",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_never_comes_up_is_reported_with_what_was_awaited_and_the_container() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let limits = StartLimits {
        timeout: Duration::from_secs(3),
        ..START_LIMITS
    };
    // The container runs, publishes the port, and never starts a server.
    let error = start_postgres_within(
        limits,
        runtime,
        "start-timeout",
        "Postgres",
        |container| {
            runtime_output(
                runtime,
                &[
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
                    "--publish",
                    "127.0.0.1::5432",
                    "--entrypoint",
                    "sleep",
                    POSTGRES_IMAGE,
                    "600",
                ],
            );
        },
        |address: SocketAddr| log_in_once(plain_options(address, "postgres")),
    )
    .await
    .err()
    .expect("a container without a server does not start");
    for expected in [
        "Postgres did not accept a login within 3s",
        "attempts",
        "initdb",
        "last error:",
        "status=running",
        "last 20 log lines:",
    ] {
        assert!(error.contains(expected), "no {expected:?} in:\n{error}");
    }
    assert_eq!(
        containers_of(runtime, "start-timeout"),
        Vec::<String>::new(),
        "the container that did not start was removed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_container_whose_published_port_stays_dead_is_replaced() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let limits = StartLimits {
        host_reach_grace: Duration::from_secs(2),
        containers: 2,
        ..START_LIMITS
    };
    // The host cannot log in to the first container's address, however long it waits, while the
    // server inside is fine: what a port that Docker did not publish looks like.
    let first = Mutex::new(None::<SocketAddr>);
    let started = start_postgres_within(
        limits,
        runtime,
        "start-replaced",
        "Postgres",
        |container| run_postgres(runtime, container),
        |address| {
            let dead = *first.lock().expect("first address").get_or_insert(address) == address;
            async move {
                if dead {
                    return Err("simulated: nothing listens on the published port".to_owned());
                }
                log_in_once(plain_options(address, "postgres")).await
            }
        },
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    let first = first
        .lock()
        .expect("first address")
        .expect("a first container");
    assert_ne!(started.address, first, "the second container was used");
    assert_eq!(
        containers_of(runtime, "start-replaced"),
        vec![started.container.name.clone()],
        "the container with the dead port was removed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_container_having_a_dead_port_is_reported_for_each_container() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let limits = StartLimits {
        host_reach_grace: Duration::from_secs(2),
        containers: 2,
        ..START_LIMITS
    };
    let error = start_postgres_within(
        limits,
        runtime,
        "start-dead",
        "Postgres",
        |container| run_postgres(runtime, container),
        |_address| async { Err("simulated: nothing listens on the published port".to_owned()) },
    )
    .await
    .err()
    .expect("no container is reachable");
    for expected in [
        "container 1:",
        "container 2:",
        "accepts connections (pg_isready",
        "published port is taken for dead",
        "last error: simulated",
        "last 20 log lines:",
    ] {
        assert!(error.contains(expected), "no {expected:?} in:\n{error}");
    }
    assert_eq!(
        containers_of(runtime, "start-dead"),
        Vec::<String>::new(),
        "both containers were removed"
    );
}
