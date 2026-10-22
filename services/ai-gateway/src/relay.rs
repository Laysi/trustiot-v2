//! The internal API (README.md, 「內部 API」): a caller names a model role in the path and sends
//! a request in that role's native protocol. The gateway sets the model, tries the role's
//! models in order, and returns the response or the stream as the provider sent it.
//!
//! Logs carry the role, tenant, model, attempts, outcome, error class and the provider's
//! request ID; never a request or response body, a provider message or a key.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use ai_clients::anthropic::{AnthropicClient, Message, MessageStream, Outcome, RequestOptions};
use ai_clients::decisions::{Answer, DecisionResponse};
use ai_clients::sse::SseEvent;
use ai_clients::{Error, ErrorClass, RawReply, TimeoutKind};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use tokio::time::Instant;

use crate::body::ModelSlot;
use crate::config::Protocol;
use crate::gateway::{Client, Gateway, Keys, RoleRoute, Target};
use crate::reject::{Kind, Rejection};

/// Anthropic Messages for the role in the path; `"stream": true` streams.
pub const MESSAGES_PATH: &str = "/internal/roles/{role}/v1/messages";
/// OpenAI Decisions for the role in the path.
pub const DECISIONS_PATH: &str = "/internal/roles/{role}/v1/decisions";
/// Required: the tenant the call is for, an opaque ID the gateway only logs (decision 28).
pub const TENANT_HEADER: &str = "ai-gateway-tenant";
/// On every successful response: the `<provider>/<model>` that answered.
pub const MODEL_HEADER: &str = "ai-gateway-model";
/// On non-streaming responses: `completed`, `refusal` or `max_tokens_without_text`.
pub const OUTCOME_HEADER: &str = "ai-gateway-outcome";
/// The provider's request ID, under each provider's own header name.
const ANTHROPIC_REQUEST_ID: &str = "request-id";
const OPENAI_REQUEST_ID: &str = "x-request-id";
/// The only caller header passed on (Anthropic only), after checking each value.
const BETA_HEADER: &str = "anthropic-beta";
const MAX_TENANT_CHARS: usize = 128;
const MAX_BETAS: usize = 32;
const MAX_BETA_CHARS: usize = 128;

pub async fn messages(
    State(gateway): State<Arc<Gateway>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(gateway, role, headers, body, Protocol::AnthropicMessages).await
}

pub async fn decisions(
    State(gateway): State<Arc<Gateway>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(gateway, role, headers, body, Protocol::OpenaiDecisions).await
}

async fn handle(
    gateway: Arc<Gateway>,
    role: String,
    headers: HeaderMap,
    body: Body,
    protocol: Protocol,
) -> Response {
    // The budget is at most `http.request_timeout`, itself at most an hour.
    let started = Instant::now();
    let deadline = started + gateway.budget();
    let call = match Call::new(&gateway, &role, protocol, &headers, started, deadline) {
        Ok(call) => call,
        Err(rejection) => return rejection.render(protocol),
    };
    // A caller that sends its body slowly uses up the same budget.
    let read = read_body(body, gateway.max_request_bytes());
    let body = match tokio::time::timeout_at(deadline, read).await {
        Ok(Ok(body)) => body,
        Ok(Err(rejection)) => return rejection.render(protocol),
        Err(_) => {
            return Rejection::new(
                Kind::Timeout,
                "the request body did not arrive within the gateway's time limit",
            )
            .render(protocol);
        }
    };
    let slot = match ModelSlot::parse(&body, protocol == Protocol::AnthropicMessages) {
        Ok(slot) => slot,
        Err(error) => {
            return Rejection::new(Kind::InvalidRequest, error.to_string()).render(protocol);
        }
    };
    call.run(&slot, &gateway).await
}

async fn read_body(body: Body, limit: usize) -> Result<Bytes, Rejection> {
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => Err(Rejection::new(
            Kind::TooLarge,
            format!("the request body is over {limit} bytes"),
        )),
        Err(_) => Err(Rejection::new(
            Kind::InvalidRequest,
            "the request body could not be read",
        )),
    }
}

