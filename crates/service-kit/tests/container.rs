//! `example-service` as PID 1 in a container, stopped the way the kubelet stops a pod: SIGTERM,
//! then SIGKILL when the grace period runs out. Needs podman or docker; skipped without one
//! unless `CI` is set.
//!
//! The 2026-10-10 PoC found a PID 1 that ignores SIGTERM held on until the grace period ended
//! (docs/feasibility/2026-10-10-edge-poc.md); this test proves the kit's service does not.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use support::{
    Container, EXAMPLE_SERVICE, container_runtime, get, runtime_output, unavailable, unique_name,
};

/// A minimal glibc base without a shell, so the service itself is PID 1.
const BASE_IMAGE: &str = "gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2";
/// What `stop` grants before SIGKILL, like a pod's `terminationGracePeriodSeconds`. It leaves
/// room for a request in flight and a slow `stop` on a loaded machine, so only a PID 1 that
/// ignores SIGTERM runs into it.
const GRACE_PERIOD: Duration = Duration::from_secs(40);
/// How long the service keeps serving, with readiness failing, after SIGTERM.
const DRAIN_DELAY: Duration = Duration::from_secs(3);
/// How long, in milliseconds, the request in flight when `stop` starts runs. It must still be
/// running after the listener has closed (`DRAIN_DELAY` after SIGTERM), however late a loaded
/// machine delivers the signal, so it runs for the longest `/v1/slow` allows less a margin.
const IN_FLIGHT_MS: u64 = 9_000;
/// The latest the first 503 may be seen after `stop` started. The 503 only exists between
/// SIGTERM and the end of the drain delay, so this bounds the wait for the signal, and a
/// service that never fails readiness fails the test by this deadline.
const READINESS_FAILS_BY: Duration = Duration::from_secs(30);

/// A locally built image, removed when dropped.
struct Image {
    runtime: &'static str,
    name: String,
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rmi", "--force", &self.name])
            .output();
    }
}

fn build_image(runtime: &'static str) -> Image {
    let context = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique_name("pid1-context"));
    std::fs::create_dir_all(&context).expect("create the build context");
    std::fs::copy(EXAMPLE_SERVICE, context.join("example-service")).expect("copy the binary");
    let containerfile = context.join("Containerfile");
    std::fs::write(
        &containerfile,
        format!(
            "FROM {BASE_IMAGE}\nCOPY example-service /usr/local/bin/example-service\n\
             ENTRYPOINT [\"/usr/local/bin/example-service\"]\n"
        ),
    )
    .expect("write the Containerfile");
    let image = Image {
        runtime,
        name: format!("localhost/{}:test", unique_name("pid1")),
    };
    let built = Command::new(runtime)
        .args(["build", "--quiet", "--file"])
        .arg(&containerfile)
        .args(["--tag", &image.name])
        .arg(&context)
        .output()
        .expect("run the image build");
    let _ = std::fs::remove_dir_all(&context);
    assert!(
        built.status.success(),
        "image build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    image
}

/// The service's log lines as printed.
fn raw_logs(container: &Container) -> String {
    runtime_output(container.runtime, &["logs", &container.name])
}

fn container_logs(container: &Container) -> Vec<serde_json::Value> {
    raw_logs(container)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect()
}

