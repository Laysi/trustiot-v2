//! Anthropic Messages through the gateway: the body reaches the provider byte for byte except
//! the model, responses and streams come back unchanged, and requests the gateway cannot
//! forward are refused before anything is sent.

mod support;

use axum::http::StatusCode;
use support::{
    MESSAGES, Server, Step, Vars, fixture, json_reply, key_text, post, sse_without_comments,
    stream_reply, with_caller_model,
};

/// A request a real caller sends: adaptive thinking with an earlier turn's thinking block and
/// its signature, cache breakpoints, a system message mid-conversation, escapes, unusual
/// spacing and number formats. Only the `model` value may change on the way.
const REQUEST: &str = r#"{
  "model" : "caller-choice",
  "max_tokens": 1024,
  "thinking": {"type": "adaptive"},
  "system": [{"type": "text", "text": "You answer questions about devices.", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
  "messages": [
    {"role": "user", "content": [{"type": "text", "text": "你好 \"quoted\"", "cache_control": {"type": "ephemeral"}}]},
    {"role": "assistant", "content": [
      {"type": "thinking", "thinking": "The user greets me.", "signature": "test-signature-1"},
      {"type": "text", "text": "Hello."}
    ]},
    {"role": "system", "content": "The user's time zone is Asia/Taipei."},
    {"role": "user", "content": "How many devices?"}
  ],
  "temperature":1.0E0,   "metadata": {"model": "not this one"}
}
"#;

fn single(server: &Server) -> Vars {
    Vars::new()
        .provider("anthropic", "anthropic-messages", server)
        .role("main", "anthropic/claude-opus-5-5")
}

#[tokio::test]
async fn the_body_reaches_the_provider_byte_for_byte_except_the_model() {
    let message = fixture("anthropic/message_end_turn.json");
    let server = Server::start(vec![vec![Step::Send(json_reply(
        &message,
        "request-id",
        "req_test_1",
    ))]])
    .await;
    let router = single(&server).router();

    let reply = post(
        &router,
        MESSAGES,
        &[
            ("anthropic-beta", "context-management-2025-06-27"),
            ("x-api-key", "caller-supplied"),
            ("anthropic-version", "1999-01-01"),
        ],
        REQUEST,
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), message, "the response comes back unchanged");
    assert_eq!(
        reply.header("ai-gateway-model"),
        Some("anthropic/claude-opus-5-5")
    );
    assert_eq!(reply.header("ai-gateway-outcome"), Some("completed"));
    assert_eq!(reply.header("request-id"), Some("req_test_1"));
    assert_eq!(reply.header("content-type"), Some("application/json"));

    assert_eq!(server.request_count(), 1);
    let sent = server.request(0);
    let expected = with_caller_model(REQUEST, r#""caller-choice""#, r#""claude-opus-5-5""#);
    assert_eq!(
        String::from_utf8(sent.body.clone()).expect("UTF-8"),
        expected,
        "only the model value changes"
    );
    assert!(
        sent.head.starts_with("POST /v1/messages HTTP/1.1"),
        "{}",
        sent.head
    );
    assert_eq!(
        sent.header("x-api-key"),
        Some(key_text()),
        "the gateway's key"
    );
    assert_eq!(
        sent.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        sent.header("anthropic-beta").as_deref(),
        Some("context-management-2025-06-27")
    );
    assert_eq!(
        sent.header("ai-gateway-tenant"),
        None,
        "gateway headers stay"
    );
}

#[tokio::test]
async fn a_missing_model_is_filled_in_from_the_role() {
    let server = Server::start(vec![vec![Step::Send(json_reply(
        &fixture("anthropic/message_end_turn.json"),
        "request-id",
        "req_test_2",
    ))]])
    .await;
    let router = single(&server).router();
    let body = r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#;
    let reply = post(&router, MESSAGES, &[], body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        server.request(0).body,
        br#"{"model":"claude-opus-5-5","max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#
    );
}

#[tokio::test]
async fn a_stream_comes_back_unchanged() {
    let events = fixture("anthropic/stream_thinking.sse");
    let server = Server::start(vec![vec![Step::Send(stream_reply(&events))]]).await;
    let router = single(&server).router();
    let request = with_caller_model(
        REQUEST,
        r#""max_tokens": 1024,"#,
        r#""max_tokens": 1024, "stream": true,"#,
    );

    let reply = post(&router, MESSAGES, &[], &request).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), sse_without_comments(&events));
    assert_eq!(
        reply.header("content-type"),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(reply.header("request-id"), Some("req_test_stream"));
    assert_eq!(
        reply.header("ai-gateway-model"),
        Some("anthropic/claude-opus-5-5")
    );
    let sent = server.request(0);
    assert_eq!(
        String::from_utf8(sent.body.clone()).expect("UTF-8"),
        with_caller_model(&request, r#""caller-choice""#, r#""claude-opus-5-5""#)
    );
    assert_eq!(sent.header("accept").as_deref(), Some("text/event-stream"));
}

/// Once a stream has produced output, its end is the provider's: an error event mid-stream is
/// relayed as it came, and no other model is tried.
#[tokio::test]
async fn a_stream_that_fails_after_its_first_output_is_relayed_as_it_came() {
    let events = fixture("anthropic/stream_overloaded.sse");
    let primary = Server::start(vec![vec![Step::Send(stream_reply(&events))]]).await;
    let backup = Server::start(vec![]).await;
    let router = Vars::new()
        .provider("primary", "anthropic-messages", &primary)
        .provider("backup", "anthropic-messages", &backup)
        .role("main", "primary/claude-opus-5-5,backup/claude-sonnet-5-5")
        .router();

    let reply = post(
        &router,
        MESSAGES,
        &[],
        r#"{"stream":true,"max_tokens":16,"messages":[]}"#,
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), sse_without_comments(&events));
    assert_eq!(backup.request_count(), 0);
}

