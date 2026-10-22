//! Request and response types of the Anthropic Messages API
//! (<https://platform.claude.com/docs/en/api/messages>).
//!
//! Only the fields this platform reads or writes are typed; every other field, and every
//! content block type not listed in [`ContentBlock`], is kept as JSON and written back as it
//! came. A response's content blocks therefore serialize to the same JSON they were parsed
//! from (object keys come out sorted, which is stable from call to call), which is what sending
//! the conversation history back unchanged needs (docs/architecture/ai-assistant.md,
//! 「對話歷史只往後加」).

use std::fmt;

use serde::de::{self, DeserializeOwned};
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::usage::TokenUsage;

/// Fields that are not typed, kept as received.
pub type Extra = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// A mid-conversation system message
    /// (<https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages>).
    System,
}

/// A cache breakpoint (`cache_control`). The lifetime is 5 minutes unless `ttl` says `1h`
/// (<https://platform.claude.com/docs/en/build-with-claude/prompt-caching>).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl CacheControl {
    /// A breakpoint with the default 5-minute lifetime.
    pub fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".into(),
            ttl: None,
            extra: Extra::new(),
        }
    }

    /// A breakpoint with the 1-hour lifetime.
    pub fn ephemeral_1h() -> Self {
        Self {
            ttl: Some("1h".into()),
            ..Self::ephemeral()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// `{"type": "base64", "media_type": ..., "data": ...}` or another source the API accepts.
    pub source: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A client tool call. `input` is the JSON object the model produced, unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Thinking: `thinking` is the summary (empty when the display is `omitted`), `signature` the
/// encrypted full thinking. Both must go back exactly as received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedThinkingBlock {
    pub data: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One content block. Types without their own variant (server tool calls and results,
/// documents, `fallback`, and any type added later) are [`ContentBlock::Other`], kept as JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
    Thinking(ThinkingBlock),
    RedactedThinking(RedactedThinkingBlock),
    Other(Value),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock {
            text: text.into(),
            extra: Extra::new(),
        })
    }

    /// A base64 image (`media_type` such as `image/png`; `data` already base64).
    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image(ImageBlock {
            source: serde_json::json!({
                "type": "base64",
                "media_type": media_type.into(),
                "data": data.into(),
            }),
            extra: Extra::new(),
        })
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: MessageContent,
        is_error: bool,
    ) -> Self {
        Self::ToolResult(ToolResultBlock {
            tool_use_id: tool_use_id.into(),
            content: Some(content),
            is_error: is_error.then_some(true),
            extra: Extra::new(),
        })
    }

    /// The `type` of the block.
    pub fn kind(&self) -> &str {
        match self {
            Self::Text(_) => "text",
            Self::Image(_) => "image",
            Self::ToolUse(_) => "tool_use",
            Self::ToolResult(_) => "tool_result",
            Self::Thinking(_) => "thinking",
            Self::RedactedThinking(_) => "redacted_thinking",
            Self::Other(value) => value.get("type").and_then(Value::as_str).unwrap_or(""),
        }
    }

    /// Puts a cache breakpoint on this block.
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        let value = serde_json::to_value(cache_control).unwrap_or(Value::Null);
        if let Some(extra) = self.extra_mut() {
            extra.insert("cache_control".into(), value);
        }
        self
    }

    fn extra_mut(&mut self) -> Option<&mut Extra> {
        match self {
            Self::Text(block) => Some(&mut block.extra),
            Self::Image(block) => Some(&mut block.extra),
            Self::ToolUse(block) => Some(&mut block.extra),
            Self::ToolResult(block) => Some(&mut block.extra),
            Self::Thinking(block) => Some(&mut block.extra),
            Self::RedactedThinking(block) => Some(&mut block.extra),
            Self::Other(value) => value.as_object_mut(),
        }
    }
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        fn tagged<T: Serialize>(kind: &str, block: &T) -> Result<Value, String> {
            let mut value = serde_json::to_value(block).map_err(|error| error.to_string())?;
            let Some(map) = value.as_object_mut() else {
                return Err("a content block must serialize to an object".into());
            };
            map.insert("type".into(), Value::String(kind.into()));
            Ok(value)
        }
        let value = match self {
            Self::Text(block) => tagged(self.kind(), block),
            Self::Image(block) => tagged(self.kind(), block),
            Self::ToolUse(block) => tagged(self.kind(), block),
            Self::ToolResult(block) => tagged(self.kind(), block),
            Self::Thinking(block) => tagged(self.kind(), block),
            Self::RedactedThinking(block) => tagged(self.kind(), block),
            Self::Other(value) => return value.serialize(serializer),
        }
        .map_err(S::Error::custom)?;
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn typed<T: DeserializeOwned, E: de::Error>(mut map: Extra) -> Result<T, E> {
            map.remove("type");
            serde_json::from_value(Value::Object(map)).map_err(E::custom)
        }
        let Value::Object(map) = Value::deserialize(deserializer)? else {
            return Err(de::Error::custom("a content block must be a JSON object"));
        };
        let Some(Value::String(kind)) = map.get("type") else {
            return Err(de::Error::custom("a content block needs a string `type`"));
        };
        Ok(match kind.as_str() {
            "text" => Self::Text(typed(map)?),
            "image" => Self::Image(typed(map)?),
            "tool_use" => Self::ToolUse(typed(map)?),
            "tool_result" => Self::ToolResult(typed(map)?),
            "thinking" => Self::Thinking(typed(map)?),
            "redacted_thinking" => Self::RedactedThinking(typed(map)?),
            _ => Self::Other(Value::Object(map)),
        })
    }
}

