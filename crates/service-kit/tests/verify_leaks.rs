//! Independent verification of Laysi/trustiot-v2#51: request data must never reach the JSON logs
//! or the OTLP export, through any path a request can take (headers, query, path, bodies, the
//! identity token, rejected requests, panics, timeouts), with every target logging at `trace`.
//!
//! This complements `logging.rs` and `otlp.rs`: it adds `x-internal-identity`, API-key and proxy
//! headers, a percent-encoded path, a body over the default size limit, the panic and timeout
//! paths with sensitive query strings, and a single run with OTLP export on and `trace` filter.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use support::{Service, TRACE_ID, TRACEPARENT, request};

const CANARIES: [&str; 12] = [
    "canary-bearer-6f1a",
    "canary-cookie-02be",
    "canary-query-77d3",
    "canary-path-c41e",
    "canary-body-90ab",
    "canary-xapikey-5d12",
    "canary-idtoken-3e8f",
    "canary-proxyauth-a9c7",
    "canary-referer-1b4d",
    "canary-useragent-e6f0",
    "canary-forwarded-8c35",
    "canary-email@example.com",
];

/// The path and body of one request the fake collector received.
type Received = (String, Vec<u8>);

/// A fake OTLP/HTTP receiver that records every request body.
struct Collector {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Collector {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind collector");
        let address = listener.local_addr().expect("collector address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let sink = Arc::clone(&sink);
                thread::spawn(move || read_request(stream, &sink));
            }
        });
        Self { address, received }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn all(&self) -> Vec<Received> {
        self.received.lock().expect("collector lock").clone()
    }
}

fn read_request(stream: TcpStream, sink: &Mutex<Vec<Received>>) -> Option<()> {
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
    sink.lock().expect("collector lock").push((path, body));
    let mut stream = stream;
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n",
        )
        .ok()
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
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII"), 16).expect("hex"))
        .collect()
}

