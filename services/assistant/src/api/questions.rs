//! Asking a question in a conversation and stopping it (Laysi/trustiot-v2#646). A question runs
//! in the background ([`crate::questions`]): asking answers 202 with the question's run as soon
//! as it started, and the client reads the conversation (`GET .../conversations/{id}`) until its
//! `latest_run` has ended; the answer is then the last message. Streaming the answer is
//! Laysi/trustiot-v2#147.
//!
//! Both endpoints act as the user of the request's identity in its current tenant, on the
//! caller's own conversations only, like every conversation endpoint ([`super::conversations`]):
//! a conversation (or a question) the caller cannot see is the one 404, and nothing is started,
//! stopped or sent to the identity service for it.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role this build does not know | 403 `permission_denied` |
//! | The caller's tenant is suspended or deleted (Laysi/trustiot-v2#777) | 403 `tenant_suspended` |
//! | Not the caller's conversation, or deleted; a question that is not of that conversation | 404 `not_found` |
//! | `Content-Type` other than `application/json` | 400 `invalid_request` |
//! | A body that is not a JSON object with exactly `question`, a string (stopping takes none: no body, `null` or `{}`) | 400 `invalid_request`, key `conversation.body_invalid` |
//! | A question that, after trimming, is empty or longer than `questions.max_chars` characters (a body over [`body_limit`] bytes included) | 422 `validation_failed`, field `/question`, key `question.text_invalid`, `{"max": <max_chars>}` |
//! | A question while one runs in the conversation | 409 `conflict`, key `conversation.question_running` |
//! | A question in a conversation whose history holds [`crate::store::MAX_HISTORY_BYTES`] or more | 409 `conflict`, key `question.history_full` |
//! | A question while the caller has `questions.max_running_per_user` running in the tenant | 429 `rate_limited`, key `question.running_limited`, `{"max": <max>}` |
//! | The identity service does not issue the question's run delegation | its M1 code (`unauthenticated`, `permission_denied`, `timeout`, `unavailable`, `internal`); the question's run is recorded `failed` |
//! | Stopping a question that has ended (or is past its deadline) | 409 `conflict`, key `question.run_ended` |
//! | The replica runs `questions.max_running_per_replica` questions, or its `questions.memory` has no room for this conversation ([`crate::questions::capacity`]) | 503 `unavailable`, key `question.busy`, `Retry-After` ([`BUSY_RETRY_AFTER`]); nothing was started |
//! | The service is shutting down | 503 `unavailable`; nothing was started |
//! | A lock not granted within [`super::REQUEST_LOCK_TIMEOUT`], or the database failed | 503 `unavailable`; nothing was started or stopped, unless the connection broke while the question's record committed: the database may then hold the question as `running`, with no task behind it, until its deadline (services/assistant/README.md, 「卡在 `running` 的提問」; Laysi/trustiot-v2#1030) |
//!
//! A full replica answers 503, not 429: 429 says the caller asked too often, while this caller
//! may have nothing running at all. It is the replica that is out of room, for a while: the
//! same request succeeds once a question ends, or on another replica, which a retry may reach.
//! A request refused with 404, 409 or 429 never takes a seat of the replica.

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::{LOCATION, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, Utc};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::TenantDb;
use utoipa::ToSchema;
use uuid::Uuid;

use super::BodyRefusal;
use super::conversations::{self, CONVERSATIONS_PATH};
use crate::AgentError;
use crate::questions::{Ask, BUSY_RETRY_AFTER, Questions, Refused};
use crate::store::{self, RunRecord, RunStatus};

/// Asking a question in a conversation.
pub const QUESTIONS_PATH: &str = "/api/assistant/v1/conversations/{conversation_id}/questions";
/// Stopping a question of a conversation.
pub const STOP_PATH: &str =
    "/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/cancel";

/// A question that is empty after trimming or too long (422, field `/question`); `details.max`.
pub const TEXT_INVALID: MessageKey = MessageKey::new("question.text_invalid");
/// The conversation's history is full: start a new conversation (409).
pub const HISTORY_FULL: MessageKey = MessageKey::new("question.history_full");
/// The caller has the most questions running that one user may have (429); `details.max`.
pub const RUNNING_LIMITED: MessageKey = MessageKey::new("question.running_limited");
/// The question to stop has already ended (409).
pub const RUN_ENDED: MessageKey = MessageKey::new("question.run_ended");
/// A question's run that the user (or deleting its conversation) stopped.
pub const CANCELLED: MessageKey = MessageKey::new("question.cancelled");
/// A question's run that the service's restart or its deadline cut off.
pub const INTERRUPTED: MessageKey = MessageKey::new("question.interrupted");
/// A question's run whose delegation the identity service did not issue (`delegation_<code>`).
pub const NOT_STARTED: MessageKey = MessageKey::new("question.not_started");
/// A question's answer that could not be saved (`store_<code>`).
pub const NOT_SAVED: MessageKey = MessageKey::new("question.not_saved");
/// A question's run that failed for a reason this build does not know.
pub const FAILED: MessageKey = MessageKey::new("question.failed");
/// The replica runs as many questions as it can (503, with `Retry-After`).
pub const BUSY: MessageKey = MessageKey::new("question.busy");

