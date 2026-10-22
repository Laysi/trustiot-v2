//! Streaming responses: the event types, the accumulator that builds the final [`Message`],
//! and [`MessageStream`], which reads events with the first-token and idle limits and can be
//! cancelled (<https://platform.claude.com/docs/en/build-with-claude/streaming>).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::classify_error;
use super::types::{ContentBlock, Extra, Message, StopDetails, StopReason, Usage};
use crate::error::{ApiError, ApiErrorBody, Error, TimeoutKind, json_error};
use crate::sse::{SseEvent, SseParser};
use crate::transport::ByteSource;

/// The change a `content_block_delta` event makes to its block.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    /// A piece of a tool call's `input` JSON; parse only the whole once the block stops.
    InputJson(String),
    Thinking(String),
    Signature(String),
    /// One citation to append to a text block's `citations`.
    Citation(Value),
    /// A delta type added after this crate was written: relayed, not applied.
    Other(Value),
}

impl Delta {
    /// The delta type, for error messages (never the delta's content, which is model output).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text_delta",
            Self::InputJson(_) => "input_json_delta",
            Self::Thinking(_) => "thinking_delta",
            Self::Signature(_) => "signature_delta",
            Self::Citation(_) => "citations_delta",
            Self::Other(_) => "unknown",
        }
    }
}

impl<'de> Deserialize<'de> for Delta {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let field = |name: &str| -> Result<Value, D::Error> {
            value
                .get(name)
                .cloned()
                .ok_or_else(|| serde::de::Error::custom(format!("delta without `{name}`")))
        };
        let string = |name: &str| -> Result<String, D::Error> {
            match field(name)? {
                Value::String(text) => Ok(text),
                _ => Err(serde::de::Error::custom(format!(
                    "delta `{name}` is not a string"
                ))),
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("text_delta") => string("text").map(Delta::Text),
            Some("input_json_delta") => string("partial_json").map(Delta::InputJson),
            Some("thinking_delta") => string("thinking").map(Delta::Thinking),
            Some("signature_delta") => string("signature").map(Delta::Signature),
            Some("citations_delta") => field("citation").map(Delta::Citation),
            _ => Ok(Delta::Other(value)),
        }
    }
}

/// The `delta` of a `message_delta` event.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One parsed stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The message with empty `content`, `stop_reason: null` and the input usage.
    MessageStart(Message),
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    /// Top-level changes: the stop reason, and the usage so far (cumulative).
    MessageDelta {
        delta: MessageDeltaBody,
        usage: Option<Usage>,
        extra: Extra,
    },
    MessageStop,
    Ping,
    /// An error after HTTP 200; the stream ends with it.
    Error(ApiErrorBody),
    /// An event type added after this crate was written.
    Unknown {
        event: String,
        data: Value,
    },
}

impl StreamEvent {
    /// Parses the data of one server-sent event. The `type` in the data decides; the SSE
    /// event name is used only for unknown events.
    pub fn parse(sse: &SseEvent) -> Result<Self, Error> {
        #[derive(Deserialize)]
        struct Start {
            message: Message,
        }
        #[derive(Deserialize)]
        struct BlockStart {
            index: usize,
            content_block: ContentBlock,
        }
        #[derive(Deserialize)]
        struct BlockDelta {
            index: usize,
            delta: Delta,
        }
        #[derive(Deserialize)]
        struct BlockStop {
            index: usize,
        }
        #[derive(Deserialize)]
        struct MessageDelta {
            delta: MessageDeltaBody,
            #[serde(default)]
            usage: Option<Usage>,
            #[serde(flatten)]
            extra: Extra,
        }
        #[derive(Deserialize)]
        struct ErrorEvent {
            error: ApiErrorBody,
        }
        fn decode<T: serde::de::DeserializeOwned>(event: &str, data: Value) -> Result<T, Error> {
            // `event` is one of the known event types matched below, never upstream text.
            serde_json::from_value(data).map_err(|error| {
                Error::Protocol(format!("`{event}` event: {}", json_error(&error)))
            })
        }

        let data: Value = serde_json::from_str(&sse.data).map_err(|error| {
            Error::Protocol(format!("event data is not JSON: {}", json_error(&error)))
        })?;
        let kind = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(sse.event.as_str())
            .to_owned();
        Ok(match kind.as_str() {
            "message_start" => Self::MessageStart(decode::<Start>(&kind, data)?.message),
            "content_block_start" => {
                let start: BlockStart = decode(&kind, data)?;
                Self::ContentBlockStart {
                    index: start.index,
                    content_block: start.content_block,
                }
            }
            "content_block_delta" => {
                let delta: BlockDelta = decode(&kind, data)?;
                Self::ContentBlockDelta {
                    index: delta.index,
                    delta: delta.delta,
                }
            }
            "content_block_stop" => Self::ContentBlockStop {
                index: decode::<BlockStop>(&kind, data)?.index,
            },
            "message_delta" => {
                let MessageDelta {
                    delta,
                    usage,
                    mut extra,
                } = decode(&kind, data)?;
                extra.remove("type");
                Self::MessageDelta {
                    delta,
                    usage,
                    extra,
                }
            }
            "message_stop" => Self::MessageStop,
            "ping" => Self::Ping,
            "error" => Self::Error(decode::<ErrorEvent>(&kind, data)?.error),
            _ => Self::Unknown {
                event: sse.event.clone(),
                data,
            },
        })
    }

