//! Sharing a conversation with its tenant (Laysi/trustiot-v2#650; the user's decision of
//! 2026-10-24): the owner shares and unshares ([`share`], [`unshare`]); the other members of the
//! tenant list what is shared with them ([`list_shared`]), read a shared conversation's questions
//! and answers ([`shared_view`]), and copy them into a conversation of their own
//! ([`copy_source`]). Schema: `migrations/postgres/0005_conversation_sharing.sql`.
//!
//! A reader sees a conversation only while it is shared, not deleted and not their own, in the
//! transaction's tenant (row-level security keeps every other tenant out); every other
//! conversation is `None` or [`StoreError::NotStored`] exactly as one that does not exist. The
//! owner reads their own through [`super::load`] and [`super::metadata`], never through
//! [`shared_view`]. A reader never gets more of the history than the questions and answers,
//! with what the answer guard removed from an answer (Laysi/trustiot-v2#646), as the owner's chat
//! shows them: the statement here leaves the body of every other message (the steps with their
//! thinking and tool calls, the tool results, the notes) and of every discarded round in the
//! database, and the preamble (system prompt, tenant context, tool definitions) is not selected
//! at all.
//! [`super::load`], which returns the whole history for the model, stays the owner's alone: the
//! conversation's tools run as its owner, so nobody else may continue it.

use chrono::{DateTime, Utc};
use contract::UserId;
use futures_util::TryStreamExt;
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use super::threads::{ConversationSummary, Page, Title, fetch_size, metadata, page_size, title};
use super::{StoreError, check_tenant, raw_message, read_removed};
use crate::answer_guard::RemovedContent;
use crate::history::{EntryKind, Principal};
use crate::model::{Format, NativeMessage};

/// `$1` tenant, `$2` conversation, `$3` user: shares the user's conversation unless it is
/// deleted or shared already (a share keeps its first time). Returns a row only when it changed
/// something.
const SHARE: &str = "UPDATE assistant.conversations
SET shared_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL AND shared_at IS NULL
RETURNING 1 AS updated";

/// `$1` tenant, `$2` conversation, `$3` user: unshares the user's conversation unless it is
/// deleted or not shared. Returns a row only when it changed something.
const UNSHARE: &str = "UPDATE assistant.conversations
SET shared_at = NULL
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL AND shared_at IS NOT NULL
RETURNING 1 AS updated";

/// `$1` tenant, `$2` the caller: the tenant's shared conversations that are not deleted and not
/// the caller's. Its conditions imply the predicate of the partial index `conversations_shared`
/// (0005_conversation_sharing.sql), and the two statements below read it in its order: the order
/// names the table's `conversation_id` (a `uuid`), not the output column of that name (its
/// text), so Postgres reads the index in order and sorts nothing.
macro_rules! shared_list {
    () => {
        "SELECT conversation_id::text AS conversation_id,
    user_id::text AS owner_id, title, title_source, shared_at, last_activity_at
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND shared_at IS NOT NULL AND deleted_at IS NULL
    AND user_id <> $2::uuid"
    };
}

/// [`shared_list!`]'s first page, most recently active first: `$3` how many rows.
const LIST_SHARED_FIRST: &str = concat!(
    shared_list!(),
    "
ORDER BY last_activity_at DESC, conversations.conversation_id DESC
LIMIT $3"
);

/// [`shared_list!`]'s page after the cursor `$3`, `$4` (the last item of the previous page):
/// `$5` how many rows. The cursor is one row comparison in the index's order, so it is an index
/// condition: a page starts where the cursor points, whatever cursor a client sends.
const LIST_SHARED_AFTER: &str = concat!(
    shared_list!(),
    "
    AND (last_activity_at, conversation_id) < ($3::timestamptz, $4::uuid)
ORDER BY last_activity_at DESC, conversations.conversation_id DESC
LIMIT $5"
);

