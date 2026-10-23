//! The main agent loop against a stand-in AI gateway that answers with recorded responses
//! (tests/fixtures): long analyses without a step limit, the wind-down near the token budget,
//! an append-only history sent back byte for byte, concurrent users, generic errors and the
//! per-step logs (Laysi/trustiot-v2#145).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use assistant::history::EntryKind;
use assistant::prompt::{BUDGET_EXHAUSTED_RESULT, WIND_DOWN};
use assistant::tools::{NoDelegation, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use assistant::{AgentError, AgentSettings};
use serde_json::Value;
use service_kit::telemetry::is_sensitive;
use support::{
    Capture, CountReadings, Reply, StandIn, WAIT, agent, fixture, ok, principal, respond, settings,
    tool_step,
};
use tokio::sync::{Barrier, Notify};

/// Tokens one recorded step processes: 312 input, 4096 cache reads, 188 output.
const STEP_TOKENS: u64 = 312 + 4096 + 188;

fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CountReadings)]
}

fn is_wind_down(message: &Value) -> bool {
    message["role"] == "system" && message["content"] == WIND_DOWN
}

fn messages(request: &support::Recorded) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .clone()
}

#[tokio::test]
async fn a_long_analysis_runs_without_a_step_limit() {
    const STEPS: usize = 60;
    let stand_in = StandIn::start(respond(|_request, index| {
        if index < STEPS {
            ok(tool_step(index))
        } else {
            ok(fixture("answer_end_turn.json"))
        }
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");

    let answer = agent
        .ask(
            &mut conversation,
            "Did every meter on floor 3 report?",
            Arc::new(NoDelegation),
        )
        .await
        .expect("an answer");

    assert_eq!(
        answer.text,
        "Yes. All twelve meters on floor 3 reported in the last hour."
    );
    assert!(!answer.truncated);
    assert_eq!(answer.steps, u32::try_from(STEPS + 1).expect("a u32"));
    let requests = stand_in.requests();
    assert_eq!(requests.len(), STEPS + 1);
    // Every step's tool call was answered: the last request carries 60 tool results, each with
    // the tool's output for its own input.
    let last = requests.last().expect("a request");
    for step in 0..STEPS {
        assert!(
            last.mentions(&format!("floor-3-{step:04}: 60 readings")),
            "the result of step {step} is missing"
        );
    }
    // No wind-down: the budget was not near.
    assert!(
        !requests
            .iter()
            .any(|request| messages(request).iter().any(is_wind_down))
    );
    // The history: the question, 60 steps with their results, and the answer.
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(kinds.len(), 2 + 2 * STEPS);
    assert_eq!(kinds.first(), Some(&EntryKind::Question));
    assert_eq!(kinds.last(), Some(&EntryKind::Answer));
    assert_eq!(
        conversation
            .entries()
            .filter(|(kind, _)| kind.shown())
            .count(),
        2,
        "only the question and the answer are shown; steps and tool results are kept hidden"
    );
}

#[tokio::test]
async fn the_agent_winds_down_near_the_budget_and_answers() {
    // The budget fits 10 recorded steps; the wind-down comes at 80 %, after the 8th.
    let budget = STEP_TOKENS * 10;
    let stand_in = StandIn::start(respond(|request, index| {
        // The model follows the note: the stand-in answers once the last message is it.
        if request.last_role() == "system" {
            ok(fixture("answer_end_turn.json"))
        } else {
            ok(tool_step(index))
        }
    }))
    .await;
    let agent = agent(
        &stand_in,
        tools(),
        AgentSettings {
            budget_tokens: budget,
            ..settings()
        },
    );
    let mut conversation = agent.start_conversation(principal(), "");

    let answer = agent
        .ask(
            &mut conversation,
            "Check every meter.",
            Arc::new(NoDelegation),
        )
        .await
        .expect("an answer within the budget");

    let requests = stand_in.requests();
    // 8 steps reach 80 % (8 × 4596 = 36768 of 45960); the 9th request carries the note.
    assert_eq!(requests.len(), 9);
    for (index, request) in requests.iter().enumerate() {
        let notes = messages(request).iter().filter(|m| is_wind_down(m)).count();
        assert_eq!(notes, usize::from(index == 8), "request {index}");
        if index == 8 {
            assert_eq!(request.last_role(), "system", "the note comes last");
        }
    }
    assert_eq!(answer.steps, 9);
    // The note joined the history right before the answer it led to.
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds.get(kinds.len() - 2..),
        Some(&[EntryKind::Note, EntryKind::Answer][..])
    );
}

#[tokio::test]
async fn a_call_that_would_cross_the_budget_is_not_sent() {
    // A model that ignores the budget keeps calling tools. The budget fits two and a half
    // recorded steps: the second call is sent with its output limit lowered to the room left,
    // and the third, whose input alone (the history the model counted, 4596 tokens) does not
    // fit, is not sent at all.
    let budget = STEP_TOKENS * 2 + STEP_TOKENS / 2;
    let stand_in = StandIn::start(respond(|_request, index| ok(tool_step(index)))).await;
    let agent = agent(
        &stand_in,
        tools(),
        AgentSettings {
            budget_tokens: budget,
            ..settings()
        },
    );
    let mut conversation = agent.start_conversation(principal(), "");

    let error = agent
        .ask(
            &mut conversation,
            "Check every meter.",
            Arc::new(NoDelegation),
        )
        .await
        .expect_err("the budget is used up");

    assert_eq!(error, AgentError::BudgetExhausted);
    let requests = stand_in.requests();
    assert_eq!(
        requests.len(),
        2,
        "the third call does not fit and is not sent"
    );
    // The second call: one step spent and the history counted at one step's size leave at
    // most this much for its output.
    let second_max_tokens = requests[1].json()["max_tokens"]
        .as_u64()
        .expect("max_tokens");
    assert!(
        (1_024..=budget - 2 * STEP_TOKENS).contains(&second_max_tokens),
        "the second call's output limit {second_max_tokens} does not fit the budget"
    );
    // Both steps ran their tools, and the history ends with their results.
    let (kind, last) = conversation.entries().last().expect("an entry");
    assert_eq!(kind, EntryKind::ToolResults);
    assert!(last.json().contains("60 readings"));
}

#[tokio::test]
async fn a_question_whose_first_call_does_not_fit_is_not_sent() {
    let stand_in = StandIn::start(respond(|_request, index| ok(tool_step(index)))).await;
    let agent = agent(
        &stand_in,
        tools(),
        AgentSettings {
            budget_tokens: 1_000,
            ..settings()
        },
    );
    let mut conversation = agent.start_conversation(principal(), "");

    let error = agent
        .ask(
            &mut conversation,
            "Check every meter.",
            Arc::new(NoDelegation),
        )
        .await
        .expect_err("the budget is too small for one call");

    assert_eq!(error, AgentError::BudgetExhausted);
    assert!(stand_in.requests().is_empty());
}

#[tokio::test]
async fn tool_calls_past_the_budget_are_not_run() {
    // The second step uses far more than the estimate allowed for (as when the gateway tried
    // several models): the budget is used up after it, so its tool call does not run.
    let budget = STEP_TOKENS * 3;
    let stand_in = StandIn::start(respond(|_request, index| {
        let step = tool_step(index);
        if index == 1 {
            ok(step.replace("\"output_tokens\": 188", "\"output_tokens\": 20000"))
        } else {
            ok(step)
        }
    }))
    .await;
    let agent = agent(
        &stand_in,
        tools(),
        AgentSettings {
            budget_tokens: budget,
            ..settings()
        },
    );
    let mut conversation = agent.start_conversation(principal(), "");

    let error = agent
        .ask(
            &mut conversation,
            "Check every meter.",
            Arc::new(NoDelegation),
        )
        .await
        .expect_err("the budget is used up");

    assert_eq!(error, AgentError::BudgetExhausted);
    assert_eq!(stand_in.requests().len(), 2);
    // The second step's call got a result that says why it did not run, so the history stays
    // valid for the next question.
    let (kind, last) = conversation.entries().last().expect("an entry");
    assert_eq!(kind, EntryKind::ToolResults);
    assert!(last.json().contains(BUDGET_EXHAUSTED_RESULT));
    assert!(!last.json().contains("60 readings"));
}

#[tokio::test]
async fn later_requests_resend_the_history_byte_for_byte() {
    // Each question takes two tool steps, then the answer.
    let stand_in = StandIn::start(respond(|_request, index| {
        if index % 3 == 2 {
            ok(fixture("answer_end_turn.json"))
        } else {
            ok(tool_step(index))
        }
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let user = principal();
    let mut conversation = agent.start_conversation(user, "Site: plant A; meters: kWh.");

    for question in ["First question?", "Second question?"] {
        agent
            .ask(&mut conversation, question, Arc::new(NoDelegation))
            .await
            .expect("an answer");
    }

    let requests = stand_in.requests();
    assert_eq!(requests.len(), 6);
    let first = requests.first().expect("a request").json();
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.path, "/internal/roles/main/v1/messages");
        assert_eq!(
            request.tenant,
            Some(user.tenant_id.to_string()),
            "request {index}"
        );
        let body = request.json();
        assert!(body.get("model").is_none(), "the gateway sets the model");
        // The prefix the cache keys on never changes: tools, system blocks, model settings.
        for field in [
            "tools",
            "system",
            "thinking",
            "output_config",
            "cache_control",
            "max_tokens",
        ] {
            assert_eq!(
                body[field], first[field],
                "{field} changed in request {index}"
            );
        }
    }
    // Cache layout: tools, then the system prompt and the tenant context with a breakpoint
    // each, then automatic caching for the conversation.
    assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
    // The tenant context goes in as data, between the tags the system prompt names.
    assert_eq!(
        first["system"][1]["text"],
        "<tenant_context>\nSite: plant A; meters: kWh.\n</tenant_context>"
    );
    assert_eq!(first["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(first["cache_control"]["type"], "ephemeral");
    assert_eq!(first["thinking"]["type"], "adaptive");
    assert_eq!(first["tools"][0]["name"], "count_readings");

    // Each request starts with every message of the one before, byte for byte.
    for pair in requests.windows(2) {
        let (before, after) = (pair[0].raw_messages(), pair[1].raw_messages());
        assert!(after.len() > before.len());
        assert_eq!(after.get(..before.len()), Some(&before[..]));
    }
    // A model message goes back exactly as the gateway returned its content: thinking text and
    // signatures included.
    let returned: Value = serde_json::from_str(&tool_step(0)).expect("JSON");
    let content_bytes = {
        #[derive(serde::Deserialize)]
        struct Content<'a> {
            #[serde(borrow)]
            content: &'a serde_json::value::RawValue,
        }
        let text = tool_step(0);
        let parsed: Content<'_> = serde_json::from_str(&text).expect("JSON");
        parsed.content.get().to_owned()
    };
    let sent_back = requests[1].raw_messages()[1].clone();
    assert_eq!(
        sent_back,
        format!(r#"{{"role":"assistant","content":{content_bytes}}}"#)
    );
    assert!(
        sent_back.contains(
            returned["content"][1]["signature"]
                .as_str()
                .expect("a signature")
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_users_asking_at_once_do_not_wait_for_each_other() {
    // The stand-in answers the first model call of each user only once both calls arrived: if
    // one question waited for the other, neither would get past it.
    let both_asked = Arc::new(Barrier::new(2));
    let barrier = Arc::clone(&both_asked);
    let stand_in = StandIn::start(Arc::new(move |request: support::Recorded, _index: usize| -> support::BoxFuture<Reply> {
        let barrier = Arc::clone(&barrier);
        Box::pin(async move {
            let first_call = request.last_role() == "user" && !request.mentions("tool_result");
            if first_call
                && tokio::time::timeout(WAIT, barrier.wait()).await.is_err()
            {
                return Reply::Json(
                    500,
                    r#"{"type":"error","error":{"type":"api_error","message":"the other user's call never arrived"}}"#.into(),
                );
            }
            if request.mentions("slow-meter") && !request.mentions("tool_result") {
                ok(tool_step(1)
                    .replace("floor-3-0001", "slow-meter")
                    .replace("\"count_readings\"", "\"wait_for_release\""))
            } else {
                ok(fixture("answer_end_turn.json"))
            }
        })
    }))
    .await;
    // User A's tool waits until user B has the answer; it would never finish if B's question
    // were queued behind A's.
    let b_answered = Arc::new(Notify::new());
    let agent = Arc::new(agent(
        &stand_in,
        vec![
            Arc::new(CountReadings),
            Arc::new(WaitForRelease(Arc::clone(&b_answered))),
        ],
        settings(),
    ));
    let user_a = principal();
    let user_b = principal();

    let a = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move {
            let mut conversation = agent.start_conversation(user_a, "");
            agent
                .ask(
                    &mut conversation,
                    "Is slow-meter healthy?",
                    Arc::new(NoDelegation),
                )
                .await
        })
    };
    let b = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move {
            let mut conversation = agent.start_conversation(user_b, "");
            agent
                .ask(
                    &mut conversation,
                    "Did floor 3 report?",
                    Arc::new(NoDelegation),
                )
                .await
        })
    };

    let b_answer = tokio::time::timeout(WAIT, b)
        .await
        .expect("user B was answered while user A's question was still running")
        .expect("the task")
        .expect("an answer for user B");
    assert!(
        !a.is_finished(),
        "user A waits for its tool, which waits for user B"
    );
    b_answered.notify_one();
    let a_answer = tokio::time::timeout(WAIT, a)
        .await
        .expect("user A was answered after its tool was released")
        .expect("the task")
        .expect("an answer for user A");
    assert_eq!(a_answer.steps, 2);
    assert_eq!(b_answer.steps, 1);
    let tenants: Vec<Option<String>> = stand_in
        .requests()
        .iter()
        .map(|request| request.tenant.clone())
        .collect();
    assert!(tenants.contains(&Some(user_a.tenant_id.to_string())));
    assert!(tenants.contains(&Some(user_b.tenant_id.to_string())));
}

/// User A's tool: finishes only when released.
struct WaitForRelease(Arc<Notify>);

impl Tool for WaitForRelease {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wait_for_release".into(),
            description: "Waits until the test releases it.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        self: Arc<Self>,
        _context: ToolContext,
        _input: Value,
    ) -> assistant::model::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            if tokio::time::timeout(WAIT, self.0.notified()).await.is_err() {
                return Err(ToolError::Input("the test never released the tool".into()));
            }
            Ok(ToolOutput::text("released"))
        })
    }
}

