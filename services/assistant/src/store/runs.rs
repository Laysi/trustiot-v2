//! The record of every question asked (Laysi/trustiot-v2#644): one row per run, keyed by the
//! run delegation's ID (Laysi/trustiot-v2#551), with how it ended. A conversation has at most
//! one run that is `running` and not past its deadline; a run left `running` past its deadline
//! (the service restarted mid-question) becomes `interrupted` when the next question starts,
//! and every reader takes it as interrupted before that.
//! Failed, cancelled and interrupted runs leave nothing in the history: the question API
//! (Laysi/trustiot-v2#646) appends a round only after its run answered, in one transaction with
//! the run's end ([`finish_answered`]).
//!
//! Lock order (see the parent module): the conversation's row, then the run's row.

use std::time::Duration;

use chrono::{DateTime, Utc};
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use super::{
    LOCK_STARTS, Loaded, MAX_HISTORY_BYTES, StoreError, Stored, TitleSource, append, check_tenant,
    load, lock_owned, raw_message, set_title,
};
use crate::history::{Conversation, EntryKind, Principal};
use crate::model::{Format, NativeMessage};

/// The furthest deadline a run may have (the database's limit too); a run delegation lives at
/// most 15 minutes.
const MAX_RUN_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// `$1` tenant, `$2` conversation: its running question whose deadline passed ends as
/// interrupted.
const INTERRUPT_EXPIRED: &str = "UPDATE assistant.runs
SET status = 'interrupted', ended_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'
    AND deadline_at <= now()";

/// `$1` tenant, `$2` conversation: the partial answers its ended runs left (a replica that
/// stopped without ending its run, `0009_answer_partials.sql`).
const DELETE_ENDED_PARTIALS: &str = "DELETE FROM assistant.answer_partials p
USING assistant.runs r
WHERE p.tenant_id = $1::uuid AND r.tenant_id = p.tenant_id AND r.run_id = p.run_id
    AND r.conversation_id = $2::uuid AND r.status <> 'running'";

/// `$1` tenant, `$2` run: its partial answer, when it ends.
const DELETE_PARTIAL: &str = "DELETE FROM assistant.answer_partials
WHERE tenant_id = $1::uuid AND run_id = $2::uuid";

/// `$1` tenant, `$2` run, `$3` user: locks the run while it is `running` and not past its
/// deadline, so its end waits for a write of its partial answer, and a write after its end finds
/// nothing ([`write_partial`]).
const LOCK_RUNNING_FOR_PARTIAL: &str = "SELECT 1 AS running FROM assistant.runs
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid AND status = 'running'
    AND deadline_at > statement_timestamp()
FOR SHARE";

/// `$1` tenant, `$2` run, `$3` the text.
const UPSERT_PARTIAL: &str = "INSERT INTO assistant.answer_partials (tenant_id, run_id, text)
VALUES ($1::uuid, $2::uuid, $3)
ON CONFLICT (tenant_id, run_id) DO UPDATE SET text = EXCLUDED.text";

/// `$1` tenant, `$2` conversation.
const RUNNING: &str = "SELECT 1 AS running FROM assistant.runs
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'";

/// `$1` tenant, `$2` conversation: records a stop request on its question that is `running` and
/// not past its deadline, keeping the first request's time, and returns its ID. The deadline is
/// compared with `statement_timestamp()`, the time after the caller waited for the
/// conversation's row lock, not with `now()`, the transaction's start. Through the partial index
/// `runs_one_running_per_conversation`.
const STOP_RUNNING: &str = "UPDATE assistant.runs
SET cancel_requested_at = coalesce(cancel_requested_at, now())
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'
    AND deadline_at > statement_timestamp()
RETURNING run_id::text AS run_id";

/// `$1` tenant, `$2` user: how many of the user's questions in the tenant are `running` and not
/// past their deadline. The partial index `runs_one_running_per_conversation` holds exactly the
/// running runs and leads with `tenant_id`, so the count reads only the tenant's running runs,
/// never its history.
const RUNNING_FOR_USER: &str = "SELECT count(*) FROM assistant.runs
WHERE tenant_id = $1::uuid AND user_id = $2::uuid AND status = 'running'
    AND deadline_at > statement_timestamp()";

/// `$1` tenant, `$2` conversation: what `load` would read, in bytes, without reading it: the
/// preamble (system prompt, tenant context, tool definitions) and the history's messages, the
/// discarded rounds left out as `load` leaves them out. `octet_length` reads a value's length
/// from its header, so a stored (TOAST) message is not fetched.
const STORED_BYTES: &str = "SELECT
    (SELECT octet_length(c.system_prompt)::bigint + octet_length(c.tenant_context)
        + octet_length(c.tools)
     FROM assistant.conversations c
     WHERE c.tenant_id = $1::uuid AND c.conversation_id = $2::uuid) AS preamble,
    (SELECT coalesce(sum(octet_length(m.body)), 0)::bigint
     FROM assistant.messages m
     WHERE m.tenant_id = $1::uuid AND m.conversation_id = $2::uuid
        AND NOT EXISTS (
            SELECT 1 FROM assistant.discarded_rounds r
            WHERE r.tenant_id = m.tenant_id AND r.conversation_id = m.conversation_id
                AND m.seq BETWEEN r.from_seq AND r.to_seq)) AS history";