/// `$1` tenant, `$2` conversation, `$3` the caller, `$4` whether the caller's own conversation
/// counts too (a copy, not the reader's view): the conversation, when it is not deleted and is
/// shared and not the caller's (or, with `$4`, is the caller's), with every stored message in
/// order. One row per message (one row with a NULL `seq` when it has none), so the conversation
/// and its history come from one statement and one snapshot: an unshare or a delete that commits
/// meanwhile is either wholly before it or wholly after. A message's body and what the answer
/// guard removed from it are read only when the chat shows it, a question or an answer outside a
/// discarded round; every other body stays in the database. [`shown`] reads the rows one at a
/// time and keeps only each body's shown text.
const SHOWN: &str = "SELECT c.conversation_id::text AS conversation_id,
    c.user_id::text AS owner_id, c.title, c.title_source, c.shared_at, c.last_activity_at,
    m.seq, m.kind, m.format, m.body, m.removed, m.discarded
FROM assistant.conversations c
LEFT JOIN LATERAL (
    SELECT m.seq, m.kind, m.format,
        CASE WHEN d.from_seq IS NULL AND m.kind IN ('question', 'answer') THEN m.body END
            AS body,
        CASE WHEN d.from_seq IS NULL AND m.kind IN ('question', 'answer') THEN m.removed END
            AS removed,
        d.from_seq IS NOT NULL AS discarded
    FROM assistant.messages m
    LEFT JOIN LATERAL (
        SELECT r.from_seq FROM assistant.discarded_rounds r
        WHERE r.tenant_id = m.tenant_id AND r.conversation_id = m.conversation_id
            AND m.seq BETWEEN r.from_seq AND r.to_seq
        LIMIT 1
    ) d ON true
    WHERE m.tenant_id = c.tenant_id AND m.conversation_id = c.conversation_id
) m ON true
WHERE c.tenant_id = $1::uuid AND c.conversation_id = $2::uuid AND c.deleted_at IS NULL
    AND ((c.shared_at IS NOT NULL AND c.user_id <> $3::uuid) OR ($4 AND c.user_id = $3::uuid))
ORDER BY m.seq";

/// `$1` tenant, `$2` conversation, `$3` user: whether the conversation is one [`copy_source`]
/// reads for that user, [`SHOWN`]'s condition with the owner's own included. One row by the
/// primary key; no message is read.
const COPYABLE: &str = "SELECT EXISTS (SELECT 1 FROM assistant.conversations c
    WHERE c.tenant_id = $1::uuid AND c.conversation_id = $2::uuid AND c.deleted_at IS NULL
        AND ((c.shared_at IS NOT NULL AND c.user_id <> $3::uuid) OR c.user_id = $3::uuid))";

/// A conversation as a member of its tenant sees it shared: no messages. Times are the
/// database's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedConversation {
    pub id: Uuid,
    /// Its owner, who shared it.
    pub owner_id: UserId,
    pub title: Option<Title>,
    /// When the owner shared it.
    pub shared_at: DateTime<Utc>,
    /// When messages were last added to it.
    pub last_activity_at: DateTime<Utc>,
}

/// Where a page of [`list_shared`] ended: pass it as `after` for the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedCursor {
    pub last_activity_at: DateTime<Utc>,
    pub id: Uuid,
}

/// One message the chat shows of a conversation: a question or an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownEntry {
    /// Its position in the database (`seq`), which [`super::answer_drafts`] names an answer by.
    pub position: usize,
    /// [`EntryKind::Question`] or [`EntryKind::Answer`].
    pub kind: EntryKind,
    /// The text the chat shows of it ([`NativeMessage::shown_text`]).
    pub text: String,
    /// What the answer guard removed from it: an answer's, empty for a question.
    pub removed: RemovedContent,
}

/// A shared conversation as a reader sees it: its questions and answers, in order.
#[derive(Debug)]
pub struct SharedView {
    pub conversation: SharedConversation,
    /// The questions and answers, discarded rounds left out.
    pub entries: Vec<ShownEntry>,
}

/// What a copy takes of a conversation.
#[derive(Debug)]
pub enum CopySource {
    /// Its title and its questions and answers, in order, as [`SharedView::entries`].
    Texts {
        title: Option<Title>,
        entries: Vec<ShownEntry>,
    },
    /// Its questions' and answers' text alone is over the limit the caller passed: the reading
    /// stopped there, so the store never held more than that of the history.
    TooLarge,
}

