//! Native-protocol clients for the AI providers (decision 15): request and response types,
//! stream parsing, usage by billing category and error classification, shared by the AI
//! gateway (Laysi/trustiot-v2#139) and the services that build requests and read responses.
//!
//! - [`anthropic`]: Anthropic Messages, streaming and not.
//! - [`decisions`]: OpenAI Decisions, predicate and score questions.
//!
//! The error situations and their classes, and what each test proves, are in `README.md`.
//! This crate must not depend on `crates/contract` (decision 28; `tests/no_contract.rs`).

pub mod anthropic;
pub mod decisions;
mod error;
pub mod sse;
mod transport;
mod usage;

pub use error::{ApiError, ApiErrorBody, Error, ErrorClass, TimeoutKind};
pub use transport::{ApiKey, ByteSource, ChunkFuture, EndpointConfig, Timeouts};
pub use usage::{ReportedUsage, TokenUsage};

/// A response body with the provider's request ID (for logs and support tickets).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply<T> {
    pub body: T,
    pub request_id: Option<String>,
}