/// `$1` tenant, `$2` run, `$3` conversation, `$4` user, `$5` microseconds until the deadline.
const INSERT_RUN: &str =
    "INSERT INTO assistant.runs (tenant_id, run_id, conversation_id, user_id, deadline_at)
VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, now() + $5::bigint * interval '1 microsecond')
RETURNING run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user, `$4` status, `$5` result code, `$6` the position of the
/// answer's message (only for an answered run, `0007_answer_drafts.sql`).
const FINISH_RUN: &str = "UPDATE assistant.runs
SET status = $4, result_code = $5, answer_seq = $6, ended_at = now()
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid AND status = 'running'
RETURNING run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user, `$4` conversation. A second request keeps the first time.
/// A run past its deadline is left as it is: every reader takes it as interrupted.
const REQUEST_CANCEL: &str = "UPDATE assistant.runs
SET cancel_requested_at = coalesce(cancel_requested_at, now())
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid
    AND conversation_id = $4::uuid AND status = 'running'
    AND deadline_at > statement_timestamp()
RETURNING run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user: the run, unless its conversation is deleted.
const RUN_STATUS: &str =
    "SELECT r.run_id::text AS run_id, r.conversation_id::text AS conversation_id, r.status,
    r.result_code, r.started_at, r.deadline_at, r.ended_at, r.cancel_requested_at
FROM assistant.runs r
WHERE r.tenant_id = $1::uuid AND r.run_id = $2::uuid AND r.user_id = $3::uuid
    AND EXISTS (SELECT 1 FROM assistant.conversations c
                WHERE c.tenant_id = r.tenant_id AND c.conversation_id = r.conversation_id
                    AND c.deleted_at IS NULL)";

/// `$1` tenant, `$2` run, `$3` user, `$4` conversation, `$5` whether to read the text: the
/// run, if it is that conversation's and the conversation is the user's and not deleted, whether
/// it is past its deadline and, once it is `answered`, its answer's message (none for a run that
/// ended otherwise, or one that answered before 0007 recorded where), the length of its body and
/// whether a discarded round holds that message; while it is `running`, the length of its
/// partial answer (`0009_answer_partials.sql`), if one was written. The body and the partial
/// text are read only with `$5`, so a stream reserves memory for them first, and the reads of a
/// stream that waits for a running question stay one small row ([`stream_run`]).
/// `octet_length` reads the stored value's header, not the value.
const STREAM_RUN: &str =
    "SELECT r.run_id::text AS run_id, r.conversation_id::text AS conversation_id, r.status,
    r.result_code, r.started_at, r.deadline_at, r.ended_at, r.cancel_requested_at,
    r.status = 'running' AND r.deadline_at <= statement_timestamp() AS past_deadline,
    m.kind AS answer_kind, m.format AS answer_format,
    octet_length(m.body)::bigint AS answer_bytes,
    CASE WHEN $5::boolean THEN m.body END AS answer_body,
    EXISTS (SELECT 1 FROM assistant.discarded_rounds d
            WHERE d.tenant_id = r.tenant_id AND d.conversation_id = r.conversation_id
                AND r.answer_seq BETWEEN d.from_seq AND d.to_seq) AS answer_discarded,
    octet_length(p.text)::bigint AS partial_bytes,
    CASE WHEN $5::boolean THEN p.text END AS partial_text
FROM assistant.runs r
JOIN assistant.conversations c
    ON c.tenant_id = r.tenant_id AND c.conversation_id = r.conversation_id
LEFT JOIN assistant.messages m
    ON r.status = 'answered' AND m.tenant_id = r.tenant_id
        AND m.conversation_id = r.conversation_id AND m.seq = r.answer_seq
LEFT JOIN assistant.answer_partials p
    ON r.status = 'running' AND p.tenant_id = r.tenant_id AND p.run_id = r.run_id
WHERE r.tenant_id = $1::uuid AND r.run_id = $2::uuid AND r.user_id = $3::uuid
    AND r.conversation_id = $4::uuid AND c.user_id = $3::uuid AND c.deleted_at IS NULL";

/// `$1` tenant, `$2` run, `$3` user: the run, whatever its conversation.
const OWN_RUN: &str = "SELECT 1 AS found FROM assistant.runs
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid";

/// `$1` tenant, `$2` run, `$3` user, `$4` conversation: the run, if it is that conversation's.
const RUN_IN_CONVERSATION: &str = "SELECT 1 AS found FROM assistant.runs
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid
    AND conversation_id = $4::uuid";

/// `$1` tenant, `$2` conversation, `$3` user: the conversation's question that is `running`,
/// through the partial index `runs_one_running_per_conversation` (one row at most). One that is
/// past its deadline reads as `interrupted`.
const RUNNING_RUN: &str =
    "SELECT run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at,
    deadline_at <= statement_timestamp() AS past_deadline