/// One call to a model role, before its body is read.
struct Call<'a> {
    role: &'a str,
    protocol: Protocol,
    route: &'a RoleRoute,
    tenant: String,
    options: RequestOptions,
    started: Instant,
    deadline: Instant,
}

/// A model's answer.
enum Success {
    Message(Box<RawReply<Message>>),
    Decision(Box<RawReply<DecisionResponse>>),
    /// A stream that has produced its first output (or ended before any).
    Stream(Box<Opened>),
}

struct Opened {
    stream: MessageStream,
    /// The events read before the first output, relayed first.
    events: Vec<Bytes>,
}

/// A model's failure.
struct Failed {
    error: Error,
    /// For a stream that ended with a provider error event before its first output: the
    /// events before it and the event, relayed as they came when no model is left to try.
    events: Option<Vec<Bytes>>,
}

impl From<Error> for Failed {
    fn from(error: Error) -> Self {
        Self {
            error,
            events: None,
        }
    }
}

impl<'a> Call<'a> {
    fn new(
        gateway: &'a Gateway,
        role: &'a str,
        protocol: Protocol,
        headers: &HeaderMap,
        started: Instant,
        deadline: Instant,
    ) -> Result<Self, Rejection> {
        let route = gateway.route(role, protocol)?;
        let tenant = tenant(headers)?;
        let options = match protocol {
            Protocol::AnthropicMessages => RequestOptions {
                betas: betas(headers)?,
            },
            Protocol::OpenaiDecisions => RequestOptions::default(),
        };
        Ok(Self {
            role,
            protocol,
            route,
            tenant,
            options,
            started,
            deadline,
        })
    }

    /// Tries the role's models in order until one answers, a failure is not one to fall back
    /// on (README.md, 「備援」), or the time budget runs out.
    async fn run(&self, slot: &ModelSlot<'_>, gateway: &Arc<Gateway>) -> Response {
        let targets = &self.route.targets;
        for (index, target) in targets.iter().enumerate() {
            let attempts = index + 1;
            let body = slot.with_model(&target.model);
            let sent = send(target, slot.streaming, body, &self.options, gateway.keys());
            let failed = match tokio::time::timeout_at(self.deadline, sent).await {
                Ok(Ok(success)) => return self.answer(success, target, attempts, gateway),
                Ok(Err(failed)) => failed,
                Err(_) => {
                    tracing::warn!(
                        role = self.role,
                        protocol = self.protocol.name(),
                        tenant = %self.tenant,
                        model = %target.label,
                        attempts,
                        class = "gateway_deadline",
                        duration_ms = self.duration_ms(),
                        "model call failed"
                    );
                    return Rejection::new(
                        Kind::Timeout,
                        "no model answered within the gateway's time limit",
                    )
                    .render(self.protocol);
                }
            };
            match targets.get(attempts) {
                Some(next) if falls_back(&failed.error) => {
                    self.log_fallback(&failed.error, target, next, attempts, gateway.keys());
                }
                _ => return self.fail(failed, target, attempts, gateway),
            }
        }
        // A role always has a model (checked at startup).
        Rejection::new(Kind::BadGateway, "the model role has no model").render(self.protocol)
    }

    fn answer(
        &self,
        success: Success,
        target: &Target,
        attempts: usize,
        gateway: &Arc<Gateway>,
    ) -> Response {
        match success {
            Success::Message(reply) => {
                let outcome = message_outcome(reply.body.outcome());
                let request_id = provider_field(gateway, reply.request_id.as_deref());
                self.log_completed(target, attempts, outcome, request_id.as_deref());
                let response = json_response(reply.raw);
                with_headers(
                    response,
                    target,
                    ANTHROPIC_REQUEST_ID,
                    request_id.as_deref(),
                    Some(outcome),
                )
            }
            Success::Decision(reply) => {
                let refused = reply
                    .body
                    .answers
                    .iter()
                    .any(|answer| matches!(answer, Answer::Refusal { .. }));
                let outcome = if refused { "refusal" } else { "completed" };
                let request_id = provider_field(gateway, reply.request_id.as_deref());
                self.log_completed(target, attempts, outcome, request_id.as_deref());
                let response = json_response(reply.raw);
                with_headers(
                    response,
                    target,
                    OPENAI_REQUEST_ID,
                    request_id.as_deref(),
                    Some(outcome),
                )
            }
            Success::Stream(opened) => {
                let Opened { stream, events } = *opened;
                self.relay(events, stream, target, attempts, gateway)
            }
        }
    }

