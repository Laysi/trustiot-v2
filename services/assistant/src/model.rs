//! The model behind one interface (decision 15): the agent loop, the history and the tools see
//! only the types here. A provider's own format lives in one submodule ([`anthropic`]); messages
//! in that format pass through the rest of the service as opaque [`NativeMessage`]s, which are
//! kept and sent back as received. The OpenAI-compatible implementation for on-prem
//! deployments comes after the SaaS work (docs/architecture/ai-assistant.md, 「地端」).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use contract::TenantId;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use uuid::Uuid;

use crate::history::Preamble;
use crate::tools::ToolOutput;

pub mod anthropic;

/// A boxed future, for the object-safe traits of this crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How hard the model thinks (`output_config.effort`). Fixed for a conversation: changing it
/// invalidates the cached history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The effort called `name` (see [`Effort::as_str`]).
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max]
            .into_iter()
            .find(|effort| effort.as_str() == name)
    }
}

/// The lifetime of the cache breakpoints (docs/architecture/ai-assistant.md, 「Prompt
/// caching」 point 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum CacheTtl {
    #[default]
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// The lifetime as the configuration writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    /// The lifetime called `name` (see [`CacheTtl::as_str`]).
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::FiveMinutes, Self::OneHour]
            .into_iter()
            .find(|ttl| ttl.as_str() == name)
    }
}

/// The native format a [`NativeMessage`] is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// An element of `messages` in Anthropic Messages.
    AnthropicMessages,
}

impl Format {
    /// The format's name where a message is stored ([`crate::store`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    /// The format called `name` (see [`Format::as_str`]).
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::AnthropicMessages]
            .into_iter()
            .find(|format| format.as_str() == name)
    }
}

/// One message of the history in its model's own format, as the JSON text received or first
/// written, and sent back byte for byte (docs/architecture/ai-assistant.md, 「對話歷史只往後加」).
/// It holds chat content, so `Debug` shows only its format and size.
#[derive(Clone)]
pub struct NativeMessage {
    format: Format,
    json: Box<RawValue>,
}

impl NativeMessage {
    pub(crate) fn new(format: Format, json: Box<RawValue>) -> Self {
        Self { format, json }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// The message as JSON text, exactly as it is sent.
    pub fn json(&self) -> &str {
        self.json.get()
    }

    pub(crate) fn raw(&self) -> &RawValue {
        &self.json
    }

    /// The text the chat shows of the message: the user's text of a question, the text blocks
    /// of an answer; never thinking, tool calls or their results. `None` when the message does
    /// not decode in its format.
    pub fn shown_text(&self) -> Option<String> {
        match self.format {
            Format::AnthropicMessages => anthropic::shown_text(self.json()),
        }
    }
}

impl fmt::Debug for NativeMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeMessage")
            .field("format", &self.format)
            .field("bytes", &self.json.get().len())
            .finish()
    }
}

/// One model call: the conversation's fixed preamble, its history, and messages this call adds
/// that join the history only when the call succeeds (a wind-down note: a mid-conversation
/// system message must be followed by the model's reply, so it cannot stay behind after a
/// failed call).
pub struct ModelRequest<'a> {
    /// Unique per call: the key the AI gateway's ledger will deduplicate on (`llm_call_id`,
    /// decision 28). Laysi/trustiot-v2#140 decides how it reaches the gateway; until then it
    /// only correlates logs.
    pub call_id: Uuid,
    pub tenant: TenantId,
    pub preamble: &'a Preamble,
    pub history: &'a [NativeMessage],
    pub pending: &'a [NativeMessage],
    pub max_output_tokens: u32,
}

/// A tool call the model asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The JSON the model wrote; the tool checks it.
    pub input: Value,
}

/// A tool call's result, to hand back to the model.
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A finished answer.
    EndTurn,
    /// The model asked for tool calls.
    ToolUse,
    /// The response hit `max_tokens`.
    MaxTokens,
    /// The model declined (safety classifiers); billed like any answer.
    Refusal,
    /// The conversation no longer fits the model's context window.
    ContextWindowExceeded,
    /// Anything else (`pause_turn`, which only server tools cause, or a value added later).
    Other(String),
}