FROM assistant.runs
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'
    AND user_id = $3::uuid";

/// `$1` tenant, `$2` conversation, `$3` user: the conversation's latest run, by the database's
/// clock when it was recorded (`started_at`), the run ID breaking a tie: run IDs are UUIDv7 from
/// the replicas' clocks, which may be skewed. The index `runs_by_conversation`
/// `(tenant_id, conversation_id, started_at DESC, run_id DESC)` gives the first row at once,
/// and finds nothing at once for a conversation without runs or an unknown ID. The order names
/// the table's columns: a bare `run_id` would be the text the query returns (`run_id::text`),
/// which the index does not order, so every run of the conversation would be sorted.
pub const NEWEST_RUN: &str =
    "SELECT run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at,
    status = 'running' AND deadline_at <= statement_timestamp() AS past_deadline
FROM assistant.runs r
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
ORDER BY r.started_at DESC, r.run_id DESC
LIMIT 1";

/// `$1` tenant, `$2` conversation, `$3` user: locks the user's conversation, deleted or not, and
/// tells which. A conversation past its 30 days is not returned: the service role cannot lock it
/// (`expired_stay_deleted`).
const LOCK_FOR_ANSWER: &str = "SELECT deleted_at IS NOT NULL AS deleted
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
FOR NO KEY UPDATE";

/// `$1` tenant, `$2` run, `$3` user, `$4` conversation: locks the run, after its conversation.
const LOCK_RUN: &str = "SELECT status, cancel_requested_at IS NOT NULL AS stop_requested
FROM assistant.runs
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid
    AND conversation_id = $4::uuid
FOR NO KEY UPDATE";

/// The unique index that keeps one running question per conversation.
const ONE_RUNNING: &str = "runs_one_running_per_conversation";

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";

/// Where a question stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Answered,
    Failed,
    Cancelled,
    /// The service stopped mid-question: it shut down ([`RunOutcome::Interrupted`]), or its
    /// deadline passed while it was still running.
    Interrupted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [
            Self::Running,
            Self::Answered,
            Self::Failed,
            Self::Cancelled,
            Self::Interrupted,
        ]
        .into_iter()
        .find(|status| status.as_str() == name)
    }
}

/// How a question ended, for [`finish_run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome<'a> {
    Answered,
    /// With the reason: the agent's error kind (`model_timeout`, `run_expired`, ...): a lower
    /// case letter, then up to 63 lower case letters, digits and `_`.
    Failed(&'a str),
    /// Stopped at the user's request, or because its conversation was deleted.
    Cancelled,
    /// The service shut down while it ran.
    Interrupted,
}

/// A question as stored. Times are the database's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: Uuid,
    pub conversation_id: Uuid,
    pub status: RunStatus,
    /// Why it failed (only for [`RunStatus::Failed`]).
    pub result_code: Option<String>,
    pub started_at: DateTime<Utc>,
    pub deadline_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// When the user asked to stop it.
    pub cancel_requested_at: Option<DateTime<Utc>>,
}

fn record(row: &PgRow) -> Result<RunRecord, StoreError> {
    let id = |column: &str| -> Result<Uuid, StoreError> {
        row.try_get::<String, _>(column)?
            .parse()
            .map_err(|_| StoreError::Corrupt("a run's ID"))
    };
    Ok(RunRecord {
        run_id: id("run_id")?,
        conversation_id: id("conversation_id")?,
        status: status(&row.try_get::<String, _>("status")?)?,
        result_code: row.try_get("result_code")?,
        started_at: row.try_get("started_at")?,
        deadline_at: row.try_get("deadline_at")?,
        ended_at: row.try_get("ended_at")?,
        cancel_requested_at: row.try_get("cancel_requested_at")?,
    })
}

fn status(name: &str) -> Result<RunStatus, StoreError> {
    RunStatus::from_name(name).ok_or(StoreError::Corrupt("a run's status"))
}

