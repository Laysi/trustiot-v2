//! Startup, health, and shutdown of a real `example-service` process, driven by real signals and
//! real HTTP requests.

mod support;

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use support::{OUTPUT_DRAIN_TIMEOUT, Output, Service, get, spawn_example, wait_exit};

/// Retries connecting until the connection is refused, which is how a closed listener answers.
/// The log line that announces the end of accepting is written just before the listener is
/// dropped, so a connection right after it can still succeed on a loaded machine; the deadline
/// keeps a listener that never closes a failure.
fn wait_until_refused(address: SocketAddr, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let outcome = match TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => return Ok(()),
            Err(error) => format!("connecting failed with {error}"),
            Ok(_) => "a connection was accepted".to_owned(),
        };
        if Instant::now() >= deadline {
            return Err(format!("still not refused after {timeout:?}: {outcome}"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the reader thread has consumed everything a process that already exited printed.
fn read_to_end(output: &Output) {
    assert!(
        output.wait_closed(OUTPUT_DRAIN_TIMEOUT),
        "the output of the exited service was not read to its end"
    );
}

#[test]
fn health_endpoints_answer_while_serving() {
    let service = Service::start(&[]);
    let live = get(service.address, "/healthz").expect("GET /healthz");
    assert_eq!(live.status, 200);
    assert_eq!(live.json()["status"], "ok");
    assert_eq!(live.header("cache-control"), Some("no-store"));
    let ready = get(service.address, "/readyz").expect("GET /readyz");
    assert_eq!(ready.status, 200);
    assert_eq!(ready.json()["status"], "ready");
    let hello = get(service.address, "/v1/hello").expect("GET /v1/hello");
    assert_eq!(hello.status, 200);
    assert_eq!(hello.json()["message"], "hello from the tests");
    // Probes are not request-logged: they would flood the logs.
    assert!(
        !service.stdout.text().contains("/healthz") && !service.stdout.text().contains("/readyz"),
        "{}",
        service.stdout.text()
    );
}

#[test]
fn sigterm_fails_readiness_at_once_and_drains_requests_in_flight() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "1s"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "10s"),
    ]);
    let address = service.address;
    assert_eq!(get(address, "/readyz").expect("GET /readyz").status, 200);

    let in_flight = thread::spawn(move || get(address, "/v1/slow?ms=2500"));
    // Let the slow request reach its handler before the signal.
    thread::sleep(Duration::from_millis(300));
    let signalled = Instant::now();
    service.signal("TERM");

    service
        .stdout
        .wait_for_message(
            "shutdown requested; readiness now fails",
            Duration::from_secs(2),
        )
        .expect("the service logs the shutdown request");
    let ready = get(address, "/readyz").expect("GET /readyz while draining");
    let ready_checked = signalled.elapsed();
    assert!(
        ready_checked < Duration::from_secs(1),
        "readiness was checked within the drain delay"
    );
    assert_eq!(ready.status, 503);
    assert_eq!(ready.json()["status"], "draining");
    // Still alive and still serving during the drain delay.
    assert_eq!(get(address, "/healthz").expect("GET /healthz").status, 200);
    assert_eq!(
        get(address, "/v1/hello").expect("GET /v1/hello").status,
        200
    );

    service
        .stdout
        .wait_for_message(
            "stopped accepting connections; draining requests and background tasks",
            Duration::from_secs(3),
        )
        .expect("the service stops accepting after the drain delay");
    wait_until_refused(address, Duration::from_secs(5))
        .unwrap_or_else(|why| panic!("new connections are refused once draining: {why}"));

    let slow = in_flight
        .join()
        .expect("request thread")
        .expect("the request in flight completes");
    assert_eq!(slow.status, 200);
    assert_eq!(slow.json()["waited_ms"], 2500);

    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits");
    let exited = signalled.elapsed();
    assert_eq!(status.code(), Some(0), "{status}");
    assert!(
        exited < Duration::from_secs(5),
        "exited {exited:?} after SIGTERM"
    );
    service
        .stdout
        .wait_for_message("shutdown complete", Duration::from_secs(1))
        .expect("the service logs a clean shutdown");
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!(
        "after SIGTERM: /readyz answered 503 at {} ms, the process exited 0 at {} ms",
        ready_checked.as_millis(),
        exited.as_millis()
    );
}

