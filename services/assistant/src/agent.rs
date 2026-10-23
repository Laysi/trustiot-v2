//! The main agent loop (decision 31): the model answers or calls tools, the tools run, their
//! results go back, until the model answers. No step limit; the question's token budget bounds
//! it (crate::budget). Each conversation is its own `&mut`, so questions of different users
//! (and different conversations) run concurrently without sharing anything but the model
//! client and the tools.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tokio::time::Instant;
use uuid::Uuid;

use crate::budget::Budget;
use crate::history::{Conversation, EntryKind, Preamble, Principal};
use crate::model::{
    CacheTtl, Effort, ModelClient, ModelError, ModelErrorKind, ModelRequest, ModelTurn,
    NativeMessage, Stop, TokenCount, ToolResult,
};
use crate::prompt;
use crate::tools::{Delegation, ToolContext, ToolOutput, ToolRegistry, millis};

/// The least output room a call is sent with (or the configured output limit, if lower): with
/// less, the model could not finish even a short answer.
const MIN_CALL_OUTPUT_TOKENS: u64 = 1_024;

/// How the agent calls the model and bounds a question.
#[derive(Clone, Copy, Debug)]
pub struct AgentSettings {
    pub effort: Effort,
    pub cache_ttl: CacheTtl,
    pub max_output_tokens: u32,
    pub call_timeout: Duration,
    pub budget_tokens: u64,
    pub wind_down_percent: u8,
}

/// A finished answer.
#[derive(Clone, Debug)]
pub struct Answer {
    pub text: String,
    /// The answer was cut off at the output limit.
    pub truncated: bool,
    /// Model calls this question took.
    pub steps: u32,
    /// Tokens the question's calls processed.
    pub usage: TokenCount,
}

/// Why a question has no answer. The message is what the user may see: it never carries the
/// underlying error (J2); the logs have the kind, status and error type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("The assistant took too long to answer. Please try again.")]
    ModelTimeout,
    #[error("The assistant is unavailable right now. Please try again later.")]
    ModelUnavailable,
    #[error("The assistant could not answer this question.")]
    ModelFailed,
    #[error("The assistant cannot help with this request.")]
    Refused,
    #[error("This conversation is too long to continue. Please start a new conversation.")]
    ContextFull,
    #[error(
        "This question needed more work than its budget allows. Please ask a narrower question."
    )]
    BudgetExhausted,
    /// The question reached its deadline, a little before its run delegation expires
    /// ([`Delegation::deadline`]): without a delegation no tool could read data as the user.
    #[error(
        "This question took longer than the assistant may work on one question. Please ask a \
         narrower question."
    )]
    RunExpired,
    /// The user's access ended while the question ran: the identity service refused to act for
    /// the user any longer (signed out, session ended, removed from the organisation).
    #[error(
        "Your access changed while the assistant was working, so it stopped. Please sign in again."
    )]
    AccessRevoked,
}

impl AgentError {
    fn as_str(self) -> &'static str {
        match self {
            Self::ModelTimeout => "model_timeout",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelFailed => "model_failed",
            Self::Refused => "refused",
            Self::ContextFull => "context_full",
            Self::BudgetExhausted => "budget_exhausted",
            Self::RunExpired => "run_expired",
            Self::AccessRevoked => "access_revoked",
        }
    }

    fn from_model(error: &ModelError) -> Self {
        match error.kind {
            ModelErrorKind::Timeout => Self::ModelTimeout,
            ModelErrorKind::Unavailable => Self::ModelUnavailable,
            ModelErrorKind::Rejected | ModelErrorKind::InvalidResponse => Self::ModelFailed,
        }
    }
}

/// The main agent.
pub struct Agent {
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    settings: AgentSettings,
    model_calls: Counter<u64>,
}