fn valid_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    code.len() <= 64
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Records that `principal` starts the question `run_id` (the run delegation's ID) in their
/// conversation `conversation`, which must not be deleted; its deadline is `remaining` from the
/// database's now (the run delegation's deadline, `QuestionRun`'s, at most an hour away).
///
/// Locks the conversation's row, so two questions started at once run one after the other here:
/// a running question whose deadline passed is marked [`RunStatus::Interrupted`] first, and a
/// running one whose deadline has not passed refuses the new one. The unique index
/// `runs_one_running_per_conversation` holds the same rule in the database.
///
/// # Errors
///
/// [`StoreError::AlreadyRunning`]; [`StoreError::NotStored`] when the conversation is not
/// theirs or deleted; [`StoreError::Conflict`] when `run_id` is already recorded;
/// [`StoreError::Unstorable`] when `remaining` is not between a microsecond and an hour;
/// [`StoreError::WrongTenant`] or a database failure.
pub async fn start_run(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
    run_id: Uuid,
    remaining: Duration,
) -> Result<RunRecord, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let micros = i64::try_from(remaining.as_micros()).unwrap_or(i64::MAX);
    if micros == 0 || remaining > MAX_RUN_DEADLINE {
        return Err(StoreError::Unstorable(
            "a run's deadline is between a microsecond and an hour away",
        ));
    }
    let tenant_text = principal.tenant_id.to_string();
    let conversation_text = conversation.to_string();
    if !lock_owned(tx, &tenant_text, &conversation_text, principal.user_id).await? {
        return Err(StoreError::NotStored);
    }
    sqlx::query(INTERRUPT_EXPIRED)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .execute(&mut **tx)
        .await?;
    let running = sqlx::query(RUNNING)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .fetch_optional(&mut **tx)
        .await?;
    if running.is_some() {
        return Err(StoreError::AlreadyRunning);
    }
    sqlx::query(DELETE_ENDED_PARTIALS)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .execute(&mut **tx)
        .await?;
    let row = sqlx::query(INSERT_RUN)
        .bind(&tenant_text)
        .bind(run_id.to_string())
        .bind(&conversation_text)
        .bind(principal.user_id.to_string())
        .bind(micros)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            let database = error.as_database_error();
            if database.and_then(|e| e.code()).as_deref() != Some(UNIQUE_VIOLATION) {
                return StoreError::from(error);
            }
            if database.and_then(|e| e.constraint()) == Some(ONE_RUNNING) {
                StoreError::AlreadyRunning
            } else {
                StoreError::Conflict("the run ID")
            }
        })?;
    record(&row)
}

/// A question [`start_question`] started: the conversation as it was when the question started
/// (for the agent to continue), and its run.
#[derive(Debug)]
pub struct StartedQuestion {
    pub loaded: Loaded,
    pub run: RunRecord,
}

/// A question [`admit_question`] let through, in the transaction it ran in: what loading the
/// conversation will read, for the caller's memory budget, before anything is loaded. Only
/// [`admit_question`] makes one, and only [`start_question`] takes it.
#[derive(Debug)]
pub struct Admitted {
    principal: Principal,
    conversation: Uuid,
    preamble_bytes: u64,
    history_bytes: u64,
}

impl Admitted {
    /// The bytes of the conversation's preamble and history, as stored: what the loaded
    /// conversation holds.
    #[must_use]
    pub fn stored_bytes(&self) -> u64 {
        self.preamble_bytes.saturating_add(self.history_bytes)
    }

    /// The bytes of the history's messages that loading it returns (no discarded round).
    #[must_use]
    pub fn history_bytes(&self) -> u64 {
        self.history_bytes
    }
}

/// Lets the question of `principal` in their conversation `conversation` through, for the
/// question API (Laysi/trustiot-v2#646), or refuses it, without loading anything: every refusal
/// is decided on locks, a count and the stored sizes. In this order, in `tx`, which then goes on
/// to [`start_question`]:
///
/// 1. The user's start lock (`LOCK_STARTS`, the same per-user advisory lock [`super::create`]
///    takes for the daily start limit), held until `tx` ends. The count in step 4 and the insert
///    in [`start_question`] happen under it, so concurrent questions of one user are counted one
///    after the other and the limit is exact: under READ COMMITTED each statement's snapshot is
///    taken after the lock was granted, and Postgres releases the lock only after the previous
///    holder's commit is visible. It is waited for (within the request's `lock_timeout`), not
///    tried: a try would refuse a user's second question with 429 while the first only starts,
///    below the limit. Only one user's own starts wait for each other, each for a short
///    transaction.
/// 2. The conversation's row lock, so no append, discard or delete runs before the run is
///    recorded and the conversation loaded: the agent continues exactly the history that is
///    stored.
/// 3. A running question of the conversation whose deadline passed is marked interrupted; one
///    whose deadline has not refuses this one.
/// 4. The count of the user's questions in the tenant that are `running` and not past their
///    deadline; at `max_running` the question is refused.
/// 5. The sizes of the preamble and the history (`STORED_BYTES`); a history of
///    [`MAX_HISTORY_BYTES`] or more refuses the question.
///
/// # Errors
///
/// [`StoreError::NotStored`] when the conversation is not theirs or deleted;
/// [`StoreError::AlreadyRunning`]; [`StoreError::RunLimited`]; [`StoreError::HistoryFull`];
/// [`StoreError::WrongTenant`] or a database failure. The caller rolls `tx` back then.
pub async fn admit_question(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
    max_running: u32,
) -> Result<Admitted, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let tenant_text = principal.tenant_id.to_string();
    let user_text = principal.user_id.to_string();
    let conversation_text = conversation.to_string();
    sqlx::query(LOCK_STARTS)
        .bind(&tenant_text)
        .bind(&user_text)
        .execute(&mut **tx)
        .await?;
    if !lock_owned(tx, &tenant_text, &conversation_text, principal.user_id).await? {
        return Err(StoreError::NotStored);
    }
    sqlx::query(INTERRUPT_EXPIRED)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .execute(&mut **tx)
        .await?;
    let running = sqlx::query(RUNNING)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .fetch_optional(&mut **tx)
        .await?;
    if running.is_some() {
        return Err(StoreError::AlreadyRunning);
    }
    let running: i64 = sqlx::query_scalar(RUNNING_FOR_USER)
        .bind(&tenant_text)
        .bind(&user_text)
        .fetch_one(&mut **tx)
        .await?;
    if running >= i64::from(max_running) {
        return Err(StoreError::RunLimited { max: max_running });
    }
    let sizes = sqlx::query(STORED_BYTES)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .fetch_one(&mut **tx)
        .await?;
    let bytes = |column: &str| -> Result<u64, StoreError> {
        let value: Option<i64> = sizes.try_get(column)?;
        u64::try_from(value.unwrap_or_default()).map_err(|_| StoreError::Corrupt("a size"))
    };
    let (preamble_bytes, history_bytes) = (bytes("preamble")?, bytes("history")?);
    if !usize::try_from(history_bytes).is_ok_and(|bytes| bytes < MAX_HISTORY_BYTES) {
        return Err(StoreError::HistoryFull);
    }
    Ok(Admitted {
        principal,
        conversation,
        preamble_bytes,
        history_bytes,
    })
}