#[tokio::test]
async fn a_model_call_that_times_out_ends_with_a_generic_error() {
    let stand_in = StandIn::start(respond(|_request, _index| Reply::Never)).await;
    let agent = agent(
        &stand_in,
        tools(),
        AgentSettings {
            call_timeout: Duration::from_secs(1),
            ..settings()
        },
    );
    let mut conversation = agent.start_conversation(principal(), "");

    let error = tokio::time::timeout(
        WAIT,
        agent.ask(&mut conversation, "Anything?", Arc::new(NoDelegation)),
    )
    .await
    .expect("the agent's own time limit ended the call")
    .expect_err("no answer");

    assert_eq!(error, AgentError::ModelTimeout);
    assert_eq!(
        error.to_string(),
        "The assistant took too long to answer. Please try again."
    );
}

#[tokio::test]
async fn gateway_errors_reach_the_user_without_their_text() {
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
        let (capture, _guard) = Capture::install();

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
async fn an_unreachable_gateway_is_unavailable() {
    // A port nothing listens on: bind one, then close it.
    let address = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("address")
    };
    let gateway = assistant::model::anthropic::AnthropicGateway::new(
        &format!("http://{address}"),
        "main",
        1 << 20,
    )
    .expect("a valid URL");
    let registry = assistant::tools::ToolRegistry::new(tools(), support::limits()).expect("tools");
    let agent = assistant::Agent::new(Arc::new(gateway), registry, settings());
    let mut conversation = agent.start_conversation(principal(), "");

    let error = agent
        .ask(&mut conversation, "Anything?", Arc::new(NoDelegation))
        .await
        .expect_err("no answer");
    assert_eq!(error, AgentError::ModelUnavailable);
}

#[tokio::test]
async fn a_refusal_is_not_kept_and_the_conversation_goes_on() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let stand_in = StandIn::start(respond(move |_request, _index| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            ok(fixture("refusal.json"))
        } else {
            ok(fixture("answer_end_turn.json"))
        }
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");

    let error = agent
        .ask(&mut conversation, "First?", Arc::new(NoDelegation))
        .await
        .expect_err("a refusal");
    assert_eq!(error, AgentError::Refused);
    assert!(!error.to_string().contains("cyber"));
    agent
        .ask(&mut conversation, "Second?", Arc::new(NoDelegation))
        .await
        .expect("the next question is answered");
    // The refused turn left nothing behind: both questions, then the answer.
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds,
        [EntryKind::Question, EntryKind::Question, EntryKind::Answer]
    );
}

#[tokio::test]
async fn every_step_is_logged_with_its_duration() {
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
    let (capture, _guard) = Capture::install();
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
