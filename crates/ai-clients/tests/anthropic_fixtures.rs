//! Anthropic responses and streams from the official documentation (sources in
//! `tests/fixtures/README.md`): usage by billing category, outcomes, error classification,
//! exact round trips of thinking blocks and tool calls, stream accumulation and cancellation.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use ai_clients::anthropic::{
    self, ContentBlock, InputMessage, Message, MessageStream, Outcome, Role, StopReason,
    StreamEvent, StreamLimits, Usage,
};
use ai_clients::{Error, ErrorClass, TimeoutKind, TokenUsage};
use serde_json::Value;
use support::{CountingSource, fixture, fixture_json};
use tokio::time::Instant;

const SSE_FIXTURES: [&str; 8] = [
    "anthropic/stream_text.sse",
    "anthropic/stream_tool_use.sse",
    "anthropic/stream_thinking.sse",
    "anthropic/stream_thinking_tool_use.sse",
    "anthropic/stream_max_tokens_thinking_only.sse",
    "anthropic/stream_cache_usage.sse",
    "anthropic/stream_refusal.sse",
    "anthropic/stream_overloaded.sse",
];

fn message(name: &str) -> Message {
    serde_json::from_value(fixture_json(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn limits(first_token: Duration, idle: Duration) -> StreamLimits {
    StreamLimits {
        first_token_deadline: Instant::now() + first_token,
        idle,
        total_deadline: Instant::now() + Duration::from_secs(3600),
        max_event_bytes: 1 << 20,
        max_total_bytes: 32 << 20,
    }
}

fn generous() -> StreamLimits {
    limits(Duration::from_secs(120), Duration::from_secs(120))
}

fn stream_of(source: CountingSource) -> MessageStream {
    MessageStream::from_source(Box::new(source), generous())
}

/// Reads a whole stream fixture cut into `chunk` byte pieces: every event, then the message.
async fn read_fixture(name: &str, chunk: usize) -> (Vec<StreamEvent>, Result<Message, Error>) {
    let mut stream = stream_of(CountingSource::split(&fixture(name), chunk, false));
    let mut events = Vec::new();
    loop {
        match stream.next().await {
            Ok(Some(item)) => events.push(item.event),
            Ok(None) => {
                let message = stream
                    .message()
                    .cloned()
                    .expect("a finished stream has its message");
                return (events, Ok(message));
            }
            Err(error) => return (events, Err(error)),
        }
    }
}

fn usage(input: u64, write_5m: u64, write_1h: u64, read: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input,
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
        cache_read: read,
        output,
        cache_write_ttl_assumed: false,
        input_missing: false,
        output_missing: false,
    }
}

#[test]
fn usage_categories_parse_from_recorded_responses() {
    let cases = [
        // Normal input and output only.
        ("anthropic/message_end_turn.json", usage(100, 0, 0, 0, 50)),
        // A cache pre-warm: everything written with the 5-minute lifetime.
        (
            "anthropic/message_cache_write_5m.json",
            usage(8, 5120, 0, 0, 0),
        ),
        // Writes of both lifetimes, a cache read and output.
        (
            "anthropic/message_cache_write_1h.json",
            usage(2048, 148, 100, 1800, 503),
        ),
        (
            "anthropic/message_cache_read.json",
            usage(50, 0, 0, 100_000, 37),
        ),
        // Output includes the thinking.
        (
            "anthropic/message_thinking_tool_use.json",
            usage(312, 0, 0, 4096, 188),
        ),
        (
            "anthropic/message_max_tokens_thinking_only.json",
            usage(1520, 0, 0, 0, 4096),
        ),
    ];
    for (name, expected) in cases {
        let message = message(name);
        assert_eq!(message.usage.tokens(), expected, "{name}");
        assert_eq!(
            message.usage.attempts(),
            vec![(None, expected)],
            "{name}: one attempt"
        );
    }
    let read = message("anthropic/message_cache_read.json").usage.tokens();
    assert_eq!(read.total_input(), 100_050);
}

#[test]
fn a_server_side_fallback_bills_every_attempt_at_its_own_model() {
    let message = message("anthropic/message_mixed_blocks.json");
    assert_eq!(
        message.usage.attempts(),
        vec![
            (Some("claude-fable-5"), usage(535, 0, 0, 0, 0)),
            (Some("claude-opus-4-8"), usage(412, 0, 0, 0, 264)),
        ]
    );
    // The top level describes only the attempt that answered.
    assert_eq!(message.usage.tokens(), usage(412, 0, 0, 0, 264));
}

#[test]
fn cache_writes_without_a_lifetime_split_count_as_5_minute_and_are_flagged() {
    let unsplit: Usage = serde_json::from_value(serde_json::json!({
        "input_tokens": 10, "cache_creation_input_tokens": 300, "output_tokens": 2
    }))
    .unwrap();
    assert_eq!(
        unsplit.tokens(),
        TokenUsage {
            cache_write_ttl_assumed: true,
            ..usage(10, 300, 0, 0, 2)
        }
    );
    let partly: Usage = serde_json::from_value(serde_json::json!({
        "input_tokens": 1, "cache_creation_input_tokens": 300,
        "cache_creation": {"ephemeral_1h_input_tokens": 100}, "output_tokens": 0
    }))
    .unwrap();
    assert_eq!(
        partly.tokens(),
        TokenUsage {
            cache_write_ttl_assumed: true,
            ..usage(1, 200, 100, 0, 0)
        }
    );
}

#[tokio::test]
async fn stream_usage_is_cumulative_and_ends_as_the_non_streaming_response() {
    let (_, streamed) = read_fixture("anthropic/stream_cache_usage.sse", 4096).await;
    let streamed = streamed.unwrap();
    assert_eq!(streamed.usage.tokens(), usage(2048, 148, 100, 1800, 503));
    assert_eq!(streamed, message("anthropic/message_cache_write_1h.json"));
}

#[test]
fn outcomes_of_recorded_responses() {
    let cases = [
        ("anthropic/message_end_turn.json", Outcome::Completed),
        (
            "anthropic/message_thinking_tool_use.json",
            Outcome::Completed,
        ),
        (
            "anthropic/message_refusal.json",
            Outcome::Refusal {
                category: Some("cyber".into()),
            },
        ),
        // Thinking used up max_tokens: a separate outcome, not an empty answer.
        (
            "anthropic/message_max_tokens_thinking_only.json",
            Outcome::MaxTokensWithoutText,
        ),
        // Text cut at max_tokens is still an answer.
        (
            "anthropic/message_max_tokens_with_text.json",
            Outcome::Completed,
        ),
        // A cache pre-warm (max_tokens 0) looks the same; the caller that sent it knows.
        (
            "anthropic/message_cache_write_5m.json",
            Outcome::MaxTokensWithoutText,
        ),
    ];
    for (name, expected) in cases {
        assert_eq!(message(name).outcome(), expected, "{name}");
    }
    let refusal = message("anthropic/message_refusal.json");
    assert_eq!(refusal.stop_reason, Some(StopReason::Refusal));
    let details = refusal.stop_details.unwrap();
    assert_eq!(details.kind.as_deref(), Some("refusal"));
    assert!(details.explanation.is_some());
}

#[tokio::test]
async fn streamed_refusal_and_thinking_only_responses_have_the_same_outcomes() {
    let (_, refusal) = read_fixture("anthropic/stream_refusal.sse", 4096).await;
    let refusal = refusal.unwrap();
    assert_eq!(
        refusal.outcome(),
        Outcome::Refusal {
            category: Some("cyber".into())
        }
    );
    // The partial output before the refusal is discarded, as a non-streaming refusal has none.
    assert!(refusal.content.is_empty(), "{:?}", refusal.content);
    assert_eq!(refusal.usage.tokens().output, 6);
    let (_, thinking_only) =
        read_fixture("anthropic/stream_max_tokens_thinking_only.sse", 4096).await;
    let thinking_only = thinking_only.unwrap();
    assert_eq!(thinking_only.outcome(), Outcome::MaxTokensWithoutText);
    assert_eq!(
        thinking_only,
        message("anthropic/message_max_tokens_thinking_only.json")
    );
}

#[test]
fn error_responses_classify_by_status_and_type() {
    let cases = fixture_json("anthropic/errors.json");
    for case in cases.as_array().unwrap() {
        let status = u16::try_from(case["status"].as_u64().unwrap()).unwrap();
        let error_type = case["body"]["error"]["type"].as_str();
        let expected = match status {
            429 => ErrorClass::RateLimited { retry_after: None },
            529 => ErrorClass::Overloaded,
            504 => ErrorClass::Timeout(TimeoutKind::Provider),
            _ => ErrorClass::Other,
        };
        assert_eq!(
            anthropic::classify_error(Some(status), error_type),
            expected,
            "{}",
            case["situation"]
        );
    }
    // A mid-stream error event has no status; its type decides.
    assert_eq!(
        anthropic::classify_error(None, Some("overloaded_error")),
        ErrorClass::Overloaded
    );
    assert_eq!(
        anthropic::classify_error(None, Some("api_error")),
        ErrorClass::Other
    );
}

#[tokio::test]
async fn an_overloaded_error_event_mid_stream_ends_the_stream() {
    let mut stream = stream_of(CountingSource::split(
        &fixture("anthropic/stream_overloaded.sse"),
        4096,
        false,
    ));
    let mut events = 0;
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => events += 1,
            Ok(None) => panic!("the stream ended without its error event"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        events, 3,
        "message_start, content_block_start and one delta come first"
    );
    let Error::StreamError(api) = &error else {
        panic!("expected a stream error, got {error:?}");
    };
    assert_eq!(api.status, None);
    assert_eq!(error.class(), ErrorClass::Overloaded);
    // The partial message stays readable for estimating usage.
    let partial = stream.message().unwrap();
    assert_eq!(partial.content, vec![ContentBlock::text("Hello")]);
    // A failed stream stays failed.
    assert!(matches!(stream.next().await, Err(Error::Protocol(_))));
}

/// Parsing and serializing again must give the same fields and content, block by block.
fn assert_blocks_round_trip(name: &str) -> Message {
    let original = fixture_json(name);
    let message: Message =
        serde_json::from_value(original.clone()).expect("the fixture is a message");
    let blocks = original["content"]
        .as_array()
        .expect("the fixture has content");
    assert_eq!(message.content.len(), blocks.len());
    for (block, sent) in message.content.iter().zip(blocks) {
        assert_eq!(
            &serde_json::to_value(block).expect("a block serializes"),
            sent,
            "{name}: {}",
            block.kind()
        );
    }
    // Sending the assistant turn back carries the content exactly as received.
    let turn =
        serde_json::to_value(InputMessage::from_response(&message)).expect("a turn serializes");
    assert_eq!(turn["role"], "assistant");
    assert_eq!(
        &turn["content"], &original["content"],
        "{name}: the turn sent back"
    );
    message
}

#[test]
fn thinking_blocks_and_tool_calls_round_trip_exactly() {
    let message = assert_blocks_round_trip("anthropic/message_thinking_tool_use.json");
    let kinds: Vec<&str> = message.content.iter().map(ContentBlock::kind).collect();
    assert_eq!(kinds, ["thinking", "thinking", "tool_use"]);
    let ContentBlock::Thinking(thinking) = &message.content[1] else {
        panic!("block 1 is thinking");
    };
    assert_eq!(thinking.signature, "Es8CCkYICxIM...");

    let mixed = assert_blocks_round_trip("anthropic/message_mixed_blocks.json");
    let kinds: Vec<&str> = mixed.content.iter().map(ContentBlock::kind).collect();
    assert_eq!(
        kinds,
        [
            "fallback",
            "thinking",
            "redacted_thinking",
            "text",
            "server_tool_use",
            "web_search_tool_result",
            "tool_use",
            "connector_text"
        ]
    );
    assert!(matches!(mixed.content[0], ContentBlock::Other(_)));
    assert!(matches!(
        mixed.content[2],
        ContentBlock::RedactedThinking(_)
    ));
    let ContentBlock::ToolUse(tool) = &mixed.content[6] else {
        panic!("block 6 is tool_use");
    };
    // Fields this crate does not model (`caller`) are kept.
    assert!(tool.extra.contains_key("caller"));
    assert_eq!(tool.input["note"], "電表 ✓ \"quoted\" \\ backslash");

    assert_blocks_round_trip("anthropic/message_max_tokens_thinking_only.json");
}

#[tokio::test]
async fn a_streamed_thinking_and_tool_call_turn_matches_the_non_streaming_one_exactly() {
    let (_, streamed) = read_fixture("anthropic/stream_thinking_tool_use.sse", 4096).await;
    let streamed = streamed.unwrap();
    let original = fixture_json("anthropic/message_thinking_tool_use.json");
    assert_eq!(
        streamed,
        serde_json::from_value::<Message>(original.clone()).unwrap()
    );
    let turn = serde_json::to_value(InputMessage::from_response(&streamed)).unwrap();
    assert_eq!(turn["content"], original["content"]);
}

#[tokio::test]
async fn every_chunking_gives_the_same_events_and_message() {
    for name in SSE_FIXTURES {
        let (events, whole) = read_fixture(name, usize::MAX).await;
        for chunk in [1, 2, 3, 7, 64] {
            let (split_events, split) = read_fixture(name, chunk).await;
            assert_eq!(split_events, events, "{name}, {chunk}-byte chunks");
            match (&whole, &split) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}, {chunk}-byte chunks"),
                (Err(a), Err(b)) => assert_eq!(a.class(), b.class(), "{name}"),
                _ => panic!("{name}: {chunk}-byte chunks changed the result"),
            }
        }
    }
}