#[tokio::test]
async fn a_refused_stream_is_relayed_as_it_came() {
    let events = fixture("anthropic/stream_refusal.sse");
    let server = Server::start(vec![vec![Step::Send(stream_reply(&events))]]).await;
    let router = single(&server).router();
    let reply = post(
        &router,
        MESSAGES,
        &[],
        r#"{"stream":true,"max_tokens":16,"messages":[]}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), sse_without_comments(&events));
}

#[tokio::test]
async fn requests_that_cannot_be_forwarded_are_refused_before_anything_is_sent() {
    let server = Server::start(vec![]).await;
    let router = single(&server)
        .set("AI_GATEWAY__MAX_REQUEST_BYTES", "2048")
        .router();
    let ok = r#"{"max_tokens":16,"messages":[]}"#;

    let unknown = post(&router, "/internal/roles/other/v1/messages", &[], ok).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["type"], "error");
    assert_eq!(unknown.json()["error"]["type"], "not_found_error");

    let wrong_protocol = post(&router, "/internal/roles/main/v1/decisions", &[], ok).await;
    assert_eq!(wrong_protocol.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        wrong_protocol.json()["error"]["type"],
        "invalid_request_error"
    );

    for (body, why) in [
        ("{\"max_tokens\":16", "truncated JSON"),
        ("[1,2,3]", "not an object"),
        (r#"{"model":"a","model":"b"}"#, "a repeated field"),
        (r#"{"stream":"yes"}"#, "a stream that is not a boolean"),
    ] {
        let reply = post(&router, MESSAGES, &[], body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(
            reply.json()["error"]["type"],
            "invalid_request_error",
            "{why}"
        );
    }

    let large = format!(r#"{{"pad":"{}"}}"#, "x".repeat(4096));
    let reply = post(&router, MESSAGES, &[], &large).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.json()["error"]["type"], "request_too_large");

    let reply = post(&router, MESSAGES, &[("anthropic-beta", "not a name")], ok).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let request = axum::http::Request::post(MESSAGES)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(ok))
        .expect("a request");
    let reply = support::send(&router, request).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "no tenant");
    assert!(
        reply.text().contains("ai-gateway-tenant"),
        "{}",
        reply.text()
    );

    assert_eq!(server.request_count(), 0, "nothing reached the provider");
}

#[tokio::test]
async fn a_body_that_does_not_arrive_in_time_is_a_timeout() {
    // A 3 s request timeout leaves a budget of 2.7 s; the role's own limits must fit in it.
    let server = Server::start(vec![]).await;
    let router = single(&server)
        .set("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__FIRST_TOKEN", "1s")
        .set("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE", "1s")
        .router_with_request_timeout(std::time::Duration::from_secs(3));
    let never = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
    let request = axum::http::Request::post(MESSAGES)
        .header("content-type", "application/json")
        .header(ai_gateway::TENANT_HEADER, support::TENANT)
        .body(axum::body::Body::from_stream(never))
        .expect("a request");
    let reply = support::send(&router, request).await;
    assert_eq!(
        reply.status,
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        reply.text()
    );
    assert_eq!(reply.json()["error"]["type"], "timeout_error");
    assert_eq!(server.request_count(), 0, "nothing reached the provider");
}