#[derive(Clone)]
struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
    questions: Questions,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The question routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, questions: Questions) -> Router {
    Router::new()
        .route(QUESTIONS_PATH, post(ask_question))
        .route(STOP_PATH, post(stop_question))
        .with_state(Routes {
            verifier,
            db,
            questions,
        })
}

/// A question to ask.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    /// The question: 1 to `questions.max_chars` (default 16,000) characters after the white
    /// space around it is trimmed.
    pub question: String,
}

/// Stopping a question takes no settings: no body, `null`, or an empty object.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StopQuestion {}

/// Where a question stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRunStatus {
    /// Being answered.
    Running,
    /// Answered: the answer is the conversation's last message.
    Answered,
    /// It failed (`result`, `message_key`); nothing of it is in the conversation.
    Failed,
    /// Stopped by the user, or by deleting the conversation; nothing of it is in the
    /// conversation.
    Cancelled,
    /// The service restarted or stopped while it ran, or it ran past its deadline; nothing of it
    /// is in the conversation.
    Interrupted,
}

impl From<RunStatus> for ConversationRunStatus {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Running => Self::Running,
            RunStatus::Answered => Self::Answered,
            RunStatus::Failed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::Interrupted => Self::Interrupted,
        }
    }
}

/// A question asked in a conversation, as its run stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConversationRun {
    /// The question's run ID.
    pub id: Uuid,
    pub status: ConversationRunStatus,
    /// Why it failed, an identifier: the agent's error kind (such as `model_timeout`,
    /// `budget_exhausted`), `delegation_<code>` when the identity service did not issue its
    /// delegation, `store_<class>` when its answer could not be saved. `null` unless `failed`.
    pub result: Option<String>,
    /// The message to show for a question that did not answer (`failed`, `cancelled`,
    /// `interrupted`), as a translation key; `null` otherwise.
    pub message_key: Option<String>,
    /// When it started, by the database clock.
    pub started_at: DateTime<Utc>,
    /// The time by which it ends; a question still running then is `interrupted`.
    pub deadline_at: DateTime<Utc>,
    /// When it ended; `null` while it runs, and for one `interrupted` because it ran past its
    /// deadline.
    pub ended_at: Option<DateTime<Utc>>,
    /// When the user first asked to stop it (or deleted the conversation); `null` if never.
    pub stop_requested_at: Option<DateTime<Utc>>,
}

impl From<&RunRecord> for ConversationRun {
    fn from(run: &RunRecord) -> Self {
        let message_key = match run.status {
            RunStatus::Running | RunStatus::Answered => None,
            RunStatus::Failed => Some(failure_key(run.result_code.as_deref())),
            RunStatus::Cancelled => Some(CANCELLED),
            RunStatus::Interrupted => Some(INTERRUPTED),
        };
        Self {
            id: run.run_id,
            status: run.status.into(),
            result: (run.status == RunStatus::Failed)
                .then(|| run.result_code.clone())
                .flatten(),
            message_key: message_key.map(|key| key.as_str().to_owned()),
            started_at: run.started_at,
            deadline_at: run.deadline_at,
            ended_at: run.ended_at,
            stop_requested_at: run.cancel_requested_at,
        }
    }
}

/// The message key of a failed run's code: the agent's error's own, or one per family of the
/// question API's codes. Never the code's text itself.
fn failure_key(code: Option<&str>) -> MessageKey {
    let Some(code) = code else {
        return FAILED;
    };
    if let Some(error) = AgentError::from_kind(code) {
        error.message_key()
    } else if code.starts_with("delegation_") {
        NOT_STARTED
    } else if code.starts_with("store_") {
        NOT_SAVED
    } else {
        FAILED
    }
}

/// The largest body a question may come in with `max_chars` characters: each character escaped
/// as JSON escapes the longest (`\uXXXX\uXXXX` for one outside the Basic Multilingual Plane, 12
/// bytes), and 1 KiB for the object and the white space around it. A larger body cannot hold a
/// question that fits.
#[must_use]
pub fn body_limit(max_chars: usize) -> usize {
    max_chars.saturating_mul(12).saturating_add(1024)
}

