//! Anthropic Messages (`POST /v1/messages`): the main agent and the worker (decision 15).

mod stream;
mod types;

use std::sync::Arc;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use tokio::time::Instant;

pub use stream::{
    Delta, MessageAccumulator, MessageDeltaBody, MessageStream, StreamEvent, StreamItem,
    StreamLimits,
};
pub use types::{
    CacheControl, CacheCreation, ContentBlock, Extra, ImageBlock, InputMessage, Message,
    MessageContent, MessagesRequest, Outcome, RedactedThinkingBlock, Role, StopDetails, StopReason,
    TextBlock, ThinkingBlock, Tool, ToolResultBlock, ToolUseBlock, Usage, UsageIteration,
};

use crate::Reply;
use crate::error::{ApiErrorBody, Error, ErrorClass, TimeoutKind};
use crate::transport::{
    ApiKey, EndpointConfig, Transport, error_from_response, header_text, read_body,
};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The `anthropic-version` header (<https://platform.claude.com/docs/en/api/versioning>).
pub const API_VERSION: &str = "2023-06-01";
const MESSAGES_PATH: &str = "/v1/messages";
const REQUEST_ID_HEADER: &str = "request-id";

/// Classifies an Anthropic error by HTTP status (absent for a mid-stream `error` event) and
/// error `type` (<https://platform.claude.com/docs/en/api/errors>).
pub fn classify_error(status: Option<u16>, error_type: Option<&str>) -> ErrorClass {
    match (status, error_type) {
        (Some(429), _) | (_, Some("rate_limit_error")) => {
            ErrorClass::RateLimited { retry_after: None }
        }
        (Some(529), _) | (_, Some("overloaded_error")) => ErrorClass::Overloaded,
        (Some(504), _) | (_, Some("timeout_error")) => ErrorClass::Timeout(TimeoutKind::Provider),
        _ => ErrorClass::Other,
    }
}

fn classify_response(status: StatusCode, body: &ApiErrorBody) -> ErrorClass {
    classify_error(Some(status.as_u16()), body.error_type.as_deref())
}

/// Per-request options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// `anthropic-beta` values, such as a per-message effort beta.
    pub betas: Vec<String>,
}

/// A client for one Anthropic endpoint (an API key at a base URL). Clones share the
/// concurrency limit.
#[derive(Clone)]
pub struct AnthropicClient {
    transport: Arc<Transport>,
    api_key: Arc<ApiKey>,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("endpoint", &self.transport.config.name)
            .finish_non_exhaustive()
    }
}

impl AnthropicClient {
    /// `http` brings the TLS backend and trust anchors; the client adds its own connect
    /// timeout, keepalive and no-redirect policy. Do not set timeouts on it.
    pub fn new(
        config: EndpointConfig,
        api_key: ApiKey,
        http: reqwest::ClientBuilder,
    ) -> Result<Self, Error> {
        Ok(Self {
            transport: Arc::new(Transport::new(config, http)?),
            api_key: Arc::new(api_key),
        })
    }

    fn request(
        &self,
        request: &MessagesRequest,
        stream: bool,
        options: &RequestOptions,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut body = serde_json::to_value(request)
            .map_err(|error| Error::InvalidRequest(format!("request body: {error}")))?;
        let Some(map) = body.as_object_mut() else {
            return Err(Error::InvalidRequest(
                "the request is not a JSON object".into(),
            ));
        };
        if stream {
            map.insert("stream".into(), true.into());
        } else {
            map.remove("stream");
        }
        let body = serde_json::to_vec(&body)
            .map_err(|error| Error::InvalidRequest(format!("request body: {error}")))?;
        let mut builder = self
            .transport
            .http
            .post(self.transport.url(MESSAGES_PATH)?)
            .header("x-api-key", self.api_key.header_value("")?)
            .header("anthropic-version", API_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .header(
                ACCEPT,
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(body);
        if !options.betas.is_empty() {
            let betas = HeaderValue::from_str(&options.betas.join(","))
                .map_err(|_| Error::InvalidRequest("invalid anthropic-beta value".into()))?;
            builder = builder.header("anthropic-beta", betas);
        }
        Ok(builder)
    }

    /// A non-streaming call. Headers and body must arrive within `timeouts.response`.
    pub async fn create_message(
        &self,
        request: &MessagesRequest,
        options: &RequestOptions,
    ) -> Result<Reply<Message>, Error> {
        let permit = self.transport.acquire().await?;
        let deadline = Instant::now() + self.transport.config.timeouts.response;
        let builder = self.request(request, false, options)?;
        let mut response = self
            .transport
            .send(builder, deadline, TimeoutKind::Response)
            .await?;
        if !response.status().is_success() {
            return Err(error_from_response(
                response,
                REQUEST_ID_HEADER,
                deadline,
                classify_response,
            )
            .await);
        }
        let request_id = header_text(&response, REQUEST_ID_HEADER);
        let body = read_body(
            &mut response,
            self.transport.config.max_response_bytes,
            deadline,
            TimeoutKind::Response,
        )
        .await?;
        drop(permit);
        let message = serde_json::from_slice(&body)
            .map_err(|error| Error::Protocol(format!("message: {error}")))?;
        Ok(Reply {
            body: message,
            request_id,
        })
    }

    /// A streaming call. The response headers and the first model output must arrive within
    /// `timeouts.first_token`; after that, no gap between bytes may exceed
    /// `timeouts.stream_idle`.
    pub async fn stream_message(
        &self,
        request: &MessagesRequest,
        options: &RequestOptions,
    ) -> Result<MessageStream, Error> {
        let permit = self.transport.acquire().await?;
        let timeouts = &self.transport.config.timeouts;
        let first_token_deadline = Instant::now() + timeouts.first_token;
        let builder = self.request(request, true, options)?;
        let response = self
            .transport
            .send(builder, first_token_deadline, TimeoutKind::FirstToken)
            .await?;
        if !response.status().is_success() {
            let deadline = Instant::now() + timeouts.response;
            return Err(error_from_response(
                response,
                REQUEST_ID_HEADER,
                deadline,
                classify_response,
            )
            .await);
        }
        let is_event_stream = header_text(&response, CONTENT_TYPE.as_str())
            .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));
        if !is_event_stream {
            return Err(Error::Protocol(
                "a streaming response without content type text/event-stream".into(),
            ));
        }
        let request_id = header_text(&response, REQUEST_ID_HEADER);
        let limits = StreamLimits {
            first_token_deadline,
            idle: timeouts.stream_idle,
            max_event_bytes: self.transport.config.max_event_bytes,
        };
        Ok(MessageStream::from_source(Box::new(response), limits).with_permit(permit, request_id))
    }
}