    /// The last model's failure: a stream's own error event, a provider error as the provider
    /// sent it, or the gateway's error.
    fn fail(
        &self,
        failed: Failed,
        target: &Target,
        attempts: usize,
        gateway: &Arc<Gateway>,
    ) -> Response {
        log_failure(
            &self.log_fields(target, attempts),
            &failed.error,
            gateway.keys(),
            "model call failed",
        );
        match failed.events {
            Some(events) => {
                let request_id = provider_field(gateway, failed.error.request_id());
                let fields = self.log_fields(target, attempts);
                let relay = Relay {
                    pending: events.into(),
                    stream: None,
                    gateway: Arc::clone(gateway),
                    fields,
                    request_id,
                    done: true,
                };
                stream_response(relay, target)
            }
            None => Rejection::from_error(&failed.error, gateway.keys()).render(self.protocol),
        }
    }

    fn relay(
        &self,
        events: Vec<Bytes>,
        stream: MessageStream,
        target: &Target,
        attempts: usize,
        gateway: &Arc<Gateway>,
    ) -> Response {
        let request_id = provider_field(gateway, stream.request_id());
        let relay = Relay {
            pending: events.into(),
            stream: Some(stream),
            gateway: Arc::clone(gateway),
            fields: self.log_fields(target, attempts),
            request_id,
            done: false,
        };
        stream_response(relay, target)
    }

    fn log_completed(
        &self,
        target: &Target,
        attempts: usize,
        outcome: &'static str,
        request_id: Option<&str>,
    ) {
        tracing::info!(
            role = self.role,
            protocol = self.protocol.name(),
            streaming = false,
            tenant = %self.tenant,
            model = %target.label,
            attempts,
            outcome,
            provider_request_id = request_id,
            duration_ms = self.duration_ms(),
            "model call completed"
        );
    }

    fn log_fallback(
        &self,
        error: &Error,
        target: &Target,
        next: &Target,
        attempts: usize,
        keys: &Keys,
    ) {
        let api = api_error(error);
        let error_type = api.and_then(|api| api.body.error_type.as_deref());
        tracing::warn!(
            role = self.role,
            protocol = self.protocol.name(),
            tenant = %self.tenant,
            model = %target.label,
            next_model = %next.label,
            attempts,
            class = class_name(error.class()),
            status = api.and_then(|api| api.status),
            error_type = error_type.map(|t| keys.scrub_field(t)),
            provider_request_id = error.request_id().map(|id| keys.scrub_field(id)),
            "falling back to the next model"
        );
    }

    fn log_fields(&self, target: &Target, attempts: usize) -> LogFields {
        LogFields {
            role: self.role.to_owned(),
            protocol: self.protocol,
            tenant: self.tenant.clone(),
            model: target.label.clone(),
            attempts,
            started: self.started,
        }
    }

    fn duration_ms(&self) -> u64 {
        elapsed_ms(self.started)
    }
}

/// One attempt with one model. A stream counts as answered at its first output: until then
/// its failures may still fall back to the next model.
async fn send(
    target: &Target,
    streaming: bool,
    body: Bytes,
    options: &RequestOptions,
    keys: &Keys,
) -> Result<Success, Failed> {
    match &target.client {
        Client::Anthropic(client) if streaming => open_stream(client, body, options, keys)
            .await
            .map(|opened| Success::Stream(Box::new(opened))),
        Client::Anthropic(client) => client
            .create_message_raw(body, options)
            .await
            .map(|reply| Success::Message(Box::new(reply)))
            .map_err(Failed::from),
        Client::Decisions(client) => client
            .decide_raw(body)
            .await
            .map(|reply| Success::Decision(Box::new(reply)))
            .map_err(Failed::from),
    }
}