/// Starts the question `run_id` that [`admit_question`] let through in `tx`: records its run
/// ([`start_run`], `remaining` its deadline) and loads the conversation for the agent, under
/// the locks [`admit_question`] took. Call it only after the caller's memory budget took
/// [`Admitted::stored_bytes`].
///
/// # Errors
///
/// The errors of [`start_run`] and [`load`]; the caller rolls `tx` back then.
pub async fn start_question(
    tx: &mut TenantTransaction,
    admitted: Admitted,
    run_id: Uuid,
    remaining: Duration,
) -> Result<StartedQuestion, StoreError> {
    let Admitted {
        principal,
        conversation,
        ..
    } = admitted;
    let run = start_run(tx, principal, conversation, run_id, remaining).await?;
    let loaded = load(tx, principal, conversation)
        .await?
        .ok_or(StoreError::NotStored)?;
    Ok(StartedQuestion { loaded, run })
}

/// Records a stop request on the question of `conversation` that is `running` and not past its
/// deadline, if any, and returns its ID: what [`super::delete`] does before it deletes the
/// conversation. The caller holds the conversation's row lock; this is a statement of its own
/// after that lock, so it sees a run that a question start committed while the caller waited.
pub(super) async fn stop_running(
    tx: &mut TenantTransaction,
    tenant: &str,
    conversation: &str,
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar::<_, String>(STOP_RUNNING)
        .bind(tenant)
        .bind(conversation)
        .fetch_optional(&mut **tx)
        .await?
        .map(|id| id.parse().map_err(|_| StoreError::Corrupt("a run's ID")))
        .transpose()
}

/// Records how `principal`'s question `run_id` ended. Works whether or not its conversation was
/// deleted meanwhile: the record of the run closes either way (and goes with the conversation
/// when it is purged). An answered question's round is stored with [`finish_answered`], which
/// ends the run in the same transaction.
///
/// # Errors
///
/// [`StoreError::RunEnded`] when it already ended (for example marked interrupted after its
/// deadline); [`StoreError::NoSuchRun`]; [`StoreError::Unstorable`] for a failure code that is
/// not an identifier; [`StoreError::WrongTenant`] or a database failure.
pub async fn finish_run(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
    outcome: RunOutcome<'_>,
) -> Result<RunRecord, StoreError> {
    finish(tx, principal, run_id, outcome, None).await
}

/// [`finish_run`], recording for an answered run the position of its answer's message
/// (`answer_seq`), which the database accepts only on an answered run.
async fn finish(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
    outcome: RunOutcome<'_>,
    answer_seq: Option<i32>,
) -> Result<RunRecord, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let (status, code) = match outcome {
        RunOutcome::Answered => (RunStatus::Answered, None),
        RunOutcome::Cancelled => (RunStatus::Cancelled, None),
        RunOutcome::Interrupted => (RunStatus::Interrupted, None),
        RunOutcome::Failed(code) => {
            if !valid_code(code) {
                return Err(StoreError::Unstorable("a run's failure code"));
            }
            (RunStatus::Failed, Some(code))
        }
    };
    let finished = sqlx::query(FINISH_RUN)
        .bind(principal.tenant_id.to_string())
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .bind(status.as_str())
        .bind(code)
        .bind(answer_seq)
        .fetch_optional(&mut **tx)
        .await?;
    match finished {
        Some(row) => {
            // What went out of the answer is the start of the stored answer, or no answer at
            // all: the partial is done with either way.
            sqlx::query(DELETE_PARTIAL)
                .bind(principal.tenant_id.to_string())
                .bind(run_id.to_string())
                .execute(&mut **tx)
                .await?;
            record(&row)
        }
        None => {
            let found = sqlx::query(OWN_RUN)
                .bind(principal.tenant_id.to_string())
                .bind(run_id.to_string())
                .bind(principal.user_id.to_string())
                .fetch_optional(&mut **tx)
                .await?;
            Err(ended_or_missing(found.is_some()))
        }
    }
}

