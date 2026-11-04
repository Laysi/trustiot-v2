//! Sharing a conversation with the whole tenant (Laysi/trustiot-v2#650; the user's decision of
//! 2026-10-24): the owner shares and unshares it; the other members of the tenant list what is
//! shared with them, read a shared conversation's questions and answers, and copy them into a
//! conversation of their own, where they go on asking with their own identity and permissions.
//!
//! A reader can never ask in, rename, archive, delete, restore, share or unshare another
//! member's conversation, nor read it through the owner's endpoints: those stay the owner's
//! alone ([`super::conversations`]), because the conversation's tools run as its owner. Tenant
//! and platform administrators have no special access: a conversation that is not shared is a
//! 404 for them like for everyone else, and a shared one they read and copy like any member.
//! Every conversation the caller may not see here (missing, another tenant's, not shared,
//! deleted, the caller's own on the reader's view, another member's on the owner's endpoints, a
//! path ID that is not a UUID) gets the one 404 of [`super::not_found`], byte for byte.
//!
//! What a reader sees is what the chat shows the owner, the questions and the answers' text
//! ([`crate::model::NativeMessage::shown_text`]); never the thinking, the tool calls and results,
//! the system prompt, the tenant context, the tool definitions, or whether a question is
//! running. A copy carries the same text only, one user message per question and one assistant
//! message per answer, in a new conversation with today's preamble
//! ([`crate::Agent::copied_conversation`]): nothing in it is bound to the source's thinking or
//! runs with the owner's identity. Unsharing, deleting or purging the source never takes a copy
//! back. A shared conversation's dashboard drafts follow it: its readers see them read-only and
//! run their datasets as themselves (`crate::dashboards::store::conversation_access`,
//! [`super::dashboards`]); a copy carries none.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role this build does not know | 403 `permission_denied` |
//! | Not a conversation the caller may see on that endpoint (see above) | 404 `not_found` |
//! | A copy whose `Content-Type` is not `application/json` | 400 `invalid_request` |
//! | A copy's body other than none, `null` or `{}`, or over [`super::conversations::MAX_BODY_BYTES`] | 400 `invalid_request`, key `conversation.body_invalid` |
//! | A `limit` that is not 1 to 100, or a `cursor` that is not a `next_cursor` of the shared list | 400 `invalid_request`, key `conversation.limit_invalid` or `conversation.cursor_invalid` |
//! | A copy whose history would be over [`store::MAX_HISTORY_BYTES`] | 409 `conflict`, key `conversation.copy_too_large`, `{"max_bytes": 16777216}`; nothing is stored |
//! | A copy when the caller started 100 conversations in the tenant within 24 hours | 429 `rate_limited`, key `conversation.start_limited`, as starting one |
//! | A lock not granted within [`super::REQUEST_LOCK_TIMEOUT`], or the database failed | 503 `unavailable`; nothing was written |
//!
//! Sharing, unsharing and copying each log one INFO line with the user, tenant and conversation
//! (and the copy's new conversation), never a title, a question or an answer.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::LOCATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use contract::{ErrorBody, ErrorCode, MessageKey, UserId};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantTransaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::conversations::{
    CONVERSATIONS_PATH, Conversation, ConversationMessage, MAX_BODY_BYTES, TENANT_CONTEXT,
    body_invalid, body_refused, no_settings, page, shown_kind,
};
use super::{Cursor, CursorKind};
use crate::Agent;
use crate::history::{EntryKind, Principal};
use crate::store::{self, CopySource, MAX_HISTORY_BYTES, StoreError, TitleSource};

/// Sharing (`PUT`) and unsharing (`DELETE`) one of the caller's conversations.
pub const SHARE_PATH: &str = "/api/assistant/v1/conversations/{conversation_id}/share";
/// Copying a conversation the caller owns or that another member shared.
pub const COPY_PATH: &str = "/api/assistant/v1/conversations/{conversation_id}/copy";
/// The conversations other members of the tenant shared.
pub const SHARED_CONVERSATIONS_PATH: &str = "/api/assistant/v1/shared-conversations";
/// One of them, read-only.
pub const SHARED_CONVERSATION_PATH: &str =
    "/api/assistant/v1/shared-conversations/{conversation_id}";

