//! The conversation thread around a history (Laysi/trustiot-v2#644): the list, titles,
//! archiving, soft deletion, the deleted list, restoring, and the purge 30 days after deletion.
//! Every function but [`purge_expired`] acts for one user in a [`TenantTransaction`] of their
//! tenant and sees only that user's conversations; a deleted one exists only for
//! [`list_deleted`] and [`restore`].

use std::time::Duration;

use chrono::{DateTime, Utc};
use service_kit::tenant::{PlatformTransaction, TenantTransaction};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use super::runs::running_before_deadline;
use super::{DELETED_RETENTION_DAYS, MAX_TITLE_CHARS, StoreError, check_tenant, lock_owned};
use crate::history::Principal;

/// The most items one page of [`list`] or [`list_deleted`] returns; a larger limit is lowered
/// to it, a limit of 0 raised to 1.
pub const MAX_PAGE: usize = 100;

/// How long [`purge_expired`]'s statement waits for a row lock before it gives up with SQLSTATE
/// `55P03` (`lock_not_available`), set as the transaction's `lock_timeout`. The purge locks the
/// expired conversations it deletes and, through the cascade, their messages, discarded rounds
/// and runs. Nothing of the service holds such a lock for long (a restore that started just
/// before the 30 days ended commits in milliseconds; the service role cannot even lock an expired
/// row, `expired_stay_deleted`), so a wait this long means a stuck session: the round stops
/// instead of holding its platform connection and the job lock behind it, and the next round
/// tries again. `SKIP LOCKED` would pass over such rows without waiting, but `SELECT ... FOR
/// UPDATE` needs the UPDATE privilege on the table, which the platform role does not have and
/// should not get (tests/conversation_threads.rs checks the refusal).
pub const PURGE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLSTATE `lock_not_available`: [`purge_expired`] waited [`PURGE_LOCK_TIMEOUT`] for a lock.
pub const LOCK_NOT_AVAILABLE: &str = "55P03";

/// Sets `$1` as the transaction's `lock_timeout`, like `SET LOCAL`.
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";

/// `$1` tenant, `$2` user, `$3` whether the archived ones (or else the ones not archived), `$4`
/// and `$5` the cursor (the last item of the previous page, or NULL), `$6` how many rows.
const LIST: &str = "SELECT conversation_id::text AS conversation_id, title, title_source,
    created_at, last_activity_at, archived_at
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND user_id = $2::uuid AND deleted_at IS NULL
    AND (archived_at IS NOT NULL) = $3
    AND ($4::timestamptz IS NULL
         OR last_activity_at < $4::timestamptz
         OR (last_activity_at = $4::timestamptz AND conversation_id < $5::uuid))
ORDER BY last_activity_at DESC, conversation_id DESC
LIMIT $6";

/// `$1` tenant, `$2` conversation, `$3` user.
const METADATA: &str = "SELECT conversation_id::text AS conversation_id, title, title_source,
    created_at, last_activity_at, archived_at
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL";

/// `$1` tenant, `$2` conversation, `$3` user, `$4` title, `$5` source. A user's title replaces
/// any; the model's replaces none or a provisional one; a provisional one only fills an empty
/// title.
const SET_TITLE: &str = "UPDATE assistant.conversations
SET title = $4, title_source = $5
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL
    AND ($5 = 'user' OR title_source IS NULL OR ($5 = 'model' AND title_source = 'provisional'))
RETURNING 1 AS updated";

/// `$1` tenant, `$2` conversation, `$3` user, `$4` archived. Archiving an archived conversation
/// keeps the first time.
const ARCHIVE: &str = "UPDATE assistant.conversations
SET archived_at = CASE WHEN $4 THEN coalesce(archived_at, now()) END
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL
RETURNING 1 AS updated";

/// `$1` tenant, `$2` conversation, `$3` user, `$4` the retention in days. Runs after
/// [`lock_owned`] locked the row and no question was found running.
const DELETE: &str = "UPDATE assistant.conversations
SET deleted_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL
RETURNING conversation_id::text AS conversation_id, title, title_source, deleted_at,
    deleted_at + make_interval(days => $4) AS restorable_until";

/// `$1` tenant, `$2` user, `$3` the retention in days, `$4` and `$5` the cursor, `$6` how many
/// rows.
const LIST_DELETED: &str = "SELECT conversation_id::text AS conversation_id, title, title_source,
    deleted_at, deleted_at + make_interval(days => $3) AS restorable_until
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND user_id = $2::uuid AND deleted_at IS NOT NULL
    AND deleted_at > now() - make_interval(days => $3)
    AND ($4::timestamptz IS NULL
         OR deleted_at < $4::timestamptz
         OR (deleted_at = $4::timestamptz AND conversation_id < $5::uuid))
