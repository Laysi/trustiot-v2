//! Fallback along a role's models: on 429, overload or timeout the next model answers; other
//! failures and refusals come back from the model that produced them.

mod support;

use axum::http::StatusCode;
use support::{
    MESSAGES, Server, Step, Vars, anthropic_error, fixture, http_response, json_reply, post,
    sse_without_comments, stream_reply,
};

/// The role's response limit in the test where the first model never answers: the gateway
/// must reach it for the first model, while the second model (a loopback reply of a few
/// hundred bytes) must answer well within it.
const TIMEOUT_UNDER_TEST: &str = "15s";

const BODY: &str = r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#;
const STREAM_BODY: &str = r#"{"stream":true,"max_tokens":16,"messages":[]}"#;

struct Pair {
    primary: Server,
    backup: Server,
    vars: Vars,
}

/// A role whose first model is at `primary` and whose fallback is at `backup`.
async fn pair(primary: Vec<Vec<Step>>, backup: Vec<Vec<Step>>) -> Pair {
    let primary = Server::start(primary).await;
    let backup = Server::start(backup).await;
    let vars = Vars::new()
        .provider("primary", "anthropic-messages", &primary)
        .provider("backup", "anthropic-messages", &backup)
        .role("main", "primary/claude-opus-5-5,backup/claude-sonnet-5-5");
    Pair {
        primary,
        backup,
        vars,
    }
}

fn message() -> Vec<Vec<Step>> {
    vec![vec![Step::Send(json_reply(
        &fixture("anthropic/message_end_turn.json"),
        "request-id",
        "req_backup",
    ))]]
}

fn once(response: Vec<u8>) -> Vec<Vec<Step>> {
    vec![vec![Step::Send(response)]]
}

fn body_of(server: &Server) -> String {
    String::from_utf8(server.request(0).body).expect("UTF-8")
}

#[tokio::test]
async fn rate_limits_overload_and_provider_timeouts_move_to_the_next_model() {
    for situation in ["rate limited", "overloaded", "provider timeout"] {
        let pair = pair(once(anthropic_error(situation)), message()).await;
        let reply = post(&pair.vars.router(), MESSAGES, &[], BODY).await;

        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{situation}: {}",
            reply.text()
        );
        assert_eq!(reply.text(), fixture("anthropic/message_end_turn.json"));
        assert_eq!(
            reply.header("ai-gateway-model"),
            Some("backup/claude-sonnet-5-5"),
            "{situation}"
        );
        assert_eq!(reply.header("request-id"), Some("req_backup"));
        assert_eq!(pair.primary.request_count(), 1, "{situation}");
        assert_eq!(pair.backup.request_count(), 1, "{situation}");
        // The same body went to both, each with its own model.
        assert_eq!(
            body_of(&pair.primary),
            BODY.replacen('{', r#"{"model":"claude-opus-5-5","#, 1)
        );
        assert_eq!(
            body_of(&pair.backup),
            BODY.replacen('{', r#"{"model":"claude-sonnet-5-5","#, 1)
        );
    }
}

#[tokio::test]
async fn a_model_that_does_not_answer_in_time_moves_to_the_next() {
    let pair = pair(vec![vec![Step::Hang]], message()).await;
    let router = pair
        .vars
        .clone()
        .set(
            "AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE",
            TIMEOUT_UNDER_TEST,
        )
        .router();
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.header("ai-gateway-model"),
        Some("backup/claude-sonnet-5-5")
    );
    assert_eq!(pair.primary.request_count(), 1);
}

#[tokio::test]
async fn a_model_at_its_concurrency_limit_moves_to_the_next() {
    let pair = pair(vec![vec![Step::Hang]], message()).await;
    let router = pair
        .vars
        .clone()
        .set("AI_GATEWAY__ROLES__MAIN__MAX_CONCURRENT", "1")
        .router();
    // The first call holds the first model's only slot.
    let holder = {
        let router = router.clone();
        tokio::spawn(async move { post(&router, MESSAGES, &[], BODY).await })
    };
    pair.primary.wait_for_requests(1).await;

    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.header("ai-gateway-model"),
        Some("backup/claude-sonnet-5-5")
    );
    assert_eq!(
        pair.primary.request_count(),
        1,
        "the second call was not sent there"
    );
    holder.abort();
}

