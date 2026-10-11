//! Startup, health, and shutdown of a real `example-service` process, driven by real signals and
//! real HTTP requests.

mod support;

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use support::{OUTPUT_DRAIN_TIMEOUT, Output, Service, get, request, spawn_example, wait_exit};

/// How long a test waits for something the service does as soon as it is scheduled: logging a
/// line, closing a listener. It only ever bounds a failure, so it is far above what a busy
/// machine needs (an idle one needs milliseconds).
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a test waits for a service to exit when the exit itself is what it checks and its
/// duration is asserted separately, against the delay the test configured.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The drain delay and drain timeout of the SIGTERM test. The test has to reach the service
/// within the drain delay, and the exit has to come long before the drain timeout, so both are
/// long compared with what a busy machine takes to schedule a test thread: a test thread that
/// waits one second for a CPU would otherwise find the listener closed or the delay over.
const DRAIN_DELAY: Duration = Duration::from_secs(3);
/// How long past the drain delay the request in flight keeps the drain open: the health port has
/// to be checked while requests drain, so the request must still be running when the API port
/// has closed, however late the test thread gets its turn.
const DRAIN_OVERLAP: Duration = Duration::from_secs(5);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

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
fn health_port_serves_only_the_probe_endpoints() {
    let service = Service::start(&[]);
    let health = service.health_address;
    assert_ne!(health.port(), service.address.port());
    let live = get(health, "/healthz").expect("GET /healthz on the health port");
    assert_eq!(live.status, 200);
    assert_eq!(live.json()["status"], "ok");
    assert_eq!(live.header("cache-control"), Some("no-store"));
    let ready = get(health, "/readyz").expect("GET /readyz on the health port");
    assert_eq!(ready.status, 200);
    assert_eq!(ready.json()["status"], "ready");

    // Everything the API port serves, and every near miss of the probe paths, is 404 here: the
    // node, which reaches this port unauthenticated, gets nothing else (Laysi/trustiot-v2#286).
    for target in [
        "/",
        "/v1/hello",
        "/v1/slow?ms=1",
        "/v1/panic",
        "/v1/whoami",
        "/v1/items",
        "/internal/anything",
        "/healthz/",
        "/readyz/",
        "/healthz/../v1/hello",
        "/HEALTHZ",
        "/healthzx",
        "//healthz",
        "/%68ealthz",
    ] {
        let response = get(health, target).unwrap_or_else(|error| panic!("GET {target}: {error}"));
        assert_eq!(response.status, 404, "GET {target} on the health port");
    }
    for (method, target) in [
        ("POST", "/v1/echo"),
        ("PUT", "/internal/anything"),
        ("DELETE", "/v1/hello"),
    ] {
        let response = request(health, method, target, &[], Some(r#"{"text":"x"}"#))
            .unwrap_or_else(|error| panic!("{method} {target}: {error}"));
        assert_eq!(response.status, 404, "{method} {target} on the health port");
    }
    // The probe paths answer GET only.
    let post = request(health, "POST", "/readyz", &[], Some("{}")).expect("POST /readyz");
    assert_eq!(post.status, 405);
    // The API port still serves the service, and its health routes for mesh callers.
    assert_eq!(
        get(service.address, "/v1/hello").expect("hello").status,
        200
    );
    assert_eq!(get(service.address, "/readyz").expect("readyz").status, 200);
    // Neither port logs probes, nor the requests the health port refused.
    let text = service.stdout.text();
    assert!(
        !text.contains("/healthz") && !text.contains("/readyz") && !text.contains("/internal"),
        "{text}"
    );
}

#[test]
fn sigterm_fails_readiness_at_once_and_drains_requests_in_flight() {
    let drain_delay = format!("{}s", DRAIN_DELAY.as_secs());
    let drain_timeout = format!("{}s", DRAIN_TIMEOUT.as_secs());
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", &drain_delay),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", &drain_timeout),
    ]);
    let address = service.address;
    let health = service.health_address;
    assert_eq!(get(address, "/readyz").expect("GET /readyz").status, 200);
    assert_eq!(get(health, "/readyz").expect("GET /readyz").status, 200);

    let slow_ms =
        u64::try_from((DRAIN_DELAY + DRAIN_OVERLAP).as_millis()).expect("milliseconds fit u64");
    let in_flight = thread::spawn(move || get(address, &format!("/v1/slow?ms={slow_ms}")));
    // Let the slow request reach its handler before the signal.
    thread::sleep(Duration::from_millis(300));
    let signalled = Instant::now();
    service.signal("TERM");

    service
        .stdout
        .wait_for_message("shutdown requested; readiness now fails", EVENT_TIMEOUT)
        .expect("the service logs the shutdown request");
    // Readiness fails at once on both ports: they share one state.
    let ready = get(address, "/readyz").expect("GET /readyz while draining");
    let health_ready = get(health, "/readyz").expect("GET /readyz on the health port");
    let ready_checked = signalled.elapsed();
    assert!(
        ready_checked < DRAIN_DELAY,
        "readiness was checked {ready_checked:?} after SIGTERM, not within the {DRAIN_DELAY:?} drain delay"
    );
    for ready in [&ready, &health_ready] {
        assert_eq!(ready.status, 503);
        assert_eq!(ready.json()["status"], "draining");
    }
    // Still alive and still serving during the drain delay.
    assert_eq!(get(address, "/healthz").expect("GET /healthz").status, 200);
    assert_eq!(get(health, "/healthz").expect("GET /healthz").status, 200);
    assert_eq!(
        get(address, "/v1/hello").expect("GET /v1/hello").status,
        200
    );

    service
        .stdout
        .wait_for_message(
            "stopped accepting connections; draining requests and background tasks",
            DRAIN_DELAY + EVENT_TIMEOUT,
        )
        .expect("the service stops accepting after the drain delay");
    wait_until_refused(address, EVENT_TIMEOUT)
        .unwrap_or_else(|why| panic!("new connections are refused once draining: {why}"));
    // The health port keeps answering while requests drain, so the kubelet sees a draining pod
    // instead of refused connections: still not ready, still alive.
    let draining = get(health, "/readyz").expect("GET /readyz on the health port while draining");
    assert_eq!(draining.status, 503);
    assert_eq!(draining.json()["status"], "draining");
    assert_eq!(get(health, "/healthz").expect("GET /healthz").status, 200);

    let slow = in_flight
        .join()
        .expect("request thread")
        .expect("the request in flight completes");
    assert_eq!(slow.status, 200);
    assert_eq!(slow.json()["waited_ms"], slow_ms);

    let status = service
        .wait_exit(DRAIN_DELAY + DRAIN_TIMEOUT)
        .expect("the service exits");
    let exited = signalled.elapsed();
    assert_eq!(status.code(), Some(0), "{status}");
    // The service exits when the request in flight is done, long before the drain timeout;
    // waiting for the drain timeout instead would be a hang.
    assert!(
        exited < DRAIN_TIMEOUT / 2,
        "exited {exited:?} after SIGTERM, with a drain delay of {DRAIN_DELAY:?} and a drain timeout of {DRAIN_TIMEOUT:?}"
    );
    service
        .stdout
        .wait_for_message("shutdown complete", Duration::from_secs(1))
        .expect("the service logs a clean shutdown");
    // The health port closes with the process.
    wait_until_refused(health, Duration::from_secs(5))
        .unwrap_or_else(|why| panic!("the health port closes at exit: {why}"));
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!(
        "after SIGTERM: /readyz answered 503 on both ports at {} ms, the process exited 0 at {} ms",
        ready_checked.as_millis(),
        exited.as_millis()
    );
}

