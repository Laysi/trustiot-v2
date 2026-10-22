//! Anthropic Messages (`POST /v1/messages`): the main agent and the worker (decision 15).

mod stream;
mod types;

use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;
use http::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
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

use crate::error::{ApiErrorBody, Error, ErrorClass, TimeoutKind, json_error};
use crate::transport::{
    ApiKey, EndpointConfig, Tls, Transport, error_from_response, header_text, read_body,
};
use crate::usage::ReportedUsage;
use crate::{RawReply, Reply};

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
    /// `tls` decides which certificates the endpoint may present; the client adds its own
    /// connect timeout, keepalive and no-redirect policy.
    pub fn new(config: EndpointConfig, api_key: ApiKey, tls: &Tls) -> Result<Self, Error> {
        Ok(Self {
            transport: Arc::new(Transport::new(config, tls)?),
            api_key: Arc::new(api_key),
        })
    }

    /// The request body: `request` with `stream` set by the method, never by the caller.
    fn encode(request: &MessagesRequest, stream: bool) -> Result<Bytes, Error> {
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
        serde_json::to_vec(&body)
            .map(Bytes::from)
            .map_err(|error| Error::InvalidRequest(format!("request body: {error}")))
    }

    fn request(
        &self,
        stream: bool,
        options: &RequestOptions,
    ) -> Result<http::request::Builder, Error> {
        let mut builder = self
            .transport
            .post(MESSAGES_PATH)?
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
            );
        if !options.betas.is_empty() {
            let betas = HeaderValue::from_str(&options.betas.join(","))
                .map_err(|_| Error::InvalidRequest("invalid anthropic-beta value".into()))?;
            builder = builder.header("anthropic-beta", betas);
        }
        Ok(builder)
    }

    /// A non-streaming call. Headers and body must arrive within `timeouts.response`. An HTTP
    /// 200 body over the size limit or not a message is [`Error::InvalidResponse`], with the
    /// `usage` it carries if that much could be read.
    pub async fn create_message(
        &self,
        request: &MessagesRequest,
        options: &RequestOptions,
    ) -> Result<Reply<Message>, Error> {
        let body = Self::encode(request, false)?;
        let RawReply {
            body, request_id, ..
        } = self.create(body, options).await?;
        Ok(Reply { body, request_id })
    }

    /// [`AnthropicClient::create_message`] with a request body the caller already encoded,
    /// sent byte for byte: for a gateway that forwards a caller's request unchanged. The body
    /// must be a Messages request without `"stream": true` (the caller checks; this method does
    /// not parse it). The reply holds the response body as received and parsed.
    pub async fn create_message_raw(
        &self,
        body: Bytes,
        options: &RequestOptions,
    ) -> Result<RawReply<Message>, Error> {
        self.create(body, options).await
    }

    async fn create(
        &self,
        body: Bytes,
        options: &RequestOptions,
    ) -> Result<RawReply<Message>, Error> {
        let permit = self.transport.acquire().await?;
        let deadline = Instant::now() + self.transport.config.timeouts.response;
        let builder = self.request(false, options)?;
        let response = self
            .transport
            .send(builder, body, deadline, TimeoutKind::Response)
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
        let request_id = header_text(response.headers(), REQUEST_ID_HEADER);
        let body = read_body(
            &mut response.into_body(),
            self.transport.config.max_response_bytes,
            deadline,
            TimeoutKind::Response,
        )
        .await
        .map_err(|error| {
            error.into_invalid_response(|| ReportedUsage::Unknown, request_id.clone())
        })?;
        drop(permit);
        let message = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(error) => {
                return Err(Error::InvalidResponse {
                    reason: format!("message: {}", json_error(&error)),
                    usage: usage_of_unparsed(&body),
                    request_id,
                });
            }
        };
        Ok(RawReply {
            body: message,
            raw: Bytes::from(body),
            request_id,
        })
    }

    /// A streaming call. The response headers and the first model output must arrive within
    /// `timeouts.first_token`; after that, no gap between bytes may exceed
    /// `timeouts.stream_idle`, and the whole stream must end within `timeouts.stream_total`
    /// and `max_response_bytes`.
    pub async fn stream_message(
        &self,
        request: &MessagesRequest,
        options: &RequestOptions,
    ) -> Result<MessageStream, Error> {
        let body = Self::encode(request, true)?;
        self.stream(body, options).await
    }

    /// [`AnthropicClient::stream_message`] with a request body the caller already encoded,
    /// sent byte for byte. The body must be a Messages request with `"stream": true` (the
    /// caller checks; this method does not parse it). [`StreamItem::raw`] holds each event as
    /// received.
    pub async fn stream_message_raw(
        &self,
        body: Bytes,
        options: &RequestOptions,
    ) -> Result<MessageStream, Error> {
        self.stream(body, options).await
    }

    async fn stream(&self, body: Bytes, options: &RequestOptions) -> Result<MessageStream, Error> {
        let permit = self.transport.acquire().await?;
        let timeouts = &self.transport.config.timeouts;
        let sent = Instant::now();
        let first_token_deadline = sent + timeouts.first_token;
        let builder = self.request(true, options)?;
        let response = self
            .transport
            .send(builder, body, first_token_deadline, TimeoutKind::FirstToken)
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
        let is_event_stream = header_text(response.headers(), CONTENT_TYPE.as_str())
            .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));
        if !is_event_stream {
            return Err(Error::Protocol(
                "a streaming response without content type text/event-stream".into(),
            ));
        }
        let request_id = header_text(response.headers(), REQUEST_ID_HEADER);
        let limits = StreamLimits {
            first_token_deadline,
            idle: timeouts.stream_idle,
            total_deadline: sent + timeouts.stream_total,
            max_event_bytes: self.transport.config.max_event_bytes,
            max_total_bytes: self.transport.config.max_response_bytes,
        };
        Ok(
            MessageStream::from_source(Box::new(response.into_body()), limits)
                .with_permit(permit, request_id),
        )
    }
}

/// The `usage` of a body that does not parse as a [`Message`], read on its own so the ledger
/// gets the real counts of a billed response.
fn usage_of_unparsed(body: &[u8]) -> ReportedUsage {
    #[derive(serde::Deserialize)]
    struct UsageOnly {
        #[serde(default)]
        usage: Option<Usage>,
    }
    match serde_json::from_slice::<UsageOnly>(body) {
        Ok(UsageOnly { usage: Some(usage) }) => ReportedUsage::Attempts(
            usage
                .attempts()
                .into_iter()
                .map(|(model, tokens)| (model.map(str::to_owned), tokens))
                .collect(),
        ),
        _ => ReportedUsage::Unknown,
    }
}
