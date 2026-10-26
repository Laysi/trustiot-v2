//! The answer guard ([`assistant::answer_guard`], Laysi/trustiot-v2#150, criterion 「回答文字不出現
//! SQL 或 HTML」):
//!
//! - end to end through [`assistant::Agent::ask`] against the stand-in AI gateway: an answer with
//!   a SQL code block, a SQL statement in the prose, HTML and inline code reaches the caller and
//!   the history without them, with what was removed reported and logged by kind only; the
//!   thinking block goes back to the model byte for byte, and the next question resends the
//!   cleaned answer as the history holds it;
//! - statements that change a schema, which repo-check keeps out of `services/*/src` (the unit
//!   tests in src/answer_guard.rs cover every other kind and the text that must stay).

mod support;

use std::sync::Arc;

use assistant::answer_guard::{Removed, guard};
use assistant::history::EntryKind;
use assistant::tools::{NoDelegation, Tool};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use support::{
    Capture, CountReadings, StandIn, agent, fixture, ok, principal, respond, settings, tool_step,
};

/// Text the guard must remove: none of it may reach the caller, the history or the logs.
const REMOVED_TEXTS: [&str; 9] = [
    "SELECT",
    "tenant_telemetry",
    "```",
    "<div",
    "<b>",
    "</b>",
    "<svg",
    "rect",
    "toFixed",
];

/// The answer step: the recorded answer with a thinking block and two text blocks, one of them
/// only HTML.
fn answer_with_code() -> (String, String) {
    let mut answer: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    let thinking = answer["content"][0].clone();
    answer["content"] = json!([
        thinking,
        {
            "type": "text",
            "text": "Floor 3 used 412 kWh yesterday, 8 % less than the day before.\n\n\
                     ```sql\nSELECT sum(kwh) FROM tenant_telemetry_1h WHERE ts >= today() - 1\n```\n\n\
                     The total comes from <b>hourly</b> data:\n\n\
                     SELECT device_id, max(v) FROM tenant_telemetry GROUP BY device_id;\n\n\
                     The dashboard rounds it with `value.toFixed(1)` and the peak was at 14:00."
        },
        {
            "type": "text",
            "text": "<div class=\"chart\"><svg><rect width=\"10\" height=\"4\"/></svg></div>"
        }
    ]);
    let thinking_json = serde_json::to_string(&thinking).expect("the thinking block serializes");
    (answer.to_string(), thinking_json)
}

/// The content blocks of a message as the JSON text sent.
fn blocks(message: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Message<'a> {
        #[serde(borrow)]
        content: Vec<&'a RawValue>,
    }
    let message: Message<'_> = serde_json::from_str(message).expect("a message with content");
    message
        .content
        .iter()
        .map(|block| block.get().to_owned())
        .collect()
}