ORDER BY deleted_at DESC, conversation_id DESC
LIMIT $6";

/// `$1` tenant, `$2` conversation, `$3` user, `$4` the retention in days. Waits for, and then
/// loses to, a purge that deleted the row first (and the other way round).
const RESTORE: &str = "UPDATE assistant.conversations
SET deleted_at = NULL
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NOT NULL AND deleted_at > now() - make_interval(days => $4)
RETURNING 1 AS updated";

/// `$1` the retention in days, `$2` how many. As the platform role, whose policies show it only
/// conversations deleted more than 30 days ago: the condition here only repeats them. A row that
/// a restore changed while this statement waited for its lock is checked again and skipped.
const PURGE: &str = "DELETE FROM assistant.conversations
WHERE deleted_at <= now() - make_interval(days => $1)
    AND (tenant_id, conversation_id) IN (
        SELECT tenant_id, conversation_id FROM assistant.conversations
        WHERE deleted_at <= now() - make_interval(days => $1)
        ORDER BY deleted_at
        LIMIT $2)";

/// Who named a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSource {
    /// Taken from the first question until a better one exists.
    Provisional,
    /// The user's own; nothing generated replaces it.
    User,
    /// Generated by the model; replaces a provisional title only.
    Model,
}

impl TitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::User => "user",
            Self::Model => "model",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Self::Provisional, Self::User, Self::Model]
            .into_iter()
            .find(|source| source.as_str() == name)
    }
}

/// Which of a user's conversations [`list`] returns; deleted ones never.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListFilter {
    /// Those not archived: the default list.
    Active,
    /// The archived ones only.
    Archived,
}

/// A conversation's title and who gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    pub text: String,
    pub source: TitleSource,
}

/// A conversation as the list shows it. Times are the database's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub title: Option<Title>,
    pub created_at: DateTime<Utc>,
    /// When messages were last added (when it was created, before that).
    pub last_activity_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

/// Where a page of [`list`] ended: pass it as `after` for the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListCursor {
    pub last_activity_at: DateTime<Utc>,
    pub id: Uuid,
}

/// A deleted conversation as its owner's deleted list shows it: no messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedConversation {
    pub id: Uuid,
    pub title: Option<Title>,
    pub deleted_at: DateTime<Utc>,
    /// The end of the restore window; the purge removes it after this.
    pub restorable_until: DateTime<Utc>,
}

/// Where a page of [`list_deleted`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeletedCursor {
    pub deleted_at: DateTime<Utc>,
    pub id: Uuid,
}

/// One page, and where the next starts (`None` on the last page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T, C> {
    pub items: Vec<T>,
    pub next: Option<C>,
}

fn page_size(limit: usize) -> usize {
    limit.clamp(1, MAX_PAGE)
}

/// The rows of a page, one more than its size so a next page is known without counting.
fn fetch_size(size: usize) -> i64 {
    // At most MAX_PAGE + 1.
    i64::try_from(size + 1).unwrap_or(i64::MAX)
}

fn uuid(row: &PgRow, column: &str) -> Result<Uuid, StoreError> {
    row.try_get::<String, _>(column)?
        .parse()
        .map_err(|_| StoreError::Corrupt("a conversation ID"))
}

fn title(row: &PgRow) -> Result<Option<Title>, StoreError> {
    let text: Option<String> = row.try_get("title")?;
    let source: Option<String> = row.try_get("title_source")?;
    match (text, source) {
        (None, None) => Ok(None),
        (Some(text), Some(source)) => Ok(Some(Title {
            text,
            source: TitleSource::from_name(&source)
                .ok_or(StoreError::Corrupt("a title's source"))?,
        })),
        _ => Err(StoreError::Corrupt("a title without its source")),
    }
}

fn summary(row: &PgRow) -> Result<ConversationSummary, StoreError> {
    Ok(ConversationSummary {
        id: uuid(row, "conversation_id")?,
        title: title(row)?,
        created_at: row.try_get("created_at")?,
        last_activity_at: row.try_get("last_activity_at")?,
        archived_at: row.try_get("archived_at")?,
    })
}

fn deleted(row: &PgRow) -> Result<DeletedConversation, StoreError> {
    Ok(DeletedConversation {
        id: uuid(row, "conversation_id")?,
        title: title(row)?,
        deleted_at: row.try_get("deleted_at")?,
        restorable_until: row.try_get("restorable_until")?,
    })
}