/// Asks a question in one of the caller's conversations. It runs in the background: the response
/// comes as soon as it started, and the conversation's `latest_run` tells when it ended.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/conversations/{conversation_id}/questions",
    tag = "assistant",
    operation_id = "askQuestion",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    request_body = AskQuestion,
    responses(
        (status = 202, description = "The question started; `Location` is the conversation, whose `latest_run` is this run until another question starts.", body = ConversationRun,
            headers(("Location" = String, description = "The conversation's path."))),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), or the body is not a JSON object with exactly `question`, a string (`conversation.body_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`); also when the identity service refused the question's delegation because the session ended.", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`, also when the identity service refused the question's delegation); the caller's tenant is suspended or deleted (`tenant_suspended`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`). Nothing was started.", body = ErrorBody),
        (status = 409, description = "A question of the conversation is being answered (`conflict`, `conversation.question_running`), or its history is full (`question.history_full`). Nothing was started.", body = ErrorBody),
        (status = 422, description = "The question, after trimming, is empty or longer than `questions.max_chars` characters (`validation_failed`, field `/question`, `question.text_invalid`, `details.max`).", body = ErrorBody),
        (status = 429, description = "The caller has the most questions running that one user may have in the tenant (`rate_limited`, `question.running_limited`, `details.max`). Nothing was started.", body = ErrorBody),
        (status = 503, description = "The replica runs as many questions as it may, or has no memory left for a question in this conversation (`unavailable`, `question.busy`, with `Retry-After`); the database or the identity service failed, or the service is shutting down (`unavailable`). Nothing was started.", body = ErrorBody,
            headers(("Retry-After" = u64, description = "With `question.busy`: seconds to wait before asking again."))),
        (status = 504, description = "The identity service did not answer in time (`timeout`); the question's run is recorded `failed`.", body = ErrorBody),
    )
)]
async fn ask_question(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "ask a question";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(conversation) = super::parse_id(id) else {
        return super::not_found();
    };
    let max_chars = state.questions.settings().max_chars;
    let body = match super::read_json(&headers, body, body_limit(max_chars)).await {
        Ok(body) => body,
        Err(BodyRefusal::NotJson) => {
            return super::error(ErrorBody::new(ErrorCode::InvalidRequest));
        }
        Err(BodyRefusal::TooLarge) => return super::error(text_invalid(max_chars)),
    };
    let question = match parse_question(&body, max_chars) {
        Ok(question) => question,
        Err(refused) => return super::error(refused),
    };
    let (user_id, tenant_id) = (user.user_id(), user.tenant_id());
    let asked = state
        .questions
        .ask(Ask {
            user,
            principal,
            conversation,
            question,
        })
        .await;
    match asked {
        Ok(run) => {
            let mut response = super::ok(StatusCode::ACCEPTED, &ConversationRun::from(&run));
            if let Ok(location) =
                HeaderValue::from_str(&format!("{CONVERSATIONS_PATH}/{conversation}"))
            {
                response.headers_mut().insert(LOCATION, location);
            }
            response
        }
        Err(Refused::Store(failure)) => {
            super::store_failure_of(user_id, tenant_id, Some(conversation), ACTION, &failure)
        }
        // Logged with its code by the question's task; the body carries the code only
        // (`no-store` comes from the API's layer).
        Err(Refused::Delegation(error)) => error.into_response(),
        Err(Refused::ShuttingDown) => super::error(ErrorBody::new(ErrorCode::Unavailable)),
        // Logged by the question's task, with what was full.
        Err(Refused::Busy) => busy(),
        Err(Refused::Lost) => {
            tracing::error!(
                user_id = %user_id,
                conversation_id = %conversation,
                action = ACTION,
                "a question's task ended without starting it"
            );
            super::error(ErrorBody::new(ErrorCode::Internal))
        }
    }
}

/// 503 `question.busy` with `Retry-After`.
fn busy() -> Response {
    let mut response = super::error(ErrorBody::new(ErrorCode::Unavailable).with_message_key(BUSY));
    response.headers_mut().insert(
        RETRY_AFTER,
        HeaderValue::from(BUSY_RETRY_AFTER.as_secs().max(1)),
    );
    response
}