    /// Whether the event carries model output: a content block opening or growing. The
    /// first one ends the first-token wait.
    pub fn is_output(&self) -> bool {
        matches!(
            self,
            Self::ContentBlockStart { .. } | Self::ContentBlockDelta { .. }
        )
    }
}

/// Builds the [`Message`] from the events of one stream, as the non-streaming API would have
/// returned it. Thinking text and signatures, and tool call input, come out exactly as sent.
///
/// Every content block must be started, then stopped, before `message_delta` and
/// `message_stop`. The one exception is a refusal: it can arrive mid-stream, even while a
/// tool call is still open
/// (<https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback>), and the
/// partial output before it must be discarded, so the refusal empties `content` (as a
/// non-streaming refusal has none) and closes every open block. The raw events already relayed
/// still carry that partial output; whoever shows them must drop it on a refusal.
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    message: Option<Message>,
    /// The blocks started and not yet stopped.
    open: BTreeSet<usize>,
    /// The `input_json_delta` pieces of each open block that takes them.
    partial_json: BTreeMap<usize, String>,
    complete: bool,
}

impl MessageAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The message so far: after an interruption, what had arrived (for estimating usage).
    pub fn message(&self) -> Option<&Message> {
        self.message.as_ref()
    }

    /// Whether `message_stop` arrived.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn into_message(self) -> Result<Message, Error> {
        match (self.complete, self.message) {
            (true, Some(message)) => Ok(message),
            _ => Err(Error::Protocol(
                "the stream ended before `message_stop`".into(),
            )),
        }
    }

    pub fn apply(&mut self, event: &StreamEvent) -> Result<(), Error> {
        let protocol = |what: String| Err(Error::Protocol(what));
        if self.complete && !matches!(event, StreamEvent::Ping | StreamEvent::Unknown { .. }) {
            return protocol("an event after `message_stop`".into());
        }
        if let StreamEvent::MessageStart(message) = event {
            if self.message.is_some() {
                return protocol("a second `message_start`".into());
            }
            self.message = Some(message.clone());
            return Ok(());
        }
        let Some(message) = self.message.as_mut() else {
            return match event {
                StreamEvent::Ping | StreamEvent::Unknown { .. } => Ok(()),
                _ => protocol("an event before `message_start`".into()),
            };
        };
        match event {
            StreamEvent::MessageStart(_) => {}
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if *index != message.content.len() {
                    return protocol(format!(
                        "content block {index} started, expected {}",
                        message.content.len()
                    ));
                }
                message.content.push(content_block.clone());
                self.open.insert(*index);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if !self.open.contains(index) {
                    return protocol(format!(
                        "a delta for content block {index}, which is not open"
                    ));
                }
                let Some(block) = message.content.get_mut(*index) else {
                    return protocol(format!(
                        "a delta for content block {index}, which never started"
                    ));
                };
                match (block, delta) {
                    (ContentBlock::Text(text), Delta::Text(more)) => text.text.push_str(more),
                    (ContentBlock::Thinking(thinking), Delta::Thinking(more)) => {
                        thinking.thinking.push_str(more);
                    }
                    (ContentBlock::Thinking(thinking), Delta::Signature(more)) => {
                        thinking.signature.push_str(more);
                    }
                    (ContentBlock::ToolUse(_) | ContentBlock::Other(_), Delta::InputJson(more)) => {
                        self.partial_json.entry(*index).or_default().push_str(more);
                    }
                    (ContentBlock::Text(text), Delta::Citation(citation)) => {
                        let citations = text
                            .extra
                            .entry("citations")
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if !citations.is_array() {
                            *citations = Value::Array(Vec::new());
                        }
                        if let Value::Array(list) = citations {
                            list.push(citation.clone());
                        }
                    }
                    (_, Delta::Other(_)) => {}
                    (block, delta) => {
                        return protocol(format!(
                            "a `{}` for a `{}` block",
                            delta.kind(),
                            block.kind()
                        ));
                    }
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                let Some(block) = message.content.get_mut(*index) else {
                    return protocol(format!("content block {index} stopped, never started"));
                };
                if !self.open.remove(index) {
                    return protocol(format!("content block {index} stopped twice"));
                }
                if let Some(json) = self.partial_json.remove(index) {
                    // The first piece is often empty; a block that got only empty pieces keeps
                    // the `input` it started with.
                    if !json.trim().is_empty() {
                        let input: Value = serde_json::from_str(&json).map_err(|error| {
                            Error::Protocol(format!(
                                "tool input of block {index}: {}",
                                json_error(&error)
                            ))
                        })?;
                        match block {
                            ContentBlock::ToolUse(tool) => tool.input = input,
                            ContentBlock::Other(Value::Object(map)) => {
                                map.insert("input".into(), input);
                            }
                            _ => {}
                        }
                    }
                }
            }
            StreamEvent::MessageDelta {
                delta,
                usage,
                extra,
            } => {
                if delta.stop_reason == Some(StopReason::Refusal) {
                    message.content.clear();
                    self.open.clear();
                    self.partial_json.clear();
                } else if let Some(index) = self.open.first() {
                    return protocol(format!(
                        "`message_delta` with content block {index} still open"
                    ));
                }
                if delta.stop_reason.is_some() {
                    message.stop_reason.clone_from(&delta.stop_reason);
                }
                message.stop_sequence.clone_from(&delta.stop_sequence);
                if delta.stop_details.is_some() {
                    message.stop_details.clone_from(&delta.stop_details);
                }
                for (key, value) in delta.extra.iter().chain(extra.iter()) {
                    message.extra.insert(key.clone(), value.clone());
                }
                if let Some(usage) = usage {
                    message.usage.update(usage);
                }
            }
            StreamEvent::MessageStop => {
                if let Some(index) = self.open.first() {
                    return protocol(format!(
                        "`message_stop` with content block {index} still open"
                    ));
                }
                self.complete = true;
            }
            StreamEvent::Ping | StreamEvent::Unknown { .. } | StreamEvent::Error(_) => {}
        }
        Ok(())
    }
}