/// Opens a stream and reads it up to its first output, keeping the events read so far.
async fn open_stream(
    client: &AnthropicClient,
    body: Bytes,
    options: &RequestOptions,
    keys: &Keys,
) -> Result<Opened, Failed> {
    let mut stream = client.stream_message_raw(body, options).await?;
    let mut events = Vec::new();
    loop {
        match stream.next().await {
            Ok(Some(item)) => {
                events.push(Bytes::from(item.raw.to_bytes()));
                if item.event.is_output() {
                    return Ok(Opened { stream, events });
                }
            }
            Ok(None) => return Ok(Opened { stream, events }),
            Err(error) => {
                let events = stream.error_event().map(|event| {
                    events.push(scrubbed_event(event, keys));
                    events
                });
                return Err(Failed { error, events });
            }
        }
    }
}

/// Whether the next model is tried: 429, overload, a timeout (the provider's or the
/// gateway's limits of the role), or the model's concurrency limit (nothing was sent).
fn falls_back(error: &Error) -> bool {
    matches!(
        error.class(),
        ErrorClass::RateLimited { .. }
            | ErrorClass::Overloaded
            | ErrorClass::Timeout(_)
            | ErrorClass::ConcurrencyLimited
    )
}

/// What the stream relay needs after the handler has returned.
struct LogFields {
    role: String,
    protocol: Protocol,
    tenant: String,
    model: String,
    attempts: usize,
    started: Instant,
}

/// A stream relayed to the caller: the events read before the first output, then every event
/// as it arrives. It ends after `message_stop`, or with an error event: the provider's own, or
/// one the gateway writes when the stream times out or breaks. A caller that disconnects drops
/// the relay, which closes the provider connection.
///
/// Streams are not ended at shutdown (`Shutdown::stopping`): a model's answer cannot resume on
/// another pod, so the drain lets it finish (README.md, 「關機」).
struct Relay {
    pending: VecDeque<Bytes>,
    stream: Option<MessageStream>,
    /// For the keys to scrub from an error event.
    gateway: Arc<Gateway>,
    fields: LogFields,
    request_id: Option<String>,
    /// The end has been logged.
    done: bool,
}

