//! The tools the main agent calls (docs/architecture/ai-assistant.md, 「Tools」): the [`Tool`]
//! trait, the [`ToolRegistry`] that runs a turn's calls concurrently under the limits, and the
//! [`ToolContext`] each call gets. The tools that read telemetry and device models as the user
//! are in [`telemetry`] and [`device_models`] (Laysi/trustiot-v2#591); they call the other
//! services through [`crate::callee`] with a token from the question's [`Delegation`].

pub mod device_models;
pub mod telemetry;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use contract::{InternalToken, ServiceName};
use serde_json::Value;
use tokio::task::JoinSet;
use tokio::time::Instant;
use uuid::Uuid;

use crate::history::Principal;
use crate::model::{BoxFuture, ToolCall, ToolResult};

/// The most tools the main agent has: more make the model choose worse
/// (docs/architecture/ai-assistant.md, 「Tools」).
pub const MAX_TOOLS: usize = 10;
/// The API's limit on a tool name.
const MAX_NAME_CHARS: usize = 64;

/// What the model is told about a tool. Part of the conversation's fixed preamble.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    /// 1 to 64 ASCII letters, digits, `_` or `-`.
    pub name: String,
    /// What the tool does, when to use it, what it returns and its limits.
    pub description: String,
    /// A JSON Schema object (`"type": "object"`).
    pub input_schema: Value,
}

/// An image a tool hands the model, such as a chart. The API accepts these four types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// One piece of a tool result.
#[derive(Clone, PartialEq)]
pub enum ToolContent {
    Text(String),
    /// `data` is standard base64.
    Image {
        kind: ImageType,
        data: String,
    },
}

impl fmt::Debug for ToolContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(&text.len()).finish(),
            Self::Image { kind, data } => f
                .debug_struct("Image")
                .field("kind", kind)
                .field("bytes", &data.len())
                .finish(),
        }
    }
}

/// What the model gets back from a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
    /// The call failed; `content` tells the model why and what to do instead.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            is_error: false,
        }
    }

    /// A failed call, with a message that tells the model how to fix the call.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(message.into())],
            is_error: true,
        }
    }

    pub fn with_image(mut self, kind: ImageType, data: impl Into<String>) -> Self {
        self.content.push(ToolContent::Image {
            kind,
            data: data.into(),
        });
        self
    }

    fn text_bytes(&self) -> usize {
        self.content
            .iter()
            .map(|part| match part {
                ToolContent::Text(text) => text.len(),
                ToolContent::Image { .. } => 0,
            })
            .sum()
    }
}

/// Why a tool call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The model's input is wrong. The message goes to the model: it says what is wrong and how
    /// to call the tool instead. It must not quote data the user may not see.
    #[error("invalid tool input")]
    Input(String),
    /// Something the model cannot fix (a service is down or answered with an error). The model
    /// gets a fixed message; nothing of the underlying error reaches it or the logs.
    #[error("tool failed")]
    Failed,
    /// The service refused the user (403) or has nothing of this for the user (404): the user may
    /// not read it, or it does not exist in the user's tenant. The model gets a fixed message
    /// that says not to retry; nothing of the service's answer reaches it or the logs.
    #[error("the service refused the request")]
    Denied,
    /// The user's delegation was revoked: the user signed out, the session ended or the user
    /// left the tenant ([`DelegationError::Revoked`]). The question ends: the other calls of
    /// the step are cancelled, no result goes to the model, and no tool runs again.
    #[error("the user's access ended")]
    Revoked,
}

impl From<DelegationError> for ToolError {
    /// A revoked delegation ends the question; anything else is a failure of this call.
    fn from(error: DelegationError) -> Self {
        match error {
            DelegationError::Revoked => Self::Revoked,
            DelegationError::NotConfigured | DelegationError::Unavailable => Self::Failed,
        }
    }
}

