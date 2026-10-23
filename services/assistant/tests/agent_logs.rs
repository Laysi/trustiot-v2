//! What the main agent loop logs (Laysi/trustiot-v2#145): every step with its duration, and
//! gateway errors without their text. The events are captured with a subscriber installed on
//! the test's own thread.
//!
//! A test binary of its own whose tests run one at a time (Laysi/trustiot-v2#581). tracing
//! decides once per process whether a log statement is enabled and caches that. While at most
//! one scoped subscriber exists, tracing-core 0.1.36 asks only the subscriber of the thread that
//! reaches a statement first (`DefaultCallsite::register` with `Rebuilder::JustOne` in
//! src/callsite.rs, which calls `dispatcher::get_default`; `Dispatchers::register_dispatch`
//! keeps `has_just_one` true while `dispatchers.len() <= 1`). When another test's thread, which
//! has no subscriber, reached "model call completed" first, the statement was cached as never
//! enabled and the capture got none of its events (CI run 39927989567: 0 of 3). Here no other
//! test runs while one captures, so the capturing thread is the only one that reaches the
//! agent's log statements. Every test in this file must therefore take `alone()` first and run
//! the agent on its own thread (the default current-thread runtime); a test that does not
//! capture logs belongs in tests/agent.rs.

mod support;

use std::sync::Arc;

use assistant::AgentError;
use assistant::tools::{NoDelegation, Tool};
use service_kit::telemetry::is_sensitive;
use support::{
    Capture, CountReadings, Reply, StandIn, agent, fixture, ok, principal, respond, settings,
    tool_step,
};
use tokio::sync::{Mutex, MutexGuard};
use tracing_subscriber::layer::SubscriberExt as _;

/// Held by each test of this binary for its whole run (see the module documentation). A tokio
/// mutex: it is held across awaits, and a test that panics releases it while unwinding.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// Waits until no other test of this binary runs, and keeps them waiting until the guard drops.
async fn alone() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().await
}

fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CountReadings)]
}

#[tokio::test]
async fn gateway_errors_reach_the_user_without_their_text() {
    let _alone = alone().await;
    let detail = format!("upstream detail {}", uuid::Uuid::now_v7());
    for (status, error_type, expected) in [
        (504, "timeout_error", AgentError::ModelTimeout),
        (529, "overloaded_error", AgentError::ModelUnavailable),
        (502, "api_error", AgentError::ModelUnavailable),
        (400, "invalid_request_error", AgentError::ModelFailed),
    ] {
        let body = serde_json::json!({
            "type": "error",
            "error": {"type": error_type, "message": detail},
        })
        .to_string();
        let stand_in = StandIn::start(respond(move |_request, _index| {
            Reply::Json(status, body.clone())
        }))
        .await;
        let agent = agent(&stand_in, tools(), settings());
        let mut conversation = agent.start_conversation(principal(), "");
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let error = agent
            .ask(&mut conversation, "Anything?", Arc::new(NoDelegation))
            .await
            .expect_err("no answer");

        assert_eq!(error, expected, "status {status}");
        assert!(!error.to_string().contains(&detail));
        let events = capture.events();
        let failed = events
            .iter()
            .find(|event| event.message == "model call failed")
            .expect("the failure is logged");
        assert_eq!(
            failed.fields.get("status").map(String::as_str),
            Some(&*status.to_string())
        );
        assert_eq!(
            failed.fields.get("error_type").map(String::as_str),
            Some(error_type)
        );
        for event in &events {
            assert!(
                !format!("{event:?}").contains(&detail),
                "the gateway's message reached the logs: {event:?}"
            );
        }
    }
}

#[tokio::test]
async fn every_step_is_logged_with_its_duration() {
    let _alone = alone().await;
    let stand_in = StandIn::start(respond(|_request, index| {
        if index < 2 {
            ok(tool_step(index))
        } else {
            ok(fixture("answer_end_turn.json"))
        }
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let question = "Which meters on floor 3 are quiet?";

    agent
        .ask(&mut conversation, question, Arc::new(NoDelegation))
        .await
        .expect("an answer");

    let events = capture.events();
    let named = |message: &str| -> Vec<&support::Event> {
        events
            .iter()
            .filter(|event| event.message == message)
            .collect()
    };
    let model_calls = named("model call completed");
    assert_eq!(model_calls.len(), 3);
    let mut call_ids = Vec::new();
    for (index, event) in model_calls.iter().enumerate() {
        let field = |name: &str| {
            event
                .fields
                .get(name)
                .unwrap_or_else(|| panic!("model call {index} has no {name}"))
                .clone()
        };
        assert_eq!(field("step"), (index + 1).to_string());
        field("duration_ms")
            .parse::<u64>()
            .expect("a duration in ms");
        call_ids.push(field("llm_call_id"));
        assert_eq!(field("model"), "anthropic/claude-opus-5-5");
        let expected_cache_read = if index < 2 { "4096" } else { "100000" };
        assert_eq!(field("usage.cache_read"), expected_cache_read);
    }
    call_ids.sort();
    call_ids.dedup();
    assert_eq!(
        call_ids.len(),
        3,
        "every model call has its own llm_call_id"
    );
    let tool_calls = named("tool call completed");
    assert_eq!(tool_calls.len(), 2);
    for event in &tool_calls {
        assert_eq!(event.fields.get("outcome").map(String::as_str), Some("ok"));
        assert!(event.fields.contains_key("duration_ms"));
    }
    let finished = named("agent run finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].fields.get("steps").map(String::as_str),
        Some("3")
    );
    assert!(finished[0].fields.contains_key("duration_ms"));
    // Nothing the assistant logs is chat content, and no field name is one the log pipeline
    // would redact.
    for event in events
        .iter()
        .filter(|event| event.target.starts_with("assistant"))
    {
        let text = format!("{event:?}");
        assert!(
            !text.contains(question),
            "the question reached the logs: {text}"
        );
        assert!(
            !text.contains("60 readings"),
            "a tool result reached the logs: {text}"
        );
        for name in event.fields.keys() {
            assert!(!is_sensitive(name), "field {name} would be redacted");
        }
    }
}
