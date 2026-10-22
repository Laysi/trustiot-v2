//! OpenAI Decisions through the gateway, with the recorded request and responses of
//! crates/ai-clients: forwarded byte for byte except the model, answered unchanged.

mod support;

use axum::http::StatusCode;
use support::{
    DECISIONS, Server, Step, Vars, fixture, http_response, json_reply, key_text, post,
    with_caller_model,
};

/// The recorded request names `gpt-6-luna`; the caller may name anything.
fn caller_request() -> String {
    with_caller_model(
        &fixture("decisions/router.request.json"),
        r#""model": "gpt-6-luna""#,
        r#""model": "whatever-the-caller-wrote""#,
    )
}

fn single(server: &Server) -> Vars {
    Vars::new()
        .provider("openai", "openai-decisions", server)
        .role("router", "openai/gpt-6-luna")
}

#[tokio::test]
async fn a_recorded_request_is_forwarded_and_its_response_returned() {
    let response = fixture("decisions/router.response.json");
    let server = Server::start(vec![vec![Step::Send(json_reply(
        &response,
        "x-request-id",
        "req_decisions_1",
    ))]])
    .await;
    let router = single(&server).router();

    let reply = post(
        &router,
        DECISIONS,
        &[("authorization", "Bearer caller")],
        &caller_request(),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), response);
    assert_eq!(reply.header("x-request-id"), Some("req_decisions_1"));
    assert_eq!(reply.header("ai-gateway-model"), Some("openai/gpt-6-luna"));
    assert_eq!(reply.header("ai-gateway-outcome"), Some("completed"));

    let sent = server.request(0);
    assert!(
        sent.head.starts_with("POST /v1/decisions HTTP/1.1"),
        "{}",
        sent.head
    );
    assert_eq!(
        String::from_utf8(sent.body.clone()).expect("UTF-8"),
        fixture("decisions/router.request.json"),
        "the recorded request, byte for byte"
    );
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {}", key_text())),
        "the gateway's key, not the caller's"
    );
}

#[tokio::test]
async fn a_refused_question_is_returned_as_the_models_answer() {
    let response = fixture("decisions/router_refusal.response.json");
    let server = Server::start(vec![vec![Step::Send(json_reply(
        &response,
        "x-request-id",
        "req_decisions_2",
    ))]])
    .await;
    let reply = post(&single(&server).router(), DECISIONS, &[], &caller_request()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), response);
    assert_eq!(reply.header("ai-gateway-outcome"), Some("refusal"));
}

#[tokio::test]
async fn rate_limits_and_overload_move_to_the_next_model() {
    let error = |status: u16| {
        http_response(
            status,
            &[("content-type", "application/json"), ("retry-after", "3")],
            br#"{"error":{"message":"Rate limit reached","type":"requests","param":null,"code":"rate_limit_exceeded"}}"#,
        )
    };
    for status in [429, 503] {
        let primary = Server::start(vec![vec![Step::Send(error(status))]]).await;
        let backup = Server::start(vec![vec![Step::Send(json_reply(
            &fixture("decisions/router.response.json"),
            "x-request-id",
            "req_backup",
        ))]])
        .await;
        let router = Vars::new()
            .provider("primary", "openai-decisions", &primary)
            .provider("backup", "openai-decisions", &backup)
            .role("router", "primary/gpt-6-luna,backup/gpt-6-luna-mini")
            .router();
        let reply = post(&router, DECISIONS, &[], &caller_request()).await;
        assert_eq!(reply.status, StatusCode::OK, "{status}: {}", reply.text());
        assert_eq!(
            reply.header("ai-gateway-model"),
            Some("backup/gpt-6-luna-mini")
        );
        let sent = String::from_utf8(backup.request(0).body).expect("UTF-8");
        assert!(sent.contains(r#""model": "gpt-6-luna-mini""#), "{sent}");
    }

    // With no model left, the provider's error comes back in OpenAI's envelope.
    let primary = Server::start(vec![vec![Step::Send(error(429))]]).await;
    let reply = post(
        &single(&primary).router(),
        DECISIONS,
        &[],
        &caller_request(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.header("retry-after"), Some("3"));
    assert_eq!(reply.json()["error"]["code"], "rate_limit_exceeded");
}

#[tokio::test]
async fn invalid_requests_and_unknown_roles_are_refused_before_anything_is_sent() {
    let server = Server::start(vec![]).await;
    let router = single(&server).router();

    let no_questions = r#"{"model":"x","input":"Hello"}"#;
    let reply = post(&router, DECISIONS, &[], no_questions).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "invalid_request_error");

    let reply = post(
        &router,
        "/internal/roles/missing/v1/decisions",
        &[],
        &caller_request(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"]["code"], "role_not_found");

    assert_eq!(server.request_count(), 0);
}