/// Gives a tool internal identity tokens for the conversation's user while a question runs
/// (decision 27). The internal token lives 60 seconds, shorter than a long analysis, so a tool
/// asks for a fresh one before each call to another service. The run delegation of
/// Laysi/trustiot-v2#551 backs it: [`crate::delegation::QuestionRun`], which
/// [`crate::delegation::QuestionRuns::start`] makes for one question. [`NoDelegation`] refuses.
pub trait Delegation: Send + Sync {
    /// A token that lets the conversation's user call `audience`, for this call only.
    fn token_for(
        &self,
        audience: ServiceName,
    ) -> BoxFuture<'_, Result<InternalToken, DelegationError>>;

    /// The run the delegation was started for: the question is logged under this ID, so the
    /// assistant's logs and the identity service's agree. `None`: the agent makes one.
    fn run_id(&self) -> Option<Uuid> {
        None
    }

    /// The time by which the question must have ended, a little before the delegation stops
    /// giving tokens. The agent stops the question there with
    /// [`crate::AgentError::RunExpired`]. `None`: no limit.
    fn deadline(&self) -> Option<Instant> {
        None
    }

    /// The tenant and user the delegation acts for. [`crate::Agent::ask`] refuses a question
    /// whose conversation belongs to anyone else ([`crate::AgentError::NotTheConversationsRun`]),
    /// so a run started for one conversation cannot read data for another. `None`: a delegation
    /// that acts for no one ([`NoDelegation`]).
    fn principal(&self) -> Option<Principal> {
        None
    }
}

/// The user's delegation could not give a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DelegationError {
    /// No delegation is configured: the deployment gives no `identity.exchange_url`.
    #[error("no delegation is configured")]
    NotConfigured,
    /// The user's session ended or lost access; the run must stop.
    #[error("the user's session no longer allows this")]
    Revoked,
    /// The identity service did not answer in time or failed.
    #[error("the identity service is unavailable")]
    Unavailable,
}

/// The delegation of a run that has none.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDelegation;

impl Delegation for NoDelegation {
    fn token_for(
        &self,
        _audience: ServiceName,
    ) -> BoxFuture<'_, Result<InternalToken, DelegationError>> {
        Box::pin(async { Err::<InternalToken, _>(DelegationError::NotConfigured) })
    }
}

/// What a tool call runs with: whose question it is, and how to call other services as that
/// user. A tool never takes the tenant or the user from its input.
#[derive(Clone)]
pub struct ToolContext {
    pub principal: Principal,
    pub conversation_id: Uuid,
    pub run_id: Uuid,
    pub delegation: Arc<dyn Delegation>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("principal", &self.principal)
            .field("conversation_id", &self.conversation_id)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

/// A tool. Results should be compact: aggregate in the service and return the few rows that
/// answer the question, with a count and a hint when there is more.
pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;

    /// Told to the model when a result is over the size limit: how to ask for less.
    fn narrowing_hint(&self) -> &str {
        "Ask for fewer rows, fewer fields or a shorter time range, or aggregate."
    }

    /// One call. `input` is the JSON object the model wrote; the tool checks it and answers
    /// [`ToolError::Input`] when it does not fit.
    fn call(
        self: Arc<Self>,
        context: ToolContext,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>>;
}

/// The limits on every tool call.
#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub call_timeout: Duration,
    /// How many calls of one model response run (at the same time). The 10-tool cap limits
    /// the tool set, not the calls: one response may ask for any number, so the calls after
    /// these get an error result and are not run.
    pub max_calls_per_step: usize,
    pub max_text_bytes: usize,
    pub max_images: usize,
    pub max_image_bytes: usize,
}

/// A tool set that cannot be used.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("at most 10 tools, got {0}")]
    TooMany(usize),
    #[error("tool name {0:?} must be 1 to 64 ASCII letters, digits, `_` or `-`")]
    InvalidName(String),
    #[error("tool {0} is registered twice")]
    Duplicate(String),
    #[error("tool {0} has no description")]
    NoDescription(String),
    #[error("tool {0}'s input schema must be a JSON Schema object with \"type\": \"object\"")]
    InvalidSchema(String),
}

