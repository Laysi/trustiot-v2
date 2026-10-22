//! Errors of every provider client and the classes the AI gateway acts on (fallback, retry,
//! ledger). The full list of situations and their classes is in this crate's README.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::sse::SseError;
use crate::usage::ReportedUsage;

/// Which time limit ran out ([`crate::Timeouts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// The TCP (and TLS) connection was not established in time.
    Connect,
    /// A streaming response produced no model output (no content block event) in time,
    /// counted from sending the request; queueing at the provider shows up here.
    FirstToken,
    /// A streaming response went silent (no bytes, not even a ping) for too long.
    StreamIdle,
    /// A streaming response did not finish within its total time limit, counted from
    /// sending the request (`Timeouts::stream_total`), even though data kept arriving.
    StreamTotal,
    /// A non-streaming response (headers and body) did not complete in time.
    Response,
    /// The provider reported that it timed out (Anthropic HTTP 504 `timeout_error`).
    Provider,
}

/// What the gateway does about an error. `RateLimited`, `Overloaded` and `Timeout` are the
/// conditions that move a model role to its next fallback model (docs/architecture/ai-assistant.md,
/// 「AI 閘道」 item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// HTTP 429. `retry_after` is the provider's `retry-after` header; Anthropic sends none
    /// when the organization reached its monthly spend cap, and that 429 keeps failing.
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Anthropic HTTP 529 or an `overloaded_error` event mid-stream; OpenAI HTTP 503.
    Overloaded,
    Timeout(TimeoutKind),
    /// This endpoint's concurrency limit was reached; the request was not sent.
    ConcurrencyLimited,
    /// The caller cancelled the stream.
    Cancelled,
    /// Everything else: invalid requests (4xx), authentication, provider errors (5xx),
    /// connection failures, and responses that do not follow the protocol.
    Other,
}

/// The `error` object of a provider error body or of an Anthropic `error` stream event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiErrorBody {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// OpenAI's machine-readable code, such as `insufficient_quota`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// An error the provider reported, with what the gateway needs to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status; `None` for an error event in a stream.
    pub status: Option<u16>,
    pub body: ApiErrorBody,
    pub request_id: Option<String>,
    /// The `retry-after` (or `retry-after-ms`) header.
    pub retry_after: Option<Duration>,
    pub class: ErrorClass,
}

/// How many characters of provider text an error message shows. The full text stays in the
/// structured field ([`ApiErrorBody::message`]); error strings end up in logs.
pub(crate) const MAX_MESSAGE_CHARS: usize = 512;

/// At most [`MAX_MESSAGE_CHARS`] characters of `text`, with `…` when it was cut.
pub(crate) fn truncated(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().nth(MAX_MESSAGE_CHARS).is_none() {
        return text.into();
    }
    let mut cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    cut.push('…');
    cut.into()
}

