//! The errors the kit answers by itself, from a real `example-service` process: a handler that
//! runs past `http.request_timeout` gets 504, and a handler that panics gets 500 while the panic
//! is logged as one JSON line. With the `contract` feature (on by default) both responses carry
//! the M1 error body of crates/contract; without it, the status alone.

mod support;

use std::time::Duration;

use support::{Response, Service, get};

fn assert_kit_error(response: &Response, status: u16, code: &str) {
    assert_eq!(response.status, status, "{response:?}");
    if cfg!(feature = "contract") {
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(
            response.json(),
            serde_json::json!({ "code": code, "message_key": format!("error.{code}") })
        );
    } else {
        assert!(response.body.is_empty(), "{response:?}");
    }
}

#[test]
fn handler_timeouts_answer_504() {
    let mut service = Service::start(&[("TRUSTIOT__HTTP__REQUEST_TIMEOUT", "300ms")]);
    let response = get(service.address, "/v1/slow?ms=3000").expect("GET /v1/slow");
    assert_kit_error(&response, 504, "timeout");

    let warning = service
        .stdout
        .wait_for_message(
            "request timed out before the handler responded",
            Duration::from_secs(2),
        )
        .expect("the timeout is logged");
    assert_eq!(warning["level"], "WARN");
    assert_eq!(warning["fields"]["timeout_ms"], 300);
    let failed = service
        .stdout
        .wait_for_message("request failed", Duration::from_secs(2))
        .expect("the request is logged as failed");
    assert_eq!(failed["level"], "ERROR");
    assert_eq!(failed["fields"]["http.response.status_code"], 504);

    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits");
    assert!(status.success(), "{status}");
}

#[test]
fn panics_answer_500_and_are_logged_as_json() {
    let mut service = Service::start(&[]);
    let response = get(service.address, "/v1/panic").expect("GET /v1/panic");
    assert_kit_error(&response, 500, "internal");

    let panicked = service
        .stdout
        .wait_for_message("thread panicked", Duration::from_secs(2))
        .expect("the panic is logged");
    assert_eq!(panicked["level"], "ERROR");
    assert_eq!(panicked["target"], "service_kit::panic");
    assert_eq!(
        panicked["fields"]["payload"],
        "example-service: /v1/panic was called"
    );
    let location = panicked["fields"]["location"]
        .as_str()
        .expect("panic location");
    assert!(location.contains("example-service.rs"), "{location}");
    // The panic happened inside the request, so its line carries the request's trace id.
    assert!(panicked["trace_id"].is_string(), "{panicked}");

    // The service keeps serving.
    assert_eq!(
        get(service.address, "/v1/hello")
            .expect("GET /v1/hello")
            .status,
        200
    );
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits");
    assert!(status.success(), "{status}");
    // The JSON line replaces the default panic message on stderr.
    assert!(
        service.stderr.text().is_empty(),
        "{}",
        service.stderr.text()
    );
}