/// Stops one of the caller's questions: no more model or tool calls are made for it, nothing of
/// it is added to the conversation, and it ends `cancelled`. The replica that runs it stops it at
/// once, another within [`crate::questions::CANCEL_POLL_INTERVAL`] and one short transaction.
/// Asking again while it is still stopping answers the same.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/cancel",
    tag = "assistant",
    operation_id = "stopQuestion",
    params(
        ("conversation_id" = Uuid, Path, description = "The conversation's ID."),
        ("run_id" = Uuid, Path, description = "The question's run ID."),
    ),
    request_body(content = Option<StopQuestion>, description = "Nothing to set: send `Content-Type: application/json` (required, against cross-site requests) with no body, `null`, or an empty object."),
    responses(
        (status = 202, description = "The stop is recorded; the run as it stands (`running` with `stop_requested_at` until it has stopped).", body = ConversationRun),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), or there is a body other than `null` or an empty object (`conversation.body_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`); the caller's tenant is suspended or deleted (`tenant_suspended`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, it is deleted, or the question is not of it (`not_found`).", body = ErrorBody),
        (status = 409, description = "The question has already ended (`conflict`, `question.run_ended`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing changed.", body = ErrorBody),
    )
)]
async fn stop_question(
    State(state): State<Routes>,
    user: UserIdentity,
    ids: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "stop a question";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some((conversation, run_id)) = parse_ids(ids) else {
        return super::not_found();
    };
    let body = match super::read_json(&headers, body, conversations::MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(refusal) => return conversations::body_refused(refusal),
    };
    if !conversations::no_settings::<StopQuestion>(&body) {
        return conversations::body_invalid();
    }
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = store::request_cancel(&mut tx, principal, conversation, run_id).await;
    match super::finish(tx, result).await {
        Ok(run) => {
            // After the commit: the task that stops now records `cancelled` behind it.
            let here = state.questions.cancel_here(run_id);
            tracing::info!(
                run_id = %run_id,
                conversation_id = %conversation,
                tenant_id = %principal.tenant_id,
                user_id = %principal.user_id,
                local = here,
                "question stop requested"
            );
            super::ok(StatusCode::ACCEPTED, &ConversationRun::from(&run))
        }
        Err(failure) => super::store_failure(&user, Some(conversation), ACTION, &failure),
    }
}

/// The conversation and run IDs of the path; `None` (answered 404) unless both are UUIDs.
fn parse_ids(ids: Result<Path<(String, String)>, PathRejection>) -> Option<(Uuid, Uuid)> {
    let Path((conversation, run)) = ids.ok()?;
    Some((conversation.parse().ok()?, run.parse().ok()?))
}

/// The question of `body`, trimmed, when it is a JSON object with exactly `question`, a string
/// of 1 to `max_chars` characters after trimming.
fn parse_question(body: &[u8], max_chars: usize) -> Result<String, ErrorBody> {
    let request: AskQuestion = super::parse_object(body).ok_or_else(|| {
        ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(conversations::BODY_INVALID)
    })?;
    let question = request.question.trim();
    let chars = question.chars().count();
    if chars == 0 || chars > max_chars {
        return Err(text_invalid(max_chars));
    }
    Ok(question.to_owned())
}

fn text_invalid(max_chars: usize) -> ErrorBody {
    let max = u64::try_from(max_chars).unwrap_or(u64::MAX);
    ErrorBody::new(ErrorCode::ValidationFailed)
        .with_field_error(FieldError::new("/question", TEXT_INVALID).with_detail("max", max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_is_one_string_of_one_to_max_characters_after_trimming() {
        assert_eq!(
            parse_question(br#"{"question":"  How much energy?\n"}"#, 20),
            Ok("How much energy?".to_owned())
        );
        // Characters, not bytes.
        assert_eq!(
            parse_question("{\"question\":\"用電量多少\"}".as_bytes(), 5),
            Ok("用電量多少".to_owned())
        );
        for (body, max) in [
            (r#"{"question":"   "}"#, 20),
            (r#"{"question":""}"#, 20),
            (r#"{"question":"six chars"}"#, 5),
        ] {
            let refused = parse_question(body.as_bytes(), max).expect_err(body);
            assert_eq!(refused, text_invalid(max), "{body}");
        }
        for body in [
            "",
            "null",
            "{}",
            r#"{"question":1}"#,
            r#"{"question":"a","extra":true}"#,
            r#"["a"]"#,
            r#""a""#,
        ] {
            let refused = parse_question(body.as_bytes(), 20).expect_err(body);
            assert_eq!(refused.code(), ErrorCode::InvalidRequest, "{body}");
        }
    }

    #[test]
    fn the_body_limit_holds_the_longest_question_escaped() {
        let max = 100;
        let escaped = "\\ud83d\\ude00".repeat(max);
        let body = format!("{{\"question\": \"{escaped}\"}}");
        assert!(body.len() <= body_limit(max));
        assert_eq!(
            parse_question(body.as_bytes(), max).map(|q| q.chars().count()),
            Ok(max)
        );
        assert_eq!(body_limit(usize::MAX), usize::MAX);
    }

    #[test]
    fn a_failed_run_shows_a_key_for_its_failure_never_its_code() {
        for error in AgentError::ALL {
            assert_eq!(failure_key(Some(error.kind())), error.message_key());
        }
        assert_eq!(failure_key(Some("delegation_unauthenticated")), NOT_STARTED);
        assert_eq!(failure_key(Some("store_conflict")), NOT_SAVED);
        assert_eq!(failure_key(Some("something_new")), FAILED);
        assert_eq!(failure_key(None), FAILED);
    }
}