#[tokio::test]
async fn documented_streams_build_the_documented_messages() {
    let (_, text) = read_fixture("anthropic/stream_text.sse", 4096).await;
    let text = text.unwrap();
    assert_eq!(text.content, vec![ContentBlock::text("Hello!")]);
    assert_eq!(text.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(text.usage.tokens().output, 15);

    let (_, tool) = read_fixture("anthropic/stream_tool_use.sse", 4096).await;
    let tool = tool.unwrap();
    assert_eq!(tool.stop_reason, Some(StopReason::ToolUse));
    let ContentBlock::ToolUse(call) = &tool.content[1] else {
        panic!("block 1 is tool_use");
    };
    assert_eq!(call.name, "get_weather");
    assert_eq!(
        call.input,
        serde_json::json!({"location": "San Francisco, CA"})
    );

    let (_, thinking) = read_fixture("anthropic/stream_thinking.sse", 4096).await;
    let thinking = thinking.unwrap();
    let ContentBlock::Thinking(block) = &thinking.content[0] else {
        panic!("block 0 is thinking");
    };
    assert!(
        block.thinking.ends_with("GCD(1071, 462) = 21."),
        "{}",
        block.thinking
    );
    assert!(!block.signature.is_empty());
}

#[tokio::test]
async fn raw_events_relay_unchanged() {
    for name in SSE_FIXTURES {
        let mut stream = stream_of(CountingSource::split(&fixture(name), 4096, false));
        let mut relayed = Vec::new();
        while let Ok(Some(item)) = stream.next().await {
            relayed.extend(item.raw.to_bytes());
        }
        let relayed = String::from_utf8(relayed).unwrap();
        let (_, direct) = read_fixture(name, 4096).await;
        let mut again = stream_of(CountingSource::split(&relayed, 4096, false));
        let mut events = 0;
        while let Ok(Some(_)) = again.next().await {
            events += 1;
        }
        if let Ok(direct) = direct {
            assert_eq!(again.message(), Some(&direct), "{name}");
        }
        assert!(events > 0, "{name}");
    }
}

#[tokio::test]
async fn after_cancel_nothing_more_is_read() {
    // The source would hand out the whole stream and then hang.
    let source = CountingSource::split(&fixture("anthropic/stream_tool_use.sse"), 64, true);
    let reads = std::sync::Arc::clone(&source.reads);
    let mut stream = stream_of(source);
    for _ in 0..3 {
        stream.next().await.unwrap().unwrap();
    }
    let before = reads.load(Ordering::SeqCst);
    stream.cancel();
    assert!(matches!(stream.next().await, Err(Error::Cancelled)));
    assert!(matches!(stream.next().await, Err(Error::Cancelled)));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "a cancelled stream read again"
    );
}

