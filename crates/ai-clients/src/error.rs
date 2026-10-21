//! Errors of every provider client and the classes the AI gateway acts on (fallback, retry,
//! ledger). The full list of situations and their classes is in this crate's README.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::sse::SseError;

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

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(status) = self.status {
            write!(f, "HTTP {status} ")?;
        }
        write!(
            f,
            "({}) {}",
            self.body.error_type.as_deref().unwrap_or("no error type"),
            self.body.message
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
    /// before its last event, events out of order, a body over the size limit, or a Decisions
    /// answer that does not fit the question.
    #[error("AI provider response does not follow the protocol: {0}")]
    Protocol(String),
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
            Error::Transport(_) | Error::Protocol(_) | Error::Sse(_) | Error::InvalidRequest(_) => {
                ErrorClass::Other
            }
        }
    }

    /// The provider's request ID, for support tickets and logs.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Api(error) | Error::StreamError(error) => error.request_id.as_deref(),
            _ => None,
        }
    }

    /// Maps a transport error: a connect timeout is [`TimeoutKind::Connect`]; anything else
    /// stays a transport error.
    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_connect() && error.is_timeout() {
            Error::Timeout(TimeoutKind::Connect)
        } else {
            Error::Transport(error)
        }
    }
}
