//! Claude through the AI gateway's Anthropic Messages endpoint
//! (`POST /internal/roles/{role}/v1/messages`, services/ai-gateway/README.md).
//!
//! The request is laid out for prompt caching (docs/architecture/ai-assistant.md, 「Prompt
//! caching」): tools, then the platform system prompt (breakpoint), then the tenant context
//! (breakpoint), then the conversation, cached by the top-level automatic breakpoint. The
//! gateway fills in the model of the role and forwards every other byte unchanged; the history
//! messages are written into the body as the JSON text they were stored as.

use std::time::Duration;

use ai_clients::anthropic::{
    CacheControl, ContentBlock, InputMessage, Message, MessageContent, Role, StopReason, Tool,
};
use contract::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};

use crate::history::Preamble;
use crate::model::{
    BoxFuture, CacheTtl, EncodeError, Format, ModelClient, ModelError, ModelErrorKind,
    ModelRequest, ModelTurn, NativeMessage, Stop, TokenCount, ToolCall, ToolResult,
};
use crate::tools::ToolContent;

/// The header that names the tenant a call is for.
pub const TENANT_HEADER: &str = "ai-gateway-tenant";
/// The header with the model that answered.
const MODEL_HEADER: &str = "ai-gateway-model";
/// Connecting to the gateway inside the cluster.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A rough token estimate for a count the provider did not report: three bytes per token is
/// on the high side for English and about right for Chinese, so the budget errs towards
/// stopping early.
const BYTES_PER_TOKEN_ESTIMATE: u64 = 3;
/// The estimate before a call, of the JSON a message adds: two bytes per token is high for
/// both English and Chinese (and the JSON's own syntax adds bytes), since a low estimate would
/// let a call cross the budget.
const BYTES_PER_TOKEN_BEFORE_A_CALL: u64 = 2;
/// The estimate for one image: high-resolution models count up to 4784 tokens for the largest
/// image they take, 2576 px on the long edge
/// (<https://platform.claude.com/docs/en/build-with-claude/vision>).
const TOKENS_PER_IMAGE_ESTIMATE: u64 = 5_000;
/// The longest error `type` taken from a gateway answer.
const MAX_ERROR_TYPE_CHARS: usize = 64;

