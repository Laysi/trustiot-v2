//! A user's conversation threads (Laysi/trustiot-v2#645): start a conversation, list, read,
//! rename, archive, delete and restore them. Asking a question in one is
//! Laysi/trustiot-v2#646.
//!
//! A conversation is its owner's alone: every request acts as the user of the request's identity
//! in its current tenant, whatever the role there (an administrator of the tenant included), and
//! the store looks only at that user's conversations. One response, byte for byte, answers every
//! conversation the caller cannot see: one that does not exist, another user's or another
//! tenant's, a deleted one (except to restore it), and a path ID that is not a UUID.
//!
//! A delete answers 204 without a body; the deleted list tells until when a conversation can be
//! restored. Every other success answers with the conversation (or the page) as JSON.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role this build does not know | 403 `permission_denied` |
//! | Not the caller's conversation, or deleted | 404 `not_found` |
//! | A write (`POST`, `PATCH`) whose `Content-Type` is not `application/json` | 400 `invalid_request` |
//! | A body that is not a JSON object of the endpoint's fields (starting and restoring take none: no body, `null` or `{}`), or over [`MAX_BODY_BYTES`] | 400 `invalid_request`, key `conversation.body_invalid` |
//! | An update with neither `title` nor `archived` | 400 `invalid_request`, key `conversation.update_empty` |
//! | A title that, after trimming, is empty, longer than 200 characters, has no visible character, or has a control, bidirectional formatting or line separator character (`store::title_is_valid`) | 422 `validation_failed`, field `/title`, key `conversation.title_invalid`, `{"max": 200}` |
//! | `archived` other than `true` or `false` in the list's query | 400 `invalid_request` |
//! | A `limit` that is not 1 to 100 | 400 `invalid_request`, key `conversation.limit_invalid`, `{"max": 100}` |
//! | A `cursor` that is not a `next_cursor` of the same list | 400 `invalid_request`, key `conversation.cursor_invalid` |
//! | Deleting while a question of the conversation runs | 409 `conflict`, key `conversation.question_running`; nothing changes |
//! | Starting a conversation when the caller started [`store::MAX_STARTS_PER_DAY`] (100) in the tenant within the last 24 hours, deleted ones included | 429 `rate_limited`, key `conversation.start_limited`, `{"max": 100}`, `Retry-After` in seconds; nothing is stored |
//! | A lock (another request's or a stuck session's on the conversation) not granted within [`super::REQUEST_LOCK_TIMEOUT`] | 503 `unavailable`; nothing was written |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! A write that changed something logs one INFO line with the user, tenant and conversation,
//! never its title or messages; a start refused by the daily limit logs one WARN line with the
//! user and tenant.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::LOCATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantTransaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{BodyRefusal, Cursor, CursorKind};
use crate::Agent;
use crate::history::{EntryKind, Principal};
use crate::store::{self, ListCursor, ListFilter, MAX_TITLE_CHARS, StoreError, TitleSource};

/// The collection, as the ingress gateway forwards it (no prefix is stripped).
pub const CONVERSATIONS_PATH: &str = "/api/assistant/v1/conversations";
/// One conversation.
pub const CONVERSATION_PATH: &str = "/api/assistant/v1/conversations/{conversation_id}";
/// Restoring a deleted conversation.
pub const RESTORE_PATH: &str = "/api/assistant/v1/conversations/{conversation_id}/restore";
/// The deleted conversations that can still be restored.
pub const DELETED_CONVERSATIONS_PATH: &str = "/api/assistant/v1/deleted-conversations";

/// The largest body a write accepts: an update with the longest title, every character escaped,
/// fits.
pub const MAX_BODY_BYTES: usize = 4096;

