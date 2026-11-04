//! The answer tool (Laysi/trustiot-v2#147, the user's decision of 2026-11-05) against a stand-in
//! AI gateway that streams the tool's input in pieces: finished paragraphs go out before the
//! question ends and nothing outside the tool does, a text answer goes out only at the end, the
//! rules within one turn (the tool with other tools, twice, blank, cut off, malformed), the
//! answer guard inside the tool, and the history the next question sends.

mod support;

use std::sync::Arc;

use assistant::answer::ANSWER_TOOL;
use assistant::history::{Conversation, EntryKind, Preamble};
use assistant::prompt::{self, ANSWER_EMPTY_RESULT, ANSWER_NOT_ALONE_RESULT};
use assistant::tools::{NoDelegation, Tool};
use assistant::{AgentError, Answer};
use serde_json::{Value, json};
use support::{
    CountReadings, Recorded, Reply, StandIn, WAIT, agent, answer_step, call_block, event,
    events_reply, fixture, json_pieces, ok, principal, respond, settings, step, text_block,
    thinking_block, tool_results, tool_step, turn_events,
};
use tokio::sync::{mpsc, watch};

fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CountReadings)]
}

/// The messages of `request`.
fn messages(request: &Recorded) -> Vec<Value> {
    request.json()["messages"]
        .as_array()
        .expect("messages")
        .clone()
}

/// Waits until `released` holds `expected`.
async fn released_becomes(released: &mut watch::Receiver<String>, expected: &str) {
    tokio::time::timeout(WAIT, released.wait_for(|text| text == expected))
        .await
        .unwrap_or_else(|_| panic!("the released text became {expected:?} within {WAIT:?}"))
        .expect("the sender is alive");
}

const FIRST: &str = "Floor 3 used 412 kWh last week, 8 % more than the week before.";
const SECOND: &str = "The rise comes from the chillers on weekday afternoons.";

/// The first acceptance criterion of the decision: the answer written with the answer tool
/// goes out paragraph by paragraph before the question ends, and nothing outside the tool does,
/// not even the text the model wrote before calling it.
#[tokio::test]
async fn finished_paragraphs_of_the_answer_go_out_before_the_question_ends() {
    let (sender, reply) = events_reply();
    let reply = std::sync::Mutex::new(Some(reply));
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(tool_step(0)),
        _ => reply.lock().expect("the lock").take().expect("one answer"),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, mut watching) = watch::channel(String::new());
    let answer_text = format!("{FIRST}\n\n{SECOND}");
    let pieces = json_pieces(&answer_text, 7);
    // Where the first paragraph and its blank line are written: what comes after it holds
    // "The rise".
    let cut = pieces
        .iter()
        .scan(String::new(), |written, piece| {
            written.push_str(piece);
            Some(written.contains("The rise"))
        })
        .position(|done| done)
        .expect("the second paragraph starts");
    let narration = "Let me check the chillers before I answer: SELECT * FROM readings";

    let driver = async {
        let mut events = turn_events(
            &[
                thinking_block(),
                text_block(narration),
                call_block(
                    "toolu_answer",
                    ANSWER_TOOL,
                    &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
            ],
            "tool_use",
        );
        // Everything up to the second paragraph's first words: message_start, the thinking
        // (3 events with its start and stop), the narration (3), the tool call's start, its
        // pieces up to `cut`.
        let held = events.split_off(1 + 4 + 3 + 1 + cut + 1);
        for event in events {
            sender.send(event).await.expect("the gateway reads");
        }
        released_becomes(&mut watching, FIRST).await;
        assert!(
            !watching.borrow().contains("SELECT") && !watching.borrow().contains("Let me check"),
            "nothing outside the answer tool goes out"
        );
        for event in held {
            sender.send(event).await.expect("the gateway reads");
        }
        drop(sender);
    };
    let (answer, ()) = tokio::join!(
        agent.ask_streaming(
            &mut conversation,
            "How much did floor 3 use?",
            Arc::new(NoDelegation),
            &released,
        ),
        driver
    );
    let answer = answer.expect("an answer");
    assert_eq!(answer.text, answer_text);
    assert!(!answer.truncated);
    assert!(answer.removed.is_empty());
    // The last paragraph went out only with the answer: the release never had it.
    assert_eq!(*released.borrow(), FIRST);
    assert!(answer.text.starts_with(released.borrow().as_str()));

    // The history: the question, the tool step and its results, then the answer as a text
    // message; the turn that called the answer tool (its thinking, narration and call) is not
    // kept.
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds,
        [
            EntryKind::Question,
            EntryKind::Step,
            EntryKind::ToolResults,
            EntryKind::Answer
        ]
    );
    let (_, last) = conversation.entries().last().expect("the answer");
    let last: Value = serde_json::from_str(last.json()).expect("JSON");
    assert_eq!(
        last,
        json!({"role": "assistant", "content": [{"type": "text", "text": answer_text}]})
    );
    let history = conversation
        .messages()
        .iter()
        .map(|message| message.json().to_owned())
        .collect::<String>();
    assert!(!history.contains("Let me check the chillers"));
    assert!(!history.contains("toolu_answer"));
}