#[tokio::test]
async fn an_answer_reaches_the_caller_and_the_history_without_code_html_or_sql() {
    let (answer_step, thinking) = answer_with_code();
    // A tool step, the answer with code, then the answer to the next question.
    let stand_in = StandIn::start(respond(move |_request, index| match index {
        0 => ok(tool_step(0)),
        1 => ok(answer_step.clone()),
        _ => ok(fixture("answer_end_turn.json")),
    }))
    .await;
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountReadings)];
    let agent = agent(&stand_in, tools, settings());
    let mut conversation = agent.start_conversation(principal(), "");
    let (capture, _guard) = Capture::install();

    let answer = agent
        .ask(
            &mut conversation,
            "How much did floor 3 use yesterday?",
            Arc::new(NoDelegation),
        )
        .await
        .expect("an answer");

    // The caller gets the prose, without what the guard removes (the block of only HTML is gone),
    // and which kinds it removed.
    assert_eq!(
        answer.text,
        "Floor 3 used 412 kWh yesterday, 8 % less than the day before.\n\n\
         The total comes from hourly data:\n\n\
         The dashboard rounds it with and the peak was at 14:00."
    );
    for removed in REMOVED_TEXTS {
        assert!(
            !answer.text.contains(removed),
            "{removed:?} in {:?}",
            answer.text
        );
    }
    assert_eq!(
        answer.removed.kinds().collect::<Vec<_>>(),
        [Removed::Code, Removed::Html, Removed::Sql]
    );
    assert_eq!(
        serde_json::to_value(answer.removed).expect("JSON"),
        json!(["code", "html", "sql"])
    );

    // The history holds the same cleaned answer; its thinking block is the gateway's, byte for
    // byte, and no text block is left with only what was removed.
    let (kind, stored) = conversation.entries().last().expect("the answer");
    assert_eq!(kind, EntryKind::Answer);
    let stored = stored.json().to_owned();
    let stored_blocks = blocks(&stored);
    assert_eq!(stored_blocks.first(), Some(&thinking));
    let texts: Vec<String> = stored_blocks
        .iter()
        .skip(1)
        .map(|block| {
            let block: Value = serde_json::from_str(block).expect("a block");
            assert_eq!(block["type"], "text", "{block}");
            block["text"].as_str().expect("text").to_owned()
        })
        .collect();
    assert_eq!(texts.concat(), answer.text);
    for removed in REMOVED_TEXTS {
        assert!(!stored.contains(removed), "{removed:?} in {stored}");
    }

    // Only the kinds reach the log, never the removed text.
    let events = capture.events();
    let logged = events
        .iter()
        .find(|event| event.message == "removed content from an answer")
        .expect("the removal is logged");
    assert_eq!(
        logged.fields.get("removed").map(String::as_str),
        Some("code,html,sql")
    );
    for event in &events {
        let line = format!("{} {:?}", event.message, event.fields);
        for removed in REMOVED_TEXTS {
            assert!(!line.contains(removed), "{removed:?} in {line}");
        }
    }

    // The next question sends the history back as stored: the cleaned answer, thinking intact.
    agent
        .ask(
            &mut conversation,
            "And the day before?",
            Arc::new(NoDelegation),
        )
        .await
        .expect("the second answer");
    let requests = stand_in.requests();
    assert_eq!(requests.len(), 3);
    let resent = requests[2].raw_messages();
    assert_eq!(resent.get(3), Some(&stored));
}

/// The answer's text is the history's text blocks joined, also with blocks of only blanks
/// between the others, or beside the placeholder when nothing else is left.
#[tokio::test]
async fn the_answer_is_the_historys_text_with_its_blank_blocks() {
    for (texts, expected) in [
        (
            vec!["Intro.", "\n\n", "<div></div>", "\n", "Outro."],
            "Intro.\n\n\nOutro.",
        ),
        (vec!["\n", "<div></div>"], "\n…"),
    ] {
        let mut step: Value =
            serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
        let mut content = vec![step["content"][0].clone()];
        content.extend(
            texts
                .iter()
                .map(|text| json!({"type": "text", "text": text})),
        );
        step["content"] = Value::Array(content);
        let step = step.to_string();
        let stand_in = StandIn::start(respond(move |_request, _index| ok(step.clone()))).await;
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountReadings)];
        let agent = agent(&stand_in, tools, settings());
        let mut conversation = agent.start_conversation(principal(), "");

        let answer = agent
            .ask(&mut conversation, "How much?", Arc::new(NoDelegation))
            .await
            .expect("an answer");

        assert_eq!(answer.text, expected, "{texts:?}");
        let (_, stored) = conversation.entries().last().expect("the answer");
        let stored_texts: String = blocks(stored.json())
            .iter()
            .skip(1)
            .map(|block| {
                let block: Value = serde_json::from_str(block).expect("a block");
                block["text"].as_str().expect("a text block").to_owned()
            })
            .collect();
        assert_eq!(stored_texts, answer.text, "{texts:?}");
    }
}

#[test]
fn statements_that_change_a_schema_are_removed() {
    for statement in [
        "CREATE OR REPLACE VIEW v AS SELECT 1",
        "CREATE TABLE t (a Int32) ENGINE = Memory",
        "DROP TABLE t",
        "ALTER TABLE t ADD COLUMN c Int32",
        "TRUNCATE TABLE t",
        "以 `DROP TABLE tenant_telemetry` 清除。",
    ] {
        let guarded = guard(statement);
        assert!(
            !guarded.text.contains("TABLE") && !guarded.text.contains("VIEW"),
            "{statement} -> {}",
            guarded.text
        );
        assert_eq!(
            guarded.removed.kinds().collect::<Vec<_>>(),
            [Removed::Sql],
            "{statement}"
        );
    }
}