/// The body is not a JSON object of the endpoint's fields, or is too large (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("conversation.body_invalid");
/// An update that changes nothing: neither `title` nor `archived` (400).
pub const UPDATE_EMPTY: MessageKey = MessageKey::new("conversation.update_empty");
/// A title the rules refuse (422, field `/title`); `details.max`.
pub const TITLE_INVALID: MessageKey = MessageKey::new("conversation.title_invalid");
/// A `cursor` that is not a `next_cursor` of the same list (400).
pub const CURSOR_INVALID: MessageKey = MessageKey::new("conversation.cursor_invalid");
/// A `limit` out of range (400); `details.max`.
pub const LIMIT_INVALID: MessageKey = MessageKey::new("conversation.limit_invalid");
/// A question of the conversation is running, so it is not deleted (409).
pub const QUESTION_RUNNING: MessageKey = MessageKey::new("conversation.question_running");
/// The caller started [`store::MAX_STARTS_PER_DAY`] conversations in the tenant within the last
/// [`store::START_WINDOW_HOURS`] hours (429); `details.max`.
pub const START_LIMITED: MessageKey = MessageKey::new("conversation.start_limited");

/// The tenant context of a new conversation: none yet. Nothing builds it so far (the tenant's
/// devices, data points, plan, language and time zone), and the preamble fixes it for the
/// conversation's whole life, so a conversation started now keeps an empty one.
const TENANT_CONTEXT: &str = "";

#[derive(Clone)]
struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
    agent: Arc<Agent>,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The conversation routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, agent: Arc<Agent>) -> Router {
    Router::new()
        .route(
            CONVERSATIONS_PATH,
            get(list_conversations).post(create_conversation),
        )
        .route(
            CONVERSATION_PATH,
            get(get_conversation)
                .patch(update_conversation)
                .delete(delete_conversation),
        )
        .route(RESTORE_PATH, post(restore_conversation))
        .route(DELETED_CONVERSATIONS_PATH, get(list_deleted_conversations))
        .with_state(Routes {
            verifier,
            db,
            agent,
        })
}

/// Who gave a conversation its title.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationTitleSource {
    /// Taken from the first question until a better one comes.
    Provisional,
    /// The user named it.
    User,
    /// The model named it.
    Model,
}

impl From<TitleSource> for ConversationTitleSource {
    fn from(source: TitleSource) -> Self {
        match source {
            TitleSource::Provisional => Self::Provisional,
            TitleSource::User => Self::User,
            TitleSource::Model => Self::Model,
        }
    }
}

/// A conversation as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Conversation {
    /// The conversation's ID.
    pub id: Uuid,
    /// Its title; `null` until it gets one.
    pub title: Option<String>,
    /// Who gave the title; `null` without one.
    pub title_source: Option<ConversationTitleSource>,
    /// When it was started, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When messages were last added to it (when it was started, before that); the list's order.
    pub last_activity_at: DateTime<Utc>,
    /// When it was archived; `null` when it is not.
    pub archived_at: Option<DateTime<Utc>>,
}

impl From<&store::ConversationSummary> for Conversation {
    fn from(summary: &store::ConversationSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title.as_ref().map(|title| title.text.clone()),
            title_source: summary.title.as_ref().map(|title| title.source.into()),
            created_at: summary.created_at,
            last_activity_at: summary.last_activity_at,
            archived_at: summary.archived_at,
        }
    }
}

/// What a message of the chat is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMessageKind {
    /// The user's question.
    Question,
    /// The assistant's answer to it.
    Answer,
}

/// One message the chat shows: only its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConversationMessage {
    pub kind: ConversationMessageKind,
    /// The question as the user asked it, or the text of the answer.
    pub text: String,
}

/// A conversation with the messages the chat shows: the questions and the answers, oldest
/// first. The assistant's reasoning, its tool calls and their results, and the instructions it
/// started with are not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConversationDetail {
    /// The conversation's ID.
    pub id: Uuid,
    /// Its title; `null` until it gets one.
    pub title: Option<String>,
    /// Who gave the title; `null` without one.
    pub title_source: Option<ConversationTitleSource>,
    /// When it was started, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When messages were last added to it (when it was started, before that).
    pub last_activity_at: DateTime<Utc>,
    /// When it was archived; `null` when it is not.
    pub archived_at: Option<DateTime<Utc>>,
    /// The questions and answers, oldest first.
    pub messages: Vec<ConversationMessage>,
}

/// One page of the caller's conversations, most recently active first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConversationList {
    /// The conversations, most recently active first (by `last_activity_at`, then `id`).
    pub conversations: Vec<Conversation>,
    /// Pass it as `cursor` for the next page of the same list; `null` on the last page. Opaque.
    pub next_cursor: Option<String>,
}

