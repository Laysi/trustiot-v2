//! Conversations in Postgres (Laysi/trustiot-v2#553 and Laysi/trustiot-v2#644;
//! docs/architecture/ai-assistant.md, 「對話歷史只往後加」): the preamble a conversation fixed
//! when it started, its history, one row per message, stored as the bytes sent to the model and
//! read back byte for byte, and the conversation thread around it: title, archiving, soft
//! deletion with a 30-day restore window and the purge after it, discarded rounds, and the record
//! of every question asked. Schema: `migrations/postgres/0001_assistant.sql` and
//! `0002_conversation_threads.sql`; services/assistant/README.md, 「對話的儲存（Postgres）」, has
//! the rules and the privilege table.
//!
//! The history is append-only at both ends. The database gives the service role no UPDATE or
//! DELETE on messages, so no code path of the service can change or remove a stored message;
//! a conversation leaves the database only as a whole, purged 30 days after its owner deleted it
//! ([`purge_expired`], the platform role). Every function but the purge takes a
//! [`TenantTransaction`], so row-level security limits it to the transaction's tenant (decision
//! 29), and the caller decides when the writes commit.
//!
//! Who sees what: a conversation belongs to the user who started it. Every function here takes
//! the acting [`Principal`] and treats a conversation of another user exactly like one that does
//! not exist ([`StoreError::NotStored`], `None`, or an empty list), so a handler cannot leak
//! whether it exists. A deleted conversation also counts as not existing, except in
//! [`list_deleted`] and [`restore`].
//!
//! How the chat API (Laysi/trustiot-v2#645, #646) uses it: [`create`] when a conversation starts,
//! [`append`] after each answered question with the [`Stored`] that [`create`], [`load`] or the
//! previous [`append`] returned, and [`load`] to continue a conversation. Appending is
//! idempotent: messages that are already stored with the same bytes are skipped, so retrying
//! after a commit whose outcome was lost is safe; a stored message that differs is a
//! [`StoreError::Conflict`], never overwritten. A question is recorded with [`start_run`] and
//! [`finish_run`]; [`discard_last_round`] takes the last round back out of the history before
//! the answer is regenerated.

use std::fmt;
use std::sync::Arc;

use contract::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use uuid::Uuid;

use crate::history::{Conversation, EntryKind, Preamble, Principal, SystemPrompt};
use crate::model::{CacheTtl, Effort, Format, NativeMessage};
use crate::tools::ToolSpec;

mod runs;
mod threads;

pub use runs::{
    RunOutcome, RunRecord, RunStatus, finish_run, request_cancel, run_status, start_run,
};
pub use threads::{
    ConversationSummary, DeletedConversation, DeletedCursor, LOCK_NOT_AVAILABLE, ListCursor,
    ListFilter, MAX_PAGE, PURGE_LOCK_TIMEOUT, Page, Title, TitleSource, archive, delete, list,
    list_deleted, metadata, purge_expired, restore, set_title,
};

/// How long a deleted conversation can be restored; after it the purge removes it for good. A
/// fixed product rule (Laysi/trustiot-v2#146, 2026-10-24). The database enforces the same 30
/// days in `0002_conversation_threads.sql` (the purge role's policies and the service role's
/// restrictive policy); a unit test compares the two.
pub const DELETED_RETENTION_DAYS: i32 = 30;

/// The largest message, in bytes, the store writes: the AI gateway refuses a request over 32 MiB
/// (`max_request_bytes`, Anthropic Messages' limit), and every message is part of the later
/// requests of its conversation. The database refuses a larger `body` too
/// (`messages_body_size`); the derivation is in `0002_conversation_threads.sql`.
pub const MAX_MESSAGE_BYTES: usize = 32 << 20;

/// The longest title, in characters (the database's limit too).
pub const MAX_TITLE_CHARS: usize = 200;

/// `$1` tenant, `$2` conversation, `$3` user, then the preamble. Stores nothing when the
/// conversation was stored before.
const INSERT_CONVERSATION: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl)
VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, $7, $8, $9)
ON CONFLICT (tenant_id, conversation_id) DO NOTHING";

/// `$1` tenant, `$2` conversation: the owner and preamble of a conversation that is not deleted.
const SELECT_CONVERSATION: &str = "SELECT user_id::text AS user_id, prompt_version,
    system_prompt, tenant_context, tools, effort, cache_ttl, deleted_at IS NOT NULL AS deleted
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid";

/// [`SELECT_CONVERSATION`], locking the row against a concurrent delete, discard, run start or
/// append until the transaction ends. A deleted conversation's row is returned too (`deleted`),
/// so `append` can tell another owner's from a deleted one; a conversation past its 30 days is
/// not (the service role's restrictive UPDATE policy also applies to `FOR NO KEY UPDATE`).
const LOCK_CONVERSATION: &str = "SELECT user_id::text AS user_id, prompt_version,
    system_prompt, tenant_context, tools, effort, cache_ttl, deleted_at IS NOT NULL AS deleted
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid
FOR NO KEY UPDATE";