impl Relay {
    async fn step(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        if let Some(bytes) = self.pending.pop_front() {
            return Some((Ok(bytes), self));
        }
        let next = self.stream.as_mut()?.next().await;
        match next {
            Ok(Some(item)) => Some((Ok(Bytes::from(item.raw.to_bytes())), self)),
            Ok(None) => {
                let outcome = self
                    .stream
                    .as_ref()
                    .and_then(MessageStream::message)
                    .map_or("completed", |message| message_outcome(message.outcome()));
                self.stream = None;
                self.done = true;
                let fields = &self.fields;
                tracing::info!(
                    role = %fields.role,
                    protocol = fields.protocol.name(),
                    streaming = true,
                    tenant = %fields.tenant,
                    model = %fields.model,
                    attempts = fields.attempts,
                    outcome,
                    provider_request_id = self.request_id.as_deref(),
                    duration_ms = elapsed_ms(fields.started),
                    "model call completed"
                );
                None
            }
            Err(error) => {
                let bytes = match self.stream.as_ref().and_then(MessageStream::error_event) {
                    Some(event) => scrubbed_event(event, self.gateway.keys()),
                    None => gateway_error_event(&error),
                };
                self.stream = None;
                self.done = true;
                log_failure(
                    &self.fields,
                    &error,
                    self.gateway.keys(),
                    "model stream failed",
                );
                Some((Ok(bytes), self))
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if !self.done {
            let fields = &self.fields;
            tracing::warn!(
                role = %fields.role,
                protocol = fields.protocol.name(),
                tenant = %fields.tenant,
                model = %fields.model,
                attempts = fields.attempts,
                provider_request_id = self.request_id.as_deref(),
                duration_ms = elapsed_ms(fields.started),
                "the caller left before the stream ended"
            );
        }
    }
}

fn stream_response(relay: Relay, target: &Target) -> Response {
    let request_id = relay.request_id.clone();
    let body = Body::from_stream(futures_util::stream::unfold(relay, Relay::step));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    with_headers(
        response,
        target,
        ANTHROPIC_REQUEST_ID,
        request_id.as_deref(),
        None,
    )
}

fn json_response(raw: Bytes) -> Response {
    let mut response = Response::new(Body::from(raw));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn with_headers(
    mut response: Response,
    target: &Target,
    request_id_header: &'static str,
    request_id: Option<&str>,
    outcome: Option<&'static str>,
) -> Response {
    let headers = response.headers_mut();
    // Model references are printable ASCII (checked at startup); a provider's request ID that
    // is not a valid header value is left out.
    if let Ok(value) = HeaderValue::from_str(&target.label) {
        headers.insert(HeaderName::from_static(MODEL_HEADER), value);
    }
    if let Some(value) = request_id.and_then(|id| HeaderValue::from_str(id).ok()) {
        headers.insert(HeaderName::from_static(request_id_header), value);
    }
    if let Some(outcome) = outcome {
        headers.insert(
            HeaderName::from_static(OUTCOME_HEADER),
            HeaderValue::from_static(outcome),
        );
    }
    response
}

fn message_outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "completed",
        Outcome::Refusal { .. } => "refusal",
        Outcome::MaxTokensWithoutText => "max_tokens_without_text",
    }
}

/// A provider's error event with the keys scrubbed from it. The keys are looked for in every
/// decoded string and object name of the event's JSON data, so a key written with JSON escapes
/// is found too, and in the raw text. Data without a key is relayed byte for byte; otherwise
/// the scrubbed value is written out again (object members may change order). Data that is not
/// JSON is replaced by the gateway's own error event.
fn scrubbed_event(event: &SseEvent, keys: &Keys) -> Bytes {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&event.data) else {
        return error_event("api_error", "the model stream failed");
    };
    let decoded_had_key = scrub_value(&mut value, keys);
    let data = if !decoded_had_key && keys.scrub(&event.data) == event.data {
        event.data.clone()
    } else {
        keys.scrub(&value.to_string())
    };
    let event = SseEvent {
        event: keys.scrub(&event.event),
        data,
    };
    Bytes::from(event.to_bytes())
}