#[test]
fn pid_1_in_a_container_drains_and_exits_within_the_grace_period() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let image = build_image(runtime);
    let container = Container {
        runtime,
        name: unique_name("pid1"),
    };
    runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--name",
            &container.name,
            // As the Helm chart runs services (deploy/chart/README.md): a read-only root, an
            // arbitrary non-root UID as OpenShift assigns, no capabilities, no privilege gain.
            "--read-only",
            "--user",
            "54321:0",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--publish",
            "127.0.0.1::8080",
            "--env",
            "TRUSTIOT__GREETING=hello from a container",
            "--env",
            &format!("TRUSTIOT__SHUTDOWN__DRAIN_DELAY={}s", DRAIN_DELAY.as_secs()),
            "--env",
            "TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT=10s",
            &image.name,
        ],
    );
    let address = container.host_address("8080/tcp");
    let started = Instant::now();
    while get(address, "/readyz").map(|response| response.status).ok() != Some(200) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the service never became ready: {:?}",
            container_logs(&container)
        );
        thread::sleep(Duration::from_millis(100));
    }
    let starting = container_logs(&container)
        .into_iter()
        .find(|line| line["message"] == "starting")
        .expect("startup log line");
    assert_eq!(starting["fields"]["process.pid"], 1, "{starting}");

    let in_flight = thread::spawn(move || get(address, &format!("/v1/slow?ms={IN_FLIGHT_MS}")));
    thread::sleep(Duration::from_millis(500));
    let stop_started = Instant::now();
    let name = container.name.clone();
    let grace = GRACE_PERIOD.as_secs().to_string();
    let stop = thread::spawn(move || {
        Command::new(runtime)
            .args(["stop", "--time", &grace, &name])
            .output()
            .expect("run stop")
    });

    // Readiness fails while the service still serves during the drain delay. The 503 exists
    // from the moment SIGTERM arrives until the listener closes `DRAIN_DELAY` later, and on a
    // loaded machine `stop` can take seconds to deliver the signal, so the poll starts with
    // `stop` and waits for the 503 instead of assuming when it comes. Connections that fail
    // before the signal, or after the listener closed, are not an answer; they are retried
    // until the deadline and reported if no 503 was seen.
    let mut draining = None;
    let mut last_outcome = String::from("no request completed");
    while stop_started.elapsed() < READINESS_FAILS_BY {
        match get(address, "/readyz") {
            Ok(response) if response.status == 503 => {
                draining = Some(response);
                break;
            }
            Ok(response) => last_outcome = format!("status {}", response.status),
            Err(error) => last_outcome = format!("request failed: {error}"),
        }
        thread::sleep(Duration::from_millis(20));
    }
    let seen_after = stop_started.elapsed();
    let draining = draining.unwrap_or_else(|| {
        panic!(
            "readiness never failed within {READINESS_FAILS_BY:?} of stop starting; last \
             outcome: {last_outcome}; logs: {:?}",
            raw_logs(&container)
        )
    });
    assert_eq!(draining.json()["status"], "draining");
    assert!(
        !in_flight.is_finished(),
        "the request in flight ended before readiness failed, so draining was not exercised \
         (seen {seen_after:?} after stop started)"
    );

    let slow = in_flight
        .join()
        .expect("request thread")
        .expect("the request in flight completes");
    assert_eq!(slow.status, 200);
    assert_eq!(slow.json()["waited_ms"], IN_FLIGHT_MS);

    let stopped = stop.join().expect("stop thread");
    assert!(stopped.status.success(), "{stopped:?}");
    let stop_took = stop_started.elapsed();
    assert!(
        stop_took < GRACE_PERIOD - Duration::from_secs(5),
        "took {stop_took:?}; SIGKILL would come at {GRACE_PERIOD:?}"
    );
    let exit_code = runtime_output(
        runtime,
        &[
            "inspect",
            "--format",
            "{{.State.ExitCode}}",
            &container.name,
        ],
    );
    assert_eq!(exit_code, "0", "137 would mean SIGKILL");

    let logs = container_logs(&container);
    let requested = logs
        .iter()
        .find(|line| line["message"] == "shutdown requested; readiness now fails")
        .expect("shutdown request logged");
    assert_eq!(requested["fields"]["signal"], "SIGTERM");
    assert!(
        logs.iter()
            .any(|line| line["message"] == "shutdown complete"),
        "{logs:?}"
    );
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!(
        "stop returned after {} ms (grace period {} s); exit code {exit_code}",
        stop_took.as_millis(),
        GRACE_PERIOD.as_secs()
    );
    eprintln!("{}", raw_logs(&container));
}