#[test]
fn sigint_drains_like_sigterm() {
    let mut service = Service::start(&[]);
    service.signal("INT");
    let status = service
        .wait_exit(Duration::from_secs(5))
        .expect("the service exits");
    assert_eq!(status.code(), Some(0), "{status}");
    let requested = service
        .stdout
        .wait_for_message(
            "shutdown requested; readiness now fails",
            Duration::from_secs(1),
        )
        .expect("logged");
    assert_eq!(requested["fields"]["signal"], "SIGINT");
}

#[test]
fn drain_timeout_bounds_shutdown() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "0s"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "500ms"),
    ]);
    let address = service.address;
    let in_flight = thread::spawn(move || get(address, "/v1/slow?ms=8000"));
    thread::sleep(Duration::from_millis(300));
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(5))
        .expect("the service exits");
    let elapsed = signalled.elapsed();
    assert_eq!(
        status.code(),
        Some(1),
        "a cut-off drain is a failure: {status}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "exited {elapsed:?} after SIGTERM"
    );
    service
        .stdout
        .wait_for_message(
            "drain timed out; cut off the work still in flight",
            Duration::from_secs(1),
        )
        .expect("the cut-off is logged");
    // The handler still running at the drain timeout is dropped and its request answers 504,
    // which the ingress does not retry.
    let cut_off = in_flight
        .join()
        .expect("request thread")
        .expect("the cut-off request gets a response");
    assert_eq!(cut_off.status, 504);
    let line = service
        .stdout
        .wait_for_message(
            "request cut off by the drain timeout",
            Duration::from_secs(1),
        )
        .expect("the cut-off request is logged");
    assert_eq!(line["level"], "WARN");
}

#[test]
fn second_signal_exits_without_draining() {
    let mut service = Service::start(&[("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "10s")]);
    service.signal("TERM");
    service
        .stdout
        .wait_for_message(
            "shutdown requested; readiness now fails",
            Duration::from_secs(2),
        )
        .expect("first signal logged");
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(5))
        .expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    assert!(signalled.elapsed() < Duration::from_secs(2));
}

#[test]
fn missing_required_configuration_stops_startup() {
    let (mut child, stdout, _stderr) = spawn_example(&[("TRUSTIOT__GREETING", "")]);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    read_to_end(&stdout);
    let lines = stdout.json_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["level"], "ERROR");
    assert_eq!(lines[0]["message"], "invalid configuration; not starting");
    let error = lines[0]["fields"]["error"].as_str().expect("error text");
    assert!(error.contains("TRUSTIOT__GREETING"), "{error}");
    assert!(!stdout.text().contains("listening"));
}

#[test]
fn invalid_configuration_names_the_key_without_its_value() {
    let (mut child, stdout, _stderr) = spawn_example(&[
        ("TRUSTIOT__HTTP__LISTEN_ADDR", "secret-looking-value"),
        (
            "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
            "https://collector:4318",
        ),
    ]);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    read_to_end(&stdout);
    let text = stdout.text();
    assert!(text.contains("http.listen_addr"), "{text}");
    assert!(text.contains("TRUSTIOT__HTTP__LISTEN_ADDR"), "{text}");
    assert!(!text.contains("secret-looking-value"), "{text}");
}

#[test]
fn listen_address_in_use_stops_startup() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let address = taken.local_addr().expect("local address").to_string();
    let (mut child, stdout, _stderr) =
        spawn_example(&[("TRUSTIOT__HTTP__LISTEN_ADDR", address.as_str())]);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    stdout
        .wait_for_message("cannot listen; not starting", Duration::from_secs(1))
        .expect("logged");
}