/// A copy whose history would be over [`MAX_HISTORY_BYTES`] (409); `details.max_bytes`.
pub const COPY_TOO_LARGE: MessageKey = MessageKey::new("conversation.copy_too_large");

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

/// The sharing routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, agent: Arc<Agent>) -> Router {
    Router::new()
        .route(
            SHARE_PATH,
            put(share_conversation).delete(unshare_conversation),
        )
        .route(COPY_PATH, post(copy_conversation))
        .route(SHARED_CONVERSATIONS_PATH, get(list_shared_conversations))
        .route(SHARED_CONVERSATION_PATH, get(get_shared_conversation))
        .with_state(Routes {
            verifier,
            db,
            agent,
        })
}

/// A conversation another member of the tenant shared, as the list shows it: no messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SharedConversation {
    /// The conversation's ID.
    pub id: Uuid,
    /// Its title; `null` if it has none.
    pub title: Option<String>,
    /// The user who owns and shared it.
    pub owner_user_id: UserId,
    /// When the owner shared it.
    pub shared_at: DateTime<Utc>,
    /// When messages were last added to it; the list's order.
    pub last_activity_at: DateTime<Utc>,
}

impl From<&store::SharedConversation> for SharedConversation {
    fn from(shared: &store::SharedConversation) -> Self {
        Self {
            id: shared.id,
            title: shared.title.as_ref().map(|title| title.text.clone()),
            owner_user_id: shared.owner_id,
            shared_at: shared.shared_at,
            last_activity_at: shared.last_activity_at,
        }
    }
}

/// One page of the conversations other members of the tenant shared, most recently active
/// first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SharedConversationList {
    /// The conversations, most recently active first (by `last_activity_at`, then `id`).
    pub conversations: Vec<SharedConversation>,
    /// Pass it as `cursor` for the next page of this list; `null` on the last page. Opaque.
    pub next_cursor: Option<String>,
}

/// A shared conversation as its readers see it: the questions and the answers' text, oldest
/// first. The assistant's reasoning, its tool calls and their results, the instructions it
/// started with, and whether a question is being answered are not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SharedConversationDetail {
    /// The conversation's ID.
    pub id: Uuid,
    /// Its title; `null` if it has none.
    pub title: Option<String>,
    /// The user who owns and shared it.
    pub owner_user_id: UserId,
    /// When the owner shared it.
    pub shared_at: DateTime<Utc>,
    /// When messages were last added to it.
    pub last_activity_at: DateTime<Utc>,
    /// The questions and answers, oldest first. The answers were produced with the owner's
    /// permissions.
    pub messages: Vec<ConversationMessage>,
}

/// The shared list's query parameters as they arrive. Unknown parameters are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct SharedListQuery {
    pub limit: Option<String>,
    pub cursor: Option<String>,
}

/// Copying a conversation takes no settings: no body, `null`, or an empty object.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyConversation {}

/// Shares one of the caller's conversations with every member of the tenant, who can then read
/// its questions and answers and copy them. The answers were produced with the caller's
/// permissions, so they can hold data other members cannot see otherwise.
#[utoipa::path(
    put,
    path = "/api/assistant/v1/conversations/{conversation_id}/share",
    tag = "assistant",
    operation_id = "shareConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    responses(
        (status = 200, description = "Shared (sharing a shared conversation keeps its `shared_at`); the conversation as it is now.", body = Conversation),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing changed.", body = ErrorBody),
    )
)]
async fn share_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    set_shared(state, user, id, true).await
}