/// The second acceptance criterion: an answer written as text (a turn without tool calls) goes
/// out only once the question answered.
#[tokio::test]
async fn a_text_answer_is_released_only_at_the_end() {
    let stand_in = StandIn::start(respond(|_, index| match index {
        0 => ok(tool_step(0)),
        _ => ok(fixture("answer_end_turn.json")),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, watching) = watch::channel(String::new());
    let answer = agent
        .ask_streaming(
            &mut conversation,
            "Did every meter report?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect("an answer");
    assert_eq!(
        answer.text,
        "Yes. All twelve meters on floor 3 reported in the last hour."
    );
    assert!(!watching.has_changed().expect("the sender is alive"));
    assert_eq!(*released.borrow(), "");
}

/// The answer guard runs inside the tool: SQL and HTML never go out, neither in the released
/// paragraphs nor in the stored answer, and the answer says what was removed.
#[tokio::test]
async fn the_answer_guard_cleans_the_answer_tool_text() {
    let text = format!(
        "{FIRST}\n\n```sql\nSELECT sum(kwh) FROM readings\n```\n\nIt ran as <b>bold</b> \
         text.\n\n{SECOND}"
    );
    let reply = answer_step(1, &text);
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(tool_step(0)),
        _ => ok(reply.clone()),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, _watching) = watch::channel(String::new());
    let answer = agent
        .ask_streaming(
            &mut conversation,
            "How much did floor 3 use?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect("an answer");
    for removed in ["SELECT", "readings", "<b>", "```"] {
        assert!(
            !answer.text.contains(removed),
            "{removed} in {:?}",
            answer.text
        );
        assert!(!released.borrow().contains(removed), "{removed} released");
    }
    assert!(answer.text.starts_with(FIRST));
    assert!(answer.text.ends_with(SECOND));
    assert!(answer.text.starts_with(released.borrow().as_str()));
    assert!(!released.borrow().is_empty(), "paragraphs went out");
    assert!(!answer.removed.is_empty());
    let (kind, _) = conversation.entries().last().expect("the answer");
    assert_eq!(kind, EntryKind::Answer);
    assert_eq!(
        conversation.removed(conversation.len() - 1),
        answer.removed,
        "the answer's entry carries what was removed"
    );
}

/// The next question sends a valid history whose prefix is the last request's messages: the
/// answer follows the last tool results as a text message, every tool call has its result, and
/// the roles alternate.
#[tokio::test]
async fn the_next_question_continues_the_history_byte_for_byte() {
    let reply = answer_step(1, &format!("{FIRST}\n\n{SECOND}"));
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(tool_step(0)),
        1 => ok(reply.clone()),
        _ => ok(fixture("answer_end_turn.json")),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let first = ask(&agent, &mut conversation, "How much did floor 3 use?")
        .await
        .expect("an answer");
    ask(&agent, &mut conversation, "And floor 4?")
        .await
        .expect("an answer");
    let requests = stand_in.requests();
    assert_eq!(requests.len(), 3);
    let (before, after) = (requests[1].raw_messages(), requests[2].raw_messages());
    assert_eq!(
        after.get(..before.len()),
        Some(&before[..]),
        "the same prefix"
    );
    let added: Vec<Value> = after[before.len()..]
        .iter()
        .map(|message| serde_json::from_str(message).expect("JSON"))
        .collect();
    assert_eq!(added.len(), 2);
    assert_eq!(
        added[0],
        json!({"role": "assistant", "content": [{"type": "text", "text": first.text}]})
    );
    assert_eq!(added[1]["role"], "user");
    assert!(added[1].to_string().contains("And floor 4?"));
    // Every tool_use has its result in the next message, and the roles alternate.
    let messages = messages(&requests[2]);
    for pair in messages.windows(2) {
        assert_ne!(pair[0]["role"], pair[1]["role"], "{pair:?}");
        let calls: Vec<&Value> = pair[0]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        for call in calls {
            assert!(
                pair[1]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|block| block["type"] == "tool_result"
                        && block["tool_use_id"] == call["id"]),
                "{call} has its result"
            );
        }
    }
    // The answer tool's call is not in the history.
    assert!(!requests[2].body.contains("toolu_0001_0"));
}

async fn ask(
    agent: &assistant::Agent,
    conversation: &mut Conversation,
    question: &str,
) -> Result<Answer, AgentError> {
    agent
        .ask(conversation, question, Arc::new(NoDelegation))
        .await
}

/// An answer called after another tool in the same response is not delivered: the other call
/// runs, the answer call gets an error result, and the loop goes on to the answer that comes
/// alone. Nothing of the first answer goes out.
#[tokio::test]
async fn an_answer_after_another_call_is_refused_and_the_loop_goes_on() {
    let both = step(
        0,
        &[
            (
                "count_readings",
                json!({"meter": "floor-3-0001", "hours": 24}),
            ),
            (ANSWER_TOOL, json!({"text": "Too early.\n\nNot this one."})),
        ],
    );
    let reply = answer_step(1, &format!("{FIRST}\n\n{SECOND}"));
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(both.clone()),
        _ => ok(reply.clone()),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, mut watching) = watch::channel(String::new());
    let answer = agent
        .ask_streaming(
            &mut conversation,
            "How much did floor 3 use?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect("an answer");
    assert_eq!(answer.text, format!("{FIRST}\n\n{SECOND}"));
    let requests = stand_in.requests();
    assert_eq!(requests.len(), 2);
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 2);
    assert!(!results[0].is_error);
    assert_eq!(results[0].text, "floor-3-0001: 60 readings");
    assert!(results[1].is_error);
    assert_eq!(results[1].text, ANSWER_NOT_ALONE_RESULT);
    // Every release was of the delivered answer.
    assert!(!watching.borrow_and_update().contains("Too early"));
    assert!(answer.text.starts_with(released.borrow().as_str()));
}

/// Calls after the answer in the same response never run, and are not kept.
#[tokio::test]
async fn calls_after_the_answer_never_run() {
    let reply = step(
        1,
        &[
            (ANSWER_TOOL, json!({"text": FIRST})),
            (
                "count_readings",
                json!({"meter": "floor-9-0009", "hours": 1}),
            ),
        ],
    );
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(tool_step(0)),
        _ => ok(reply.clone()),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let answer = ask(&agent, &mut conversation, "How much?")
        .await
        .expect("an answer");
    assert_eq!(answer.text, FIRST);
    assert_eq!(stand_in.requests().len(), 2);
    let history: String = conversation
        .messages()
        .iter()
        .map(|message| message.json().to_owned())
        .collect();
    assert!(!history.contains("floor-9-0009"));
}

/// Blank text is not an answer: the model is told and answers again.
#[tokio::test]
async fn a_blank_answer_is_refused_and_the_loop_goes_on() {
    let blank = answer_step(0, " \n\n ");
    let reply = answer_step(1, FIRST);
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(blank.clone()),
        _ => ok(reply.clone()),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let answer = ask(&agent, &mut conversation, "How much?")
        .await
        .expect("an answer");
    assert_eq!(answer.text, FIRST);
    let requests = stand_in.requests();
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert_eq!(results[0].text, ANSWER_EMPTY_RESULT);
}

/// An answer cut off at the output limit is what was written of it, marked truncated; what went
/// out is its start.
#[tokio::test]
async fn an_answer_cut_off_at_the_output_limit_is_kept_as_written() {
    let pieces = json_pieces(&format!("{FIRST}\n\n{SECOND}"), 6);
    // Cut inside the second paragraph: the JSON never closes.
    let written: Vec<&str> = pieces
        .iter()
        .map(String::as_str)
        .take(pieces.len() - 3)
        .collect();
    let events = turn_events(
        &[
            thinking_block(),
            call_block("toolu_cut", ANSWER_TOOL, &written),
        ],
        "max_tokens",
    );
    let stand_in = StandIn::start(respond(move |_, _| {
        let (sender, reply) = events_reply();
        let events = events.clone();
        tokio::spawn(async move {
            for event in events {
                let _ = sender.send(event).await;
            }
        });
        reply
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, _watching) = watch::channel(String::new());
    let answer = agent
        .ask_streaming(
            &mut conversation,
            "How much?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect("an answer");
    assert!(answer.truncated);
    assert!(answer.text.starts_with(&format!("{FIRST}\n\nThe rise")));
    assert!(SECOND.starts_with(answer.text.trim_start_matches(&format!("{FIRST}\n\n"))));
    assert_eq!(*released.borrow(), FIRST);
}

/// Input that is not the tool's (JSON that does not parse without the output limit, another
/// key) fails the question: no answer, so the client drops what went out.
#[tokio::test]
async fn an_answer_whose_input_is_not_the_tools_fails_the_question() {
    let cases: Vec<Vec<&str>> = vec![
        // Not JSON, though the response ended normally.
        vec!["{\"text\": \"", FIRST, "\\n\\nThe rise\" ,,}"],
        // Another key after the text.
        vec!["{\"text\": \"", FIRST, "\\n\\nThe rise\", \"extra\": 1}"],
    ];
    for pieces in cases {
        let events = turn_events(
            &[
                thinking_block(),
                call_block("toolu_bad", ANSWER_TOOL, &pieces),
            ],
            "tool_use",
        );
        let stand_in = StandIn::start(respond(move |_, _| {
            let (sender, reply) = events_reply();
            let events = events.clone();
            tokio::spawn(async move {
                for event in events {
                    let _ = sender.send(event).await;
                }
            });
            reply
        }))
        .await;
        let agent = agent(&stand_in, tools(), settings());
        let mut conversation = agent.start_conversation(principal(), "");
        let (released, _watching) = watch::channel(String::new());
        let error = agent
            .ask_streaming(
                &mut conversation,
                "How much?",
                Arc::new(NoDelegation),
                &released,
            )
            .await
            .expect_err("no answer");
        assert_eq!(error, AgentError::ModelFailed, "{pieces:?}");
        assert!(
            conversation
                .entries()
                .all(|(kind, _)| kind != EntryKind::Answer),
            "no answer is kept"
        );
    }
}

/// A conversation that started before the answer tool (its preamble has no such tool) never
/// takes a call of that name as its answer: it is a tool the registry does not know, and the
/// text answer comes as before.
#[tokio::test]
async fn an_older_conversation_has_no_answer_tool() {
    let reply = answer_step(0, FIRST);
    let stand_in = StandIn::start(respond(move |_, index| match index {
        0 => ok(reply.clone()),
        _ => ok(fixture("answer_end_turn.json")),
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let started = agent.start_conversation(principal(), "");
    let preamble = started.preamble();
    let older = Preamble::new(
        preamble.system_prompt().clone(),
        preamble.tenant_context().to_owned(),
        preamble
            .tools()
            .iter()
            .filter(|tool| tool.name != ANSWER_TOOL)
            .cloned()
            .collect(),
        preamble.effort(),
        preamble.cache_ttl(),
    );
    let mut conversation = Conversation::new(started.principal(), Arc::new(older));
    let (released, _watching) = watch::channel(String::new());
    let answer = agent
        .ask_streaming(
            &mut conversation,
            "How much?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect("an answer");
    assert_eq!(
        answer.text,
        "Yes. All twelve meters on floor 3 reported in the last hour."
    );
    assert_eq!(*released.borrow(), "", "nothing went out early");
    let requests = stand_in.requests();
    assert!(
        requests[0].json()["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .all(|tool| tool["name"] != ANSWER_TOOL)
    );
    let results = tool_results(&requests[1]);
    assert!(results[0].is_error);
    assert!(
        results[0]
            .text
            .starts_with("There is no tool named \"answer\"")
    );
}

/// A provider error in the middle of the answer: no answer, and nothing of the round is kept.
#[tokio::test]
async fn an_error_in_the_middle_of_the_answer_fails_the_question() {
    let pieces = json_pieces(&format!("{FIRST}\n\n{SECOND}"), 6);
    let mut events = turn_events(
        &[
            thinking_block(),
            call_block(
                "toolu_error",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
        ],
        "tool_use",
    );
    // The stream stops after the first paragraph with an error event.
    events.truncate(1 + 4 + 1 + pieces.len() - 4);
    events.push(event(
        "error",
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ));
    let (sender, receiver) = mpsc::channel(64);
    for event in events {
        sender.try_send(event).expect("room for every event");
    }
    drop(sender);
    let reply = std::sync::Mutex::new(Some(Reply::Events(receiver)));
    let stand_in = StandIn::start(respond(move |_, _| {
        reply
            .lock()
            .expect("the lock")
            .take()
            .unwrap_or(Reply::Never)
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, _watching) = watch::channel(String::new());
    let error = agent
        .ask_streaming(
            &mut conversation,
            "How much?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect_err("no answer");
    assert_eq!(error, AgentError::ModelUnavailable);
    assert_eq!(*released.borrow(), FIRST, "the client drops it at the end");
    assert!(
        conversation
            .entries()
            .all(|(kind, _)| kind != EntryKind::Answer)
    );
}

const THIRD: &str = "Nothing else changed.";

/// The answer tool's input with `text` given twice, the second one empty: the API does not check
/// an input it streams (`eager_input_streaming`), and a JSON parser keeps the last value.
fn twice_keyed(text: &str) -> String {
    let once = json!({ "text": text }).to_string();
    format!(
        "{},\"text\":\"\"}}",
        once.strip_suffix('}').expect("an object")
    )
}

/// The events of a turn that calls the answer tool with `input`, in pieces of 6 bytes.
fn answer_events(input: &str) -> mpsc::Receiver<String> {
    let characters: Vec<char> = input.chars().collect();
    let pieces: Vec<String> = characters
        .chunks(6)
        .map(|chunk| chunk.iter().collect())
        .collect();
    let events = turn_events(
        &[
            thinking_block(),
            call_block(
                "toolu_twice",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
        ],
        "tool_use",
    );
    let (sender, receiver) = mpsc::channel(events.len());
    for event in events {
        sender.try_send(event).expect("room for every event");
    }
    receiver
}

/// The review's scenario (Laysi/trustiot-v2#147): the answer call gives `text` twice. What the
/// decoder read of the first (two paragraphs) went out; the parser keeps the second, empty one.
/// That disagreement is a malformed input: the question fails `model_failed` at once, the text
/// that went out stays what it was (the client drops it at the end), nothing is stored, and the
/// model is not called again, whether its next turn would have answered with the tool or as
/// text, neither of which starts with what went out.
#[tokio::test]
async fn an_answer_whose_streamed_text_is_not_its_parsed_text_fails_the_question() {
    let written = format!("{FIRST}\n\n{SECOND}\n\n{THIRD}");
    let text_answer = {
        let mut message: Value =
            serde_json::from_str(&fixture("answer_end_turn.json")).expect("JSON");
        message["content"] = json!([{"type": "text", "text": "Q1.\n\nQ2."}]);
        message.to_string()
    };
    for (name, next) in [
        ("a second tool answer", answer_step(1, "Q1.\n\nQ2.")),
        ("a text answer", text_answer),
    ] {
        let first =
            std::sync::Mutex::new(Some(Reply::Events(answer_events(&twice_keyed(&written)))));
        let stand_in = StandIn::start(respond(move |_, index| match index {
            0 => first
                .lock()
                .expect("the lock")
                .take()
                .expect("one first turn"),
            _ => ok(next.clone()),
        }))
        .await;
        let agent = agent(&stand_in, tools(), settings());
        let mut conversation = agent.start_conversation(principal(), "");
        let (released, _watching) = watch::channel(String::new());
        let error = agent
            .ask_streaming(
                &mut conversation,
                "How much did floor 3 use?",
                Arc::new(NoDelegation),
                &released,
            )
            .await
            .expect_err("no answer");
        assert_eq!(error, AgentError::ModelFailed, "{name}");
        assert_eq!(
            *released.borrow(),
            format!("{FIRST}\n\n{SECOND}"),
            "{name}: the two paragraphs went out before the second key"
        );
        assert_eq!(stand_in.requests().len(), 1, "{name}: no second model call");
        assert!(
            conversation
                .entries()
                .all(|(kind, _)| kind != EntryKind::Answer),
            "{name}"
        );
    }
}

/// A turn that ends (`end_turn`) with text and an answer call that released paragraphs is read
/// as a text answer, which is the text before the call, not what went out: the question fails
/// `model_failed` rather than store an answer that does not start with what went out.
#[tokio::test]
async fn a_text_answer_that_does_not_continue_what_went_out_fails_the_question() {
    let pieces = json_pieces(&format!("{FIRST}\n\n{SECOND}\n\n{THIRD}"), 6);
    let events = turn_events(
        &[
            text_block("Here is what I found."),
            call_block(
                "toolu_end_turn",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
        ],
        "end_turn",
    );
    let (sender, receiver) = mpsc::channel(events.len());
    for event in events {
        sender.try_send(event).expect("room for every event");
    }
    let reply = std::sync::Mutex::new(Some(Reply::Events(receiver)));
    let stand_in = StandIn::start(respond(move |_, _| {
        reply
            .lock()
            .expect("the lock")
            .take()
            .unwrap_or(Reply::Never)
    }))
    .await;
    let agent = agent(&stand_in, tools(), settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (released, _watching) = watch::channel(String::new());
    let error = agent
        .ask_streaming(
            &mut conversation,
            "How much?",
            Arc::new(NoDelegation),
            &released,
        )
        .await
        .expect_err("no answer");
    assert_eq!(error, AgentError::ModelFailed);
    assert_eq!(*released.borrow(), format!("{FIRST}\n\n{SECOND}"));
    assert!(
        conversation
            .entries()
            .all(|(kind, _)| kind != EntryKind::Answer)
    );
}

/// The prompt asks for the answer tool.
#[test]
fn the_prompt_names_the_answer_tool() {
    assert!(prompt::system_prompt().text.contains("the answer tool"));
}
