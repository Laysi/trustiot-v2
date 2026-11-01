//! The tool registry with fake tools: the tool set's rules, concurrent calls whose results
//! come back in call order, the limits on results with errors that tell the model how to ask
//! for less, failures that stay inside one call, and images reaching the model.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use assistant::history::Principal;
use assistant::model::{BoxFuture, ToolCall};
use assistant::tools::{
    DelegationError, ImageType, MAX_TOOLS, NoDelegation, RegistryError, Tool, ToolContent,
    ToolContext, ToolError, ToolLimits, ToolOutput, ToolRegistry, ToolSpec,
};
use contract::ServiceName;
use serde_json::{Value, json};
use support::{CountReadings, StandIn, WAIT, fixture, limits, ok, principal, respond, settings};
use tokio::sync::Notify;

/// A 1×1 PNG.
const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// A tool call's future, boxed (the bound fixes the output type of the async blocks).
fn boxed(
    future: impl Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
    Box::pin(future)
}

/// A tool whose behaviour is a closure over its input.
struct Fake {
    name: String,
    behave: Arc<dyn Fn(Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync>,
}

impl Fake {
    fn tool(
        name: &str,
        behave: impl Fn(Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<dyn Tool> {
        Arc::new(Self {
            name: name.into(),
            behave: Arc::new(behave),
        })
    }
}

impl Tool for Fake {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!("The fake tool {}.", self.name),
            input_schema: json!({"type": "object"}),
        }
    }

    fn narrowing_hint(&self) -> &str {
        "Ask for one site at a time."
    }

    fn call(
        self: Arc<Self>,
        _context: ToolContext,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
        (self.behave)(input)
    }
}

fn context() -> ToolContext {
    ToolContext {
        principal: principal(),
        conversation_id: uuid::Uuid::now_v7(),
        run_id: uuid::Uuid::now_v7(),
        delegation: Arc::new(NoDelegation),
    }
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn text_of(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect()
}

#[test]
fn the_tool_set_follows_the_rules() {
    let many: Vec<Arc<dyn Tool>> = (0..=MAX_TOOLS)
        .map(|index| {
            Fake::tool(&format!("tool_{index}"), |_| {
                boxed(async { Ok(ToolOutput::text("")) })
            })
        })
        .collect();
    assert_eq!(
        ToolRegistry::new(many, limits()).err(),
        Some(RegistryError::TooMany(MAX_TOOLS + 1))
    );
    let twice = vec![
        Arc::new(CountReadings) as Arc<dyn Tool>,
        Arc::new(CountReadings),
    ];
    assert_eq!(
        ToolRegistry::new(twice, limits()).err(),
        Some(RegistryError::Duplicate("count_readings".into()))
    );
    for bad in ["", "has space", "ünïcode", &"x".repeat(65)] {
        let tool = Fake::tool(bad, |_| boxed(async { Ok(ToolOutput::text("")) }));
        assert_eq!(
            ToolRegistry::new(vec![tool], limits()).err(),
            Some(RegistryError::InvalidName(bad.into())),
            "{bad:?}"
        );
    }
    struct NoSchema;
    impl Tool for NoSchema {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "no_schema".into(),
                description: "A tool without an object schema.".into(),
                input_schema: json!({"type": "string"}),
            }
        }
        fn call(
            self: Arc<Self>,
            _context: ToolContext,
            _input: Value,
        ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
            boxed(async { Ok(ToolOutput::text("")) })
        }
    }
    assert_eq!(
        ToolRegistry::new(vec![Arc::new(NoSchema)], limits()).err(),
        Some(RegistryError::InvalidSchema("no_schema".into()))
    );
    let ten: Vec<Arc<dyn Tool>> = (0..MAX_TOOLS)
        .map(|index| {
            Fake::tool(&format!("tool_{index}"), |_| {
                boxed(async { Ok(ToolOutput::text("")) })
            })
        })
        .collect();
    let registry = ToolRegistry::new(ten, limits()).expect("ten tools are allowed");
    let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "the definitions come in name order");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_run_concurrently_and_results_keep_the_call_order() {
    // `first` finishes only after `second` has started: run one after the other, `first` would
    // never finish.
    let second_started = Arc::new(Notify::new());
    let signal = Arc::clone(&second_started);
    let first = Fake::tool("first", move |_| {
        let signal = Arc::clone(&signal);
        boxed(async move {
            if tokio::time::timeout(WAIT, signal.notified()).await.is_err() {
                return Err(ToolError::Input("second never started".into()));
            }
            Ok(ToolOutput::text("first done"))
        })
    });
    let notify = Arc::clone(&second_started);
    let second = Fake::tool("second", move |_| {
        let notify = Arc::clone(&notify);
        boxed(async move {
            notify.notify_one();
            Ok(ToolOutput::text("second done"))
        })
    });
    let registry = ToolRegistry::new(vec![first, second], limits()).expect("tools");

    let results = tokio::time::timeout(
        WAIT,
        registry.run(
            &context(),
            &[
                call("a", "first", json!({})),
                call("b", "second", json!({})),
            ],
        ),
    )
    .await
    .expect("both calls finished")
    .expect("no access revoked");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].call_id, "a");
    assert_eq!(text_of(&results[0].output), "first done");
    assert_eq!(results[1].call_id, "b");
    assert_eq!(text_of(&results[1].output), "second done");
}

