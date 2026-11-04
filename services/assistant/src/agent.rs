//! The main agent loop (decision 31): the model answers or calls tools, the tools run, their
//! results go back, until the model answers. No step limit; the question's token budget bounds
//! it (crate::budget). Each conversation is its own `&mut`, so questions of different users
//! (and different conversations) run concurrently without sharing anything but the model
//! client and the tools.

use std::sync::Arc;
use std::time::Duration;

use contract::MessageKey;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use crate::answer::{self, ANSWER_TOOL, MAX_ANSWER_BYTES, Release, TextDecoder};
use crate::answer_guard::{self, REMOVED_PLACEHOLDER, RemovedContent};
use crate::budget::Budget;
use crate::history::{Conversation, EntryKind, Preamble, Principal};
use crate::model::{
    CacheTtl, Effort, EncodeError, ModelClient, ModelError, ModelErrorKind, ModelRequest,
    ModelTurn, NativeMessage, Stop, TokenCount, ToolResult, TurnEvent,
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
    /// The answer, after the answer guard removed code, HTML and SQL from it
    /// ([`crate::answer_guard`]): the text blocks of the answer the history holds, joined.
    pub text: String,
    /// What the answer guard removed; empty when nothing. The chat shows
    /// [`crate::answer_guard::CONTENT_REMOVED`] when it is not. The answer's history entry
    /// carries it too ([`Conversation::removed`]), so the store keeps it with the message and the
    /// conversation API returns it with the answer (Laysi/trustiot-v2#646).
    pub removed: RemovedContent,
    /// The answer was cut off at the output limit.
    pub truncated: bool,
    /// Model calls this question took.
    pub steps: u32,
    /// Tokens the question's calls processed.
    pub usage: TokenCount,
}

/// One answered round of a conversation being copied ([`Agent::copied_conversation`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopiedRound {
    /// The question's text.
    pub question: String,
    /// The answer's text, as the chat shows it.
    pub answer: String,
    /// What the answer guard removed from the answer: the copy's answer keeps it, so the copy's
    /// chat shows it as the source's does (Laysi/trustiot-v2#646). It is never sent to the model.
    pub removed: RemovedContent,
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
    /// The question's run was started for another tenant or user than the conversation's
    /// ([`Delegation::principal`]): a fault of the caller. Nothing was sent and the history is
    /// unchanged.
    #[error("The assistant could not start this question. Please try again.")]
    NotTheConversationsRun,
}

impl AgentError {
    /// Every error, for reading a recorded kind back ([`AgentError::from_kind`]).
    pub const ALL: [Self; 9] = [
        Self::ModelTimeout,
        Self::ModelUnavailable,
        Self::ModelFailed,
        Self::Refused,
        Self::ContextFull,
        Self::BudgetExhausted,
        Self::RunExpired,
        Self::AccessRevoked,
        Self::NotTheConversationsRun,
    ];