/// How a tool call ended, for the logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    InputError,
    Failed,
    Denied,
    Revoked,
    Timeout,
    UnknownTool,
    TooLarge,
    InvalidImage,
    OverCallLimit,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InputError => "input_error",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
            Self::Timeout => "timeout",
            Self::UnknownTool => "unknown_tool",
            Self::TooLarge => "too_large",
            Self::InvalidImage => "invalid_image",
            Self::OverCallLimit => "over_call_limit",
        }
    }
}

const FAILED_MESSAGE: &str = "The tool failed for a reason outside your control. You may call \
it once more; if it fails again, answer without it and say which data could not be read.";

const DENIED_MESSAGE: &str = "The service refused this request for the user: the user may not \
read this data, or it does not exist in the user's organisation. Do not call the tool again with \
the same input; tell the user which data could not be read.";

/// A step's tool calls ended the question: the user's access ended ([`ToolError::Revoked`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the user's access ended during a tool call")]
pub struct AccessRevoked;

/// The tools of the main agent.
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    limits: ToolLimits,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .finish()
    }
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>, limits: ToolLimits) -> Result<Self, RegistryError> {
        if tools.len() > MAX_TOOLS {
            return Err(RegistryError::TooMany(tools.len()));
        }
        let mut by_name = BTreeMap::new();
        for tool in tools {
            let spec = tool.spec();
            check_spec(&spec)?;
            if by_name.insert(spec.name.clone(), tool).is_some() {
                return Err(RegistryError::Duplicate(spec.name));
            }
        }
        Ok(Self {
            tools: by_name,
            limits,
        })
    }

    /// The tool definitions, in name order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn limits(&self) -> ToolLimits {
        self.limits
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Runs one turn's calls concurrently, each under the time limit and in its own task (a
    /// panicking tool fails only its own call). Only the first
    /// [`ToolLimits::max_calls_per_step`] calls run; each later one gets an error result.
    /// Returns one result per call, in the order of the calls. Dropping the future cancels the
    /// calls still running.
    ///
    /// # Errors
    ///
    /// [`AccessRevoked`] as soon as one call ends with [`ToolError::Revoked`]: the calls still
    /// running are cancelled and no result is returned, so the question can end.
    pub async fn run(
        &self,
        context: &ToolContext,
        calls: &[ToolCall],
    ) -> Result<Vec<ToolResult>, AccessRevoked> {
        let mut results: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        let mut started = Vec::with_capacity(calls.len());
        let mut tasks = JoinSet::new();
        let mut index_of = HashMap::new();
        for (index, call) in calls.iter().enumerate() {
            started.push(Instant::now());
            if index >= self.limits.max_calls_per_step {
                let output = ToolOutput::error(format!(
                    "Not run: at most {} tool calls run in one step. Call the tools again in \
                     the next step for what is still missing, or ask for more in fewer calls.",
                    self.limits.max_calls_per_step
                ));
                log_call(
                    context,
                    call,
                    Outcome::OverCallLimit,
                    started[index],
                    &output,
                );
                results[index] = Some(output);
                continue;
            }
            let Some(tool) = self.tools.get(&call.name) else {
                let names: Vec<&str> = self.tools.keys().map(String::as_str).collect();
                let output = ToolOutput::error(format!(
                    "There is no tool named {:?}. The tools are: {}.",
                    truncate(&call.name, MAX_NAME_CHARS),
                    names.join(", ")
                ));
                log_call(context, call, Outcome::UnknownTool, started[index], &output);
                results[index] = Some(output);
                continue;
            };
            if !call.input.is_object() {
                let output = ToolOutput::error(
                    "The input must be a JSON object that follows the tool's input schema.",
                );
                log_call(context, call, Outcome::InputError, started[index], &output);
                results[index] = Some(output);
                continue;
            }
            let call_future = Arc::clone(tool).call(context.clone(), call.input.clone());
            let timeout = self.limits.call_timeout;
            let handle =
                tasks.spawn(async move { tokio::time::timeout(timeout, call_future).await });
            index_of.insert(handle.id(), index);
        }
        while let Some(joined) = tasks.join_next_with_id().await {
            let (id, outcome) = match joined {
                Ok((id, Ok(Ok(output)))) => (id, Ok(output)),
                Ok((id, Ok(Err(ToolError::Input(message))))) => {
                    (id, Err((Outcome::InputError, ToolOutput::error(message))))
                }
                Ok((id, Ok(Err(ToolError::Failed)))) => (
                    id,
                    Err((Outcome::Failed, ToolOutput::error(FAILED_MESSAGE))),
                ),
                Ok((id, Ok(Err(ToolError::Denied)))) => (
                    id,
                    Err((Outcome::Denied, ToolOutput::error(DENIED_MESSAGE))),
                ),
                Ok((id, Ok(Err(ToolError::Revoked)))) => {
                    if let Some(&index) = index_of.get(&id) {
                        log_call(
                            context,
                            &calls[index],
                            Outcome::Revoked,
                            started[index],
                            &ToolOutput::error(""),
                        );
                    }
                    // The other calls of the step are cancelled; none of their results goes on.
                    tasks.abort_all();
                    return Err(AccessRevoked);
                }
                Ok((id, Err(_elapsed))) => (
                    id,
                    Err((
                        Outcome::Timeout,
                        ToolOutput::error(format!(
                            "The tool did not finish within {} seconds. Ask for less data or a \
                             shorter time range.",
                            self.limits.call_timeout.as_secs()
                        )),
                    )),
                ),
                // A panic: the task's error carries the panic message, which is not logged.
                Err(error) => (
                    error.id(),
                    Err((Outcome::Failed, ToolOutput::error(FAILED_MESSAGE))),
                ),
            };
            let Some(&index) = index_of.get(&id) else {
                continue;
            };
            let (outcome, output) = match outcome {
                Ok(output) => {
                    self.check_output(&narrowing_hint_of(&self.tools, &calls[index]), output)
                }
                Err(failed) => failed,
            };
            log_call(context, &calls[index], outcome, started[index], &output);
            results[index] = Some(output);
        }
        Ok(calls
            .iter()
            .zip(results)
            .map(|(call, output)| ToolResult {
                call_id: call.id.clone(),
                output: output.unwrap_or_else(|| ToolOutput::error(FAILED_MESSAGE)),
            })
            .collect())
    }

    /// The limits on what a tool hands the model. A result over them is replaced by an error
    /// that tells the model how to ask for less.
    fn check_output(&self, hint: &str, output: ToolOutput) -> (Outcome, ToolOutput) {
        let text_bytes = output.text_bytes();
        if text_bytes > self.limits.max_text_bytes {
            return (
                Outcome::TooLarge,
                ToolOutput::error(format!(
                    "The result has {text_bytes} bytes of text, over the limit of {} bytes, so \
                     it was not returned. {hint}",
                    self.limits.max_text_bytes
                )),
            );
        }
        let mut images = 0_usize;
        for part in &output.content {
            let ToolContent::Image { data, .. } = part else {
                continue;
            };
            images += 1;
            if data.len() > self.limits.max_image_bytes {
                return (
                    Outcome::TooLarge,
                    ToolOutput::error(format!(
                        "An image of the result is over the limit of {} bytes, so the result \
                         was not returned. Ask for a smaller chart or fewer series.",
                        self.limits.max_image_bytes
                    )),
                );
            }
            if base64::engine::general_purpose::STANDARD
                .decode(data)
                .is_err()
            {
                return (Outcome::InvalidImage, ToolOutput::error(FAILED_MESSAGE));
            }
        }
        if images > self.limits.max_images {
            return (
                Outcome::TooLarge,
                ToolOutput::error(format!(
                    "The result has {images} images, over the limit of {}, so it was not \
                     returned. Ask for fewer charts.",
                    self.limits.max_images
                )),
            );
        }
        if output.content.is_empty() {
            return (
                Outcome::Ok,
                ToolOutput {
                    content: vec![ToolContent::Text("(no result)".into())],
                    is_error: output.is_error,
                },
            );
        }
        (Outcome::Ok, output)
    }
}