#[test]
fn sigint_drains_like_sigterm() {
    let mut service = Service::start(&[]);
    service.signal("INT");
    let status = service.wait_exit(EXIT_TIMEOUT).expect("the service exits");
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
    let status = service.wait_exit(EXIT_TIMEOUT).expect("the service exits");
    let elapsed = signalled.elapsed();
    assert_eq!(
        status.code(),
        Some(1),
        "a cut-off drain is a failure: {status}"
    );
    // The handler needs 8 s: an exit before 5 s was not waiting for it.
    assert!(
        elapsed < Duration::from_secs(5),
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
    let mut service = Service::start(&[("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "30s")]);
    service.signal("TERM");
    service
        .stdout
        .wait_for_message("shutdown requested; readiness now fails", EVENT_TIMEOUT)
        .expect("first signal logged");
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service.wait_exit(EXIT_TIMEOUT).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    // Draining would take the 30 s of the drain delay.
    let exited = signalled.elapsed();
    assert!(
        exited < Duration::from_secs(15),
        "exited {exited:?} after the second SIGTERM"
    );
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
fn durations_beyond_the_caps_stop_startup() {
    // 500000000000y made SIGTERM panic (drain_timeout) or never exit (drain_delay) before the
    // caps (Laysi/trustiot-v2#283). Now each is refused at startup, naming the setting and its
    // variable but not the value.
    let cases = [
        (
            "TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT",
            "500000000000y",
            "shutdown.drain_timeout",
        ),
        (
            "TRUSTIOT__SHUTDOWN__DRAIN_DELAY",
            "500000000000y",
            "shutdown.drain_delay",
        ),
        (
            "TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT",
            "3601s",
            "shutdown.drain_timeout",
        ),
        (
            "TRUSTIOT__SHUTDOWN__DRAIN_DELAY",
            "3601s",
            "shutdown.drain_delay",
        ),
        (
            "TRUSTIOT__HTTP__REQUEST_TIMEOUT",
            "500000000000y",
            "http.request_timeout",
        ),
    ];
    for (variable, value, key) in cases {
        let (mut child, stdout, _stderr) = spawn_example(&[(variable, value)]);
        let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
        assert_eq!(status.code(), Some(1), "{variable}={value}: {status}");
        read_to_end(&stdout);
        let text = stdout.text();
        assert!(text.contains(key), "{variable}={value}: {text}");
        assert!(text.contains(variable), "{variable}={value}: {text}");
        assert!(!text.contains(value), "{variable}={value}: {text}");
        assert!(!text.contains("listening"), "{variable}={value}: {text}");
    }
    // Both within one hour, but their sum is not.
    let (mut child, stdout, _stderr) = spawn_example(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "2400s"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "2400s"),
    ]);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    read_to_end(&stdout);
    let text = stdout.text();
    assert!(text.contains("shutdown.drain_timeout"), "{text}");
    assert!(!text.contains("2400"), "{text}");
}

#[test]
fn durations_at_the_caps_still_start_and_drain_on_sigterm() {
    // The limits are inclusive: one hour of drain timeout starts, and with nothing in flight the
    // service exits at once on SIGTERM. (A delay of one hour is covered by the unit tests; it
    // would keep this test waiting.)
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "0s"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "1h"),
        ("TRUSTIOT__HTTP__REQUEST_TIMEOUT", "1h"),
    ]);
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(5))
        .expect("the service exits");
    assert_eq!(status.code(), Some(0), "{status}");
}