#[tokio::test]
async fn a_token_cancels_a_stream_waiting_in_another_task() {
    let head = support::sse_prefix(&fixture("anthropic/stream_text.sse"), "content_block_delta");
    let source = CountingSource::split(&head, 4096, true);
    let reads = std::sync::Arc::clone(&source.reads);
    let mut stream = stream_of(source);
    let token = stream.cancellation_token();
    let reader = tokio::spawn(async move {
        let mut items = 0;
        let error = loop {
            match stream.next().await {
                Ok(Some(_)) => items += 1,
                Ok(None) => panic!("the stream finished"),
                Err(error) => break error,
            }
        };
        (items, error, stream)
    });
    // Wait until the reader has consumed what there is and waits on the hanging source.
    tokio::time::timeout(support::WAIT, async {
        while reads.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the reader did not reach the end of the available bytes");
    token.cancel();
    let (items, error, mut stream) = tokio::time::timeout(support::WAIT, reader)
        .await
        .expect("cancelling did not wake the reader")
        .unwrap();
    assert_eq!(items, 3, "message_start, content_block_start, ping");
    assert!(matches!(error, Error::Cancelled), "{error:?}");
    let after = reads.load(Ordering::SeqCst);
    assert!(matches!(stream.next().await, Err(Error::Cancelled)));
    assert_eq!(reads.load(Ordering::SeqCst), after);
}

#[tokio::test]
async fn no_output_before_the_first_token_limit_is_a_first_token_timeout() {
    // message_start and a ping arrive, then nothing: pings do not count as output.
    let head = support::sse_prefix(&fixture("anthropic/stream_text.sse"), "content_block_start");
    let head = format!("{head}event: ping\ndata: {{\"type\": \"ping\"}}\n\n");
    let mut stream = MessageStream::from_source(
        Box::new(CountingSource::split(&head, 4096, true)),
        limits(Duration::from_millis(300), Duration::from_secs(120)),
    );
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::FirstToken),
        "{error:?}"
    );
}