/// What became of an answered question's round in [`finish_answered`].
#[derive(Debug)]
pub enum Answered {
    /// The round is stored and the run is `answered`; `stored` is the history's new end.
    Stored { run: RunRecord, stored: Stored },
    /// A stop request or the conversation's deletion committed first: nothing of the round is
    /// stored, and the run is `cancelled`.
    Cancelled(RunRecord),
}

/// Stores the answered round of `principal`'s question `run_id` and ends the run, in one
/// transaction (`tx`, which the caller commits): `conversation` is the conversation the agent
/// answered in, `stored` what [`super::load`] returned when the question started, and `title` a
/// provisional title ([`super::provisional_title`]) set when the conversation has none.
///
/// The run records where the round's answer is (`answer_seq`, `0007_answer_drafts.sql`), in the
/// UPDATE that ends it, so the conversation's readers list the draft versions the run wrote under
/// that answer ([`super::answer_drafts`]).
///
/// It locks the conversation's row first, then the run's row (the lock order of the parent
/// module), and stores the round only while the run is still `running`, nobody asked to stop
/// it, and the conversation is not deleted. A stop ([`request_cancel`]) or a delete
/// ([`super::delete`]) that committed first wins: the round is not stored and the run ends
/// [`RunStatus::Cancelled`] ([`Answered::Cancelled`]). One that comes later waits for this
/// transaction, then finds the run ended (a stop answers [`StoreError::RunEnded`]) or deletes
/// a conversation whose round is stored.
///
/// # Errors
///
/// [`StoreError::NotStored`] when the conversation is not theirs (or is past its 30 days);
/// [`StoreError::NoSuchRun`] when the run is not of this conversation; [`StoreError::RunEnded`]
/// when the run already ended (interrupted after its deadline); the errors of
/// [`super::append`] and [`super::set_title`]; the caller rolls `tx` back then.
pub async fn finish_answered(
    tx: &mut TenantTransaction,
    conversation: &Conversation,
    stored: Stored,
    run_id: Uuid,
    title: Option<&str>,
) -> Result<Answered, StoreError> {
    let principal = conversation.principal();
    check_tenant(tx, principal.tenant_id)?;
    let tenant_text = principal.tenant_id.to_string();
    let conversation_text = conversation.id().to_string();
    let user_text = principal.user_id.to_string();
    let deleted: bool = sqlx::query_scalar(LOCK_FOR_ANSWER)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .bind(&user_text)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotStored)?;
    let run = sqlx::query(LOCK_RUN)
        .bind(&tenant_text)
        .bind(run_id.to_string())
        .bind(&user_text)
        .bind(&conversation_text)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NoSuchRun)?;
    if status(&run.try_get::<String, _>("status")?)? != RunStatus::Running {
        return Err(StoreError::RunEnded);
    }
    if deleted || run.try_get::<bool, _>("stop_requested")? {
        let cancelled = finish_run(tx, principal, run_id, RunOutcome::Cancelled).await?;
        return Ok(Answered::Cancelled(cancelled));
    }
    let answer_seq = answer_position(conversation, stored)?;
    let stored = append(tx, conversation, stored).await?;
    if let Some(title) = title {
        set_title(
            tx,
            principal,
            conversation.id(),
            title,
            TitleSource::Provisional,
        )
        .await?;
    }
    let run = finish(tx, principal, run_id, RunOutcome::Answered, answer_seq).await?;
    Ok(Answered::Stored { run, stored })
}

/// Where the answer of the round after `stored` goes in the database: the position of the
/// round's last [`EntryKind::Answer`], or `None` when the round has none. The run records it
/// (`answer_seq`), so the conversation's reader lists the draft versions the run wrote under
/// that answer ([`super::answer_drafts`]).
fn answer_position(conversation: &Conversation, stored: Stored) -> Result<Option<i32>, StoreError> {
    let Some(offset) = conversation
        .entries()
        .skip(stored.messages)
        .enumerate()
        .filter(|(_, (kind, _))| *kind == EntryKind::Answer)
        .map(|(offset, _)| offset)
        .last()
    else {
        return Ok(None);
    };
    super::seq(stored.next_seq + offset).map(Some)
}

/// The most bytes of a partial answer (`0009_answer_partials.sql`'s CHECK).
pub const MAX_PARTIAL_BYTES: usize = 512 << 10;

/// Writes `text` as the partial answer of `principal`'s question `run_id`
/// (Laysi/trustiot-v2#147, `0009_answer_partials.sql`): what of the answer may go out before the
/// question ends. Only while the run is `running` and not past its deadline: it locks the run's
/// row (FOR SHARE) first, so the run's end ([`finish_run`], [`finish_answered`]), which deletes
/// the partial in its transaction, comes before or after this write, never in between. Returns
/// whether it wrote.
///
/// # Errors
///
/// [`StoreError::Unstorable`] for a text over [`MAX_PARTIAL_BYTES`]; [`StoreError::WrongTenant`]
/// or a database failure.
pub async fn write_partial(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
    text: &str,
) -> Result<bool, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    if text.len() > MAX_PARTIAL_BYTES {
        return Err(StoreError::Unstorable("a partial answer over its limit"));
    }
    let tenant_text = principal.tenant_id.to_string();
    let run_text = run_id.to_string();
    let running = sqlx::query(LOCK_RUNNING_FOR_PARTIAL)
        .bind(&tenant_text)
        .bind(&run_text)
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    if running.is_none() {
        return Ok(false);
    }
    sqlx::query(UPSERT_PARTIAL)
        .bind(&tenant_text)
        .bind(&run_text)
        .bind(text)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Records that `principal` asked to stop their question `run_id` of their conversation