/// A deleted conversation: no messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeletedConversation {
    /// The conversation's ID.
    pub id: Uuid,
    /// Its title; `null` if it had none.
    pub title: Option<String>,
    /// When it was deleted, by the database clock.
    pub deleted_at: DateTime<Utc>,
    /// Until when it can be restored (30 days after `deleted_at`); it is removed for good after.
    pub restorable_until: DateTime<Utc>,
}

impl From<&store::DeletedConversation> for DeletedConversation {
    fn from(deleted: &store::DeletedConversation) -> Self {
        Self {
            id: deleted.id,
            title: deleted.title.as_ref().map(|title| title.text.clone()),
            deleted_at: deleted.deleted_at,
            restorable_until: deleted.restorable_until,
        }
    }
}

/// One page of the caller's deleted conversations that can still be restored, most recently
/// deleted first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeletedConversationList {
    /// The conversations, most recently deleted first (by `deleted_at`, then `id`).
    pub conversations: Vec<DeletedConversation>,
    /// Pass it as `cursor` for the next page; `null` on the last page. Opaque.
    pub next_cursor: Option<String>,
}

/// Starting a conversation takes no settings: no body, `null`, or an empty object.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateConversation {}

/// Restoring a conversation takes no settings: no body, `null`, or an empty object.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreConversation {}

/// What to change: at least one of the two.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateConversation {
    /// The new title, 1 to 200 characters after trimming the surrounding white space, at least
    /// one of them visible, without control, bidirectional or deprecated formatting (U+202A to
    /// U+202E, U+2066 to U+206F) or line separator characters. It replaces any title, the
    /// model's included.
    #[serde(default)]
    pub title: Option<String>,
    /// `true` archives the conversation, `false` takes it out of the archive. An archived
    /// conversation is listed only with `archived=true`, and can still be read and continued.
    #[serde(default)]
    pub archived: Option<bool>,
}

/// The list's query parameters as they arrive. Unknown parameters are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub archived: Option<String>,
    pub limit: Option<String>,
    pub cursor: Option<String>,
}

/// The deleted list's query parameters as they arrive. Unknown parameters are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct DeletedListQuery {
    pub limit: Option<String>,
    pub cursor: Option<String>,
}