/// `$1` tenant, `$2` conversation, `$3` user: locks the user's conversation, unless it is
/// deleted.
const LOCK_OWNED: &str = "SELECT 1 AS found
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL
FOR NO KEY UPDATE";

/// `$1` tenant, `$2` conversation: every stored message in order, whether a discarded round
/// holds it (then without its body, which is not read), and how many rounds were discarded. One
/// statement, so one snapshot.
const SELECT_HISTORY: &str = "SELECT m.seq, m.kind, m.format,
    CASE WHEN d.from_seq IS NULL THEN m.body END AS body,
    d.from_seq IS NOT NULL AS discarded,
    (SELECT count(*) FROM assistant.discarded_rounds r
     WHERE r.tenant_id = $1::uuid AND r.conversation_id = $2::uuid) AS discards
FROM assistant.messages m
LEFT JOIN LATERAL (
    SELECT r.from_seq FROM assistant.discarded_rounds r
    WHERE r.tenant_id = m.tenant_id AND r.conversation_id = m.conversation_id
        AND m.seq BETWEEN r.from_seq AND r.to_seq
    LIMIT 1
) d ON true
WHERE m.tenant_id = $1::uuid AND m.conversation_id = $2::uuid
ORDER BY m.seq";

/// `$1` tenant, `$2` conversation: the position after the last stored message, and how many
/// rounds were discarded.
const HISTORY_END: &str = "SELECT
    (SELECT coalesce(max(seq)::bigint + 1, 0) FROM assistant.messages
     WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid) AS next,
    (SELECT count(*) FROM assistant.discarded_rounds
     WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid) AS discards";

/// `$1` tenant, `$2` conversation, `$3` position, `$4` kind, `$5` format, `$6` the bytes.
/// Returns the position when this statement stored the message, nothing when one was there.
const INSERT_MESSAGE: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
ON CONFLICT (tenant_id, conversation_id, seq) DO NOTHING
RETURNING seq";

/// `$1` tenant, `$2` conversation, `$3` position.
const SELECT_MESSAGE: &str = "SELECT kind, format, body
FROM assistant.messages
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND seq = $3";

/// `$1` tenant, `$2` conversation: the conversation was active now. Never moves backwards (a
/// transaction that started earlier than the last writer's keeps the later time).
const TOUCH: &str = "UPDATE assistant.conversations
SET last_activity_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND last_activity_at < now()";

/// `$1` tenant, `$2` conversation, `$3` first position, `$4` last position.
const INSERT_DISCARD: &str = "INSERT INTO assistant.discarded_rounds
    (tenant_id, conversation_id, from_seq, to_seq)
VALUES ($1::uuid, $2::uuid, $3, $4)";