impl Agent {
    /// Created after service-kit installed the meter provider (in `start`).
    pub fn new(model: Arc<dyn ModelClient>, tools: ToolRegistry, settings: AgentSettings) -> Self {
        let model_calls = opentelemetry::global::meter("assistant")
            .u64_counter("assistant.model_calls")
            .with_description(
                "Main agent model calls by outcome and cache use (cache=miss: nothing was read \
                 from the prompt cache)",
            )
            .build();
        Self {
            model,
            tools,
            settings,
            model_calls,
        }
    }

    /// How many tools the agent has.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// A new conversation for `principal`, with today's system prompt, tool set and model
    /// settings fixed for its whole life. `tenant_context` is the tenant's own text; it is
    /// escaped and marked as data ([`prompt::tenant_context_block`]).
    pub fn start_conversation(&self, principal: Principal, tenant_context: &str) -> Conversation {
        let preamble = Preamble::new(
            prompt::system_prompt(),
            prompt::tenant_context_block(tenant_context),
            self.tools.specs(),
            self.settings.effort,
            self.settings.cache_ttl,
        );
        Conversation::new(principal, Arc::new(preamble))
    }

    /// Answers one question in `conversation`. `delegation` lets the tools call other services
    /// as the conversation's user while the question runs; the question is logged under its
    /// run ID and ends at its deadline ([`AgentError::RunExpired`]), so it never outlives the
    /// delegation. A revoked delegation ends it at the tool call that found out
    /// ([`AgentError::AccessRevoked`]).
    ///
    /// The history only grows. A model turn joins it together with the messages it answers (the
    /// tool results, a wind-down note), so a failed or cancelled question leaves a history the
    /// model accepts on the next question.
    pub async fn ask(
        &self,
        conversation: &mut Conversation,
        question: &str,
        delegation: Arc<dyn Delegation>,
    ) -> Result<Answer, AgentError> {
        let mut run = Run {
            id: delegation.run_id().unwrap_or_else(Uuid::now_v7),
            started: Instant::now(),
            steps: 0,
            usage: TokenCount::default(),
            budget: Budget::new(self.settings.budget_tokens, self.settings.wind_down_percent),
        };
        let deadline = delegation.deadline();
        // Every await of the loop leaves the history whole (a turn and its results join it
        // together), so cutting the question off at the deadline is safe. A question with no
        // time left does not start: nothing is sent or added to the history.
        let result = match deadline {
            Some(deadline) if deadline <= Instant::now() => Err(AgentError::RunExpired),
            Some(deadline) => tokio::time::timeout_at(
                deadline,
                self.run(&mut run, conversation, question, delegation),
            )
            .await
            .unwrap_or(Err(AgentError::RunExpired)),
            None => self.run(&mut run, conversation, question, delegation).await,
        };
        tracing::info!(
            run_id = %run.id,
            conversation_id = %conversation.id(),
            tenant_id = %conversation.principal().tenant_id,
            steps = run.steps,
            outcome = result.as_ref().map_or_else(|error| error.as_str(), |_| "answered"),
            duration_ms = millis(run.started.elapsed()),
            budget.spent = run.budget.spent(),
            budget.limit = run.budget.limit(),
            usage.estimated = run.usage.estimated,
            "agent run finished"
        );
        result
    }