/// The gateway client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum GatewayConfigError {
    #[error("the AI gateway URL is invalid: {0}")]
    Url(&'static str),
    #[error("the HTTP client could not be built")]
    Client(#[source] reqwest::Error),
}

/// Calls one model role of the AI gateway with Anthropic Messages.
#[derive(Clone, Debug)]
pub struct AnthropicGateway {
    http: reqwest::Client,
    endpoint: String,
    max_response_bytes: usize,
}

impl AnthropicGateway {
    /// `base_url` is the gateway's plain `http` origin and `role` a role name, both checked by
    /// the configuration.
    pub fn new(
        base_url: &str,
        role: &str,
        max_response_bytes: usize,
    ) -> Result<Self, GatewayConfigError> {
        crate::config::check_gateway_url(base_url).map_err(GatewayConfigError::Url)?;
        if !crate::config::is_role_name(role) {
            return Err(GatewayConfigError::Url("the role name is invalid"));
        }
        let http = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            // Inside the mesh: no proxy from the environment, no redirects.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(GatewayConfigError::Client)?;
        Ok(Self {
            http,
            endpoint: format!(
                "{}/internal/roles/{role}/v1/messages",
                base_url.trim_end_matches('/')
            ),
            max_response_bytes,
        })
    }

    async fn send(&self, request: ModelRequest<'_>) -> Result<ModelTurn, ModelError> {
        let body = request_body(&request).map_err(|_| ModelError::new(ModelErrorKind::Rejected))?;
        let request_bytes = body.len();
        let response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(TENANT_HEADER, tenant_header(request.tenant))
            .body(body)
            .send()
            .await
            .map_err(|error| {
                ModelError::new(if error.is_timeout() {
                    ModelErrorKind::Timeout
                } else {
                    ModelErrorKind::Unavailable
                })
            })?;
        let status = response.status();
        let answered_by = response
            .headers()
            .get(MODEL_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_limited(response, self.max_response_bytes)
            .await
            .map_err(|kind| ModelError {
                kind,
                status: Some(status.as_u16()),
                error_type: None,
            })?;
        if !status.is_success() {
            return Err(ModelError {
                kind: error_kind(status.as_u16()),
                status: Some(status.as_u16()),
                error_type: error_type(&body),
            });
        }
        parse_turn(&body, request_bytes, answered_by)
    }
}

impl ModelClient for AnthropicGateway {
    fn format(&self) -> Format {
        Format::AnthropicMessages
    }

    fn user_text(&self, text: &str) -> Result<NativeMessage, EncodeError> {
        encode(&InputMessage::user_text(text))
    }

    fn system_note(&self, text: &str) -> Result<NativeMessage, EncodeError> {
        encode(&InputMessage::system_text(text))
    }

    fn tool_results(&self, results: &[ToolResult]) -> Result<NativeMessage, EncodeError> {
        encode(&tool_results_message(results))
    }

    fn estimate_tokens(&self, message: &NativeMessage) -> u64 {
        estimate_message(message.json())
    }

    fn estimate_preamble_tokens(&self, preamble: &Preamble) -> u64 {
        let (system, tools) = preamble_parts(preamble);
        let bytes = serde_json::to_vec(&(system, tools)).map_or(0, |json| json.len());
        estimate_before_a_call(bytes)
    }

    fn call<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelTurn, ModelError>> {
        Box::pin(self.send(request))
    }
}

/// The user message that answers a turn's tool calls, in their order.
pub(crate) fn tool_results_message(results: &[ToolResult]) -> InputMessage {
    let blocks = results
        .iter()
        .map(|result| {
            let content = result
                .output
                .content
                .iter()
                .map(|part| match part {
                    ToolContent::Text(text) => ContentBlock::text(text.clone()),
                    ToolContent::Image { kind, data } => {
                        ContentBlock::image_base64(kind.media_type(), data.clone())
                    }
                })
                .collect();
            ContentBlock::tool_result(
                result.call_id.clone(),
                MessageContent::Blocks(content),
                result.output.is_error,
            )
        })
        .collect();
    InputMessage::new(Role::User, MessageContent::Blocks(blocks))
}

fn encode(message: &InputMessage) -> Result<NativeMessage, EncodeError> {
    Ok(NativeMessage::new(
        Format::AnthropicMessages,
        to_raw_value(message)?,
    ))
}

#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: &'static str,
}

/// The request body. No `model`: the gateway sets the role's model.
#[derive(Serialize)]
struct Body<'a> {
    max_tokens: u32,
    thinking: Thinking,
    output_config: OutputConfig,
    /// Automatic caching: a breakpoint that moves with the end of the conversation.
    cache_control: CacheControl,
    system: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool>,
    messages: Vec<&'a RawValue>,
}

fn cache_control(ttl: CacheTtl) -> CacheControl {
    match ttl {
        CacheTtl::FiveMinutes => CacheControl::ephemeral(),
        CacheTtl::OneHour => CacheControl::ephemeral_1h(),
    }
}

/// The parts of the body fixed for the conversation: the system blocks and the tools.
fn preamble_parts(preamble: &Preamble) -> (Vec<ContentBlock>, Vec<Tool>) {
    let breakpoint = cache_control(preamble.cache_ttl());
    let mut system = vec![
        ContentBlock::text(preamble.system_prompt().text.clone())
            .with_cache_control(breakpoint.clone()),
    ];
    if !preamble.tenant_context().is_empty() {
        system.push(
            ContentBlock::text(preamble.tenant_context().to_owned()).with_cache_control(breakpoint),
        );
    }
    let tools = preamble
        .tools()
        .iter()
        .map(|spec| Tool {
            name: spec.name.clone(),
            description: Some(spec.description.clone()),
            input_schema: Some(spec.input_schema.clone()),
            extra: ai_clients::anthropic::Extra::new(),
        })
        .collect();
    (system, tools)
}

pub(crate) fn request_body(request: &ModelRequest<'_>) -> Result<Vec<u8>, serde_json::Error> {
    let (system, tools) = preamble_parts(request.preamble);
    let messages = request
        .history
        .iter()
        .chain(request.pending)
        .map(NativeMessage::raw)
        .collect();
    serde_json::to_vec(&Body {
        max_tokens: request.max_output_tokens,
        thinking: Thinking { kind: "adaptive" },
        output_config: OutputConfig {
            effort: request.preamble.effort().as_str(),
        },
        cache_control: cache_control(request.preamble.cache_ttl()),
        system,
        tools,
        messages,
    })
}