/// Why a store operation failed. Carries no message content, and no database error text: see
/// [`DatabaseFailure`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The transaction is for another tenant than the conversation's.
    #[error("the transaction's tenant is not the conversation's")]
    WrongTenant,
    /// No conversation of this user with that ID: it was never stored, it is another user's, or
    /// it is deleted (only [`list_deleted`] and [`restore`] see deleted ones). [`restore`]: no
    /// deleted conversation of this user that can still be restored.
    #[error("the conversation is not stored")]
    NotStored,
    /// [`append`] from a position after the end of the stored history, which would leave a gap.
    #[error("appending from message {from}, but only {stored} messages are stored")]
    Gap { from: usize, stored: usize },
    /// What is stored differs from what the caller read: another writer appended or discarded
    /// a round first, or the conversation in memory has another owner or preamble than the
    /// stored one. Nothing is overwritten; the caller loads the stored conversation again.
    #[error("the stored conversation differs at {0}")]
    Conflict(&'static str),
    /// The value cannot be stored as it is (a message over [`MAX_MESSAGE_BYTES`], a title that
    /// is empty, too long or has control characters, text with U+0000, a deadline out of range).
    #[error("the value cannot be stored: {0}")]
    Unstorable(&'static str),
    /// A stored row does not read back as a conversation.
    #[error("a stored conversation is corrupt: {0}")]
    Corrupt(&'static str),
    /// [`discard_last_round`] from a position that is not the start of the history's last
    /// round: not a question, a later question follows it, or past the end.
    #[error("only the last round of the history can be discarded")]
    NotLastRound,
    /// A question of the conversation is running and its deadline has not passed: [`start_run`]
    /// does not start another, and [`delete`] does not delete the conversation.
    #[error("a question of the conversation is running")]
    AlreadyRunning,
    /// No run of this user with that ID (or its conversation is deleted, for
    /// [`request_cancel`] and [`run_status`]).
    #[error("the run is not stored")]
    NoSuchRun,
    /// [`finish_run`] or [`request_cancel`] for a run that has already ended (for example
    /// marked `interrupted` after its deadline).
    #[error("the run has already ended")]
    RunEnded,
    /// The database failed.
    #[error("{0}")]
    Database(DatabaseFailure),
}

/// What kind of database failure happened, without the text of the error. Postgres puts column
/// values into its messages and DETAIL (`Key (tenant_id, conversation_id)=(...) already exists`),
/// and those are tenant data that must not reach a log or a response, so only the class
/// survives: the SQLSTATE for an error the server returned, otherwise what failed.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseFailure {
    kind: FailureKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FailureKind {
    /// The server returned an error with this SQLSTATE (five ASCII characters).
    Server(String),
    /// The server returned an error without a valid SQLSTATE.
    ServerUnknown,
    /// The connection (I/O or TLS) failed.
    Connection,
    /// No pooled connection became available, or the pool is closed.
    Pool,
    /// A row did not have the expected shape, or a value did not decode or encode.
    Decode,
    /// The connection is in an unexpected state, or something else failed.
    Other,
}

impl DatabaseFailure {
    /// The SQLSTATE of an error the server returned, such as `23505` or `42501`.
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        match &self.kind {
            FailureKind::Server(code) => Some(code),
            _ => None,
        }
    }

    fn of(error: &sqlx::Error) -> Self {
        let kind = match error {
            sqlx::Error::Database(database) => match database.code() {
                Some(code)
                    if code.len() == 5 && code.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    FailureKind::Server(code.into_owned())
                }
                _ => FailureKind::ServerUnknown,
            },
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) => FailureKind::Connection,
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => FailureKind::Pool,
            sqlx::Error::RowNotFound
            | sqlx::Error::TypeNotFound { .. }
            | sqlx::Error::ColumnIndexOutOfBounds { .. }
            | sqlx::Error::ColumnNotFound(_)
            | sqlx::Error::ColumnDecode { .. }
            | sqlx::Error::Encode(_)
            | sqlx::Error::Decode(_) => FailureKind::Decode,
            _ => FailureKind::Other,
        };
        Self { kind }
    }
}

impl fmt::Display for DatabaseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FailureKind::Server(code) => write!(f, "database error (SQLSTATE {code})"),
            FailureKind::ServerUnknown => f.write_str("database error (no SQLSTATE)"),
            FailureKind::Connection => f.write_str("database error: the connection failed"),
            FailureKind::Pool => f.write_str("database error: no connection available"),
            FailureKind::Decode => f.write_str("database error: a row did not decode"),
            FailureKind::Other => f.write_str("database error"),
        }
    }
}

impl fmt::Debug for DatabaseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DatabaseFailure").field(&self.kind).finish()
    }
}

impl From<sqlx::Error> for StoreError {
    /// Keeps only the class of the failure (see [`DatabaseFailure`]); the error itself is
    /// dropped here.
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DatabaseFailure::of(&error))
    }
}

impl StoreError {
    /// The SQLSTATE when the database refused the statement.
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Self::Database(failure) => failure.sqlstate(),
            _ => None,
        }
    }
}

/// How much of a conversation is stored: what [`create`], [`append`] and [`load`] return, for
/// the next [`append`]. It ties the conversation in memory to its rows: how many of its messages
/// are stored, the position the next one gets in the database (positions keep counting past
/// discarded rounds, so they are no longer the messages' places in the history), and how many
/// rounds were discarded when it was read, so an append after another writer discarded a round
/// is refused instead of continuing a history that no longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
    conversation: Uuid,
    messages: usize,
    next_seq: usize,
    discards: i64,
}

impl Stored {
    /// Before anything of `conversation` is stored.
    fn start(conversation: Uuid) -> Self {
        Self {
            conversation,
            messages: 0,
            next_seq: 0,
            discards: 0,
        }
    }

    /// How many messages of the conversation in memory are stored.
    #[must_use]
    pub fn messages(&self) -> usize {
        self.messages
    }
}

/// A stored conversation as [`load`] read it, and where its stored part ends.
#[derive(Debug)]
pub struct Loaded {
    pub conversation: Conversation,
    pub stored: Stored,
}

/// A tool definition as stored, in the order of [`ToolSpec`]'s fields.
#[derive(Serialize)]
struct StoredTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadedTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl From<LoadedTool> for ToolSpec {
    fn from(tool: LoadedTool) -> Self {
        Self {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
        }
    }
}