/// Shares `principal`'s conversation `id` with every member of its tenant. Sharing a shared
/// conversation keeps the time it was first shared. Returns the conversation as it is now, and
/// whether this call shared it (`false` when it was shared already).
///
/// # Errors
///
/// [`StoreError::NotStored`] when there is no such conversation of theirs, or it is deleted;
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn share(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<(ConversationSummary, bool), StoreError> {
    set_shared(tx, principal, id, true).await
}

/// Takes `principal`'s conversation `id` back from the tenant: its readers cannot see it any
/// more. The copies they made stay theirs. Unsharing a conversation that is not shared changes
/// nothing. Returns the conversation as it is now, and whether this call unshared it.
///
/// # Errors
///
/// As [`share`].
pub async fn unshare(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<(ConversationSummary, bool), StoreError> {
    set_shared(tx, principal, id, false).await
}

async fn set_shared(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    shared: bool,
) -> Result<(ConversationSummary, bool), StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let query = if shared {
        sqlx::query(SHARE)
    } else {
        sqlx::query(UNSHARE)
    };
    let changed = query
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .is_some();
    // Read after the update, in the same transaction: not found means not the caller's, or
    // deleted (the update then changed nothing either).
    let summary = metadata(tx, principal, id)
        .await?
        .ok_or(StoreError::NotStored)?;
    Ok((summary, changed))
}

/// The conversations other members of `principal`'s tenant shared, not deleted, most recently
/// active first, at most `limit` (1 to [`super::MAX_PAGE`]) after `after`. The caller's own
/// shared conversations are not in it: the caller has them in their own list.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn list_shared(
    tx: &mut TenantTransaction,
    principal: Principal,
    after: Option<SharedCursor>,
    limit: usize,
) -> Result<Page<SharedConversation, SharedCursor>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let size = page_size(limit);
    let query = match after {
        None => sqlx::query(LIST_SHARED_FIRST)
            .bind(principal.tenant_id.to_string())
            .bind(principal.user_id.to_string())
            .bind(fetch_size(size)),
        Some(cursor) => sqlx::query(LIST_SHARED_AFTER)
            .bind(principal.tenant_id.to_string())
            .bind(principal.user_id.to_string())
            .bind(cursor.last_activity_at)
            .bind(cursor.id.to_string())
            .bind(fetch_size(size)),
    };
    let rows = query.fetch_all(&mut **tx).await?;
    let mut items = rows
        .iter()
        .map(shared_conversation)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if items.len() > size {
        items.truncate(size);
        items.last().map(|last| SharedCursor {
            last_activity_at: last.last_activity_at,
            id: last.id,
        })
    } else {
        None
    };
    Ok(Page { items, next })
}