/// The caller's conversations, most recently active first.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/conversations",
    tag = "assistant",
    operation_id = "listConversations",
    params(
        ("archived" = Option<bool>, Query, description = "`true` lists only the archived conversations; without it, or `false`, only the ones not archived."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 100, description = "Conversations per page, 1 to 100; default 50."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor` of the same list, for the next page."),
    ),
    responses(
        (status = 200, description = "A page of the caller's conversations; deleted ones are never listed.", body = ConversationList),
        (status = 400, description = "A malformed query, `archived` other than `true` or `false` (`invalid_request`), a `limit` out of range (`conversation.limit_invalid`), or a `cursor` that is not a `next_cursor` of this list (`conversation.cursor_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_conversations(
    State(state): State<Routes>,
    user: UserIdentity,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list conversations";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Ok(Query(query)) = query else {
        return super::error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let (filter, kind) = match query.archived.as_deref() {
        None | Some("false") => (ListFilter::Active, CursorKind::Active),
        Some("true") => (ListFilter::Archived, CursorKind::Archived),
        Some(_) => return super::error(ErrorBody::new(ErrorCode::InvalidRequest)),
    };
    let (limit, after) = match page(query.limit.as_deref(), query.cursor.as_deref(), kind) {
        Ok(page) => page,
        Err(refused) => return refused,
    };
    let after = after.map(|cursor| ListCursor {
        last_activity_at: cursor.at,
        id: cursor.id,
    });
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = store::list(&mut tx, principal, filter, after, limit).await;
    match super::finish(tx, result).await {
        Ok(page) => super::ok(
            StatusCode::OK,
            &ConversationList {
                conversations: page.items.iter().map(Conversation::from).collect(),
                next_cursor: page.next.map(|next| {
                    Cursor {
                        kind,
                        at: next.last_activity_at,
                        id: next.id,
                    }
                    .encode()
                }),
            },
        ),
        Err(failure) => super::store_failure(&user, None, ACTION, &failure),
    }
}

/// Starts a conversation for the caller: empty, with today's system prompt, tools and model
/// settings fixed for its whole life.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/conversations",
    tag = "assistant",
    operation_id = "createConversation",
    request_body(content = Option<CreateConversation>, description = "Nothing to set: send `Content-Type: application/json` (required, against cross-site requests) with no body, `null`, or an empty object."),
    responses(
        (status = 201, description = "The conversation was started; `Location` names it.", body = Conversation,
            headers(("location" = String, description = "The conversation's path."))),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), or there is a body other than `null` or an empty object (`conversation.body_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 429, description = "The caller started 100 conversations in this tenant within the last 24 hours, deleted ones included (`rate_limited`, key `conversation.start_limited`, `details.max`); nothing was stored.", body = ErrorBody,
            headers(("retry-after" = u64, description = "Seconds until the oldest of those conversations leaves the 24 hours and a start is allowed again."))),
        (status = 503, description = "The database failed (`unavailable`); nothing was written.", body = ErrorBody),
    )
)]
async fn create_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "start a conversation";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let body = match super::read_json(&headers, body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(refusal) => return body_refused(refusal),
    };
    if !no_settings::<CreateConversation>(&body) {
        return body_invalid();
    }
    let conversation = state.agent.start_conversation(principal, TENANT_CONTEXT);
    let id = conversation.id();
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = create(&mut tx, principal, &conversation).await;
    match super::finish(tx, result).await {
        Ok(created) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %principal.tenant_id,
                conversation_id = %id,
                "conversation started"
            );
            let mut response = super::ok(StatusCode::CREATED, &Conversation::from(&created));
            if let Ok(location) = HeaderValue::from_str(&format!("{CONVERSATIONS_PATH}/{id}")) {
                response.headers_mut().insert(LOCATION, location);
            }
            response
        }
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

async fn create(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: &crate::history::Conversation,
) -> Result<store::ConversationSummary, StoreError> {
    store::create(tx, conversation).await?;
    store::metadata(tx, principal, conversation.id())
        .await?
        .ok_or(StoreError::Corrupt("a conversation that was just stored"))
}

/// One of the caller's conversations with its questions and answers.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/conversations/{conversation_id}",
    tag = "assistant",
    operation_id = "getConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    responses(
        (status = 200, description = "The conversation and the messages the chat shows.", body = ConversationDetail),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    const ACTION: &str = "read a conversation";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(id) = super::parse_id(id) else {
        return super::not_found();
    };
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = detail(&mut tx, principal, id).await;
    match super::finish(tx, result).await {
        Ok(detail) => super::ok(StatusCode::OK, &detail),
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

async fn detail(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<ConversationDetail, StoreError> {
    let summary = store::metadata(tx, principal, id)
        .await?
        .ok_or(StoreError::NotStored)?;
    let loaded = store::load(tx, principal, id)
        .await?
        .ok_or(StoreError::NotStored)?;
    let messages = loaded
        .conversation
        .entries()
        .filter_map(|(kind, message)| {
            let kind = match kind {
                EntryKind::Question => ConversationMessageKind::Question,
                EntryKind::Answer => ConversationMessageKind::Answer,
                EntryKind::Step | EntryKind::ToolResults | EntryKind::Note => return None,
            };
            Some(
                message
                    .shown_text()
                    .map(|text| ConversationMessage { kind, text })
                    .ok_or(StoreError::Corrupt("a shown message")),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let conversation = Conversation::from(&summary);
    Ok(ConversationDetail {
        id: conversation.id,
        title: conversation.title,
        title_source: conversation.title_source,
        created_at: conversation.created_at,
        last_activity_at: conversation.last_activity_at,
        archived_at: conversation.archived_at,
        messages,
    })
}

/// Renames, archives or unarchives one of the caller's conversations, both in one transaction.
#[utoipa::path(
    patch,
    path = "/api/assistant/v1/conversations/{conversation_id}",
    tag = "assistant",
    operation_id = "updateConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    request_body = UpdateConversation,
    responses(
        (status = 200, description = "The conversation as it is now.", body = Conversation),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), the body is not a JSON object of `title` and `archived` (`conversation.body_invalid`), or it has neither (`conversation.update_empty`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`).", body = ErrorBody),
        (status = 422, description = "The title, after trimming, is empty, longer than 200 characters, has no visible character, or has a control, bidirectional formatting or line separator character (`validation_failed`, field `/title`, `conversation.title_invalid`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing was written.", body = ErrorBody),
    )
)]
async fn update_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "update a conversation";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(id) = super::parse_id(id) else {
        return super::not_found();
    };
    let body = match super::read_json(&headers, body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(refusal) => return body_refused(refusal),
    };
    let update = match parse_update(&body) {
        Ok(update) => update,
        Err(refused) => return super::error(refused),
    };
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = update_in(&mut tx, principal, id, &update).await;
    match super::finish(tx, result).await {
        Ok(updated) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %principal.tenant_id,
                conversation_id = %id,
                renamed = update.title.is_some(),
                archived = update.archived,
                "conversation updated"
            );
            super::ok(StatusCode::OK, &Conversation::from(&updated))
        }
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