fn tenant_header(tenant: TenantId) -> String {
    tenant.to_string()
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ModelErrorKind> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err(ModelErrorKind::InvalidResponse);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) if error.is_timeout() => return Err(ModelErrorKind::Timeout),
            Err(_) => return Err(ModelErrorKind::Unavailable),
        }
    }
}

/// What a failed status means for the assistant. The gateway already moved on to the role's
/// next model on 429, overload and timeouts; what reaches here is final for this call.
fn error_kind(status: u16) -> ModelErrorKind {
    match status {
        504 => ModelErrorKind::Timeout,
        429 | 529 | 500..=599 => ModelErrorKind::Unavailable,
        _ => ModelErrorKind::Rejected,
    }
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    kind: String,
}

/// The error `type` of an Anthropic-format error body, when it is a short identifier. The
/// `message` is never read: it can quote the request.
fn error_type(body: &[u8]) -> Option<String> {
    let envelope: ErrorEnvelope = serde_json::from_slice(body).ok()?;
    let kind = envelope.error.kind;
    let is_identifier = !kind.is_empty()
        && kind.len() <= MAX_ERROR_TYPE_CHARS
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
    is_identifier.then_some(kind)
}

/// The `content` of a response, as received.
#[derive(Deserialize)]
struct RawContent<'a> {
    #[serde(borrow)]
    content: &'a RawValue,
}

#[derive(Serialize)]
struct AssistantMessage<'a> {
    role: Role,
    content: &'a RawValue,
}

fn invalid() -> ModelError {
    ModelError::new(ModelErrorKind::InvalidResponse)
}

fn estimate(bytes: usize) -> u64 {
    u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .div_ceil(BYTES_PER_TOKEN_ESTIMATE)
}

fn estimate_before_a_call(bytes: usize) -> u64 {
    u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .div_ceil(BYTES_PER_TOKEN_BEFORE_A_CALL)
}

/// The input tokens of one message, estimated high: the JSON outside image data by its size,
/// each image (whose base64 data says little about its tokens) at the most one image costs.
fn estimate_message(json: &str) -> u64 {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return estimate_before_a_call(json.len());
    };
    let mut images = ImageCount::default();
    count_images(&value, &mut images);
    estimate_before_a_call(json.len().saturating_sub(images.data_bytes))
        .saturating_add(images.count.saturating_mul(TOKENS_PER_IMAGE_ESTIMATE))
}

#[derive(Default)]
struct ImageCount {
    count: u64,
    data_bytes: usize,
}