    async fn run(
        &self,
        run: &mut Run,
        conversation: &mut Conversation,
        question: &str,
        delegation: Arc<dyn Delegation>,
    ) -> Result<Answer, AgentError> {
        let question = self
            .model
            .user_text(question)
            .map_err(|_| AgentError::ModelFailed)?;
        conversation.append(EntryKind::Question, question);
        let context = ToolContext {
            principal: conversation.principal(),
            conversation_id: conversation.id(),
            run_id: run.id,
            delegation,
        };
        let mut wound_down = false;
        loop {
            let mut pending = Vec::new();
            if run.budget.winding_down() && !wound_down {
                pending.push(
                    self.model
                        .system_note(prompt::WIND_DOWN)
                        .map_err(|_| AgentError::ModelFailed)?,
                );
            }
            // The call must fit what is left of the budget before it is sent: its estimated
            // input, and an output limit lowered to the room that leaves.
            let input = self.estimate_input(conversation, &pending);
            let room = run.budget.remaining().saturating_sub(input);
            let max_output = self.settings.max_output_tokens;
            if room < u64::from(max_output).min(MIN_CALL_OUTPUT_TOKENS) {
                tracing::info!(
                    run_id = %run.id,
                    conversation_id = %conversation.id(),
                    tenant_id = %conversation.principal().tenant_id,
                    input_estimate = input,
                    budget.spent = run.budget.spent(),
                    budget.limit = run.budget.limit(),
                    "model call not sent: it does not fit the budget"
                );
                return Err(AgentError::BudgetExhausted);
            }
            let max_output = u32::try_from(room).map_or(max_output, |room| room.min(max_output));
            let mut turn = self.call(run, conversation, &pending, max_output).await?;
            let Some(message) = turn.message.take() else {
                // No content (a refusal before any output): nothing joins the history, and the
                // note is dropped with it, since a system message must precede the reply.
                return Err(match turn.stop {
                    Stop::Refusal => AgentError::Refused,
                    Stop::ContextWindowExceeded => AgentError::ContextFull,
                    _ => AgentError::ModelFailed,
                });
            };
            wound_down |= !pending.is_empty();
            match turn.stop {
                Stop::ToolUse | Stop::MaxTokens if !turn.tool_calls.is_empty() => {
                    let results = if turn.stop == Stop::MaxTokens {
                        not_run(&turn, prompt::CUT_OFF_RESULT)
                    } else if run.budget.exhausted() {
                        not_run(&turn, prompt::BUDGET_EXHAUSTED_RESULT)
                    } else {
                        // The step is not kept: its tool calls have no results, and no tool
                        // runs again for this question.
                        self.tools
                            .run(&context, &turn.tool_calls)
                            .await
                            .map_err(|_revoked| AgentError::AccessRevoked)?
                    };
                    let results = self
                        .model
                        .tool_results(&results)
                        .map_err(|_| AgentError::ModelFailed)?;
                    commit(conversation, pending, EntryKind::Step, message, &turn);
                    conversation.append(EntryKind::ToolResults, results);
                    if run.budget.exhausted() {
                        return Err(AgentError::BudgetExhausted);
                    }
                }
                Stop::EndTurn | Stop::MaxTokens => {
                    commit(conversation, pending, EntryKind::Answer, message, &turn);
                    if turn.text.trim().is_empty() {
                        return Err(AgentError::ModelFailed);
                    }
                    return Ok(Answer {
                        text: turn.text,
                        truncated: turn.stop == Stop::MaxTokens,
                        steps: run.steps,
                        usage: run.usage,
                    });
                }
                // A refusal after some output, a full context window, a tool-use stop without
                // tool calls, or a stop reason the loop does not handle: the partial turn is
                // not kept.
                Stop::Refusal => return Err(AgentError::Refused),
                Stop::ContextWindowExceeded => return Err(AgentError::ContextFull),
                Stop::ToolUse | Stop::Other(_) => return Err(AgentError::ModelFailed),
            }
        }
    }

    /// A high estimate of the next request's input: the model's last count of the history, plus
    /// estimates of the messages after it and of `pending`. Without a count (the first call
    /// of a conversation), the preamble and every message are estimated.
    fn estimate_input(&self, conversation: &Conversation, pending: &[NativeMessage]) -> u64 {
        let (counted, from) = match conversation.measured() {
            Some(measured) => (measured.tokens, measured.messages),
            None => (
                self.model.estimate_preamble_tokens(conversation.preamble()),
                0,
            ),
        };
        conversation
            .messages()
            .get(from..)
            .unwrap_or_default()
            .iter()
            .chain(pending)
            .fold(counted, |sum, message| {
                sum.saturating_add(self.model.estimate_tokens(message))
            })
    }