/// An update whose title, if any, is trimmed and passes the rules.
#[derive(Debug, PartialEq, Eq)]
struct Update {
    title: Option<String>,
    archived: Option<bool>,
}

/// The update `body` asks for, or the error to answer.
fn parse_update(body: &Bytes) -> Result<Update, ErrorBody> {
    let request: UpdateConversation = super::parse_object(body)
        .ok_or_else(|| ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID))?;
    if request.title.is_none() && request.archived.is_none() {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(UPDATE_EMPTY));
    }
    let title = match request.title.as_deref() {
        None => None,
        Some(raw) => Some(parse_title(raw).ok_or_else(|| {
            let max = u64::try_from(MAX_TITLE_CHARS).unwrap_or(u64::MAX);
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/title", TITLE_INVALID).with_detail("max", max))
        })?),
    };
    Ok(Update {
        title,
        archived: request.archived,
    })
}

/// `raw` without its surrounding white space, when the store accepts that as a title
/// ([`store::title_is_valid`], checked here so the client gets a field error).
fn parse_title(raw: &str) -> Option<String> {
    let title = raw.trim();
    store::title_is_valid(title).then(|| title.to_owned())
}

async fn update_in(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    update: &Update,
) -> Result<store::ConversationSummary, StoreError> {
    if let Some(title) = &update.title {
        store::set_title(tx, principal, id, title, TitleSource::User).await?;
    }
    if let Some(archived) = update.archived {
        store::archive(tx, principal, id, archived).await?;
    }
    store::metadata(tx, principal, id)
        .await?
        .ok_or(StoreError::NotStored)
}

/// Deletes one of the caller's conversations. It leaves every list but the deleted one, and
/// can be restored for 30 days; then it is removed for good.
#[utoipa::path(
    delete,
    path = "/api/assistant/v1/conversations/{conversation_id}",
    tag = "assistant",
    operation_id = "deleteConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    responses(
        (status = 204, description = "Deleted; it can be restored for 30 days (the deleted list gives until when). No body."),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is already deleted (`not_found`).", body = ErrorBody),
        (status = 409, description = "A question of the conversation is being answered (`conflict`, `conversation.question_running`). Nothing changed.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing changed.", body = ErrorBody),
    )
)]
async fn delete_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    const ACTION: &str = "delete a conversation";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(id) = super::parse_id(id) else {
        return super::not_found();
    };
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = store::delete(&mut tx, principal, id).await;
    match super::finish(tx, result).await {
        // The deletion time and the restore deadline are the deleted list's to give: the
        // contract for DELETE is 204 without a body (Laysi/trustiot-v2#645).
        Ok(_) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %principal.tenant_id,
                conversation_id = %id,
                "conversation deleted"
            );
            super::no_content()
        }
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