/// `conversation`; the question API stops it and then finishes it as [`RunOutcome::Cancelled`]
/// (the replica that runs it at once, the others within a poll). Asking again keeps the first
/// time. Locks the conversation's row first (the lock order of the parent module), so it runs
/// before or after a delete or an answered round's [`finish_answered`], never in between.
///
/// # Errors
///
/// [`StoreError::NotStored`] when the conversation is not theirs or deleted;
/// [`StoreError::NoSuchRun`] when the run is not of that conversation;
/// [`StoreError::RunEnded`] when it ended or is past its deadline; [`StoreError::WrongTenant`]
/// or a database failure.
pub async fn request_cancel(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
    run_id: Uuid,
) -> Result<RunRecord, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let tenant_text = principal.tenant_id.to_string();
    let conversation_text = conversation.to_string();
    if !lock_owned(tx, &tenant_text, &conversation_text, principal.user_id).await? {
        return Err(StoreError::NotStored);
    }
    let requested = sqlx::query(REQUEST_CANCEL)
        .bind(&tenant_text)
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .bind(&conversation_text)
        .fetch_optional(&mut **tx)
        .await?;
    match requested {
        Some(row) => record(&row),
        None => {
            let found = sqlx::query(RUN_IN_CONVERSATION)
                .bind(&tenant_text)
                .bind(run_id.to_string())
                .bind(principal.user_id.to_string())
                .bind(&conversation_text)
                .fetch_optional(&mut **tx)
                .await?;
            Err(ended_or_missing(found.is_some()))
        }
    }
}

/// `principal`'s question `run_id`; `None` when there is no such run of theirs or its
/// conversation is deleted. The status is the stored one: a run still `running` past its
/// deadline reads as running here (see [`latest_run`]).
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn run_status(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
) -> Result<Option<RunRecord>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    sqlx::query(RUN_STATUS)
        .bind(principal.tenant_id.to_string())
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .as_ref()
        .map(record)
        .transpose()
}

/// A question's run as its answer stream reads it ([`stream_run`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamedRun {
    /// The run; one still `running` past its deadline reads as [`RunStatus::Interrupted`], as in
    /// [`latest_run`].
    pub run: RunRecord,
    /// The length of its answer's stored message in bytes, once it is `answered`: what reading
    /// the answer loads. `None` while it runs, when it ended without an answer, when it answered
    /// before the release that records where its answer is (0007), and when its round was
    /// discarded since (the chat shows no answer of a discarded round).
    pub answer_bytes: Option<u64>,
    /// The text the chat shows of that answer (`NativeMessage::shown_text`): the stored answer,
    /// after the answer guard. Read only when asked for; `None` whenever `answer_bytes` is.
    pub answer: Option<String>,
    /// The length in bytes of the partial answer ([`write_partial`]) while the run is
    /// `running` (and not past its deadline); `None` when none was written, and once it ended.
    pub partial_bytes: Option<u64>,
    /// The partial answer's text: read only when asked for; `None` whenever `partial_bytes` is.
    pub partial: Option<String>,
}