#[tokio::test]
async fn failures_stay_in_their_call_and_tell_the_model_what_to_do() {
    let secret_detail = format!("internal detail {}", uuid::Uuid::now_v7());
    let detail = secret_detail.clone();
    let tools = vec![
        Fake::tool("ok", |_| boxed(async { Ok(ToolOutput::text("fine")) })),
        Fake::tool("bad_input", |_| {
            boxed(async {
                Err(ToolError::Input(
                    "`site` is required: the site's name, such as plant-a.".into(),
                ))
            })
        }),
        Fake::tool("fails", |_| boxed(async { Err(ToolError::Failed) })),
        Fake::tool("panics", move |_| {
            let detail = detail.clone();
            boxed(async move { panic!("{detail}") })
        }),
        Fake::tool("hangs", |_| boxed(std::future::pending())),
    ];
    let registry = ToolRegistry::new(
        tools,
        ToolLimits {
            call_timeout: Duration::from_secs(1),
            ..limits()
        },
    )
    .expect("tools");

    let results = tokio::time::timeout(
        WAIT,
        registry.run(
            &context(),
            &[
                call("1", "ok", json!({})),
                call("2", "bad_input", json!({})),
                call("3", "fails", json!({})),
                call("4", "panics", json!({})),
                call("5", "hangs", json!({})),
                call("6", "missing", json!({})),
                call("7", "ok", json!("not an object")),
            ],
        ),
    )
    .await
    .expect("every call ended")
    .expect("no access revoked");

    let outputs: Vec<(bool, String)> = results
        .iter()
        .map(|result| (result.output.is_error, text_of(&result.output)))
        .collect();
    assert_eq!(outputs[0], (false, "fine".into()));
    assert_eq!(
        outputs[1],
        (
            true,
            "`site` is required: the site's name, such as plant-a.".into()
        )
    );
    assert!(outputs[2].0 && outputs[2].1.contains("outside your control"));
    assert!(outputs[3].0 && outputs[3].1.contains("outside your control"));
    assert!(
        !outputs[3].1.contains(&secret_detail),
        "a panic message reached the model"
    );
    assert!(outputs[4].0 && outputs[4].1.contains("did not finish within 1 seconds"));
    assert!(outputs[5].0 && outputs[5].1.contains("no tool named \"missing\""));
    assert!(outputs[5].1.contains("bad_input, fails, hangs, ok, panics"));
    assert!(outputs[6].0 && outputs[6].1.contains("must be a JSON object"));
}