/// The tool definitions as stored: a JSON array. The request sends them from the parsed
/// `serde_json::Value`, so they must parse back to the same value, or a resumed conversation
/// would send different tool definitions than it started with (serde_json reads a few long
/// floating-point literals one bit off unless its `float_roundtrip` feature is on).
fn tools_json(tools: &[ToolSpec]) -> Result<String, StoreError> {
    let json = serde_json::to_string(&stored_tools(tools))
        .map_err(|_| StoreError::Unstorable("the tool definitions are not JSON"))?;
    let again = parse_tools(&json)
        .ok()
        .and_then(|parsed| serde_json::to_string(&stored_tools(&parsed)).ok());
    if again.as_deref() != Some(json.as_str()) {
        return Err(StoreError::Unstorable(
            "the tool definitions do not read back as the same JSON",
        ));
    }
    Ok(json)
}

fn parse_tools(json: &str) -> Result<Vec<ToolSpec>, serde_json::Error> {
    serde_json::from_str::<Vec<LoadedTool>>(json)
        .map(|tools| tools.into_iter().map(ToolSpec::from).collect())
}

fn stored_tools(tools: &[ToolSpec]) -> Vec<StoredTool<'_>> {
    tools
        .iter()
        .map(|tool| StoredTool {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.input_schema,
        })
        .collect()
}

/// Postgres `text` cannot hold U+0000; refuse it up front with a clear error rather than a
/// database one.
fn text<'a>(value: &'a str, what: &'static str) -> Result<&'a str, StoreError> {
    if value.contains('\0') {
        return Err(StoreError::Unstorable(what));
    }
    Ok(value)
}

fn check_tenant(tx: &TenantTransaction, tenant: TenantId) -> Result<(), StoreError> {
    if tx.tenant_id() == tenant {
        Ok(())
    } else {
        Err(StoreError::WrongTenant)
    }
}

/// A position as Postgres stores it.
fn seq(position: usize) -> Result<i32, StoreError> {
    i32::try_from(position).map_err(|_| StoreError::Unstorable("the history has too many messages"))
}

/// Stores a new conversation, its preamble and every message it has so far, in `tx`, which
/// must be a transaction of the conversation's tenant. Returns where the stored part ends, for
/// [`append`].
///
/// Storing a conversation that is already stored with the same owner, preamble and messages is
/// not an error (a retry); with a different one it is a [`StoreError::Conflict`].
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Unstorable`], [`StoreError::Conflict`],
/// [`StoreError::NotStored`] (a retry after the user deleted it), [`StoreError::Corrupt`] or a
/// database failure; the caller rolls `tx` back then.
pub async fn create(
    tx: &mut TenantTransaction,
    conversation: &Conversation,
) -> Result<Stored, StoreError> {
    let principal = conversation.principal();
    check_tenant(tx, principal.tenant_id)?;
    let preamble = conversation.preamble();
    let tools = tools_json(preamble.tools())?;
    sqlx::query(INSERT_CONVERSATION)
        .bind(principal.tenant_id.to_string())
        .bind(conversation.id().to_string())
        .bind(principal.user_id.to_string())
        .bind(text(
            &preamble.system_prompt().version,
            "the system prompt version contains U+0000",
        )?)
        .bind(text(
            &preamble.system_prompt().text,
            "the system prompt contains U+0000",
        )?)
        .bind(text(
            preamble.tenant_context(),
            "the tenant context contains U+0000",
        )?)
        .bind(&tools)
        .bind(preamble.effort().as_str())
        .bind(preamble.cache_ttl().as_str())
        .execute(&mut **tx)
        .await?;
    // Stored now or before: `append` compares the stored owner and preamble with this
    // conversation's, so only a retry of the same conversation passes.
    append(tx, conversation, Stored::start(conversation.id())).await
}

/// Stores the messages of `conversation` after the `stored` ones, in `tx`, which must be a
/// transaction of the conversation's tenant. `stored` is what [`create`], [`load`] or the
/// previous [`append`] returned for this conversation; returns the new end.
///
/// It locks the conversation's row first, so a concurrent [`delete`] or
/// [`discard_last_round`] either waits for this append to commit or makes it fail: a deleted
/// conversation gets no more messages ([`StoreError::NotStored`]), and a round discarded since
/// `stored` was read refuses the append ([`StoreError::Conflict`]).
///
/// The stored conversation must have the same owner and preamble as `conversation`: its tools
/// run as the stored owner, so a conversation built for another user (or with another
/// preamble) under a stored ID never adds to that history. A message already stored at its
/// position with the same kind and bytes is skipped, so a retry is harmless. One that differs
/// means another writer appended first: the call fails with [`StoreError::Conflict`] and the
/// caller rolls `tx` back.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::NotStored`], [`StoreError::Gap`],
/// [`StoreError::Conflict`], [`StoreError::Unstorable`] or a database failure.
pub async fn append(
    tx: &mut TenantTransaction,
    conversation: &Conversation,
    stored: Stored,
) -> Result<Stored, StoreError> {
    let principal = conversation.principal();
    let tenant = principal.tenant_id;
    check_tenant(tx, tenant)?;
    if stored.conversation != conversation.id() {
        return Err(StoreError::Conflict("the position of another conversation"));
    }
    if stored.messages > conversation.len() {
        return Err(StoreError::Gap {
            from: stored.messages,
            stored: conversation.len(),
        });
    }
    let pending: Vec<(EntryKind, &NativeMessage)> =
        conversation.entries().skip(stored.messages).collect();
    if pending
        .iter()
        .any(|(_, message)| message.json().len() > MAX_MESSAGE_BYTES)
    {
        return Err(StoreError::Unstorable("a message is over 32 MiB"));
    }
    let tenant_text = tenant.to_string();
    let id_text = conversation.id().to_string();
    let row = sqlx::query(LOCK_CONVERSATION)
        .bind(&tenant_text)
        .bind(&id_text)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotStored)?;
    let owner = stored_conversation(&row)?;
    if owner.user_id != principal.user_id || *owner.preamble != **conversation.preamble() {
        return Err(StoreError::Conflict("the conversation's owner or preamble"));
    }
    if row.try_get::<bool, _>("deleted")? {
        return Err(StoreError::NotStored);
    }
    let (next, discards) = history_end(tx, &tenant_text, &id_text).await?;
    if discards != stored.discards {
        return Err(StoreError::Conflict("a round discarded since it was read"));
    }
    if stored.next_seq > next {
        return Err(StoreError::Gap {
            from: stored.next_seq,
            stored: next,
        });
    }
    if next > stored.next_seq + pending.len() {
        // Another writer stored more than this conversation has.
        return Err(StoreError::Conflict("the end of the history"));
    }
    let mut inserted_any = false;
    for (offset, (kind, message)) in pending.iter().enumerate() {
        let position = seq(stored.next_seq + offset)?;
        let inserted = sqlx::query(INSERT_MESSAGE)
            .bind(&tenant_text)
            .bind(&id_text)
            .bind(position)
            .bind(kind.as_str())
            .bind(message.format().as_str())
            .bind(message.json().as_bytes())
            .fetch_optional(&mut **tx)
            .await?;
        if inserted.is_some() {
            inserted_any = true;
            continue;
        }
        let row = sqlx::query(SELECT_MESSAGE)
            .bind(&tenant_text)
            .bind(&id_text)
            .bind(position)
            .fetch_one(&mut **tx)
            .await?;
        let same = row.try_get::<String, _>("kind")? == kind.as_str()
            && row.try_get::<String, _>("format")? == message.format().as_str()
            && row.try_get::<Vec<u8>, _>("body")? == message.json().as_bytes();
        if !same {
            return Err(StoreError::Conflict("a message of the history"));
        }
    }
    if inserted_any {
        sqlx::query(TOUCH)
            .bind(&tenant_text)
            .bind(&id_text)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Stored {
        conversation: conversation.id(),
        messages: conversation.len(),
        next_seq: stored.next_seq + pending.len(),
        discards,
    })
}