    /// One model call under the time limit, logged with its duration and usage.
    async fn call(
        &self,
        run: &mut Run,
        conversation: &Conversation,
        pending: &[NativeMessage],
        max_output_tokens: u32,
    ) -> Result<ModelTurn, AgentError> {
        run.steps = run.steps.saturating_add(1);
        let call_id = Uuid::now_v7();
        let request = ModelRequest {
            call_id,
            tenant: conversation.principal().tenant_id,
            preamble: conversation.preamble(),
            history: conversation.messages(),
            pending,
            max_output_tokens,
        };
        let started = Instant::now();
        let result = tokio::time::timeout(self.settings.call_timeout, self.model.call(request))
            .await
            .unwrap_or_else(|_elapsed| Err(ModelError::new(ModelErrorKind::Timeout)));
        let duration_ms = millis(started.elapsed());
        match result {
            Ok(turn) => {
                run.budget.record(&turn.usage);
                run.usage.add(&turn.usage);
                tracing::info!(
                    run_id = %run.id,
                    conversation_id = %conversation.id(),
                    tenant_id = %conversation.principal().tenant_id,
                    step = run.steps,
                    llm_call_id = %call_id,
                    model = turn.model.as_str(),
                    duration_ms,
                    stop_reason = stop_name(&turn.stop),
                    tool_calls = turn.tool_calls.len(),
                    output_limit = max_output_tokens,
                    usage.input = turn.usage.input,
                    usage.cache_write = turn.usage.cache_write,
                    usage.cache_read = turn.usage.cache_read,
                    usage.output = turn.usage.output,
                    usage.estimated = turn.usage.estimated,
                    budget.spent = run.budget.spent(),
                    budget.limit = run.budget.limit(),
                    "model call completed"
                );
                self.model_calls.add(
                    1,
                    &[
                        KeyValue::new("outcome", "completed"),
                        KeyValue::new(
                            "cache",
                            if turn.usage.cache_read > 0 {
                                "hit"
                            } else {
                                "miss"
                            },
                        ),
                    ],
                );
                Ok(turn)
            }
            Err(error) => {
                tracing::warn!(
                    run_id = %run.id,
                    conversation_id = %conversation.id(),
                    tenant_id = %conversation.principal().tenant_id,
                    step = run.steps,
                    llm_call_id = %call_id,
                    duration_ms,
                    kind = error.kind.as_str(),
                    status = error.status,
                    error_type = error.error_type.as_deref(),
                    "model call failed"
                );
                self.model_calls
                    .add(1, &[KeyValue::new("outcome", error.kind.as_str())]);
                Err(AgentError::from_model(&error))
            }
        }
    }
}

struct Run {
    id: Uuid,
    started: Instant,
    steps: u32,
    usage: TokenCount,
    budget: Budget,
}

/// Appends a model turn with the notes sent right before it, and records the model's count of
/// the history up to it.
fn commit(
    conversation: &mut Conversation,
    pending: Vec<NativeMessage>,
    kind: EntryKind,
    message: NativeMessage,
    turn: &ModelTurn,
) {
    for note in pending {
        conversation.append(EntryKind::Note, note);
    }
    conversation.append(kind, message);
    if let Some(tokens) = turn.context_tokens {
        conversation.record_measured(tokens);
    }
}

/// The same error result for every tool call of a turn.
fn not_run(turn: &ModelTurn, text: &str) -> Vec<ToolResult> {
    turn.tool_calls
        .iter()
        .map(|call| ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::error(text),
        })
        .collect()
}

fn stop_name(stop: &Stop) -> &'static str {
    match stop {
        Stop::EndTurn => "end_turn",
        Stop::ToolUse => "tool_use",
        Stop::MaxTokens => "max_tokens",
        Stop::Refusal => "refusal",
        Stop::ContextWindowExceeded => "context_window_exceeded",
        Stop::Other(_) => "other",
    }
}