#[tokio::test]
async fn calls_past_the_per_step_limit_are_not_run() {
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&runs);
    let tools = vec![Fake::tool("count", move |_| {
        let counted = Arc::clone(&counted);
        boxed(async move {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("counted"))
        })
    })];
    let registry = ToolRegistry::new(
        tools,
        ToolLimits {
            max_calls_per_step: 2,
            ..limits()
        },
    )
    .expect("tools");

    let calls: Vec<ToolCall> = (1..=5)
        .map(|n| call(&n.to_string(), "count", json!({})))
        .collect();
    let results = tokio::time::timeout(WAIT, registry.run(&context(), &calls))
        .await
        .expect("every call ended")
        .expect("no access revoked");

    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "only the first two calls ran"
    );
    let ids: Vec<&str> = results
        .iter()
        .map(|result| result.call_id.as_str())
        .collect();
    assert_eq!(ids, ["1", "2", "3", "4", "5"]);
    for result in &results[..2] {
        assert_eq!(
            (result.output.is_error, text_of(&result.output)),
            (false, "counted".into())
        );
    }
    for result in &results[2..] {
        assert!(result.output.is_error, "call {} ran", result.call_id);
        assert!(
            text_of(&result.output).contains("at most 2 tool calls run in one step"),
            "call {}: {}",
            result.call_id,
            text_of(&result.output)
        );
    }
}

#[tokio::test]
async fn results_over_the_limits_become_a_request_to_narrow() {
    let tools = vec![
        Fake::tool("big", |_| {
            boxed(async { Ok(ToolOutput::text("x".repeat(101))) })
        }),
        Fake::tool("fits", |_| {
            boxed(async { Ok(ToolOutput::text("x".repeat(100))) })
        }),
        Fake::tool("many_images", |_| {
            boxed(async {
                Ok(ToolOutput::text("charts")
                    .with_image(ImageType::Png, PIXEL_PNG)
                    .with_image(ImageType::Png, PIXEL_PNG)
                    .with_image(ImageType::Png, PIXEL_PNG))
            })
        }),
        Fake::tool("not_base64", |_| {
            boxed(async { Ok(ToolOutput::text("chart").with_image(ImageType::Png, "%%%")) })
        }),
        Fake::tool("one_image", |_| {
            boxed(async { Ok(ToolOutput::text("chart").with_image(ImageType::Png, PIXEL_PNG)) })
        }),
    ];
    let registry = ToolRegistry::new(
        tools,
        ToolLimits {
            max_text_bytes: 100,
            max_images: 2,
            ..limits()
        },
    )
    .expect("tools");

    let results = registry
        .run(
            &context(),
            &[
                call("1", "big", json!({})),
                call("2", "fits", json!({})),
                call("3", "many_images", json!({})),
                call("4", "not_base64", json!({})),
                call("5", "one_image", json!({})),
            ],
        )
        .await
        .expect("no access revoked");

    let big = &results[0].output;
    assert!(big.is_error);
    assert!(text_of(big).contains("101 bytes of text, over the limit of 100 bytes"));
    assert!(text_of(big).ends_with("Ask for one site at a time."));
    assert!(!results[1].output.is_error);
    assert!(results[2].output.is_error);
    assert!(text_of(&results[2].output).contains("3 images, over the limit of 2"));
    assert!(results[3].output.is_error);
    assert!(!results[4].output.is_error);
    assert_eq!(results[4].output.content.len(), 2);
}