/// `principal`'s conversations in the transaction's tenant, most recently active first, at most
/// `limit` (1 to [`MAX_PAGE`]) after `after`: those not archived, or only the archived ones
/// (`filter`); deleted ones never. A cursor from the other filter's list only moves the position.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn list(
    tx: &mut TenantTransaction,
    principal: Principal,
    filter: ListFilter,
    after: Option<ListCursor>,
    limit: usize,
) -> Result<Page<ConversationSummary, ListCursor>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let size = page_size(limit);
    let rows = sqlx::query(LIST)
        .bind(principal.tenant_id.to_string())
        .bind(principal.user_id.to_string())
        .bind(filter == ListFilter::Archived)
        .bind(after.map(|cursor| cursor.last_activity_at))
        .bind(after.map(|cursor| cursor.id.to_string()))
        .bind(fetch_size(size))
        .fetch_all(&mut **tx)
        .await?;
    let mut items = rows.iter().map(summary).collect::<Result<Vec<_>, _>>()?;
    let next = if items.len() > size {
        items.truncate(size);
        items.last().map(|last| ListCursor {
            last_activity_at: last.last_activity_at,
            id: last.id,
        })
    } else {
        None
    };
    Ok(Page { items, next })
}

/// `principal`'s conversation `id` as the list shows it; `None` when there is no such
/// conversation of theirs or it is deleted.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn metadata(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<Option<ConversationSummary>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    sqlx::query(METADATA)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .as_ref()
        .map(summary)
        .transpose()
}

/// Names `principal`'s conversation `id`. A [`TitleSource::User`] title always replaces the
/// current one (renaming); a [`TitleSource::Model`] one only an empty or provisional title; a
/// [`TitleSource::Provisional`] one only an empty title. Returns whether the title changed.
///
/// # Errors
///
/// [`StoreError::Unstorable`] for an empty title, one over [`MAX_TITLE_CHARS`] characters or
/// one with control characters; [`StoreError::NotStored`], [`StoreError::WrongTenant`] or a
/// database failure.
pub async fn set_title(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    title: &str,
    source: TitleSource,
) -> Result<bool, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let chars = title.chars().count();
    if chars == 0 || chars > MAX_TITLE_CHARS || title.chars().any(char::is_control) {
        return Err(StoreError::Unstorable(
            "a title has 1 to 200 characters and no control characters",
        ));
    }
    let updated = sqlx::query(SET_TITLE)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .bind(title)
        .bind(source.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    if updated.is_some() {
        return Ok(true);
    }
    match metadata(tx, principal, id).await? {
        Some(_) => Ok(false),
        None => Err(StoreError::NotStored),
    }
}

/// Archives `principal`'s conversation `id` (`archived`), or takes it out of the archive.
/// Archiving an archived conversation keeps when it was first archived.
///
/// # Errors
///
/// [`StoreError::NotStored`], [`StoreError::WrongTenant`] or a database failure.
pub async fn archive(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    archived: bool,
) -> Result<(), StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    sqlx::query(ARCHIVE)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .bind(archived)
        .fetch_optional(&mut **tx)
        .await?
        .map(|_| ())
        .ok_or(StoreError::NotStored)
}

/// Deletes `principal`'s conversation `id`: it is gone for every other function until the owner
/// restores it within [`DELETED_RETENTION_DAYS`] days, after which the purge removes it with its
/// messages, discarded rounds and runs. Afterwards nothing more is appended.
///
/// A conversation with a question running is not deleted ([`StoreError::AlreadyRunning`], and
/// nothing is changed): stopping it and deleting during a question is Laysi/trustiot-v2#646. A
/// question still `running` past its deadline (the service stopped mid-question) does not
/// count, also when the deadline passed while the delete waited for the row lock. The delete
/// leaves such a run as it is: the next [`start_run`](super::start_run) after a restore marks
/// it [`RunStatus::Interrupted`](super::RunStatus::Interrupted), and the purge removes it with
/// the conversation.
///
/// It locks the conversation's row first, so it waits for an [`append`](super::append) or a
/// [`start_run`](super::start_run) that holds it, and only then looks for a running question,
/// in a statement of its own. Under READ COMMITTED a statement sees the rows committed before
/// it started: a single `UPDATE ... WHERE NOT EXISTS (running question)` that waited for
/// `start_run`'s lock would re-check only the conversation's row after the wait, and miss the
/// question `start_run` just committed. Of a delete and a question start that meet, exactly one
/// happens: the start commits its run first and the delete refuses, or the delete commits first
/// and the start finds no conversation ([`StoreError::NotStored`]).
///
/// # Errors
///
/// [`StoreError::NotStored`] (also when it is already deleted), [`StoreError::AlreadyRunning`],
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn delete(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<DeletedConversation, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let tenant_text = principal.tenant_id.to_string();
    let id_text = id.to_string();
    if !lock_owned(tx, &tenant_text, &id_text, principal.user_id).await? {
        return Err(StoreError::NotStored);
    }
    if running_before_deadline(tx, &tenant_text, &id_text).await? {
        return Err(StoreError::AlreadyRunning);
    }
    sqlx::query(DELETE)
        .bind(&tenant_text)
        .bind(&id_text)
        .bind(principal.user_id.to_string())
        .bind(DELETED_RETENTION_DAYS)
        .fetch_optional(&mut **tx)
        .await?
        .as_ref()
        .map(deleted)
        .transpose()?
        .ok_or(StoreError::NotStored)
}