#[tokio::test]
async fn a_stream_that_fails_before_its_first_output_moves_to_the_next_model() {
    let overloaded = fixture("anthropic/stream_overloaded.sse");
    // `message_start`, then the provider's overloaded error event, before any content.
    let start = overloaded
        .split("event: content_block_start")
        .next()
        .expect("events");
    let error = overloaded
        .split("event: error")
        .nth(1)
        .map(|rest| format!("event: error{rest}"))
        .expect("the error event");
    let failing = format!("{start}{error}");
    let events = fixture("anthropic/stream_text.sse");

    for (first, why) in [
        (stream_reply(&failing), "error event"),
        (anthropic_error("rate limited"), "HTTP 429"),
    ] {
        let pair = pair(once(first), once(stream_reply(&events))).await;
        let reply = post(&pair.vars.router(), MESSAGES, &[], STREAM_BODY).await;
        assert_eq!(reply.status, StatusCode::OK, "{why}");
        assert_eq!(
            reply.text(),
            sse_without_comments(&events),
            "{why}: only the second model's stream reaches the caller"
        );
        assert_eq!(
            reply.header("ai-gateway-model"),
            Some("backup/claude-sonnet-5-5")
        );
    }

    // With no model left, the last stream's events and its error come back as they came.
    let pair = pair(once(stream_reply(&failing)), once(stream_reply(&failing))).await;
    let reply = post(&pair.vars.router(), MESSAGES, &[], STREAM_BODY).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), sse_without_comments(&failing));
    assert_eq!(pair.backup.request_count(), 1);
}

#[tokio::test]
async fn when_every_model_is_rate_limited_the_last_error_comes_back() {
    let pair = pair(
        once(anthropic_error("rate limited")),
        once(anthropic_error("rate limited")),
    )
    .await;
    let reply = post(&pair.vars.router(), MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.header("retry-after"), Some("17"));
    let body = reply.json();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("rate limit")),
        "{body}"
    );
    assert_eq!(pair.backup.request_count(), 1);
}

#[tokio::test]
async fn other_errors_come_back_without_trying_the_next_model() {
    for (situation, status) in [
        (
            "invalid request (prefill not supported)",
            StatusCode::BAD_REQUEST,
        ),
        ("internal error", StatusCode::INTERNAL_SERVER_ERROR),
        ("authentication", StatusCode::BAD_GATEWAY),
    ] {
        let pair = pair(once(anthropic_error(situation)), message()).await;
        let reply = post(&pair.vars.router(), MESSAGES, &[], BODY).await;
        assert_eq!(reply.status, status, "{situation}: {}", reply.text());
        assert_eq!(pair.backup.request_count(), 0, "{situation}");
    }
}

#[tokio::test]
async fn a_refusal_is_the_models_answer_not_a_failure() {
    let refusal = fixture("anthropic/message_refusal.json");
    let whole = pair(
        once(json_reply(&refusal, "request-id", "req_refusal")),
        message(),
    )
    .await;
    let reply = post(&whole.vars.router(), MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), refusal);
    assert_eq!(reply.header("ai-gateway-outcome"), Some("refusal"));
    assert_eq!(whole.backup.request_count(), 0);

    let events = fixture("anthropic/stream_refusal.sse");
    let streamed = pair(once(stream_reply(&events)), message()).await;
    let reply = post(&streamed.vars.router(), MESSAGES, &[], STREAM_BODY).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), sse_without_comments(&events));
    assert_eq!(streamed.backup.request_count(), 0);
}

#[tokio::test]
async fn a_single_model_that_does_not_answer_in_time_is_a_timeout() {
    let server = Server::start(vec![vec![Step::Hang]]).await;
    let router = Vars::new()
        .provider("anthropic", "anthropic-messages", &server)
        .role("main", "anthropic/claude-opus-5-5")
        .set("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE", "1s")
        .router();
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(reply.json()["error"]["type"], "timeout_error");
}

#[tokio::test]
async fn a_provider_error_never_echoes_the_gateways_key() {
    let key = support::key_text();
    let echo = serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": format!("bad header {key}")},
    })
    .to_string();
    let server = Server::start(vec![vec![Step::Send(http_response(
        400,
        &[("content-type", "application/json")],
        echo.as_bytes(),
    ))]])
    .await;
    let router = Vars::new()
        .provider("anthropic", "anthropic-messages", &server)
        .role("main", "anthropic/claude-opus-5-5")
        .router();
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(!reply.text().contains(&key), "{}", reply.text());
    assert_eq!(reply.json()["error"]["message"], "bad header [REDACTED]");
}