#[tokio::test]
async fn tools_get_the_principal_and_the_delegation_not_the_input() {
    struct Recording(std::sync::Mutex<Option<(Principal, Result<(), DelegationError>)>>);
    struct WhoAmI(Arc<Recording>);
    impl Tool for WhoAmI {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "who_am_i".into(),
                description: "Reports its caller.".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn call(
            self: Arc<Self>,
            context: ToolContext,
            _input: Value,
        ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
            boxed(async move {
                let token = context
                    .delegation
                    .token_for(ServiceName::TelemetryQuery)
                    .await;
                *self.0.0.lock().expect("the lock") = Some((context.principal, token.map(|_| ())));
                Ok(ToolOutput::text("ok"))
            })
        }
    }
    let recording = Arc::new(Recording(std::sync::Mutex::new(None)));
    let registry =
        ToolRegistry::new(vec![Arc::new(WhoAmI(Arc::clone(&recording)))], limits()).expect("tools");
    let context = context();

    registry
        .run(
            &context,
            &[call("1", "who_am_i", json!({"tenant_id": "someone-else"}))],
        )
        .await
        .expect("no access revoked");

    let seen = recording
        .0
        .lock()
        .expect("the lock")
        .take()
        .expect("the tool ran");
    assert_eq!(seen.0, context.principal);
    assert_eq!(seen.1, Err(DelegationError::NotConfigured));
}

#[tokio::test]
async fn tool_result_images_reach_the_model() {
    let stand_in = StandIn::start(respond(|_request, index| {
        if index == 0 {
            ok(support::tool_step(0).replace("\"count_readings\"", "\"chart\""))
        } else {
            ok(fixture("answer_end_turn.json"))
        }
    }))
    .await;
    let chart = Fake::tool("chart", |_| {
        boxed(async { Ok(ToolOutput::text("kWh per hour").with_image(ImageType::Png, PIXEL_PNG)) })
    });
    let agent = support::agent(&stand_in, vec![chart], settings());
    let mut conversation = agent.start_conversation(principal(), "");

    agent
        .ask(&mut conversation, "Chart floor 3.", Arc::new(NoDelegation))
        .await
        .expect("an answer");

    let requests = stand_in.requests();
    let body = requests[1].json();
    let results = &body["messages"][2];
    assert_eq!(results["role"], "user");
    let block = &results["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "toolu_01D7FLrfh4GYq7yT1ULF0000");
    assert_eq!(
        block["content"][0],
        json!({"type": "text", "text": "kWh per hour"})
    );
    assert_eq!(
        block["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PIXEL_PNG}})
    );
    assert!(block.get("is_error").is_none());
}

/// Dropping a step cancels its calls still running, as the question's deadline does: the agent
/// drops the question's work there (`Agent::ask`), the step with it. A call waiting to send
/// is dropped before it sends, so no call goes on after the question ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_step_cancels_its_calls_before_they_send() {
    /// Tells when the call's future is dropped.
    struct OnDrop(Arc<Notify>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let started = Arc::new(Notify::new());
    let go = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let sent = Arc::new(AtomicUsize::new(0));
    let tool = {
        let (started, go, dropped, sent) = (
            Arc::clone(&started),
            Arc::clone(&go),
            Arc::clone(&dropped),
            Arc::clone(&sent),
        );
        Fake::tool("sends_later", move |_| {
            let (started, go, dropped, sent) = (
                Arc::clone(&started),
                Arc::clone(&go),
                Arc::clone(&dropped),
                Arc::clone(&sent),
            );
            boxed(async move {
                let _dropped = OnDrop(dropped);
                started.notify_one();
                go.notified().await;
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput::text("sent"))
            })
        })
    };
    let registry = ToolRegistry::new(vec![tool], limits()).expect("one tool");
    let context = context();
    let calls = [call("1", "sends_later", json!({}))];
    {
        let mut step = std::pin::pin!(registry.run(&context, &calls));
        tokio::select! {
            _ = &mut step => panic!("the call waits until it may send"),
            () = started.notified() => {}
        }
        // The step is dropped here, as at the question's deadline.
    }
    tokio::time::timeout(WAIT, dropped.notified())
        .await
        .expect("the call was dropped with its step");
    // A call still running would send now.
    go.notify_waiters();
    tokio::task::yield_now().await;
    assert_eq!(
        sent.load(Ordering::SeqCst),
        0,
        "the cancelled call sent nothing"
    );
}