/// One event of a [`MessageStream`]: as received (to relay unchanged) and parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamItem {
    pub raw: SseEvent,
    pub event: StreamEvent,
}

/// Limits of one stream ([`crate::Timeouts`], [`crate::EndpointConfig`]).
#[derive(Debug, Clone)]
pub struct StreamLimits {
    /// Model output must start before this instant.
    pub first_token_deadline: Instant,
    pub idle: Duration,
    /// The stream must end (`message_stop`) before this instant, however steadily data
    /// arrives; then it fails with [`TimeoutKind::StreamTotal`].
    pub total_deadline: Instant,
    pub max_event_bytes: usize,
    /// Bytes the whole stream may bring (pings and comments included); more is
    /// [`Error::Protocol`]. This bounds what the accumulator holds.
    pub max_total_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Finished,
    Failed,
    Cancelled,
}

/// A streaming Messages response. Holds the endpoint's concurrency slot until the stream
/// finishes, fails, is cancelled or dropped. [`MessageStream::cancel`] drops the response at
/// once, which closes the connection and frees the slot; nothing more is read. A token from
/// [`MessageStream::cancellation_token`] does the same when the owner next polls the stream (at
/// once if it is waiting in [`MessageStream::next`]) or drops it: until then the connection and
/// the slot stay held.
pub struct MessageStream {
    source: Option<Box<dyn ByteSource>>,
    permit: Option<OwnedSemaphorePermit>,
    parser: SseParser,
    pending: VecDeque<SseEvent>,
    accumulator: MessageAccumulator,
    limits: StreamLimits,
    output_started: bool,
    /// Bytes received so far, against `limits.max_total_bytes`.
    received: usize,
    cancel: CancellationToken,
    state: State,
    request_id: Option<String>,
}

impl MessageStream {
    /// A stream over any byte source: the client uses the HTTP response; a relay or a test
    /// can feed recorded bytes.
    pub fn from_source(source: Box<dyn ByteSource>, limits: StreamLimits) -> Self {
        Self {
            source: Some(source),
            permit: None,
            parser: SseParser::new(limits.max_event_bytes),
            pending: VecDeque::new(),
            accumulator: MessageAccumulator::new(),
            limits,
            output_started: false,
            received: 0,
            cancel: CancellationToken::new(),
            state: State::Open,
            request_id: None,
        }
    }

    pub(crate) fn with_permit(
        mut self,
        permit: OwnedSemaphorePermit,
        request_id: Option<String>,
    ) -> Self {
        self.permit = Some(permit);
        self.request_id = request_id;
        self
    }