/// The question `run_id` of `principal`'s conversation `conversation`, for its answer stream
/// (Laysi/trustiot-v2#147): `None` when the conversation is not theirs, is deleted, or the run is
/// not of it, so the stream answers the one 404. With `with_text` it reads the answer's text
/// too, which loads its whole message ([`StreamedRun::answer_bytes`] says how large it is), or
/// while the run is `running` its partial answer ([`StreamedRun::partial_bytes`]). One
/// statement, so the run and its texts come from one snapshot: an `answered` run's round
/// committed with its end ([`finish_answered`]), which deleted its partial answer.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] when the answer's message does not read
/// back as a shown answer, or a database failure.
pub async fn stream_run(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
    run_id: Uuid,
    with_text: bool,
) -> Result<Option<StreamedRun>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let Some(row) = sqlx::query(STREAM_RUN)
        .bind(principal.tenant_id.to_string())
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .bind(conversation.to_string())
        .bind(with_text)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let mut run = record(&row)?;
    if run.status == RunStatus::Running && row.try_get::<bool, _>("past_deadline")? {
        run.status = RunStatus::Interrupted;
    }
    let answer_bytes = match row.try_get::<Option<i64>, _>("answer_bytes")? {
        Some(bytes) if !row.try_get::<bool, _>("answer_discarded")? => {
            let kind: String = row.try_get("answer_kind")?;
            if EntryKind::from_name(&kind) != Some(EntryKind::Answer) {
                return Err(StoreError::Corrupt("a run's answer that is not an answer"));
            }
            Some(u64::try_from(bytes).map_err(|_| StoreError::Corrupt("a message's length"))?)
        }
        _ => None,
    };
    let running = run.status == RunStatus::Running;
    let partial_bytes = match row.try_get::<Option<i64>, _>("partial_bytes")? {
        Some(bytes) if running => {
            Some(u64::try_from(bytes).map_err(|_| StoreError::Corrupt("a partial's length"))?)
        }
        _ => None,
    };
    let partial = row
        .try_get::<Option<String>, _>("partial_text")?
        .filter(|_| partial_bytes.is_some());
    let answer = match row.try_get::<Option<Vec<u8>>, _>("answer_body")? {
        Some(body) if answer_bytes.is_some() => {
            let format = Format::from_name(&row.try_get::<String, _>("answer_format")?)
                .ok_or(StoreError::Corrupt("a message's format"))?;
            drop(row);
            Some(
                NativeMessage::new(format, raw_message(body)?)
                    .shown_text()
                    .ok_or(StoreError::Corrupt("a shown message"))?,
            )
        }
        _ => None,
    };
    Ok(Some(StreamedRun {
        run,
        answer_bytes,
        answer,
        partial_bytes,
        partial,
    }))
}

/// The latest question of `principal`'s conversation `conversation`, as its reader sees it;
/// `None` when it has none. A run still `running` past its deadline reads as
/// [`RunStatus::Interrupted`] (its `ended_at` stays `None`): the service stopped mid-question,
/// and the next [`start_run`] marks it so.
///
/// A running question is the latest one (no question starts while one runs before its
/// deadline), and the partial index `runs_one_running_per_conversation` finds it with one index
/// lookup: the case of a client that polls while its question runs. Otherwise the latest run by
/// the database's clock ([`NEWEST_RUN`], the index `runs_by_conversation`).
///
/// It reads only `principal`'s runs and does not look at the conversation: the caller finds the
/// conversation (not deleted) in the same transaction, and reads the run first when it reads the
/// history too (a run read as answered then has its round in the later reads).
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn latest_run(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
) -> Result<Option<RunRecord>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let (tenant, conversation, user) = (
        principal.tenant_id.to_string(),
        conversation.to_string(),
        principal.user_id.to_string(),
    );
    // Two literal queries, not a loop over them: tools/sql-check accepts only a constant or a
    // literal as the SQL passed to sqlx.
    let running = sqlx::query(RUNNING_RUN)
        .bind(&tenant)
        .bind(&conversation)
        .bind(&user)
        .fetch_optional(&mut **tx)
        .await?;
    let row = match running {
        Some(row) => row,
        None => {
            let newest = sqlx::query(NEWEST_RUN)
                .bind(&tenant)
                .bind(&conversation)
                .bind(&user)
                .fetch_optional(&mut **tx)
                .await?;
            let Some(row) = newest else {
                return Ok(None);
            };
            row
        }
    };
    let mut run = record(&row)?;
    if run.status == RunStatus::Running && row.try_get::<bool, _>("past_deadline")? {
        run.status = RunStatus::Interrupted;
    }
    Ok(Some(run))
}

/// Why an update of a running run found nothing: it ended, or there is no such run.
fn ended_or_missing(found: bool) -> StoreError {
    if found {
        StoreError::RunEnded
    } else {
        StoreError::NoSuchRun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_codes_are_identifiers() {
        for code in ["run_expired", "model_timeout", "a", "x9_"] {
            assert!(valid_code(code), "{code}");
        }
        let long = "a".repeat(65);
        for code in [
            "",
            "Run",
            "9a",
            "_a",
            "run-expired",
            "run expired",
            long.as_str(),
        ] {
            assert!(!valid_code(code), "{code}");
        }
    }

    /// The question API records a store failure as `store_<code>`, and a delegation that did not
    /// start as `delegation_<M1 code>`: both are failure codes the database accepts.
    #[test]
    fn store_and_delegation_failures_are_failure_codes() {
        let errors = [
            StoreError::WrongTenant,
            StoreError::NotStored,
            StoreError::Gap { from: 1, stored: 0 },
            StoreError::Conflict("x"),
            StoreError::Unstorable("x"),
            StoreError::Corrupt("x"),
            StoreError::NotLastRound,
            StoreError::AlreadyRunning,
            StoreError::HistoryFull,
            StoreError::RunLimited { max: 3 },
            StoreError::StartLimited {
                retry_after_secs: 1,
            },
            StoreError::NoSuchRun,
            StoreError::RunEnded,
            StoreError::from(sqlx::Error::PoolTimedOut),
        ];
        for error in &errors {
            let code = format!("store_{}", error.code());
            assert!(valid_code(&code), "{code}");
        }
        for code in contract::ErrorCode::ALL {
            let code = format!("delegation_{}", code.as_str());
            assert!(valid_code(&code), "{code}");
        }
    }
}