/// A serde_json error without its message text: serde quotes the offending value (`invalid
/// type: string "…"`, `unknown variant …`), which can be model output or tenant data. Only the
/// category and the position remain, plus a missing field name, which comes from our own types.
pub(crate) fn json_error(error: &serde_json::Error) -> String {
    let text = error.to_string();
    let mut out = match error.classify() {
        serde_json::error::Category::Io => "unreadable".to_owned(),
        serde_json::error::Category::Syntax => "invalid JSON".to_owned(),
        serde_json::error::Category::Eof => "truncated JSON".to_owned(),
        serde_json::error::Category::Data => match text.strip_prefix("missing field ") {
            Some(rest) => format!(
                "missing field {}",
                rest.split_whitespace().next().unwrap_or_default()
            ),
            None => "unexpected JSON content".to_owned(),
        },
    };
    if error.line() > 0 {
        out.push_str(&format!(
            " at line {} column {}",
            error.line(),
            error.column()
        ));
    }
    out
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(status) = self.status {
            write!(f, "HTTP {status} ")?;
        }
        write!(
            f,
            "({}) {}",
            truncated(self.body.error_type.as_deref().unwrap_or("no error type")),
            truncated(&self.body.message)
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The endpoint's concurrency limit was reached and no slot freed up within the queue
    /// wait; nothing was sent.
    #[error("the concurrency limit of AI endpoint {endpoint} is reached")]
    ConcurrencyLimited { endpoint: String },
    /// The provider answered with an error status.
    #[error("AI provider answered with an error: {0}")]
    Api(Box<ApiError>),
    /// An Anthropic `error` event arrived in the middle of a stream (after HTTP 200).
    #[error("AI provider sent an error event mid-stream: {0}")]
    StreamError(Box<ApiError>),
    #[error("AI provider request timed out ({0:?})")]
    Timeout(TimeoutKind),
    /// Connecting, sending or reading failed for a reason other than a timeout.
    #[error("AI provider connection failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// The response does not follow the provider's protocol: invalid JSON, a stream that ends
    /// before its last event, events out of order, a stream over its size limit, or (from
    /// `DecisionResponse::parse` and `validate_against` called directly) a Decisions answer
    /// that does not fit the question. The clients turn a non-streaming HTTP 200 body that
    /// cannot be used into [`Error::InvalidResponse`] instead. The message never quotes the
    /// response.
    #[error("AI provider response does not follow the protocol: {0}")]
    Protocol(String),
    /// The provider answered HTTP 200, so it billed the call, but the body cannot be used: it
    /// is over the size limit, does not parse, or (Decisions) does not fit the questions.
    /// `usage` is what could be read from it, for the ledger; `reason` never quotes the body.
    #[error("AI provider answered HTTP 200 with an unusable response: {reason}")]
    InvalidResponse {
        reason: String,
        usage: ReportedUsage,
        request_id: Option<String>,
    },
    #[error("AI provider stream: {0}")]
    Sse(#[from] SseError),
    #[error("the stream was cancelled")]
    Cancelled,
    /// The request was rejected before sending (for example a Decisions request without
    /// question names).
    #[error("invalid AI request: {0}")]
    InvalidRequest(String),
}

impl Error {
    pub fn class(&self) -> ErrorClass {
        match self {
            Error::ConcurrencyLimited { .. } => ErrorClass::ConcurrencyLimited,
            Error::Api(error) | Error::StreamError(error) => error.class,
            Error::Timeout(kind) => ErrorClass::Timeout(*kind),
            Error::Cancelled => ErrorClass::Cancelled,
            Error::Transport(_)
            | Error::Protocol(_)
            | Error::InvalidResponse { .. }
            | Error::Sse(_)
            | Error::InvalidRequest(_) => ErrorClass::Other,
        }
    }

    /// The provider's request ID, for support tickets and logs.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Api(error) | Error::StreamError(error) => error.request_id.as_deref(),
            Error::InvalidResponse { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    /// Maps a transport error: a connect timeout is [`TimeoutKind::Connect`]; anything else
    /// stays a transport error.
    /// The usage the provider reported for a billed response that could not be used
    /// ([`Error::InvalidResponse`]); `None` for every other error, whose usage (if any) the
    /// gateway estimates (a stream's partial usage is in `MessageStream::message`).
    pub fn reported_usage(&self) -> Option<&ReportedUsage> {
        match self {
            Error::InvalidResponse { usage, .. } => Some(usage),
            _ => None,
        }
    }

    /// Turns an error from reading or checking the body of an HTTP 200 response into
    /// [`Error::InvalidResponse`]; timeouts and transport errors stay as they are.
    pub(crate) fn into_invalid_response(
        self,
        usage: impl FnOnce() -> ReportedUsage,
        request_id: Option<String>,
    ) -> Self {
        match self {
            Error::Protocol(reason) => Error::InvalidResponse {
                reason,
                usage: usage(),
                request_id,
            },
            other => other,
        }
    }

    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_connect() && error.is_timeout() {
            Error::Timeout(TimeoutKind::Connect)
        } else {
            Error::Transport(error)
        }
    }
}