fn count_images(value: &serde_json::Value, images: &mut ImageCount) {
    match value {
        serde_json::Value::Object(fields) => {
            if fields.get("type").and_then(serde_json::Value::as_str) == Some("image") {
                images.count = images.count.saturating_add(1);
                if let Some(data) = fields
                    .get("source")
                    .and_then(|source| source.get("data"))
                    .and_then(serde_json::Value::as_str)
                {
                    images.data_bytes = images.data_bytes.saturating_add(data.len());
                }
                return;
            }
            for field in fields.values() {
                count_images(field, images);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                count_images(item, images);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_turn(
    body: &[u8],
    request_bytes: usize,
    answered_by: Option<String>,
) -> Result<ModelTurn, ModelError> {
    let message: Message = serde_json::from_slice(body).map_err(|_| invalid())?;
    let raw: RawContent<'_> = serde_json::from_slice(body).map_err(|_| invalid())?;
    let stop = match message.stop_reason.as_ref().ok_or_else(invalid)? {
        StopReason::EndTurn | StopReason::StopSequence => Stop::EndTurn,
        StopReason::ToolUse => Stop::ToolUse,
        StopReason::MaxTokens => Stop::MaxTokens,
        StopReason::Refusal => Stop::Refusal,
        StopReason::ModelContextWindowExceeded => Stop::ContextWindowExceeded,
        other @ (StopReason::PauseTurn | StopReason::Other(_)) => {
            Stop::Other(other.as_str().to_owned())
        }
    };
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(block) => text.push_str(&block.text),
            ContentBlock::ToolUse(block) => tool_calls.push(ToolCall {
                id: block.id.clone(),
                name: block.name.clone(),
                input: block.input.clone(),
            }),
            _ => {}
        }
    }
    let attempts = message.usage.attempts();
    // The answering attempt (the last) counted the whole request and this reply.
    let context_tokens = attempts
        .last()
        .filter(|(_model, attempt)| !attempt.input_missing && !attempt.output_missing)
        .map(|(_model, attempt)| attempt.total_input().saturating_add(attempt.output));
    let mut usage = TokenCount::default();
    for (_model, attempt) in attempts {
        usage.add(&TokenCount {
            input: if attempt.input_missing {
                estimate(request_bytes)
            } else {
                attempt.input
            },
            cache_write: attempt
                .cache_write_5m
                .saturating_add(attempt.cache_write_1h),
            cache_read: attempt.cache_read,
            output: if attempt.output_missing {
                estimate(body.len())
            } else {
                attempt.output
            },
            estimated: attempt.is_incomplete(),
        });
    }
    let message_json = if message.content.is_empty() {
        None
    } else {
        let json = to_raw_value(&AssistantMessage {
            role: Role::Assistant,
            content: raw.content,
        })
        .map_err(|_| invalid())?;
        Some(NativeMessage::new(Format::AnthropicMessages, json))
    };
    Ok(ModelTurn {
        message: message_json,
        text,
        tool_calls,
        stop,
        usage,
        context_tokens,
        model: answered_by.unwrap_or(message.model),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_kinds() {
        assert_eq!(error_kind(504), ModelErrorKind::Timeout);
        for status in [429, 529, 500, 502, 503] {
            assert_eq!(error_kind(status), ModelErrorKind::Unavailable, "{status}");
        }
        for status in [400, 404, 413, 401] {
            assert_eq!(error_kind(status), ModelErrorKind::Rejected, "{status}");
        }
    }

    #[test]
    fn only_identifier_error_types_are_kept() {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"x"}}"#;
        assert_eq!(error_type(body), Some("overloaded_error".to_owned()));
        let odd = br#"{"type":"error","error":{"type":"Contains spaces and text","message":"x"}}"#;
        assert_eq!(error_type(odd), None);
        assert_eq!(error_type(b"not json"), None);
    }

    #[test]
    fn a_missing_usage_is_estimated_not_zero() {
        let body = br#"{"id":"msg_1","type":"message","role":"assistant","model":"m",
            "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#;
        let turn = parse_turn(body, 3_000, None).expect("a valid message");
        assert!(turn.usage.estimated);
        assert_eq!(turn.usage.input, 1_000);
        assert!(turn.usage.output > 0);
    }

    #[test]
    fn an_empty_refusal_has_no_message_to_keep() {
        let body = br#"{"id":"msg_1","type":"message","role":"assistant","model":"m",
            "content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":null},
            "usage":{"input_tokens":10,"output_tokens":0}}"#;
        let turn = parse_turn(body, 10, None).expect("a valid message");
        assert_eq!(turn.stop, Stop::Refusal);
        assert!(turn.message.is_none());
        assert_eq!(turn.context_tokens, Some(10));
    }

    #[test]
    fn the_context_is_what_the_answering_attempt_counted() {
        let body = br#"{"id":"msg_1","type":"message","role":"assistant","model":"m",
            "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":312,"cache_creation_input_tokens":50,
            "cache_read_input_tokens":4096,"output_tokens":188}}"#;
        let turn = parse_turn(body, 10, None).expect("a valid message");
        assert_eq!(turn.context_tokens, Some(312 + 50 + 4096 + 188));

        let no_usage = br#"{"id":"msg_1","type":"message","role":"assistant","model":"m",
            "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#;
        let turn = parse_turn(no_usage, 10, None).expect("a valid message");
        assert_eq!(turn.context_tokens, None, "an estimate is not a count");
    }

    #[test]
    fn messages_are_estimated_high_and_images_by_count() {
        let text = r#"{"role":"user","content":"How many meters reported?"}"#;
        assert_eq!(text.len(), 53);
        assert_eq!(
            estimate_message(text),
            27,
            "two bytes per token, rounded up"
        );

        let data = "A".repeat(3_000_000);
        let image = format!(
            r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":[
            {{"type":"text","text":"chart"}},
            {{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{data}"}}}}
            ]}}]}}"#
        );
        let estimate = estimate_message(&image);
        let rest = u64::try_from(image.len() - data.len()).expect("a u64");
        assert_eq!(estimate, rest.div_ceil(2) + TOKENS_PER_IMAGE_ESTIMATE);

        assert_eq!(estimate_message("not json"), 4);
    }
}