/// Takes one of the caller's conversations back from the tenant: its readers cannot see it any
/// more. The copies they made stay theirs.
#[utoipa::path(
    delete,
    path = "/api/assistant/v1/conversations/{conversation_id}/share",
    tag = "assistant",
    operation_id = "unshareConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    responses(
        (status = 200, description = "Not shared (unsharing a conversation that is not shared changes nothing); the conversation as it is now.", body = Conversation),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`); nothing changed.", body = ErrorBody),
    )
)]
async fn unshare_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    set_shared(state, user, id, false).await
}

/// Shares (`shared`) or unshares the caller's conversation. Neither takes a body: a `PUT` or a
/// `DELETE` from another origin needs a CORS preflight, which nothing answers, so they need no
/// JSON `Content-Type` against cross-site requests (like deleting a conversation).
async fn set_shared(
    state: Routes,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
    shared: bool,
) -> Response {
    let action = if shared {
        "share a conversation"
    } else {
        "unshare a conversation"
    };
    let principal = match super::principal(&user, action) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some(id) = super::parse_id(id) else {
        return super::not_found();
    };
    let mut tx = match super::begin(&state.db, &user, action).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = if shared {
        store::share(&mut tx, principal, id).await
    } else {
        store::unshare(&mut tx, principal, id).await
    };
    match super::finish(tx, result).await {
        Ok((summary, changed)) => {
            let outcome = match (shared, changed) {
                (true, true) => "shared",
                (true, false) => "already_shared",
                (false, true) => "unshared",
                (false, false) => "not_shared",
            };
            // Two call sites: a tracing event's message is a literal.
            if shared {
                tracing::info!(
                    user_id = %user.user_id(),
                    tenant_id = %principal.tenant_id,
                    conversation_id = %id,
                    outcome,
                    "conversation shared"
                );
            } else {
                tracing::info!(
                    user_id = %user.user_id(),
                    tenant_id = %principal.tenant_id,
                    conversation_id = %id,
                    outcome,
                    "conversation unshared"
                );
            }
            super::ok(StatusCode::OK, &Conversation::from(&summary))
        }
        Err(failure) => super::store_failure(&user, Some(id), action, &failure),
    }
}

/// The conversations other members of the caller's tenant shared, most recently active first:
/// titles, owners and times, no messages. The caller's own shared conversations are in their own
/// list instead.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/shared-conversations",
    tag = "assistant",
    operation_id = "listSharedConversations",
    params(
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 100, description = "Conversations per page, 1 to 100; default 50."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor` of this list, for the next page."),
    ),
    responses(
        (status = 200, description = "A page of the conversations shared with the caller; deleted ones are never listed.", body = SharedConversationList),
        (status = 400, description = "A malformed query (`invalid_request`), a `limit` out of range (`conversation.limit_invalid`), or a `cursor` that is not a `next_cursor` of this list (`conversation.cursor_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_shared_conversations(
    State(state): State<Routes>,
    user: UserIdentity,
    query: Result<Query<SharedListQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list shared conversations";
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
        CursorKind::Shared,
    ) {
        Ok(page) => page,
        Err(refused) => return refused,
    };
    let after = after.map(|cursor| store::SharedCursor {
        last_activity_at: cursor.at,
        id: cursor.id,
    });
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let result = store::list_shared(&mut tx, principal, after, limit).await;
    match super::finish(tx, result).await {
        Ok(page) => super::ok(
            StatusCode::OK,
            &SharedConversationList {
                conversations: page.items.iter().map(SharedConversation::from).collect(),
                next_cursor: page.next.map(|next| {
                    Cursor {
                        kind: CursorKind::Shared,
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

/// A conversation another member of the caller's tenant shared, read-only: its questions and
/// answers.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/shared-conversations/{conversation_id}",
    tag = "assistant",
    operation_id = "getSharedConversation",
    params(("conversation_id" = Uuid, Path, description = "The conversation's ID.")),
    responses(
        (status = 200, description = "The shared conversation and the messages the chat shows.", body = SharedConversationDetail),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No conversation another member of the tenant shares with this ID: missing, not shared, deleted, or the caller's own (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_shared_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
) -> Response {
    const ACTION: &str = "read a shared conversation";
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
    let result = shared_detail(&mut tx, principal, id).await;
    match super::finish(tx, result).await {
        Ok(detail) => super::ok(StatusCode::OK, &detail),
        Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
    }
}

async fn shared_detail(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<SharedConversationDetail, StoreError> {
    let view = store::shared_view(tx, principal, id)
        .await?
        .ok_or(StoreError::NotStored)?;
    let messages = view
        .entries
        .into_iter()
        .filter_map(|(kind, text)| shown_kind(kind).map(|kind| ConversationMessage { kind, text }))
        .collect();
    let conversation = SharedConversation::from(&view.conversation);
    Ok(SharedConversationDetail {
        id: conversation.id,
        title: conversation.title,
        owner_user_id: conversation.owner_user_id,
        shared_at: conversation.shared_at,
        last_activity_at: conversation.last_activity_at,
        messages,
    })
}

/// Copies a conversation into a new one of the caller's: the caller's own, or one another member
/// of the tenant shared. The copy has the source's title and, for each answered question, the
/// question's and the answer's text; no reasoning, tool calls or tool results. It starts with
/// today's system prompt, tools and model settings, and its questions run as the caller with the
/// caller's permissions. It is not shared and not archived, and stays when the source is
/// unshared, deleted or purged.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/conversations/{conversation_id}/copy",
    tag = "assistant",
    operation_id = "copyConversation",
    params(("conversation_id" = Uuid, Path, description = "The ID of the conversation to copy.")),
    request_body(content = Option<CopyConversation>, description = "Nothing to set: send `Content-Type: application/json` (required, against cross-site requests) with no body, `null`, or an empty object."),
    responses(
        (status = 201, description = "The copy, a new conversation of the caller's; `Location` names it.", body = Conversation,
            headers(("location" = String, description = "The new conversation's path."))),
        (status = 400, description = "The `Content-Type` is not `application/json` (`invalid_request`), or there is a body other than `null` or an empty object (`conversation.body_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No conversation with this ID that the caller owns or another member shares, or it is deleted (`not_found`).", body = ErrorBody),
        (status = 409, description = "The copy's questions and answers would be over 16 MiB (`conflict`, key `conversation.copy_too_large`, `details.max_bytes`); nothing was stored.", body = ErrorBody),
        (status = 429, description = "The caller started 100 conversations in this tenant within the last 24 hours, deleted ones and copies included (`rate_limited`, key `conversation.start_limited`, `details.max`); nothing was stored.", body = ErrorBody,
            headers(("retry-after" = u64, description = "Seconds until a start is allowed again."))),
        (status = 503, description = "The database failed (`unavailable`); nothing was written.", body = ErrorBody),
    )
)]
async fn copy_conversation(
    State(state): State<Routes>,
    user: UserIdentity,
    id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "copy a conversation";
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
    if !no_settings::<CopyConversation>(&body) {
        return body_invalid();
    }
    let mut tx = match super::begin(&state.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    match copy(&mut tx, &state.agent, principal, id).await {
        Ok(copied) => match super::finish(tx, Ok(copied)).await {
            Ok((summary, rounds)) => {
                tracing::info!(
                    user_id = %user.user_id(),
                    tenant_id = %principal.tenant_id,
                    conversation_id = %id,
                    new_conversation_id = %summary.id,
                    rounds,
                    outcome = "copied",
                    "conversation copied"
                );
                let mut response = super::ok(StatusCode::CREATED, &Conversation::from(&summary));
                if let Ok(location) =
                    HeaderValue::from_str(&format!("{CONVERSATIONS_PATH}/{}", summary.id))
                {
                    response.headers_mut().insert(LOCATION, location);
                }
                response
            }
            Err(failure) => super::store_failure(&user, Some(id), ACTION, &failure),
        },
        Err(CopyFailure::TooLarge) => {
            // A rollback that fails leaves nothing behind: the connection is closed then.
            let _ = tx.rollback().await;
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %principal.tenant_id,
                conversation_id = %id,
                outcome = "too_large",
                "conversation copy refused: the copy would be too large"
            );
            let max = u64::try_from(MAX_HISTORY_BYTES).unwrap_or(u64::MAX);
            super::error(
                ErrorBody::new(ErrorCode::Conflict)
                    .with_message_key(COPY_TOO_LARGE)
                    .with_detail("max_bytes", max),
            )
        }
        Err(CopyFailure::Store(failure)) => {
            let _ = tx.rollback().await;
            super::store_failure(&user, Some(id), ACTION, &failure)
        }
    }
}

/// Why a copy was not made.
enum CopyFailure {
    /// The new history would be over [`MAX_HISTORY_BYTES`].
    TooLarge,
    Store(StoreError),
}

impl From<StoreError> for CopyFailure {
    fn from(failure: StoreError) -> Self {
        Self::Store(failure)
    }
}

/// Makes the copy in `tx`: reads the source's questions and answers, builds and stores the new
/// conversation of `principal` with the source's title, and returns it with the number of rounds
/// it carries.
async fn copy(
    tx: &mut TenantTransaction,
    agent: &Agent,
    principal: Principal,
    id: Uuid,
) -> Result<(store::ConversationSummary, usize), CopyFailure> {
    // The texts alone over the limit make a copy over it too (writing them as messages only adds
    // to them), so the reading stops there; the copy's own size is checked below.
    let (title, entries) = match store::copy_source(tx, principal, id, MAX_HISTORY_BYTES)
        .await?
        .ok_or(StoreError::NotStored)?
    {
        CopySource::Texts { title, entries } => (title, entries),
        CopySource::TooLarge => return Err(CopyFailure::TooLarge),
    };
    let rounds = rounds(entries);
    let conversation = agent
        .copied_conversation(principal, TENANT_CONTEXT, &rounds)
        .map_err(|_| StoreError::Unstorable("a copied message"))?;
    let bytes: usize = conversation
        .messages()
        .iter()
        .map(|message| message.json().len())
        .sum();
    if bytes > MAX_HISTORY_BYTES {
        return Err(CopyFailure::TooLarge);
    }
    store::create(tx, &conversation).await?;
    let new_id = conversation.id();
    if let Some(title) = title.filter(|title| store::title_is_valid(&title.text)) {
        store::set_title(tx, principal, new_id, &title.text, TitleSource::User).await?;
    }
    let summary = store::metadata(tx, principal, new_id)
        .await?
        .ok_or(StoreError::Corrupt("a conversation that was just stored"))?;
    Ok((summary, rounds.len()))
}

/// The answered rounds of a source's questions and answers, as (question, answer) texts in
/// order. Every stored round ends with its answer (a failed, stopped or running round is never
/// stored, and a discarded one is not among the entries); should a question still have no answer
/// before the next question, or an answer no question, it is left out. A round whose question or
/// answer has no visible text is left out too: the Messages API refuses an empty or blank text
/// block, so the copy could not be asked in.
fn rounds(entries: Vec<(EntryKind, String)>) -> Vec<(String, String)> {
    let mut rounds = Vec::new();
    let mut question: Option<String> = None;
    for (kind, text) in entries {
        match kind {
            EntryKind::Question => question = Some(text),
            EntryKind::Answer => {
                let answer = text;
                if let Some(question) = question.take()
                    && !question.trim().is_empty()
                    && !answer.trim().is_empty()
                {
                    rounds.push((question, answer));
                }
            }
            EntryKind::Step | EntryKind::ToolResults | EntryKind::Note => {}
        }
    }
    rounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(text: &str) -> (EntryKind, String) {
        (EntryKind::Question, text.to_owned())
    }

    fn answer(text: &str) -> (EntryKind, String) {
        (EntryKind::Answer, text.to_owned())
    }

    #[test]
    fn a_copy_takes_each_answered_round_as_its_two_texts() {
        let entries = vec![
            question("How much energy?"),
            answer("412 kWh."),
            question("And last week?"),
            answer("438 kWh."),
        ];
        assert_eq!(
            rounds(entries),
            [
                ("How much energy?".to_owned(), "412 kWh.".to_owned()),
                ("And last week?".to_owned(), "438 kWh.".to_owned()),
            ]
        );
        // Unanswered questions, answers without a question and blank texts are left out.
        let entries = vec![
            answer("Orphan."),
            question("Never answered?"),
            question("Answered?"),
            answer("Yes."),
            question("Blank answer?"),
            answer(" \n"),
            question("  "),
            answer("Blank question."),
            question("Last, unanswered?"),
        ];
        assert_eq!(
            rounds(entries),
            [("Answered?".to_owned(), "Yes.".to_owned())]
        );
    }
}