/// Message (or system prompt, or tool result) content: a string or a list of blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A message of the request's `messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: Role,
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

impl InputMessage {
    pub fn new(role: Role, content: MessageContent) -> Self {
        Self {
            role,
            content,
            extra: Extra::new(),
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self::new(Role::User, MessageContent::Text(text.into()))
    }

    /// A mid-conversation system message. The API accepts it only right after a user message,
    /// as the last message or directly before an assistant reply; it carries operator
    /// instructions, so it must hold service-generated text only, never user input, tool
    /// results or document content (docs/architecture/ai-assistant.md, 「對話歷史只往後加」).
    pub fn system_text(text: impl Into<String>) -> Self {
        Self::new(Role::System, MessageContent::Text(text.into()))
    }

    /// The assistant turn of a response, to append to the history: its content blocks
    /// (thinking with signatures, tool calls, and every other block) exactly as received.
    pub fn from_response(message: &Message) -> Self {
        Self::new(
            Role::Assistant,
            MessageContent::Blocks(message.content.clone()),
        )
    }
}

/// A tool definition. Client tools have `input_schema`; server tools (`type` such as
/// `web_search_20250305`) keep their fields in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Tool {
    /// Puts a cache breakpoint after this tool definition.
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        let value = serde_json::to_value(cache_control).unwrap_or(Value::Null);
        self.extra.insert("cache_control".into(), value);
        self
    }
}

/// The body of `POST /v1/messages`. `stream` is set by the client; `thinking`,
/// `output_config`, `tool_choice`, `metadata`, top-level `cache_control` (automatic caching)
/// and everything else go in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<InputMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Why the model stopped
/// (<https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons>).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    /// A value added after this crate was written, kept as received.
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::ToolUse => "tool_use",
            Self::PauseTurn => "pause_turn",
            Self::Refusal => "refusal",
            Self::ModelContextWindowExceeded => "model_context_window_exceeded",
            Self::Other(other) => other,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for StopReason {
    fn from(value: String) -> Self {
        match value.as_str() {
            "end_turn" => Self::EndTurn,
            "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "tool_use" => Self::ToolUse,
            "pause_turn" => Self::PauseTurn,
            "refusal" => Self::Refusal,
            "model_context_window_exceeded" => Self::ModelContextWindowExceeded,
            _ => Self::Other(value),
        }
    }
}

impl Serialize for StopReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// `stop_details`: set on a refusal, `null` otherwise
/// (<https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback>).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopDetails {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The policy area, such as `cyber`; `null` when the refusal has no named category.
    #[serde(default)]
    pub category: Option<String>,
    /// Text for people; not stable, never parse it.
    #[serde(default)]
    pub explanation: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `usage.cache_creation`: cache writes by lifetime.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One attempt in `usage.iterations` (server-side fallback, compaction). Each attempt is
/// billed at the rates of its own model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UsageIteration {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl UsageIteration {
    pub fn tokens(&self) -> TokenUsage {
        token_usage(
            self.input_tokens,
            self.output_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
            self.cache_creation.as_ref(),
        )
    }
}

