//! The log output of a real `example-service` process: one JSON object per line, trace ids on
//! request logs, and no request data, even with every target logging at `trace`.

mod support;

use std::time::Duration;

use support::{Service, TRACE_ID, TRACEPARENT, request};

const BODY_CANARY: &str = "body-canary-7f3a";
const EMAIL: &str = "alice.canary@example.com";
const BEARER_CANARY: &str = "bearer-canary-91c2";
const COOKIE_CANARY: &str = "cookie-canary-55d0";
const QUERY_CANARY: &str = "query-canary-e81b";
const PATH_CANARY: &str = "path-canary-3b6d";

#[test]
fn logs_are_json_with_trace_ids_and_never_contain_request_data() {
    // `trace` for every target: the kit, axum, hyper, and everything else log all they can.
    let mut service = Service::start(&[("TRUSTIOT__TELEMETRY__LOG_FILTER", "trace")]);
    let address = service.address;
    let sensitive_headers = [
        ("Authorization", format!("Bearer {BEARER_CANARY}")),
        ("Cookie", format!("session={COOKIE_CANARY}")),
        ("traceparent", TRACEPARENT.to_owned()),
    ];
    let headers: Vec<(&str, &str)> = sensitive_headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let body = format!("{{\"text\":\"{BODY_CANARY} {EMAIL}\"}}");
    let echoed = request(
        address,
        "POST",
        &format!("/v1/echo?token={QUERY_CANARY}"),
        &headers,
        Some(&body),
    )
    .expect("POST /v1/echo");
    assert_eq!(echoed.status, 200);
    assert_eq!(echoed.json()["length"], BODY_CANARY.len() + 1 + EMAIL.len());

    // A body the handler rejects, a body that is not JSON, and an unknown path.
    let rejected = request(
        address,
        "POST",
        "/v1/echo",
        &[],
        Some(&format!(
            "{{\"text\":[\"{BODY_CANARY}\"],\"email\":\"{EMAIL}\"}}"
        )),
    )
    .expect("POST /v1/echo with a wrong type");
    assert_eq!(rejected.status, 422);
    let malformed = request(
        address,
        "POST",
        "/v1/echo",
        &[],
        Some(&format!("{BODY_CANARY}{{")),
    )
    .expect("POST /v1/echo with broken JSON");
    assert_eq!(malformed.status, 400);
    let missing = request(address, "GET", &format!("/v1/{PATH_CANARY}"), &[], None)
        .expect("GET an unknown path");
    assert_eq!(missing.status, 404);

    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits");
    assert!(status.success(), "{status}");

    let everything = format!("{}\n{}", service.stdout.text(), service.stderr.text());
    for canary in [
        BODY_CANARY,
        EMAIL,
        BEARER_CANARY,
        COOKIE_CANARY,
        QUERY_CANARY,
        PATH_CANARY,
    ] {
        assert!(
            !everything.contains(canary),
            "`{canary}` reached the logs:\n{everything}"
        );
    }
    assert!(
        service.stderr.text().is_empty(),
        "nothing is logged outside the JSON lines on stdout: {}",
        service.stderr.text()
    );

    let lines = service.stdout.json_lines();
    for line in &lines {
        for key in ["timestamp", "level", "service", "target", "message"] {
            assert!(line.get(key).is_some(), "`{key}` missing: {line}");
        }
        assert_eq!(line["service"], "example-service");
    }
    // The filter really let more than `info` through. (With the kit's dependency features no
    // dependency emits `trace` events, so there are none to look for.)
    assert!(
        lines.iter().any(|line| line["level"] == "DEBUG"),
        "debug events are logged"
    );

    let completed: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["message"] == "request completed")
        .collect();
    assert_eq!(completed.len(), 4, "{completed:?}");
    // The caller's trace continues: the trace id comes from `traceparent`.
    let first = completed[0];
    assert_eq!(first["trace_id"], TRACE_ID, "{first}");
    assert_eq!(first["fields"]["http.route"], "/v1/echo");
    assert_eq!(first["fields"]["http.request.method"], "POST");
    assert_eq!(first["fields"]["http.response.status_code"], 200);
    // Requests without `traceparent` start their own trace.
    for line in &completed[1..] {
        let trace_id = line["trace_id"].as_str().expect("trace id");
        assert_eq!(trace_id.len(), 32, "{line}");
        assert_ne!(trace_id, TRACE_ID);
        assert_ne!(trace_id, "0".repeat(32));
    }
    // An unmatched path has no route, and its raw path is not logged.
    assert!(completed[3]["fields"].get("http.route").is_none());
    // Events inside a handler share the request's trace id.
    let echo = lines
        .iter()
        .find(|line| line["message"] == "echo request")
        .expect("the handler's debug event");
    assert_eq!(echo["trace_id"], TRACE_ID);
    assert_eq!(
        echo["fields"]["length"],
        BODY_CANARY.len() + 1 + EMAIL.len()
    );
}
