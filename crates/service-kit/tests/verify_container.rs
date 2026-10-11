//! Independent verification of Laysi/trustiot-v2#51, acceptance criterion 1: `example-service` as
//! PID 1 in a container handles SIGTERM. This variant sends the signal with `kill --signal TERM`
//! (what the kubelet's runtime does) instead of `stop`, and checks the order of events: readiness
//! fails first, a request in flight completes, new connections are refused after the drain, and
//! the process exits 0 well inside the grace period. Skipped without podman or docker unless
//! `CI` is set.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use support::{
    Container, EXAMPLE_SERVICE, container_runtime, get, runtime_output, unavailable, unique_name,
};

const BASE_IMAGE: &str = "gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2";
const GRACE_PERIOD: Duration = Duration::from_secs(15);

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
    let context = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique_name("verify-context"));
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
        name: format!("localhost/{}:test", unique_name("verify")),
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

#[test]
fn sigterm_to_pid_1_flips_readiness_then_drains_and_exits_zero() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let image = build_image(runtime);
    let container = Container {
        runtime,
        name: unique_name("verify-pid1"),
    };
    runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--memory",
            "128m",
            "--name",
            &container.name,
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
            "TRUSTIOT__SHUTDOWN__DRAIN_DELAY=3s",
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
            "never became ready"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // Five seconds of work in flight: it outlives the 3 s drain delay, so the answer has to come
    // from the drain phase, after the listener closed.
    let in_flight = thread::spawn(move || get(address, "/v1/slow?ms=5000"));
    thread::sleep(Duration::from_millis(300));
    runtime_output(runtime, &["kill", "--signal", "TERM", &container.name]);
    let signalled = Instant::now();

    // 1. Readiness fails inside the drain delay while liveness is still served.
    let mut readiness_failed_after = None;
    while signalled.elapsed() < Duration::from_secs(3) {
        if get(address, "/readyz").is_ok_and(|response| response.status == 503) {
            readiness_failed_after = Some(signalled.elapsed());
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let readiness_failed_after = readiness_failed_after.expect("readiness fails within the delay");
    assert_eq!(get(address, "/healthz").map(|r| r.status).ok(), Some(200));

    // 2. After the delay the listener is closed: new connections are refused.
    let mut refused_after = None;
    while signalled.elapsed() < Duration::from_secs(8) {
        if get(address, "/healthz").is_err() {
            refused_after = Some(signalled.elapsed());
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let refused_after = refused_after.expect("new connections are refused after the drain delay");

    // 3. The request in flight still completes with its full answer.
    let slow = in_flight
        .join()
        .expect("request thread")
        .expect("the request in flight completes");
    assert_eq!(slow.status, 200);
    assert_eq!(slow.json()["waited_ms"], 5000);
    let answered_after = signalled.elapsed();

    // 4. Exit 0 well inside the grace period.
    let waited = runtime_output(runtime, &["wait", &container.name]);
    let exited_after = signalled.elapsed();
    assert_eq!(waited, "0", "137 would mean SIGKILL");
    assert!(
        exited_after < GRACE_PERIOD - Duration::from_secs(5),
        "{exited_after:?}"
    );
    eprintln!(
        "readiness 503 after {readiness_failed_after:?}, connections refused after \
         {refused_after:?}, in-flight answered after {answered_after:?}, exit 0 after \
         {exited_after:?} (grace {GRACE_PERIOD:?})"
    );
    assert!(readiness_failed_after < refused_after);
}