/// The position after the last stored message, and the number of discarded rounds.
async fn history_end(
    tx: &mut TenantTransaction,
    tenant: &str,
    id: &str,
) -> Result<(usize, i64), StoreError> {
    let row = sqlx::query(HISTORY_END)
        .bind(tenant)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    let next = usize::try_from(row.try_get::<i64, _>("next")?)
        .map_err(|_| StoreError::Corrupt("a negative position"))?;
    Ok((next, row.try_get("discards")?))
}

/// A stored conversation's owner and preamble.
struct StoredConversation {
    user_id: UserId,
    preamble: Arc<Preamble>,
}

fn stored_conversation(row: &sqlx::postgres::PgRow) -> Result<StoredConversation, StoreError> {
    let user_id: UserId = row
        .try_get::<String, _>("user_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt("a user ID"))?;
    let tools = parse_tools(&row.try_get::<String, _>("tools")?)
        .map_err(|_| StoreError::Corrupt("the tool definitions"))?;
    let effort = Effort::from_name(&row.try_get::<String, _>("effort")?)
        .ok_or(StoreError::Corrupt("the effort"))?;
    let cache_ttl = CacheTtl::from_name(&row.try_get::<String, _>("cache_ttl")?)
        .ok_or(StoreError::Corrupt("the cache lifetime"))?;
    let preamble = Preamble::new(
        SystemPrompt {
            version: row.try_get("prompt_version")?,
            text: row.try_get("system_prompt")?,
        },
        row.try_get("tenant_context")?,
        tools,
        effort,
        cache_ttl,
    );
    Ok(StoredConversation {
        user_id,
        preamble: Arc::new(preamble),
    })
}

/// The conversation `id` of `principal`, as it was stored, in `tx`, which must be a
/// transaction of the principal's tenant. `None` when the tenant has no such conversation, it
/// belongs to another user of the tenant, or it is deleted: the conversation's tools run as its
/// owner, so only the owner continues it (sharing, Laysi/trustiot-v2#650, adds read access for
/// others without this).
///
/// The history leaves out the messages of discarded rounds ([`discard_last_round`]), so it is,
/// byte for byte, the history as it was before the discarded round. Every message reads back as
/// the bytes that were stored; a row that does not (an unknown kind or format, bytes that are
/// not one JSON value, a gap in the positions) fails the load instead of sending the model an
/// altered history.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn load(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<Option<Loaded>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let tenant_text = principal.tenant_id.to_string();
    let id_text = id.to_string();
    let Some(row) = sqlx::query(SELECT_CONVERSATION)
        .bind(&tenant_text)
        .bind(&id_text)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let stored = stored_conversation(&row)?;
    if stored.user_id != principal.user_id || row.try_get::<bool, _>("deleted")? {
        return Ok(None);
    }
    let rows = sqlx::query(SELECT_HISTORY)
        .bind(&tenant_text)
        .bind(&id_text)
        .fetch_all(&mut **tx)
        .await?;
    let mut entries = Vec::with_capacity(rows.len());
    let mut discards = 0;
    for (index, row) in rows.iter().enumerate() {
        let seq: i32 = row.try_get("seq")?;
        if usize::try_from(seq).ok() != Some(index) {
            return Err(StoreError::Corrupt("a gap in the history's positions"));
        }
        discards = row.try_get("discards")?;
        if row.try_get::<bool, _>("discarded")? {
            continue;
        }
        let kind = EntryKind::from_name(&row.try_get::<String, _>("kind")?)
            .ok_or(StoreError::Corrupt("a message's kind"))?;
        let format = Format::from_name(&row.try_get::<String, _>("format")?)
            .ok_or(StoreError::Corrupt("a message's format"))?;
        let body: Vec<u8> = row.try_get("body")?;
        entries.push((kind, NativeMessage::new(format, raw_message(body)?)));
    }
    let stored_part = Stored {
        conversation: id,
        messages: entries.len(),
        next_seq: rows.len(),
        discards,
    };
    Ok(Some(Loaded {
        conversation: Conversation::restore(id, principal, stored.preamble, entries),
        stored: stored_part,
    }))
}

