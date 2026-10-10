//! OTLP export from a real `example-service` process to a fake OTLP/HTTP receiver: off unless
//! configured, and when on, logs and traces arrive with the caller's trace id, metrics arrive,
//! and none of them carries request data.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use support::{Service, TRACE_ID, TRACEPARENT, request};

const BODY_CANARY: &str = "otlp-body-canary-2c9e";

/// The path and body of one request the fake collector received.
type Request = (String, Vec<u8>);

/// Accepts OTLP/HTTP requests and records them.
struct FakeCollector {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Request>>>,
}

impl FakeCollector {
    fn start() -> Self {
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

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.received
            .lock()
            .expect("collector lock")
            .iter()
            .filter(|(received, _)| received == path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn requests(&self) -> usize {
        self.received.lock().expect("collector lock").len()
    }
}

/// Reads one request (Content-Length or chunked body) and answers 200 with an empty export
/// response, then closes the connection.
fn read_request(stream: TcpStream) -> Option<(String, Vec<u8>)> {
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn trace_id_bytes() -> Vec<u8> {
    TRACE_ID
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII trace id");
            u8::from_str_radix(pair, 16).expect("hex trace id")
        })
        .collect()
}

fn echo_with_canary(service: &Service) {
    let body = format!("{{\"text\":\"{BODY_CANARY}\"}}");
    let response = request(
        service.address,
        "POST",
        "/v1/echo",
        &[("traceparent", TRACEPARENT)],
        Some(&body),
    )
    .expect("POST /v1/echo");
    assert_eq!(response.status, 200);
}

#[test]
fn export_is_off_unless_configured() {
    let collector = FakeCollector::start();
    // The SDK's standard variable alone must not turn export on: only the kit's key does.
    let mut service = Service::start(&[("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.url())]);
    echo_with_canary(&service);
    service.signal("TERM");
    assert!(
        service
            .wait_exit(Duration::from_secs(10))
            .expect("exits")
            .success()
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(collector.requests(), 0);
}

#[test]
fn logs_traces_and_metrics_export_without_request_data() {
    let collector = FakeCollector::start();
    let mut service = Service::start(&[
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", &collector.url()),
        ("TRUSTIOT__TELEMETRY__LOG_FILTER", "debug"),
    ]);
    echo_with_canary(&service);
    service.signal("TERM");
    // Buffered telemetry is exported during shutdown.
    assert!(
        service
            .wait_exit(Duration::from_secs(15))
            .expect("exits")
            .success()
    );

    let traces = collector.bodies("/v1/traces");
    let logs = collector.bodies("/v1/logs");
    assert!(!traces.is_empty(), "no traces exported");
    assert!(!logs.is_empty(), "no logs exported");
    let traces = traces.concat();
    let logs = logs.concat();
    let trace_id = trace_id_bytes();
    assert!(
        contains(&traces, &trace_id),
        "the request span continues the caller's trace"
    );
    assert!(
        contains(&logs, &trace_id),
        "request logs carry the caller's trace id"
    );
    assert!(
        contains(&traces, b"POST /v1/echo"),
        "span named after the route"
    );
    assert!(contains(&logs, b"request completed"));
    assert!(
        contains(&logs, b"echo request"),
        "the handler's debug event is exported"
    );
    // Metrics: the final reading is exported at shutdown.
    let metrics = collector.bodies("/v1/metrics");
    assert!(!metrics.is_empty(), "no metrics exported");
    let metrics = metrics.concat();
    assert!(
        contains(&metrics, b"example_service.echo.requests"),
        "the service's counter is exported"
    );
    for payload in [&traces, &logs, &metrics] {
        assert!(
            contains(payload, b"example-service"),
            "service.name resource"
        );
        assert!(
            !contains(payload, BODY_CANARY.as_bytes()),
            "request body exported"
        );
    }
}