#[test]
fn listen_address_in_use_stops_startup() {
    for variable in [
        "TRUSTIOT__HTTP__LISTEN_ADDR",
        "TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR",
    ] {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let address = taken.local_addr().expect("local address").to_string();
        let (mut child, stdout, _stderr) = spawn_example(&[(variable, address.as_str())]);
        let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
        assert_eq!(status.code(), Some(1), "{variable}: {status}");
        let logged = stdout
            .wait_for_message("cannot listen; not starting", Duration::from_secs(1))
            .unwrap_or_else(|| panic!("{variable}: not logged"));
        assert_eq!(logged["fields"]["address"], address.as_str(), "{variable}");
        assert!(!stdout.text().contains("\"listening\""), "{variable}");
    }
}

#[test]
fn health_port_equal_to_the_api_port_stops_startup() {
    let (mut child, stdout, _stderr) = spawn_example(&[
        ("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:18080"),
        ("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "0.0.0.0:18080"),
    ]);
    let status = wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits");
    assert_eq!(status.code(), Some(1), "{status}");
    let logged = stdout
        .wait_for_message(
            "invalid configuration; not starting",
            Duration::from_secs(1),
        )
        .expect("logged");
    let error = logged["fields"]["error"].as_str().expect("error text");
    assert!(error.contains("http.health_listen_addr"), "{error}");
    assert!(!error.contains("18080"), "{error}");
}