/// Tokens one call used, by billing category, summed over the provider's attempts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenCount {
    /// Input after the last cache breakpoint.
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    /// Output, thinking included.
    pub output: u64,
    /// The provider did not report input or output, and the missing part is estimated from
    /// the size of the request or response (never counted as 0).
    pub estimated: bool,
}

impl TokenCount {
    /// Every token the call processed: what the budget of a question counts.
    pub fn processed(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }

    pub(crate) fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
        self.estimated |= other.estimated;
    }
}

/// A completed call.
#[derive(Debug)]
pub struct ModelTurn {
    /// The model's message for the history; `None` when it has no content (a refusal), since
    /// the API does not accept an empty message back.
    pub message: Option<NativeMessage>,
    /// The text of the message's text blocks, joined.
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop: Stop,
    pub usage: TokenCount,
    /// The size of the request and this reply together, as the answering model counted them
    /// (its input, cache writes and reads, and output): what the history weighs once the reply
    /// joins it. `None` when the usage lacks a count.
    pub context_tokens: Option<u64>,
    /// The model that answered (`ai-gateway` may have fallen back to another one).
    pub model: String,
}

/// Why a call failed. Carries no provider or gateway message text: those can quote the request
/// and never reach the user, the model or the logs (J2).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("model call failed: {kind}")]
pub struct ModelError {
    pub kind: ModelErrorKind,
    /// The HTTP status of the gateway's answer, if there was one.
    pub status: Option<u16>,
    /// The error `type` of the gateway's answer (such as `overloaded_error`), when it is a
    /// short identifier.
    pub error_type: Option<String>,
}

impl ModelError {
    pub fn new(kind: ModelErrorKind) -> Self {
        Self {
            kind,
            status: None,
            error_type: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelErrorKind {
    /// No answer in time: the gateway's own time limit (504) or the assistant's.
    Timeout,
    /// The gateway or every model of the role is unavailable (429, 529, 5xx, connection).
    Unavailable,
    /// The gateway refused the request (4xx): a bug or a misconfigured role, not retried.
    Rejected,
    /// An answer that is not a usable message.
    InvalidResponse,
}

impl ModelErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Rejected => "rejected",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

impl fmt::Display for ModelErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A message could not be written in the model's format.
#[derive(Debug, thiserror::Error)]
#[error("could not encode a message: {0}")]
pub struct EncodeError(#[from] serde_json::Error);

/// A model behind its native protocol. Every implementation keeps the messages it returns and
/// receives byte for byte; the agent loop never looks inside them.
pub trait ModelClient: Send + Sync {
    fn format(&self) -> Format;

    /// The user's question.
    fn user_text(&self, text: &str) -> Result<NativeMessage, EncodeError>;

    /// A service-generated instruction appended mid-conversation (Anthropic's mid-conversation
    /// system message). Never user input, tool output or document text.
    fn system_note(&self, text: &str) -> Result<NativeMessage, EncodeError>;

    /// The results of one turn's tool calls, in one message, in the order of the calls.
    fn tool_results(&self, results: &[ToolResult]) -> Result<NativeMessage, EncodeError>;

    /// `message`, a model turn in this format, with the text of each text block replaced by
    /// `rewrite(text)` where that returns `Some` (the answer guard, [`crate::answer_guard`]). A
    /// block whose new text is blank is dropped, and when no text block with more than blanks is
    /// left, one with `fallback` ends the message, since the history needs text. Every other block (thinking
    /// with its signature, tool calls) stays byte for byte. `Ok(None)` when no block changed:
    /// the caller keeps `message` as it is.
    fn rewrite_text(
        &self,
        message: &NativeMessage,
        rewrite: &mut dyn FnMut(&str) -> Option<String>,
        fallback: &str,
    ) -> Result<Option<NativeMessage>, EncodeError>;

    /// A high estimate of the input tokens `message` adds to a request, for the budget check
    /// before a call. It errs high: a low estimate lets a call cross the budget.
    fn estimate_tokens(&self, message: &NativeMessage) -> u64;

    /// The same for the fixed start of every request: the tools and the system blocks.
    fn estimate_preamble_tokens(&self, preamble: &Preamble) -> u64;

    /// One call. The caller bounds it in time.
    fn call<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>>;
}