#[test]
fn nothing_a_request_carries_reaches_logs_or_otlp_on_any_path() {
    let collector = Collector::start();
    let keys_file = support::keys_file(
        r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}"#,
    );
    let mut service = Service::start(&[
        ("TRUSTIOT__TELEMETRY__LOG_FILTER", "trace"),
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", &collector.url()),
        // A short handler timeout so /v1/slow takes the 504 path.
        ("TRUSTIOT__HTTP__REQUEST_TIMEOUT", "300ms"),
        // Identity keys set, so /v1/whoami exists and rejects the garbage token below.
        (support::KEYS_FILE_VARIABLE, &keys_file),
    ]);
    let address = service.address;
    let headers = [
        ("Authorization", format!("Bearer {}", CANARIES[0])),
        ("Cookie", format!("session={}", CANARIES[1])),
        ("X-Api-Key", CANARIES[5].to_owned()),
        ("X-Internal-Identity", format!("{0}.{0}.{0}", CANARIES[6])),
        ("Proxy-Authorization", format!("Basic {}", CANARIES[7])),
        ("Referer", format!("https://example.com/?t={}", CANARIES[8])),
        ("User-Agent", CANARIES[9].to_owned()),
        ("X-Forwarded-For", CANARIES[10].to_owned()),
        ("traceparent", TRACEPARENT.to_owned()),
    ];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let body = format!("{{\"text\":\"{} {}\"}}", CANARIES[4], CANARIES[11]);

    let statuses = [
        // Ordinary success, with the query and headers carrying secrets.
        request(
            address,
            "POST",
            &format!("/v1/echo?token={}", CANARIES[2]),
            &headers,
            Some(&body),
        ),
        request(
            address,
            "GET",
            &format!("/v1/hello?access_token={}", CANARIES[2]),
            &headers,
            None,
        ),
        // Unknown path with the secret in the path, plain and percent-encoded.
        request(
            address,
            "GET",
            &format!("/v1/{}", CANARIES[3]),
            &headers,
            None,
        ),
        request(
            address,
            "GET",
            &format!("/v1/%63anary-path-c41e/{}", CANARIES[3]),
            &headers,
            None,
        ),
        // The identity extractor rejecting a garbage token.
        request(address, "GET", "/v1/whoami", &headers, None),
        // Panic and timeout with sensitive query strings.
        request(
            address,
            "GET",
            &format!("/v1/panic?token={}", CANARIES[2]),
            &headers,
            None,
        ),
        request(
            address,
            "GET",
            &format!("/v1/slow?ms=2000&token={}", CANARIES[2]),
            &headers,
            None,
        ),
        // Rejected query and body.
        request(
            address,
            "GET",
            &format!("/v1/slow?ms={}", CANARIES[2]),
            &headers,
            None,
        ),
        request(
            address,
            "POST",
            "/v1/echo",
            &headers,
            Some(&format!("{}{{", CANARIES[4])),
        ),
        // Body over axum's default limit (2 MiB) with the canary at both ends.
        request(
            address,
            "POST",
            "/v1/echo",
            &headers,
            Some(&format!(
                "{{\"text\":\"{}{}{}\"}}",
                CANARIES[4],
                "x".repeat(3 * 1024 * 1024),
                CANARIES[11]
            )),
        ),
    ]
    .map(|response| response.map(|response| response.status).ok());
    // Record what each request got, for the report.
    eprintln!("statuses: {statuses:?}");
    assert_eq!(statuses[0], Some(200));
    assert_eq!(statuses[2], Some(404));
    // Without the `contract` feature the example has no identity route.
    assert_eq!(
        statuses[4],
        Some(if cfg!(feature = "contract") { 401 } else { 404 })
    );
    assert_eq!(statuses[5], Some(500));
    assert_eq!(statuses[6], Some(504));

    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(20))
        .expect("the service exits");
    assert!(status.success(), "{status}");
    thread::sleep(Duration::from_millis(300));

    let stdout = service.stdout.text();
    let stderr = service.stderr.text();
    let received = collector.all();
    assert!(!received.is_empty(), "nothing was exported");
    for canary in CANARIES {
        assert!(!stdout.contains(canary), "`{canary}` in stdout:\n{stdout}");
        assert!(!stderr.contains(canary), "`{canary}` in stderr:\n{stderr}");
        for (path, payload) in &received {
            assert!(
                !contains(payload, canary.as_bytes()),
                "`{canary}` exported to {path}"
            );
        }
    }
    assert!(stderr.is_empty(), "stderr: {stderr}");
    // Every stdout line is one JSON object; the request logs carry a trace id.
    let lines = service.stdout.json_lines();
    let completed = lines
        .iter()
        .filter(|line| {
            line["message"] == "request completed" || line["message"] == "request failed"
        })
        .collect::<Vec<_>>();
    assert!(completed.len() >= 9, "{} request lines", completed.len());
    for line in &completed {
        assert_eq!(line["trace_id"].as_str().map(str::len), Some(32), "{line}");
    }
    let traces: Vec<u8> = received
        .iter()
        .filter(|(path, _)| path == "/v1/traces")
        .flat_map(|(_, body)| body.clone())
        .collect();
    assert!(contains(&traces, &trace_id_bytes()));
}

#[test]
fn an_unreachable_collector_url_with_a_secret_path_does_not_reach_stdout() {
    // The SDK reports failed exports through its own diagnostics on stdout. A collector URL may
    // carry a token in its path, so the diagnostics must not repeat it. At the default filter
    // (`info`) they do not; below `info` the kit drops them (`tests/otlp.rs` covers `debug` and
    // `trace`).
    let mut service = Service::start(&[(
        "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
        "http://127.0.0.1:9/canary-endpoint-path-4a7b",
    )]);
    let response = request(service.address, "GET", "/v1/hello", &[], None).expect("GET");
    assert_eq!(response.status, 200);
    service.signal("TERM");
    let status = service.wait_exit(Duration::from_secs(20)).expect("exits");
    assert!(status.success(), "{status}");
    let everything = format!("{}\n{}", service.stdout.text(), service.stderr.text());
    assert!(
        !everything.contains("canary-endpoint-path-4a7b"),
        "the OTLP endpoint path reached the logs:\n{everything}"
    );
}