/// `usage` of a message, or of a `message_start` / `message_delta` stream event. A field the
/// provider did not send is `None`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input after the last cache breakpoint (not the whole input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output, thinking included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Cache writes of both lifetimes; the sum of `cache_creation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<UsageIteration>>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Usage {
    /// The top-level counts by billing category. With `iterations`, these describe only the
    /// attempt that produced the returned message; bill from [`Usage::attempts`]. A count the
    /// provider did not send is 0 and flagged (`TokenUsage::input_missing`, `output_missing`).
    pub fn tokens(&self) -> TokenUsage {
        token_usage(
            self.input_tokens,
            self.output_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
            self.cache_creation.as_ref(),
        )
    }

    /// What to bill: one entry per attempt in `iterations` (with its model) when the provider
    /// sent them, otherwise the top-level counts with no model (the requested one). An attempt
    /// whose input or output count is missing is flagged in its [`TokenUsage`]; a message
    /// without any `usage` gives one attempt with both flagged.
    pub fn attempts(&self) -> Vec<(Option<&str>, TokenUsage)> {
        match &self.iterations {
            Some(iterations) if !iterations.is_empty() => iterations
                .iter()
                .map(|iteration| (iteration.model.as_deref(), iteration.tokens()))
                .collect(),
            _ => vec![(None, self.tokens())],
        }
    }

    /// Applies a later `usage` of the same stream. Stream counts are cumulative, so every
    /// field the later one carries replaces the earlier value
    /// (<https://platform.claude.com/docs/en/build-with-claude/streaming#event-types>).
    pub fn update(&mut self, later: &Usage) {
        fn take<T: Clone>(slot: &mut Option<T>, later: &Option<T>) {
            if later.is_some() {
                slot.clone_from(later);
            }
        }
        take(&mut self.input_tokens, &later.input_tokens);
        take(&mut self.output_tokens, &later.output_tokens);
        take(
            &mut self.cache_creation_input_tokens,
            &later.cache_creation_input_tokens,
        );
        take(
            &mut self.cache_read_input_tokens,
            &later.cache_read_input_tokens,
        );
        take(&mut self.cache_creation, &later.cache_creation);
        take(&mut self.iterations, &later.iterations);
        for (key, value) in &later.extra {
            self.extra.insert(key.clone(), value.clone());
        }
    }
}

fn token_usage(
    input: Option<u64>,
    output: Option<u64>,
    cache_creation_total: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<&CacheCreation>,
) -> TokenUsage {
    let total_writes = cache_creation_total.unwrap_or(0);
    let (mut write_5m, write_1h) = cache_creation
        .map(|split| {
            (
                split.ephemeral_5m_input_tokens.unwrap_or(0),
                split.ephemeral_1h_input_tokens.unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    let split_sum = write_5m.saturating_add(write_1h);
    // The documented invariant is total == 5m + 1h. Writes the split does not cover are
    // counted at the default (5-minute) lifetime and flagged, never dropped.
    let assumed = total_writes > split_sum;
    if assumed {
        write_5m = write_5m.saturating_add(total_writes - split_sum);
    }
    // `input_tokens` and `output_tokens` are required in every usage object; without them
    // (a proxy stripped `usage`) the count is unknown, not zero, and the ledger estimates it.
    // The cache counts are nullable in the API, so an absent one is 0.
    TokenUsage {
        input: input.unwrap_or(0),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
        cache_read: cache_read.unwrap_or(0),
        output: output.unwrap_or(0),
        cache_write_ttl_assumed: assumed,
        input_missing: input.is_none(),
        output_missing: output.is_none(),
    }
}

/// A response of `POST /v1/messages`, or the message a stream builds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(flatten)]
    pub extra: Extra,
}

/// What a completed response means for the caller and the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A usable answer (also a tool call, a pause, or text cut at `max_tokens`).
    Completed,
    /// The model declined (`stop_reason: refusal`). Billed like any response; the caller
    /// shows a generic message (J2). Partial output before a mid-stream refusal is discarded.
    Refusal { category: Option<String> },
    /// `stop_reason: max_tokens` with no text: thinking used up the limit.
    MaxTokensWithoutText,
}

impl Message {
    /// Whether any `text` block has non-whitespace text.
    pub fn has_text(&self) -> bool {
        self.content.iter().any(|block| match block {
            ContentBlock::Text(text) => !text.text.trim().is_empty(),
            _ => false,
        })
    }

    pub fn outcome(&self) -> Outcome {
        match self.stop_reason {
            Some(StopReason::Refusal) => Outcome::Refusal {
                category: self
                    .stop_details
                    .as_ref()
                    .and_then(|details| details.category.clone()),
            },
            Some(StopReason::MaxTokens) if !self.has_text() => Outcome::MaxTokensWithoutText,
            _ => Outcome::Completed,
        }
    }
}