/// `principal`'s deleted conversations that can still be restored, most recently deleted first:
/// titles, deletion times and restore deadlines, no messages.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn list_deleted(
    tx: &mut TenantTransaction,
    principal: Principal,
    after: Option<DeletedCursor>,
    limit: usize,
) -> Result<Page<DeletedConversation, DeletedCursor>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let size = page_size(limit);
    let rows = sqlx::query(LIST_DELETED)
        .bind(principal.tenant_id.to_string())
        .bind(principal.user_id.to_string())
        .bind(DELETED_RETENTION_DAYS)
        .bind(after.map(|cursor| cursor.deleted_at))
        .bind(after.map(|cursor| cursor.id.to_string()))
        .bind(fetch_size(size))
        .fetch_all(&mut **tx)
        .await?;
    let mut items = rows.iter().map(deleted).collect::<Result<Vec<_>, _>>()?;
    let next = if items.len() > size {
        items.truncate(size);
        items.last().map(|last| DeletedCursor {
            deleted_at: last.deleted_at,
            id: last.id,
        })
    } else {
        None
    };
    Ok(Page { items, next })
}

/// Restores `principal`'s deleted conversation `id`, as it was when it was deleted. Only within
/// [`DELETED_RETENTION_DAYS`] days of the deletion; the database refuses the service role any
/// change to a conversation past them, so a bug cannot bring one back either. Against a
/// concurrent purge exactly one wins: the purge skips a row this restored, and this finds
/// nothing to restore once the purge deleted it.
///
/// # Errors
///
/// [`StoreError::NotStored`] when there is no such deleted conversation of theirs that can
/// still be restored; [`StoreError::WrongTenant`] or a database failure.
pub async fn restore(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<(), StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    sqlx::query(RESTORE)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .bind(DELETED_RETENTION_DAYS)
        .fetch_optional(&mut **tx)
        .await?
        .map(|_| ())
        .ok_or(StoreError::NotStored)
}

/// Removes up to `limit` conversations of any tenant that were deleted more than
/// [`DELETED_RETENTION_DAYS`] days ago, oldest first, with everything that belongs to them
/// (messages, discarded rounds, runs: `ON DELETE CASCADE`). Returns how many it removed; the
/// caller repeats until it gets fewer than `limit`.
///
/// `tx` is a transaction of the assistant's platform role (`assistant_platform`, through
/// service-kit's `PlatformDb`, which logs each one): the role sees and deletes only expired
/// conversations, so this cannot remove a conversation that is live or still restorable,
/// whatever the statement says. The scheduled job that calls it is Laysi/trustiot-v2#645's.
///
/// It sets the transaction's `lock_timeout` to [`PURGE_LOCK_TIMEOUT`] first, so every caller
/// waits at most that long for a row another session holds: then it fails with SQLSTATE
/// [`LOCK_NOT_AVAILABLE`], nothing of the statement is deleted, and the caller rolls `tx` back
/// and tries again later.
///
/// # Errors
///
/// A database failure; [`StoreError::sqlstate`] is [`LOCK_NOT_AVAILABLE`] after the lock wait.
pub async fn purge_expired(tx: &mut PlatformTransaction, limit: u32) -> Result<u64, StoreError> {
    sqlx::query(SET_LOCK_TIMEOUT)
        .bind(format!("{}ms", PURGE_LOCK_TIMEOUT.as_millis()))
        .execute(&mut **tx)
        .await?;
    Ok(sqlx::query(PURGE)
        .bind(DELETED_RETENTION_DAYS)
        .bind(i64::from(limit))
        .execute(&mut **tx)
        .await?
        .rows_affected())
}