/// Takes the last round back out of the history of `principal`'s conversation `id`: from
/// `from`, the place in the history (as [`load`] returned it) of the round's question, to the
/// end. `stored` is what [`load`] or [`append`] returned; the history must not have changed
/// since. The messages stay in the database until the conversation is purged; [`load`] leaves
/// them out, and the next round is appended after them. Load the conversation again afterwards:
/// `stored` and the conversation in memory still include the discarded round, and [`append`]
/// refuses them.
///
/// Locks the conversation's row, so it does not interleave with an [`append`] or another
/// discard.
///
/// # Errors
///
/// [`StoreError::NotLastRound`] when `from` is not the question of the history's last round
/// (not a question, a later question follows, or past the end); [`StoreError::NotStored`],
/// [`StoreError::Conflict`] (the history changed since `stored` was read),
/// [`StoreError::WrongTenant`] or a database failure.
pub async fn discard_last_round(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    from: usize,
    stored: Stored,
) -> Result<(), StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    if stored.conversation != id {
        return Err(StoreError::Conflict("the position of another conversation"));
    }
    let tenant_text = principal.tenant_id.to_string();
    let id_text = id.to_string();
    if !lock_owned(tx, &tenant_text, &id_text, principal.user_id).await? {
        return Err(StoreError::NotStored);
    }
    let rows = sqlx::query(SELECT_HISTORY)
        .bind(&tenant_text)
        .bind(&id_text)
        .fetch_all(&mut **tx)
        .await?;
    let discards = match rows.first() {
        Some(row) => row.try_get::<i64, _>("discards")?,
        None => 0,
    };
    if rows.len() != stored.next_seq || discards != stored.discards {
        return Err(StoreError::Conflict(
            "the history changed since it was read",
        ));
    }
    let mut visible = Vec::with_capacity(rows.len());
    for row in &rows {
        if !row.try_get::<bool, _>("discarded")? {
            let kind = EntryKind::from_name(&row.try_get::<String, _>("kind")?)
                .ok_or(StoreError::Corrupt("a message's kind"))?;
            visible.push((row.try_get::<i32, _>("seq")?, kind));
        }
    }
    let Some(&(from_seq, EntryKind::Question)) = visible.get(from) else {
        return Err(StoreError::NotLastRound);
    };
    if visible[from + 1..]
        .iter()
        .any(|(_, kind)| *kind == EntryKind::Question)
    {
        return Err(StoreError::NotLastRound);
    }
    let to_seq = seq(rows.len() - 1)?;
    sqlx::query(INSERT_DISCARD)
        .bind(&tenant_text)
        .bind(&id_text)
        .bind(from_seq)
        .bind(to_seq)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Locks `user`'s conversation `id` unless it is deleted; `false` when there is none.