    /// The error's kind, an identifier (`model_timeout`, `run_expired`, ...): the `outcome` of
    /// the `agent run finished` log line, and the failure code of the question's run record.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::ModelTimeout => "model_timeout",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelFailed => "model_failed",
            Self::Refused => "refused",
            Self::ContextFull => "context_full",
            Self::BudgetExhausted => "budget_exhausted",
            Self::RunExpired => "run_expired",
            Self::AccessRevoked => "access_revoked",
            Self::NotTheConversationsRun => "not_the_conversations_run",
        }
    }

    /// The error whose [`AgentError::kind`] is `kind`.
    #[must_use]
    pub fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.kind() == kind)
    }

    /// The message the user sees, as a message key: the translations of the fixed English text
    /// of `Display` (`frontend/src/i18n/locales/`). The chat API answers with it, never with the
    /// underlying error (J2).
    #[must_use]
    pub fn message_key(self) -> MessageKey {
        match self {
            Self::ModelTimeout => MessageKey::new("question.model_timeout"),
            Self::ModelUnavailable => MessageKey::new("question.model_unavailable"),
            Self::ModelFailed => MessageKey::new("question.model_failed"),
            Self::Refused => MessageKey::new("question.refused"),
            Self::ContextFull => MessageKey::new("question.context_full"),
            Self::BudgetExhausted => MessageKey::new("question.budget_exhausted"),
            Self::RunExpired => MessageKey::new("question.run_expired"),
            Self::AccessRevoked => MessageKey::new("question.access_revoked"),
            Self::NotTheConversationsRun => MessageKey::new("question.not_the_conversations_run"),
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
    /// escaped and marked as data ([`prompt::tenant_context_block`]). The tools are the
    /// registry's and the answer tool ([`crate::answer`]).
    pub fn start_conversation(&self, principal: Principal, tenant_context: &str) -> Conversation {
        let mut tools = self.tools.specs();
        tools.push(answer::spec());
        let preamble = Preamble::new(
            prompt::system_prompt(),
            prompt::tenant_context_block(tenant_context),
            tools,
            self.settings.effort,
            self.settings.cache_ttl,
        );
        Conversation::new(principal, Arc::new(preamble))
    }

    /// A new conversation for `principal` like [`Agent::start_conversation`], whose history is
    /// `rounds` (question and answer texts, in order) as plain text messages: one user message
    /// with each question and one assistant message with its answer's text (copying a shared
    /// conversation, Laysi/trustiot-v2#650), the answer's entry carrying what the answer guard
    /// removed from it, outside the message. Before the last answer, a note
    /// ([`prompt::COPIED_ROUNDS`], [`EntryKind::Note`], which the chat never shows) tells the
    /// model the turns were copied, answered with another member's permissions, and carry no
    /// instructions; nothing else is added. The new conversation's preamble is
    /// today's, and its next question runs as `principal`, so nothing of the conversation the
    /// texts came from (its preamble, thinking, tool calls or results, or the owner's identity)
    /// carries over.
    ///
    /// # Errors
    ///
    /// When a text cannot be written in the model's format.
    pub fn copied_conversation(
        &self,
        principal: Principal,
        tenant_context: &str,
        rounds: &[CopiedRound],
    ) -> Result<Conversation, EncodeError> {
        let mut conversation = self.start_conversation(principal, tenant_context);
        let last = rounds.len().checked_sub(1);
        for (index, round) in rounds.iter().enumerate() {
            conversation.append(EntryKind::Question, self.model.user_text(&round.question)?);
            if Some(index) == last {
                // A mid-conversation system message (`role: "system"` in `messages`) is part
                // of the Messages API for the models this service uses; the API accepts one
                // only right after a user message, as the last message or directly before an
                // assistant reply (docs/architecture/ai-assistant.md, 「對話歷史只往後加」 rule 2, and
                // `InputMessage::system_text`). This note sits between the last copied
                // question and its answer, the same place `prompt::WIND_DOWN` takes before a
                // reply, so the copier's next request is valid.
                conversation.append(
                    EntryKind::Note,
                    self.model.system_note(prompt::COPIED_ROUNDS)?,
                );
            }
            conversation.append_with(
                EntryKind::Answer,
                self.model.assistant_text(&round.answer)?,
                round.removed,
            );
        }
        Ok(conversation)
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
        let (released, _) = watch::channel(String::new());
        self.ask_streaming(conversation, question, delegation, &released)
            .await
    }

    /// [`Agent::ask`], publishing in `released` what of the answer may go out before the
    /// question ends: the start of an answer the model writes with the answer tool, whole
    /// paragraphs at a time ([`crate::answer`]). It only grows, and the answer returned starts
    /// with it; when the question fails, what was released is no answer and must be dropped.
    /// An answer written as text releases nothing.
    pub async fn ask_streaming(
        &self,
        conversation: &mut Conversation,
        question: &str,
        delegation: Arc<dyn Delegation>,
        released: &watch::Sender<String>,
    ) -> Result<Answer, AgentError> {
        let mut run = Run {
            id: delegation.run_id().unwrap_or_else(Uuid::now_v7),
            started: Instant::now(),
            steps: 0,
            usage: TokenCount::default(),
            budget: Budget::new(self.settings.budget_tokens, self.settings.wind_down_percent),
        };
        let deadline = delegation.deadline();
        // A run acts for the user it was started for; the tools would call as that user with
        // this conversation's principal, so a run of anyone else does not start.
        let foreign = delegation
            .principal()
            .is_some_and(|principal| principal != conversation.principal());
        // Every await of the loop leaves the history whole (a turn and its results join it
        // together), so cutting the question off at the deadline is safe. A question with no
        // time left does not start: nothing is sent or added to the history.
        let result = match deadline {
            _ if foreign => Err(AgentError::NotTheConversationsRun),
            Some(deadline) if deadline <= Instant::now() => Err(AgentError::RunExpired),
            Some(deadline) => tokio::time::timeout_at(
                deadline,
                self.run(&mut run, conversation, question, delegation, released),
            )
            .await
            .unwrap_or(Err(AgentError::RunExpired)),
            None => {
                self.run(&mut run, conversation, question, delegation, released)
                    .await
            }
        };
        tracing::info!(
            run_id = %run.id,
            conversation_id = %conversation.id(),
            tenant_id = %conversation.principal().tenant_id,
            steps = run.steps,
            outcome = result.as_ref().map_or_else(|error| error.kind(), |_| "answered"),
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
        released: &watch::Sender<String>,
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
        // Only a conversation that started with the answer tool treats a call of that name as
        // its answer; for an older one it is a tool the registry does not know.
        let answer_tool = conversation
            .preamble()
            .tools()
            .iter()
            .any(|tool| tool.name == ANSWER_TOOL);
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
            let mut filter = AnswerFilter::new(answer_tool, released);
            let mut turn = self
                .call(
                    run,
                    conversation,
                    &pending,
                    max_output,
                    &mut |event: TurnEvent<'_>| filter.observe(event),
                )
                .await?;
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
            let answers = answer_tool
                && turn
                    .tool_calls
                    .first()
                    .is_some_and(|call| call.name == ANSWER_TOOL);
            match turn.stop {
                // The turn's first tool call is the answer: the question's last turn, unless
                // its text is blank.
                Stop::ToolUse | Stop::MaxTokens if answers => {
                    let delivered = self.delivered(run, conversation, &turn, filter.decoder())?;
                    if delivered.text.trim().is_empty() {
                        // Blank text releases nothing, so nothing went out unless an earlier
                        // call's text did, which no answer could take back now (a fault).
                        continues_release(run, conversation, "", &released.borrow(), "blank")?;
                        // The model is told and writes the answer again.
                        let results = self
                            .model
                            .tool_results(&blank_answer_results(&turn))
                            .map_err(|_| AgentError::ModelFailed)?;
                        commit(
                            conversation,
                            pending,
                            EntryKind::Step,
                            message,
                            RemovedContent::default(),
                            &turn,
                        );
                        conversation.append(EntryKind::ToolResults, results);
                        if run.budget.exhausted() {
                            return Err(AgentError::BudgetExhausted);
                        }
                        continue;
                    }
                    let released = released.borrow().clone();
                    return self.answer_with_tool(
                        run,
                        conversation,
                        pending,
                        &turn,
                        delivered,
                        &released,
                    );
                }
                Stop::ToolUse | Stop::MaxTokens if !turn.tool_calls.is_empty() => {
                    let results = if turn.stop == Stop::MaxTokens {
                        not_run(&turn, prompt::CUT_OFF_RESULT)
                    } else if run.budget.exhausted() {
                        not_run(&turn, prompt::BUDGET_EXHAUSTED_RESULT)
                    } else {
                        // The step is not kept: its tool calls have no results, and no tool
                        // runs again for this question.
                        self.run_tools(&context, &turn, answer_tool).await?
                    };
                    let results = self
                        .model
                        .tool_results(&results)
                        .map_err(|_| AgentError::ModelFailed)?;
                    commit(
                        conversation,
                        pending,
                        EntryKind::Step,
                        message,
                        RemovedContent::default(),
                        &turn,
                    );
                    conversation.append(EntryKind::ToolResults, results);
                    if run.budget.exhausted() {
                        return Err(AgentError::BudgetExhausted);
                    }
                }
                Stop::EndTurn | Stop::MaxTokens => {
                    let blank = turn.text.trim().is_empty();
                    // The guard runs before the answer joins the history: the history and the
                    // caller get the same cleaned text.
                    let (message, text, removed) = self.guard(message, &turn.text)?;
                    // What an answer call of an earlier turn released must start this answer
                    // too, or the stream would have sent text that is not the answer.
                    continues_release(run, conversation, &text, &released.borrow(), "text")?;
                    commit(
                        conversation,
                        pending,
                        EntryKind::Answer,
                        message,
                        removed,
                        &turn,
                    );
                    if blank {
                        return Err(AgentError::ModelFailed);
                    }
                    if !removed.is_empty() {
                        tracing::info!(
                            run_id = %run.id,
                            conversation_id = %conversation.id(),
                            tenant_id = %conversation.principal().tenant_id,
                            removed = removed.names(),
                            "removed content from an answer"
                        );
                    }
                    return Ok(Answer {
                        text,
                        removed,
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

    /// The text of the answer tool call that opens `turn` (its first tool call), read from the
    /// call's input, or from what `decoder` read of it when the output limit cut it off.
    ///
    /// # Errors
    ///
    /// [`AgentError::ModelFailed`] when the input is not the tool's (`{"text": "..."}`), did not
    /// arrive whole without the output limit cutting it, is longer than [`MAX_ANSWER_BYTES`], or
    /// is not what `decoder` read of it as it streamed.
    fn delivered(
        &self,
        run: &Run,
        conversation: &Conversation,
        turn: &ModelTurn,
        decoder: Option<&TextDecoder>,
    ) -> Result<Delivered, AgentError> {
        let fail = |reason: &'static str| {
            tracing::warn!(
                run_id = %run.id,
                conversation_id = %conversation.id(),
                tenant_id = %conversation.principal().tenant_id,
                reason,
                "the answer tool's input was not accepted"
            );
            AgentError::ModelFailed
        };
        let Some(call) = turn.tool_calls.first() else {
            return Err(fail("no_call"));
        };
        // Cut off at the output limit: what was read of the text is the answer, as a text answer
        // cut off there is, whatever the parser made of the cut input.
        let cut_off = turn.stop == Stop::MaxTokens
            && decoder.is_some_and(|decoder| !decoder.complete() && !decoder.failed());
        if turn.malformed_calls.contains(&call.id) || cut_off {
            return match decoder {
                Some(decoder) if turn.stop == Stop::MaxTokens && !decoder.failed() => {
                    Ok(Delivered {
                        text: decoder.text().to_owned(),
                        truncated: true,
                    })
                }
                _ => Err(fail("malformed")),
            };
        }
        let text = match &call.input {
            Value::Object(input) if input.len() == 1 => input.get("text").and_then(Value::as_str),
            _ => None,
        };
        let Some(text) = text else {
            return Err(fail("not_the_tools_input"));
        };
        if text.len() > MAX_ANSWER_BYTES {
            return Err(fail("too_long"));
        }
        // What was released came from what the decoder read; the answer stored comes from the
        // parsed input. When they differ (a key given twice, whose last value the parser keeps;
        // any other disagreement), the input is malformed. A reply that did not stream (no
        // decoder) released nothing.
        if let Some(decoder) = decoder
            && !(decoder.complete() && decoder.text() == text)
        {
            return Err(fail("decoder_disagrees"));
        }
        Ok(Delivered {
            text: text.to_owned(),
            truncated: false,
        })
    }

    /// Ends the question with the answer the answer tool delivered. The turn that called the
    /// tool is not kept: the history gets the answer as a text message instead, after the last
    /// tool results (or the question), as a copied conversation holds its answers. Nothing else
    /// of the turn (its thinking, text, or the calls after the answer) is kept or ever sent.
    ///
    /// # Errors
    ///
    /// [`AgentError::ModelFailed`] when the stored answer would not start with what was
    /// `released` for the question (a fault: the stream could not take it back), or cannot be
    /// written in the model's format.
    fn answer_with_tool(
        &self,
        run: &Run,
        conversation: &mut Conversation,
        pending: Vec<NativeMessage>,
        turn: &ModelTurn,
        delivered: Delivered,
        released: &str,
    ) -> Result<Answer, AgentError> {
        let guarded = answer_guard::guard_streamed(&delivered.text);
        let removed = guarded.removed;
        let text = if guarded.text.trim().is_empty() {
            REMOVED_PLACEHOLDER.to_owned()
        } else {
            guarded.text
        };
        continues_release(run, conversation, &text, released, "answer_tool")?;
        let message = self
            .model
            .assistant_text(&text)
            .map_err(|_| AgentError::ModelFailed)?;
        commit(
            conversation,
            pending,
            EntryKind::Answer,
            message,
            removed,
            turn,
        );
        if !removed.is_empty() {
            tracing::info!(
                run_id = %run.id,
                conversation_id = %conversation.id(),
                tenant_id = %conversation.principal().tenant_id,
                removed = removed.names(),
                "removed content from an answer"
            );
        }
        Ok(Answer {
            text,
            removed,
            truncated: delivered.truncated,
            steps: run.steps,
            usage: run.usage,
        })
    }

    /// Runs a turn's tool calls, except those that must not run: an answer tool call (the
    /// answer must come alone, after the other results) and a call whose input did not arrive
    /// as a JSON object. Those get an error result. One result per call, in the calls' order.
    async fn run_tools(
        &self,
        context: &ToolContext,
        turn: &ModelTurn,
        answer_tool: bool,
    ) -> Result<Vec<ToolResult>, AgentError> {
        let mut results: Vec<Option<ToolResult>> = vec![None; turn.tool_calls.len()];
        let mut runnable = Vec::new();
        let mut positions = Vec::new();
        for (position, call) in turn.tool_calls.iter().enumerate() {
            let refusal = if answer_tool && call.name == ANSWER_TOOL {
                Some(prompt::ANSWER_NOT_ALONE_RESULT)
            } else if turn.malformed_calls.contains(&call.id) {
                Some(prompt::MALFORMED_INPUT_RESULT)
            } else {
                None
            };
            match refusal {
                Some(text) => {
                    tracing::info!(
                        run_id = %context.run_id,
                        conversation_id = %context.conversation_id,
                        tenant_id = %context.principal.tenant_id,
                        tool = call.name.as_str(),
                        "tool call not run: {}",
                        if call.name == ANSWER_TOOL { "answer_not_alone" } else { "malformed_input" }
                    );
                    results[position] = Some(ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::error(text),
                    });
                }
                None => {
                    runnable.push(call.clone());
                    positions.push(position);
                }
            }
        }
        if !runnable.is_empty() {
            let ran = self
                .tools
                .run(context, &runnable)
                .await
                .map_err(|_revoked| AgentError::AccessRevoked)?;
            for (position, result) in positions.into_iter().zip(ran) {
                results[position] = Some(result);
            }
        }
        Ok(results.into_iter().flatten().collect())
    }

    /// The answer `message` (whose text blocks joined are `text`) with the answer guard applied
    /// to each text block: the message for the history, the answer's text and what was removed.
    /// Nothing removed: `message` and `text` as they are.
    fn guard(
        &self,
        message: NativeMessage,
        text: &str,
    ) -> Result<(NativeMessage, String, RemovedContent), AgentError> {
        let mut removed = RemovedContent::default();
        // Each text block as the rewritten message holds it (`ModelClient::rewrite_text`): an
        // unchanged block as it was, blank ones too; a changed block with its new text, or not at
        // all when that is blank.
        let mut texts = Vec::new();
        let rewritten = self
            .model
            .rewrite_text(
                &message,
                &mut |block| {
                    let guarded = answer_guard::guard(block);
                    if guarded.removed.is_empty() {
                        texts.push(block.to_owned());
                        return None;
                    }
                    removed = removed.union(guarded.removed);
                    if !guarded.text.trim().is_empty() {
                        texts.push(guarded.text.clone());
                    }
                    Some(guarded.text)
                },
                REMOVED_PLACEHOLDER,
            )
            .map_err(|_| AgentError::ModelFailed)?;
        let Some(rewritten) = rewritten else {
            return Ok((message, text.to_owned(), removed));
        };
        // The rewritten message ends with the placeholder's block when no block has text left,
        // so the answer's text is its text blocks joined, as the history holds them.
        if texts.iter().all(|block| block.trim().is_empty()) {
            texts.push(REMOVED_PLACEHOLDER.to_owned());
        }
        Ok((rewritten, texts.concat(), removed))
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

    /// One model call under the time limit, logged with its duration and usage. `observe`
    /// sees the turn's tool calls as they arrive ([`ModelClient::call`]).
    async fn call(
        &self,
        run: &mut Run,
        conversation: &Conversation,
        pending: &[NativeMessage],
        max_output_tokens: u32,
        observe: &mut (dyn FnMut(TurnEvent<'_>) + Send),
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
        let result = tokio::time::timeout(
            self.settings.call_timeout,
            self.model.call(request, observe),
        )
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

/// The text of an answer tool call.
struct Delivered {
    text: String,
    /// The output limit cut the call off: `text` is what was read of it.
    truncated: bool,
}

/// Follows one model call for the answer tool (`crate::answer`): when the turn's first tool
/// call is the answer, it reads the call's text as it arrives and publishes what may go out.
struct AnswerFilter<'a> {
    released: &'a watch::Sender<String>,
    state: Filtering,
}

enum Filtering {
    /// No tool call yet.
    Waiting,
    /// The turn's first tool call is the answer, content block `index`.
    Answer {
        index: usize,
        decoder: TextDecoder,
        release: Release,
    },
    /// The first call is another tool's, or the conversation has no answer tool: nothing of
    /// this turn goes out.
    Off,
}

impl<'a> AnswerFilter<'a> {
    /// The filter of one model call; `released` holds what the question released so far, which
    /// this call's release can only continue.
    fn new(answer_tool: bool, released: &'a watch::Sender<String>) -> Self {
        Self {
            released,
            state: if answer_tool {
                Filtering::Waiting
            } else {
                Filtering::Off
            },
        }
    }

    fn observe(&mut self, event: TurnEvent<'_>) {
        match (&mut self.state, event) {
            (Filtering::Waiting, TurnEvent::ToolCallStarted { index, name }) => {
                self.state = if name == ANSWER_TOOL {
                    Filtering::Answer {
                        index,
                        decoder: TextDecoder::new(),
                        release: self.continuing_release(),
                    }
                } else {
                    Filtering::Off
                };
            }
            (
                Filtering::Answer {
                    index,
                    decoder,
                    release,
                },
                TurnEvent::ToolInput { index: at, piece },
            ) if *index == at => {
                decoder.push(piece);
                if decoder.failed() {
                    return;
                }
                if let Some(text) = release.advance(decoder.text()) {
                    self.released.send_replace(text.to_owned());
                }
            }
            _ => {}
        }
    }

    /// The release of this call's answer: it continues what the question released so far.
    fn continuing_release(&self) -> Release {
        Release::continuing(self.released.borrow().clone())
    }

    /// What read the answer call's text, if the turn opened with one.
    fn decoder(&self) -> Option<&TextDecoder> {
        match &self.state {
            Filtering::Answer { decoder, .. } => Some(decoder),
            Filtering::Waiting | Filtering::Off => None,
        }
    }
}

/// Fails the question when `answer`, about to be stored (`path` says how it was written), does
/// not start with `released`, everything the question released: the stream sent that text as
/// the answer's start and cannot take it back. Only a fault gets here; it is logged without
/// the text.
fn continues_release(
    run: &Run,
    conversation: &Conversation,
    answer: &str,
    released: &str,
    path: &'static str,
) -> Result<(), AgentError> {
    if answer.starts_with(released) {
        return Ok(());
    }
    tracing::error!(
        run_id = %run.id,
        conversation_id = %conversation.id(),
        tenant_id = %conversation.principal().tenant_id,
        path,
        released_bytes = released.len(),
        answer_bytes = answer.len(),
        "the answer does not start with the text released from it"
    );
    Err(AgentError::ModelFailed)
}

/// The results of a turn whose answer tool call had blank text: that call is told so, and each
/// call after it was not run.
fn blank_answer_results(turn: &ModelTurn) -> Vec<ToolResult> {
    turn.tool_calls
        .iter()
        .enumerate()
        .map(|(position, call)| ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::error(if position == 0 {
                prompt::ANSWER_EMPTY_RESULT
            } else {
                prompt::AFTER_ANSWER_RESULT
            }),
        })
        .collect()
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
    removed: RemovedContent,
    turn: &ModelTurn,
) {
    for note in pending {
        conversation.append(EntryKind::Note, note);
    }
    // What the guard removed joins the history with the answer, so the store keeps it with the
    // message (Laysi/trustiot-v2#646).
    conversation.append_with(kind, message, removed);
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_error_reads_back_from_its_kind_and_has_its_own_message() {
        // A new variant fails to compile here until it is listed in `AgentError::ALL` too.
        for error in AgentError::ALL {
            match error {
                AgentError::ModelTimeout
                | AgentError::ModelUnavailable
                | AgentError::ModelFailed
                | AgentError::Refused
                | AgentError::ContextFull
                | AgentError::BudgetExhausted
                | AgentError::RunExpired
                | AgentError::AccessRevoked
                | AgentError::NotTheConversationsRun => {}
            }
            assert_eq!(AgentError::from_kind(error.kind()), Some(error));
            assert_eq!(
                error.message_key().as_str(),
                format!("question.{}", error.kind())
            );
        }
        let kinds: BTreeSet<_> = AgentError::ALL.iter().map(|error| error.kind()).collect();
        assert_eq!(kinds.len(), AgentError::ALL.len());
        assert_eq!(AgentError::from_kind("answered"), None);
    }
}