/// Replaces the keys in every string and object name of `value`; true when one was found.
/// `serde_json` parses at most 128 levels deep, which bounds the recursion.
fn scrub_value(value: &mut serde_json::Value, keys: &Keys) -> bool {
    use serde_json::Value;
    match value {
        Value::String(text) => {
            let scrubbed = keys.scrub(text);
            let changed = scrubbed != *text;
            *text = scrubbed;
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| scrub_value(item, keys) | changed),
        Value::Object(members) => {
            let mut changed = false;
            for (name, mut item) in std::mem::take(members) {
                changed |= scrub_value(&mut item, keys);
                let scrubbed = keys.scrub(&name);
                changed |= scrubbed != name;
                members.insert(scrubbed, item);
            }
            changed
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// The error event the gateway writes when a stream fails without one of the provider's.
fn gateway_error_event(error: &Error) -> Bytes {
    match error.class() {
        ErrorClass::Timeout(_) => error_event("timeout_error", "the model stream timed out"),
        _ => error_event("api_error", "the model stream failed"),
    }
}

fn error_event(error_type: &str, message: &str) -> Bytes {
    let data = serde_json::json!({
        "type": "error",
        "error": {"type": error_type, "message": message},
    });
    let event = SseEvent {
        event: "error".to_owned(),
        data: data.to_string(),
    };
    Bytes::from(event.to_bytes())
}

fn log_failure(fields: &LogFields, error: &Error, keys: &Keys, message: &'static str) {
    let api = api_error(error);
    // The text of other errors names causes and limits, never the request or the response
    // (ai-clients leaves the values out of JSON errors), scrubbed all the same; a provider's
    // message and the clients' request checks are left out.
    let detail = match error {
        Error::Api(_) | Error::StreamError(_) | Error::InvalidRequest(_) => None,
        other => Some(keys.scrub(&other.to_string())),
    };
    let error_type = api.and_then(|api| api.body.error_type.as_deref());
    tracing::warn!(
        role = %fields.role,
        protocol = fields.protocol.name(),
        tenant = %fields.tenant,
        model = %fields.model,
        attempts = fields.attempts,
        class = class_name(error.class()),
        status = api.and_then(|api| api.status),
        error_type = error_type.map(|t| keys.scrub_field(t)),
        provider_request_id = error.request_id().map(|id| keys.scrub_field(id)),
        detail = detail.as_deref(),
        duration_ms = elapsed_ms(fields.started),
        "{message}"
    );
}

/// A provider's request ID as the gateway logs and passes it on (`Keys::scrub_field`).
fn provider_field(gateway: &Gateway, value: Option<&str>) -> Option<String> {
    value.map(|value| gateway.keys().scrub_field(value))
}

fn api_error(error: &Error) -> Option<&ai_clients::ApiError> {
    match error {
        Error::Api(api) | Error::StreamError(api) => Some(api),
        _ => None,
    }
}

fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::RateLimited { .. } => "rate_limited",
        ErrorClass::Overloaded => "overloaded",
        ErrorClass::Timeout(TimeoutKind::Connect) => "timeout_connect",
        ErrorClass::Timeout(TimeoutKind::FirstToken) => "timeout_first_token",
        ErrorClass::Timeout(TimeoutKind::StreamIdle) => "timeout_stream_idle",
        ErrorClass::Timeout(TimeoutKind::StreamTotal) => "timeout_stream_total",
        ErrorClass::Timeout(TimeoutKind::Response) => "timeout_response",
        ErrorClass::Timeout(TimeoutKind::Provider) => "timeout_provider",
        ErrorClass::ConcurrencyLimited => "concurrency_limited",
        ErrorClass::Cancelled => "cancelled",
        ErrorClass::Other => "other",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The tenant header: exactly one, 1 to 128 letters, digits, `.`, `_`, `:` or `-`.
fn tenant(headers: &HeaderMap) -> Result<String, Rejection> {
    let mut values = headers.get_all(TENANT_HEADER).iter();
    let value = match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    };
    value
        .filter(|text| {
            (1..=MAX_TENANT_CHARS).contains(&text.len())
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            Rejection::new(
                Kind::InvalidRequest,
                format!(
                    "the {TENANT_HEADER} header is required once: 1 to {MAX_TENANT_CHARS} \
                     letters, digits, `.`, `_`, `:` or `-`"
                ),
            )
        })
}

/// The caller's `anthropic-beta` values: comma-separated names of letters, digits, `.`, `_` or
/// `-`, at most 32.
fn betas(headers: &HeaderMap) -> Result<Vec<String>, Rejection> {
    let invalid = || {
        Rejection::new(
            Kind::InvalidRequest,
            format!(
                "{BETA_HEADER} must list at most {MAX_BETAS} names of 1 to {MAX_BETA_CHARS} \
                 letters, digits, `.`, `_` or `-`"
            ),
        )
    };
    let mut betas = Vec::new();
    for value in headers.get_all(BETA_HEADER) {
        let text = value.to_str().map_err(|_| invalid())?;
        for name in text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let valid = name.len() <= MAX_BETA_CHARS
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
            if !valid || betas.len() == MAX_BETAS {
                return Err(invalid());
            }
            betas.push(name.to_owned());
        }
    }
    Ok(betas)
}

#[cfg(test)]
mod tests {
    use ai_clients::{ApiError, ApiErrorBody};

    use super::*;

    fn api(class: ErrorClass) -> Error {
        Error::Api(Box::new(ApiError {
            status: Some(500),
            body: ApiErrorBody::default(),
            request_id: None,
            retry_after: None,
            class,
        }))
    }

    #[test]
    fn only_rate_limits_overload_timeouts_and_full_queues_fall_back() {
        for error in [
            api(ErrorClass::RateLimited { retry_after: None }),
            api(ErrorClass::Overloaded),
            api(ErrorClass::Timeout(TimeoutKind::Provider)),
            Error::Timeout(TimeoutKind::Connect),
            Error::Timeout(TimeoutKind::FirstToken),
            Error::Timeout(TimeoutKind::Response),
            Error::ConcurrencyLimited {
                endpoint: "main:anthropic/claude-opus-5-5".into(),
            },
        ] {
            assert!(falls_back(&error), "{error}");
        }
        for error in [
            api(ErrorClass::Other),
            Error::InvalidRequest("questions".into()),
            Error::Protocol("truncated".into()),
            Error::Cancelled,
        ] {
            assert!(!falls_back(&error), "{error}");
        }
    }

    #[test]
    fn tenant_and_beta_headers_are_checked() {
        let mut headers = HeaderMap::new();
        assert!(tenant(&headers).is_err());
        headers.insert(TENANT_HEADER, HeaderValue::from_static("tenant:0f3a-1"));
        assert_eq!(tenant(&headers).unwrap(), "tenant:0f3a-1");
        headers.append(TENANT_HEADER, HeaderValue::from_static("other"));
        assert!(tenant(&headers).is_err());
        headers.insert(TENANT_HEADER, HeaderValue::from_static("has space"));
        assert!(tenant(&headers).is_err());

        headers.insert(
            BETA_HEADER,
            HeaderValue::from_static("context-management-2025-06-27, effort-2025-11-24"),
        );
        assert_eq!(
            betas(&headers).unwrap(),
            ["context-management-2025-06-27", "effort-2025-11-24"]
        );
        headers.insert(BETA_HEADER, HeaderValue::from_static("a b"));
        assert!(betas(&headers).is_err());
        let many = vec!["x"; MAX_BETAS + 1].join(",");
        headers.insert(BETA_HEADER, HeaderValue::from_str(&many).unwrap());
        assert!(betas(&headers).is_err());
    }

    fn sse(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.to_owned(),
            data: data.to_owned(),
        }
    }

    #[test]
    fn error_events_are_relayed_unchanged_without_a_key() {
        let keys = Keys::new(vec![["test", "key", "a1"].join("-")]);
        let data =
            r#"{"type":"error", "error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let event = sse("error", data);
        assert_eq!(scrubbed_event(&event, &keys), Bytes::from(event.to_bytes()));
    }

    #[test]
    fn a_key_in_an_error_event_is_scrubbed_however_it_is_written() {
        // Built at run time; the escaped form spells every character of the key as \uXXXX.
        let key = ["test", "key", "a1"].join("-");
        let keys = Keys::new(vec![key.clone()]);
        let escaped: String = key
            .chars()
            .map(|c| format!("\\u{:04x}", u32::from(c)))
            .collect();
        for data in [
            format!(r#"{{"type":"error","error":{{"type":"api_error","message":"bad {key}"}}}}"#),
            format!(
                r#"{{"type":"error","error":{{"type":"api_error","message":"bad {escaped}"}}}}"#
            ),
            format!(r#"{{"type":"error","error":{{"type":"api_error","{escaped}":[1,"{key}"]}}}}"#),
        ] {
            let bytes = scrubbed_event(&sse("error", &data), &keys);
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!text.contains(&key), "{text}");
            assert!(!text.contains(&escaped), "{text}");
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(data).unwrap();
            assert!(!value.to_string().contains(&key), "{value}");
            assert!(data.contains("[REDACTED]"), "{data}");
        }

        let not_json = scrubbed_event(&sse("error", &format!("bad {escaped}")), &keys);
        let text = String::from_utf8(not_json.to_vec()).unwrap();
        assert!(!text.contains(&escaped), "{text}");
        assert!(text.contains("api_error"), "{text}");
    }
}