async fn lock_owned(
    tx: &mut TenantTransaction,
    tenant: &str,
    id: &str,
    user: UserId,
) -> Result<bool, StoreError> {
    Ok(sqlx::query(LOCK_OWNED)
        .bind(tenant)
        .bind(id)
        .bind(user.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .is_some())
}

/// The stored bytes as a message, unchanged: they must be UTF-8 and exactly one JSON value.
/// Parsing as a `RawValue` keeps the text as it is and drops only surrounding whitespace, so an
/// equal length proves the message is the stored bytes.
fn raw_message(body: Vec<u8>) -> Result<Box<RawValue>, StoreError> {
    let length = body.len();
    let text = String::from_utf8(body).map_err(|_| StoreError::Corrupt("a message's bytes"))?;
    let raw = RawValue::from_string(text).map_err(|_| StoreError::Corrupt("a message's JSON"))?;
    if raw.get().len() != length {
        return Err(StoreError::Corrupt("a message's JSON"));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(schema: Value) -> ToolSpec {
        ToolSpec {
            name: "count_readings".into(),
            description: "Counts readings.".into(),
            input_schema: schema,
        }
    }

    #[test]
    fn tool_definitions_read_back_as_the_same_json() {
        let tools = vec![tool(json!({
            "type": "object",
            "properties": {"hours": {"type": "number", "minimum": 0.1, "maximum": 1e300}},
            "required": ["hours"]
        }))];
        let json = tools_json(&tools).expect("storable");
        let parsed = parse_tools(&json).expect("parses");
        assert_eq!(parsed, tools);
        assert_eq!(
            serde_json::to_string(&stored_tools(&parsed)).expect("JSON"),
            json
        );
    }

    #[test]
    fn a_message_reads_back_only_as_its_exact_bytes() {
        let message = br#"{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"c2ln"}]}"#;
        let raw = raw_message(message.to_vec()).expect("a message");
        assert_eq!(raw.get().as_bytes(), message);
        for corrupt in [
            b" {\"role\":\"user\"}".to_vec(),
            b"{\"role\":\"user\"} ".to_vec(),
            b"{\"role\":".to_vec(),
            b"{}{}".to_vec(),
            vec![b'{', 0xff, b'}'],
        ] {
            assert!(
                matches!(raw_message(corrupt), Err(StoreError::Corrupt(_))),
                "refused"
            );
        }
    }

    /// The limits the migration writes as SQL are the ones this module enforces and documents.
    #[test]
    fn the_migration_enforces_the_stores_limits() {
        let migration = include_str!("../migrations/postgres/0002_conversation_threads.sql");
        let retention = format!("interval '{DELETED_RETENTION_DAYS} days'");
        // The three platform policies and the service role's restrictive policy.
        assert_eq!(
            migration.matches(&format!("now() - {retention}")).count(),
            4,
            "every 30-day bound of the migration is {retention}"
        );
        assert!(
            migration.contains(&format!(
                "CHECK (octet_length(body) <= {MAX_MESSAGE_BYTES})"
            )),
            "the body limit is MAX_MESSAGE_BYTES"
        );
        assert!(
            migration.contains(&format!(
                "char_length(title) BETWEEN 1 AND {MAX_TITLE_CHARS}"
            )),
            "the title limit is MAX_TITLE_CHARS"
        );
        // One tool result at the default limits fits in a message, with room for the JSON
        // around it (config.rs's defaults; the derivation is in the migration).
        let tools = crate::config::Tools::default();
        let result = tools.max_text_bytes + tools.max_images * tools.max_image_bytes;
        assert!(
            result + (8 << 20) < MAX_MESSAGE_BYTES,
            "a default tool result ({result} bytes) leaves room in MAX_MESSAGE_BYTES"
        );
    }

    #[test]
    fn a_database_failure_keeps_only_its_class() {
        let failure = DatabaseFailure::of(&sqlx::Error::Protocol(
            "a value of tenant 0192 in the message".into(),
        ));
        assert_eq!(format!("{failure}"), "database error");
        assert_eq!(format!("{failure:?}"), "DatabaseFailure(Other)");
        let failure = DatabaseFailure::of(&sqlx::Error::ColumnNotFound("secret_column".into()));
        assert!(!format!("{failure} {failure:?}").contains("secret_column"));
        assert_eq!(failure.sqlstate(), None);
    }

    /// A conversation under a stored ID but with another owner or preamble can only be built
    /// inside the crate (`Conversation::restore`), so this regression test lives here: `append`
    /// itself checks the stored owner and preamble and does not rely on how the caller built
    /// the conversation. Starts a throwaway Postgres (podman or docker); skipped without either
    /// unless `CI` is set.
    #[tokio::test(flavor = "multi_thread")]
    async fn append_refuses_a_conversation_of_another_owner_or_preamble() {
        use contract::ServiceName;
        use contract::identity::{TenantGrant, TenantRole};
        use service_kit::identity::UserIdentity;
        use service_kit::tenant::TenantDb;
        use sqlx::postgres::PgPoolOptions;
        use testkit::identity::TestIssuer;
        use testkit::product::{DATABASE, product_postgres};

        use crate::model::ModelClient;
        use crate::model::anthropic::AnthropicGateway;

        let Some(product) = product_postgres(&["assistant_app"]).await else {
            return;
        };
        let db = TenantDb::connect_lazy(
            product.postgres.options("assistant_app", DATABASE),
            PgPoolOptions::new().max_connections(2),
        )
        .expect("the service role's pool verifies the server");
        let issuer = TestIssuer::new();
        let tenant = TenantId::generate();
        let user = |issuer: &TestIssuer| -> UserIdentity {
            let claims = TestIssuer::claims(
                ServiceName::Assistant,
                Some(TenantGrant {
                    id: tenant,
                    role: TenantRole::Viewer,
                }),
            );
            issuer.user(ServiceName::Assistant, &claims)
        };
        let principal = |user: &UserIdentity| Principal {
            tenant_id: tenant,
            user_id: user.user_id(),
        };
        let model = AnthropicGateway::new("http://127.0.0.1:9", "main", 1 << 20)
            .expect("a valid gateway URL");
        let preamble = |effort: Effort| {
            Arc::new(Preamble::new(
                SystemPrompt {
                    version: "test-1".into(),
                    text: "You answer questions about devices.".into(),
                },
                String::new(),
                vec![tool(json!({"type": "object"}))],
                effort,
                CacheTtl::FiveMinutes,
            ))
        };
        let question = |text: &str| model.user_text(text).expect("a question");

        let owner = user(&issuer);
        let colleague = user(&issuer);
        let mut conversation = Conversation::new(principal(&owner), preamble(Effort::High));
        conversation.append(EntryKind::Question, question("Mine?"));
        let mut tx = db.begin(&owner).await.expect("a tenant transaction");
        let stored = create(&mut tx, &conversation).await.expect("stored");
        assert_eq!(stored.messages(), 1);
        tx.commit().await.expect("commit");

        let entries_then = |extra: &str| {
            let mut entries: Vec<_> = conversation
                .entries()
                .map(|(kind, message)| (kind, message.clone()))
                .collect();
            entries.push((EntryKind::Question, question(extra)));
            entries
        };
        // The same ID and history with another user of the tenant as owner, or with another
        // preamble, appended from the start or the stored end: refused before anything is
        // written.
        let forgeries = [
            (
                "another owner",
                Conversation::restore(
                    conversation.id(),
                    principal(&colleague),
                    preamble(Effort::High),
                    entries_then("Injected by a colleague"),
                ),
                &colleague,
            ),
            (
                "another preamble",
                Conversation::restore(
                    conversation.id(),
                    principal(&owner),
                    preamble(Effort::Low),
                    entries_then("Under another preamble"),
                ),
                &owner,
            ),
        ];
        for (what, forged, acting) in &forgeries {
            for from in [Stored::start(conversation.id()), stored] {
                let mut tx = db.begin(acting).await.expect("a tenant transaction");
                assert!(
                    matches!(
                        append(&mut tx, forged, from).await,
                        Err(StoreError::Conflict(_))
                    ),
                    "append of {what} from {}",
                    from.messages()
                );
                tx.rollback().await.expect("roll back");
            }
            let mut tx = db.begin(acting).await.expect("a tenant transaction");
            assert!(
                matches!(create(&mut tx, forged).await, Err(StoreError::Conflict(_))),
                "create of {what}"
            );
            tx.rollback().await.expect("roll back");
        }

        // A conversation that is not stored is refused even with nothing to write.
        let unknown = Conversation::new(principal(&owner), preamble(Effort::High));
        let mut tx = db.begin(&owner).await.expect("a tenant transaction");
        assert!(matches!(
            append(&mut tx, &unknown, Stored::start(unknown.id())).await,
            Err(StoreError::NotStored)
        ));
        tx.rollback().await.expect("roll back");

        // The owner's history is as it was.
        let mut tx = db.begin(&owner).await.expect("a tenant transaction");
        let loaded = load(&mut tx, principal(&owner), conversation.id())
            .await
            .expect("load")
            .expect("stored")
            .conversation;
        tx.commit().await.expect("commit");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.messages()[0].json(),
            conversation.messages()[0].json()
        );
    }

    #[test]
    fn text_with_u0000_is_refused_up_front() {
        assert!(matches!(
            text("a\0b", "x"),
            Err(StoreError::Unstorable("x"))
        ));
        assert_eq!(text("ab", "x").expect("text"), "ab");
    }
}