/// Restores one of the caller's deleted conversations, as it was when it was deleted. Only
/// within 30 days of the deletion.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/conversations/{conversation_id}/restore",
    tag = "assistant",
    operation_id = "restoreConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    request_body(content = Option<RestoreConversation>, description = "Nothing to set: send `Content-Type: application/json` (required, against cross-site requests) with no body, `null`, or an empty object."),
    responses(
        (status = 200, description = "Restored; the conversation as it is now.", body = Conversation),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), or there is a body other than `null` or an empty object (`conversation.body_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No deleted conversation of the caller's with this ID that can still be restored (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing changed.", body = ErrorBody),
    )
)]
async fn restore_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "restore a conversation";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(id) = super::parse_id(id) else {
        return super::not_found();
    };
    let body = match super::read_json(&headers, body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(refusal) => return body_refused(refusal),
    };
    if !no_settings::<RestoreConversation>(&body) {
        return body_invalid();
    }
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = restore(&mut tx, principal, id).await;
    match super::finish(tx, result).await {
        Ok(restored) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %principal.tenant_id,
                conversation_id = %id,
                "conversation restored"
            );
            super::ok(StatusCode::OK, &Conversation::from(&restored))
        }
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

async fn restore(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<store::ConversationSummary, StoreError> {
    store::restore(tx, principal, id).await?;
    store::metadata(tx, principal, id)
        .await?
        .ok_or(StoreError::Corrupt("a conversation that was just restored"))
}

/// The caller's deleted conversations that can still be restored, most recently deleted first:
/// titles and times, no messages.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/deleted-conversations",
    tag = "assistant",
    operation_id = "listDeletedConversations",
    params(
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 100, description = "Conversations per page, 1 to 100; default 50."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor` of this list, for the next page."),
    ),
    responses(
        (status = 200, description = "A page of the caller's deleted conversations that can still be restored.", body = DeletedConversationList),
        (status = 400, description = "A malformed query (`invalid_request`), a `limit` out of range (`conversation.limit_invalid`), or a `cursor` that is not a `next_cursor` of this list (`conversation.cursor_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_deleted_conversations(
    State(state): State<Routes>,
    user: UserIdentity,
    query: Result<Query<DeletedListQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list deleted conversations";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Ok(Query(query)) = query else {
        return super::error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let (limit, after) = match page(
        query.limit.as_deref(),
        query.cursor.as_deref(),
        CursorKind::Deleted,
    ) {
        Ok(page) => page,
        Err(refused) => return refused,
    };
    let after = after.map(|cursor| store::DeletedCursor {
        deleted_at: cursor.at,
        id: cursor.id,
    });
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = store::list_deleted(&mut tx, principal, after, limit).await;
    match super::finish(tx, result).await {
        Ok(page) => super::ok(
            StatusCode::OK,
            &DeletedConversationList {
                conversations: page.items.iter().map(DeletedConversation::from).collect(),
                next_cursor: page.next.map(|next| {
                    Cursor {
                        kind: CursorKind::Deleted,
                        at: next.deleted_at,
                        id: next.id,
                    }
                    .encode()
                }),
            },
        ),
        Err(failure) => super::store_failure(&user, None, ACTION, &failure),
    }
}

/// The page size and the cursor of a list request, or the 400 to answer.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
fn page(
    limit: Option<&str>,
    cursor: Option<&str>,
    kind: CursorKind,
) -> Result<(usize, Option<Cursor>), Response> {
    let Some(limit) = super::parse_limit(limit) else {
        let max = u64::try_from(store::MAX_PAGE).unwrap_or(u64::MAX);
        return Err(super::error(
            ErrorBody::new(ErrorCode::InvalidRequest)
                .with_message_key(LIMIT_INVALID)
                .with_detail("max", max),
        ));
    };
    let cursor = match cursor {
        None => None,
        Some(text) => Some(Cursor::decode(text, kind).ok_or_else(|| {
            super::error(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(CURSOR_INVALID))
        })?),
    };
    Ok((limit, cursor))
}

/// Whether the body of a request that takes no settings is one of the three ways to send none:
/// no body, `null`, or an empty object (`T` refuses every field).
fn no_settings<T: serde::de::DeserializeOwned>(body: &[u8]) -> bool {
    body.trim_ascii().is_empty()
        || body.trim_ascii() == b"null"
        || super::parse_object::<T>(body).is_some()
}

fn body_refused(refusal: BodyRefusal) -> Response {
    match refusal {
        BodyRefusal::NotJson => super::error(ErrorBody::new(ErrorCode::InvalidRequest)),
        BodyRefusal::TooLarge => body_invalid(),
    }
}

fn body_invalid() -> Response {
    super::error(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: &str) -> Result<Update, ErrorBody> {
        parse_update(&Bytes::copy_from_slice(json.as_bytes()))
    }

    #[test]
    fn updates_need_a_field_and_a_valid_title() {
        assert_eq!(
            update(r#"{"title":"  Monthly energy  "}"#),
            Ok(Update {
                title: Some("Monthly energy".into()),
                archived: None,
            })
        );
        assert_eq!(
            update(r#"{"archived":true}"#),
            Ok(Update {
                title: None,
                archived: Some(true),
            })
        );
        let title = "字".repeat(MAX_TITLE_CHARS);
        assert_eq!(
            update(&format!(r#"{{"title":"{title}","archived":false}}"#)),
            Ok(Update {
                title: Some(title),
                archived: Some(false),
            })
        );
        // Joiners and directional marks beside visible characters: an emoji sequence (ZWJ),
        // Persian (ZWNJ), Hebrew after a right-to-left mark, Arabic after the Arabic letter mark.
        for title in [
            "\u{1F469}\u{200D}\u{1F4BB} energy",
            "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}",
            "\u{200F}\u{05E9}\u{05DC}\u{05D5}\u{05DD}",
            "\u{061C}\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}",
            "a\u{200E}b",
        ] {
            let body = serde_json::json!({ "title": title }).to_string();
            assert_eq!(
                update(&body),
                Ok(Update {
                    title: Some(title.to_owned()),
                    archived: None,
                }),
                "{title:?}"
            );
        }

        for (body, key) in [
            ("{}", "conversation.update_empty"),
            (r#"{"title":null}"#, "conversation.update_empty"),
            ("", "conversation.body_invalid"),
            ("[]", "conversation.body_invalid"),
            (r#"{"title":1}"#, "conversation.body_invalid"),
            (r#"{"archived":"yes"}"#, "conversation.body_invalid"),
            (
                r#"{"title":"a","pinned":true}"#,
                "conversation.body_invalid",
            ),
            (r#"{"title":"a","title":"b"}"#, "conversation.body_invalid"),
            ("  [\"a\", true]", "conversation.body_invalid"),
            ("null", "conversation.body_invalid"),
        ] {
            let refused = update(body).expect_err(body);
            assert_eq!(refused.code(), ErrorCode::InvalidRequest, "{body}");
            assert_eq!(refused.message_key().as_str(), key, "{body}");
        }

        for title in [
            String::new(),
            "   ".into(),
            "字".repeat(MAX_TITLE_CHARS + 1),
            "line\nbreak".into(),
            "nul\u{0}".into(),
            "tab\tinside".into(),
            // Nothing visible: zero-width and format characters only, with or without spaces.
            "\u{200B}".into(),
            " \u{200B}\u{200D} ".into(),
            "\u{FEFF}".into(),
            "\u{00AD}".into(),
            "\u{2060}\u{200C}".into(),
            "\u{200E}\u{200F}\u{061C}".into(),
            "\u{3164}".into(),
            "\u{E0100}".into(),
            "\u{E01EF}\u{FE0F}".into(),
            "\u{2800}\u{2800}".into(),
            "\u{1D173}".into(),
            "\u{FFF9}".into(),
            "\u{0600}".into(),
            "a\u{206A}b".into(),
            // Bidirectional embedding, override and isolate controls, line and paragraph
            // separators, even beside visible characters.
            "invoice\u{202E}fdp.exe".into(),
            "a\u{202A}b".into(),
            "a\u{202B}b".into(),
            "a\u{202C}b".into(),
            "a\u{202D}b".into(),
            "a\u{2066}b\u{2069}".into(),
            "a\u{2067}b".into(),
            "a\u{2068}b".into(),
            "a\u{2028}b".into(),
            "a\u{2029}b".into(),
        ] {
            let body = serde_json::json!({ "title": title }).to_string();
            let refused = update(&body).expect_err(&body);
            assert_eq!(refused.code(), ErrorCode::ValidationFailed, "{body}");
            assert_eq!(refused.field_errors().len(), 1, "{body}");
            let field = &refused.field_errors()[0];
            assert_eq!(
                serde_json::to_value(field).expect("json"),
                serde_json::json!({
                    "field": "/title",
                    "message_key": "conversation.title_invalid",
                    "details": { "max": 200 }
                }),
                "{body}"
            );
        }
    }
}