/// The conversation `id` that another member of `principal`'s tenant shared, with its questions
/// and answers; `None` when there is no such conversation in the tenant, it is not shared, it is
/// deleted, or it is `principal`'s own (the owner reads it with [`super::load`]).
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] (a stored message that does not read
/// back as it was stored) or a database failure.
pub async fn shared_view(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<Option<SharedView>, StoreError> {
    let Some(Shown::Read { head, entries }) = shown(tx, principal, id, false, None).await? else {
        return Ok(None);
    };
    let conversation = shared_conversation(&head)?;
    Ok(Some(SharedView {
        conversation,
        entries,
    }))
}

/// What a copy of conversation `id` by `principal` takes: its title, questions and answers, when
/// it is `principal`'s own or another member shared it, and it is not deleted; `None`
/// otherwise. Never the rest of the history, even for the owner: a copy carries the text only.
/// [`CopySource::TooLarge`] as soon as the texts read add up to more than `max_text_bytes`.
///
/// # Errors
///
/// As [`shared_view`].
pub async fn copy_source(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    max_text_bytes: usize,
) -> Result<Option<CopySource>, StoreError> {
    Ok(
        match shown(tx, principal, id, true, Some(max_text_bytes)).await? {
            None => None,
            Some(Shown::TooLarge) => Some(CopySource::TooLarge),
            Some(Shown::Read { head, entries }) => Some(CopySource::Texts {
                title: title(&head)?,
                entries,
            }),
        },
    )
}

/// Whether `principal` may copy conversation `id` now: the same condition as [`copy_source`],
/// read without the messages. A cheap early check (Laysi/trustiot-v2#783), so a copy of a missing
/// or invisible conversation makes no calls to other services; [`copy_source`] decides again in
/// the copy's own transaction.
///
/// # Errors
///
/// [`StoreError::WrongTenant`] or a database failure.
pub async fn copyable(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<bool, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    Ok(sqlx::query_scalar(COPYABLE)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_one(&mut **tx)
        .await?)
}

/// What [`shown`] read.
enum Shown {
    /// [`SHOWN`]'s first row (the conversation's columns) and the shown texts.
    Read {
        head: PgRow,
        entries: Vec<ShownEntry>,
    },
    /// The texts went over the limit; the rest was not read.
    TooLarge,
}

/// The conversation and its shown texts, read row by row so that at most one message's stored
/// bytes are held at a time beside the texts kept, and stopped once the texts add up to more
/// than `max_text_bytes`: a conversation larger than its readers may take costs no more memory
/// than the limit. Checked like [`super::load`] checks a history: the positions have no gap,
/// every shown message is a known kind and format whose bytes are one JSON value, unchanged, and
/// only an answer has removed content, as written.
/// A stream dropped before its end leaves the rest of the rows to the connection, which skips
/// them before its next statement (the caller's rollback).
async fn shown(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    owner_too: bool,
    max_text_bytes: Option<usize>,
) -> Result<Option<Shown>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let mut rows = sqlx::query(SHOWN)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .bind(owner_too)
        .fetch(&mut **tx);
    let mut head = None;
    let mut entries = Vec::new();
    let mut text_bytes = 0_usize;
    let mut position = 0_usize;
    while let Some(row) = rows.try_next().await? {
        if let Some(seq) = row.try_get::<Option<i32>, _>("seq")? {
            if usize::try_from(seq).ok() != Some(position) {
                return Err(StoreError::Corrupt("a gap in the history's positions"));
            }
            if let Some(entry) = shown_entry(&row, position)? {
                text_bytes = text_bytes.saturating_add(entry.text.len());
                if max_text_bytes.is_some_and(|max| text_bytes > max) {
                    return Ok(Some(Shown::TooLarge));
                }
                entries.push(entry);
            }
            position += 1;
        }
        // A NULL `seq` is the one row of a conversation without messages.
        if head.is_none() {
            head = Some(row);
        }
    }
    Ok(head.map(|head| Shown::Read { head, entries }))
}

/// What the chat shows of one row of [`SHOWN`]; `None` for a message it does not show or one in
/// a discarded round.
fn shown_entry(row: &PgRow, position: usize) -> Result<Option<ShownEntry>, StoreError> {
    if row.try_get::<bool, _>("discarded")? {
        return Ok(None);
    }
    let kind = EntryKind::from_name(&row.try_get::<String, _>("kind")?)
        .ok_or(StoreError::Corrupt("a message's kind"))?;
    if !kind.shown() {
        return Ok(None);
    }
    let format = Format::from_name(&row.try_get::<String, _>("format")?)
        .ok_or(StoreError::Corrupt("a message's format"))?;
    let body: Option<Vec<u8>> = row.try_get("body")?;
    let body = body.ok_or(StoreError::Corrupt("a shown message without its body"))?;
    let text = NativeMessage::new(format, raw_message(body)?)
        .shown_text()
        .ok_or(StoreError::Corrupt("a shown message"))?;
    let removed = read_removed(row.try_get("removed")?)?;
    if kind != EntryKind::Answer && !removed.is_empty() {
        return Err(StoreError::Corrupt(
            "removed content on a message that is not an answer",
        ));
    }
    Ok(Some(ShownEntry {
        position,
        kind,
        text,
        removed,
    }))
}

fn shared_conversation(row: &PgRow) -> Result<SharedConversation, StoreError> {
    Ok(SharedConversation {
        id: super::threads::uuid(row, "conversation_id")?,
        owner_id: row
            .try_get::<String, _>("owner_id")?
            .parse()
            .map_err(|_| StoreError::Corrupt("a user ID"))?,
        title: title(row)?,
        shared_at: row
            .try_get::<Option<DateTime<Utc>>, _>("shared_at")?
            .ok_or(StoreError::Corrupt(
                "a shared conversation without its time",
            ))?,
        last_activity_at: row.try_get("last_activity_at")?,
    })
}