fn narrowing_hint_of(tools: &BTreeMap<String, Arc<dyn Tool>>, call: &ToolCall) -> String {
    tools
        .get(&call.name)
        .map(|tool| tool.narrowing_hint().to_owned())
        .unwrap_or_default()
}

fn log_call(
    context: &ToolContext,
    call: &ToolCall,
    outcome: Outcome,
    started: Instant,
    output: &ToolOutput,
) {
    tracing::info!(
        run_id = %context.run_id,
        conversation_id = %context.conversation_id,
        tenant_id = %context.principal.tenant_id,
        tool = truncate(&call.name, MAX_NAME_CHARS),
        outcome = outcome.as_str(),
        duration_ms = millis(started.elapsed()),
        result_bytes = output.text_bytes(),
        "tool call completed"
    );
}

/// Milliseconds, saturating.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The most characters of one text value from a service that a tool hands the model. Readings,
/// data point keys and model names are written by devices and members, so they are untrusted
/// input to the model: the cap keeps one value from filling the result or carrying a long text,
/// it does not make the text trustworthy (see README.md, 「工具」).
pub const MAX_RESULT_TEXT_CHARS: usize = 256;

/// Ends a text value that [`cap_text`] cut.
pub const CUT_MARKER: &str = "…[cut]";

/// Cuts every string in `value` (array elements and object values, recursively; object keys
/// are the tool's own) to [`MAX_RESULT_TEXT_CHARS`] characters, ending a cut one with
/// [`CUT_MARKER`]. Tools call it on what they built from a service's answer.
pub fn cap_text(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Some((end, _)) = text.char_indices().nth(MAX_RESULT_TEXT_CHARS) {
                text.truncate(end);
                text.push_str(CUT_MARKER);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(cap_text),
        Value::Object(fields) => fields.values_mut().for_each(cap_text),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// At most `max` characters of `text`, cut at a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => text.get(..end).unwrap_or(text),
        None => text,
    }
}

fn is_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn check_spec(spec: &ToolSpec) -> Result<(), RegistryError> {
    if !is_tool_name(&spec.name) {
        return Err(RegistryError::InvalidName(spec.name.clone()));
    }
    if spec.description.trim().is_empty() {
        return Err(RegistryError::NoDescription(spec.name.clone()));
    }
    let is_object_schema = spec
        .input_schema
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "object");
    if !is_object_schema {
        return Err(RegistryError::InvalidSchema(spec.name.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn long_text_is_cut_with_a_visible_marker_and_short_text_is_kept() {
        let long: String = "é".repeat(MAX_RESULT_TEXT_CHARS + 1);
        let exact: String = "a".repeat(MAX_RESULT_TEXT_CHARS);
        let mut value = json!({
            "rows": [[long.clone(), 1.5, true, null], [exact.clone()]],
            "nested": {"name": long},
            "short": "ok",
        });
        cap_text(&mut value);
        let cut = format!("{}{CUT_MARKER}", "é".repeat(MAX_RESULT_TEXT_CHARS));
        assert_eq!(
            value,
            json!({
                "rows": [[cut.clone(), 1.5, true, null], [exact]],
                "nested": {"name": cut},
                "short": "ok",
            })
        );
    }
}