#[tokio::test]
async fn silence_after_output_started_is_a_stream_idle_timeout() {
    let head = support::sse_prefix(&fixture("anthropic/stream_text.sse"), "content_block_stop");
    let mut stream = MessageStream::from_source(
        Box::new(CountingSource::split(&head, 4096, true)),
        limits(Duration::from_secs(120), Duration::from_millis(300)),
    );
    let mut output = 0;
    let error = loop {
        match stream.next().await {
            Ok(Some(item)) => output += usize::from(item.event.is_output()),
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert!(output > 0);
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::StreamIdle),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_stream_cut_before_message_stop_is_a_protocol_error() {
    let head = support::sse_prefix(&fixture("anthropic/stream_text.sse"), "message_stop");
    let mut stream = stream_of(CountingSource::split(&head, 4096, false));
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    assert_eq!(error.class(), ErrorClass::Other);
}

#[test]
fn requests_serialize_cache_breakpoints_and_mid_conversation_system_messages() {
    use ai_clients::anthropic::{CacheControl, MessageContent, MessagesRequest, Tool};
    let request = MessagesRequest {
        model: "claude-opus-5-5".into(),
        max_tokens: 1024,
        messages: vec![
            InputMessage::new(
                Role::User,
                MessageContent::Blocks(vec![
                    ContentBlock::text("site manual")
                        .with_cache_control(CacheControl::ephemeral_1h()),
                ]),
            ),
            InputMessage::from_response(&message("anthropic/message_thinking_tool_use.json")),
            InputMessage::new(
                Role::User,
                MessageContent::Blocks(vec![ContentBlock::tool_result(
                    "toolu_01D7FLrfh4GYq7yT1ULFeyMV",
                    MessageContent::Text("done".into()),
                    false,
                )]),
            ),
            InputMessage::system_text("The user switched to the floor 3 dashboard."),
        ],
        system: Some(MessageContent::Text(
            "You are the TrustIoT assistant.".into(),
        )),
        tools: vec![
            Tool {
                name: "edit_file".into(),
                description: None,
                input_schema: Some(serde_json::json!({"type": "object"})),
                extra: serde_json::Map::new(),
            }
            .with_cache_control(CacheControl::ephemeral()),
        ],
        stream: None,
        extra: serde_json::Map::new(),
    };
    let body = serde_json::to_value(&request).unwrap();
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"})
    );
    assert_eq!(
        body["messages"][1]["content"],
        fixture_json("anthropic/message_thinking_tool_use.json")["content"]
    );
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][3]["role"], "system");
    assert_eq!(
        body["tools"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert!(body.get("stream").is_none());
    // The request parses back to itself.
    let parsed: MessagesRequest = serde_json::from_value(body).unwrap();
    assert_eq!(parsed, request);
}

#[test]
fn unknown_stop_reasons_and_events_are_kept() {
    let mut value = fixture_json("anthropic/message_end_turn.json");
    value["stop_reason"] = Value::from("something_new");
    let message: Message = serde_json::from_value(value).unwrap();
    assert_eq!(
        message.stop_reason,
        Some(StopReason::Other("something_new".into()))
    );
    assert_eq!(message.outcome(), Outcome::Completed);
    let event = StreamEvent::parse(&ai_clients::sse::SseEvent {
        event: "brand_new".into(),
        data: r#"{"type":"brand_new","x":1}"#.into(),
    })
    .unwrap();
    assert!(matches!(event, StreamEvent::Unknown { .. }));
}

const MESSAGE_END: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// Reads a stream to its end or its first error.
async fn read_text(text: &str) -> Result<Message, Error> {
    let mut stream = stream_of(CountingSource::split(text, 4096, false));
    while stream.next().await?.is_some() {}
    Ok(stream
        .message()
        .cloned()
        .expect("a finished stream has its message"))
}

#[tokio::test]
async fn content_blocks_must_be_stopped_before_the_message_ends() {
    let text = fixture("anthropic/stream_text.sse");
    // The text block is never stopped.
    let unstopped = format!(
        "{}{MESSAGE_END}",
        support::sse_prefix(&text, "content_block_stop")
    );
    let error = read_text(&unstopped).await.unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");

    // A delta after the block stopped.
    let stopped = support::sse_prefix(&text, "message_delta");
    let late = format!(
        "{stopped}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"late\"}}}}\n\n{MESSAGE_END}"
    );
    let error = read_text(&late).await.unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");

    // The complete stream passes.
    read_text(&text).await.unwrap();
}

#[tokio::test]
async fn a_refusal_while_a_tool_call_is_open_ends_the_message_without_content() {
    let text = fixture("anthropic/stream_tool_use.sse");
    // Everything before the tool_use block's content_block_stop (the second one).
    let (cut, _) = text
        .match_indices("event: content_block_stop\n")
        .nth(1)
        .expect("the tool use stream stops two blocks");
    let refusal = format!(
        "{}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"refusal\",\"stop_sequence\":null,\"stop_details\":{{\"type\":\"refusal\",\"category\":null,\"explanation\":null}}}},\"usage\":{{\"output_tokens\":40}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        text.get(..cut).expect("a char boundary")
    );
    let message = read_text(&refusal).await.unwrap();
    assert_eq!(message.outcome(), Outcome::Refusal { category: None });
    assert!(message.content.is_empty());
    assert_eq!(message.usage.tokens().output, 40);
}