    /// The provider's request ID (`request-id` header).
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// A token that cancels this stream from another task. The response is released when the
    /// owner next polls the stream (at once if it is waiting in [`MessageStream::next`]) or drops
    /// it, not when the token fires.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stops reading: drops the response (closing the connection) and frees the concurrency
    /// slot. Every later [`MessageStream::next`] returns [`Error::Cancelled`].
    pub fn cancel(&mut self) {
        self.cancel.cancel();
        if self.state == State::Open {
            self.close(State::Cancelled);
        }
    }

    /// The message so far (complete after `message_stop`), with the usage reported so far.
    pub fn message(&self) -> Option<&Message> {
        self.accumulator.message()
    }

    /// The next event, `Ok(None)` after `message_stop`.
    pub async fn next(&mut self) -> Result<Option<StreamItem>, Error> {
        loop {
            match self.state {
                State::Open => {}
                State::Finished => return Ok(None),
                State::Cancelled => return Err(Error::Cancelled),
                State::Failed => {
                    return Err(Error::Protocol(
                        "the stream already ended with an error".into(),
                    ));
                }
            }
            if self.cancel.is_cancelled() {
                self.close(State::Cancelled);
                return Err(Error::Cancelled);
            }
            if let Some(raw) = self.pending.pop_front() {
                return match self.handle(&raw) {
                    Ok(event) => Ok(Some(StreamItem { raw, event })),
                    Err(error) => Err(self.fail(error)),
                };
            }
            self.read_more().await?;
        }
    }

    /// Reads to the end and returns the complete message.
    pub async fn into_message(mut self) -> Result<Message, Error> {
        while self.next().await?.is_some() {}
        self.accumulator.into_message()
    }

    fn handle(&mut self, raw: &SseEvent) -> Result<StreamEvent, Error> {
        let event = StreamEvent::parse(raw)?;
        if let StreamEvent::Error(body) = &event {
            return Err(Error::StreamError(Box::new(ApiError {
                status: None,
                class: classify_error(None, body.error_type.as_deref()),
                body: body.clone(),
                request_id: self.request_id.clone(),
                retry_after: None,
            })));
        }
        if event.is_output() {
            self.output_started = true;
        }
        self.accumulator.apply(&event)?;
        if self.accumulator.is_complete() {
            self.close(State::Finished);
        }
        Ok(event)
    }

    async fn read_more(&mut self) -> Result<(), Error> {
        // The nearest of the three limits decides how long this read may wait and which
        // timeout it is. An idle limit too large to add to now is no limit.
        let now = Instant::now();
        let mut limit = now
            .checked_add(self.limits.idle)
            .map(|until| (until, TimeoutKind::StreamIdle));
        let mut tighten = |until: Instant, kind: TimeoutKind| {
            if limit.is_none_or(|(current, _)| until <= current) {
                limit = Some((until, kind));
            }
        };
        if !self.output_started {
            tighten(self.limits.first_token_deadline, TimeoutKind::FirstToken);
        }
        tighten(self.limits.total_deadline, TimeoutKind::StreamTotal);
        let (until, kind) = limit.unwrap_or((self.limits.total_deadline, TimeoutKind::StreamTotal));
        let cancel = self.cancel.clone();
        let Some(source) = self.source.as_mut() else {
            return Err(self.fail(Error::Protocol("the stream has no body".into())));
        };
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            chunk = tokio::time::timeout_at(until, source.next_chunk()) => Some(chunk),
        };
        let Some(chunk) = chunk else {
            self.close(State::Cancelled);
            return Err(Error::Cancelled);
        };
        match chunk {
            Err(_) => Err(self.fail(Error::Timeout(kind))),
            Ok(Err(error)) => Err(self.fail(error)),
            Ok(Ok(None)) => Err(self.fail(Error::Protocol(
                "the stream ended before `message_stop`".into(),
            ))),
            Ok(Ok(Some(bytes))) => {
                self.received = self.received.saturating_add(bytes.len());
                if self.received > self.limits.max_total_bytes {
                    let limit = self.limits.max_total_bytes;
                    return Err(
                        self.fail(Error::Protocol(format!("the stream exceeds {limit} bytes")))
                    );
                }
                match self.parser.push(&bytes, &mut self.pending) {
                    Ok(()) => Ok(()),
                    Err(error) => Err(self.fail(error.into())),
                }
            }
        }
    }

    fn fail(&mut self, error: Error) -> Error {
        self.close(State::Failed);
        error
    }

    fn close(&mut self, state: State) {
        self.state = state;
        self.source = None;
        self.permit = None;
        self.pending.clear();
    }
}

impl std::fmt::Debug for MessageStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStream")
            .field("state", &self.state)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}
