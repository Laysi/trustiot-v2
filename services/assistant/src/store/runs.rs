//! The record of every question asked (Laysi/trustiot-v2#644): one row per run, keyed by the
//! run delegation's ID (Laysi/trustiot-v2#551), with how it ended. A conversation has at most
//! one run that is `running` and not past its deadline; a run left `running` past its deadline
//! (the service restarted mid-question) becomes `interrupted` when the next question starts.
//! Failed, cancelled and interrupted runs leave nothing in the history: the chat API
//! (Laysi/trustiot-v2#646) appends a round only after its run answered.

use std::time::Duration;

use chrono::{DateTime, Utc};
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use super::{StoreError, check_tenant, lock_owned};
use crate::history::Principal;

/// The furthest deadline a run may have (the database's limit too); a run delegation lives at
/// most 15 minutes.
const MAX_RUN_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// `$1` tenant, `$2` conversation: its running question whose deadline passed ends as
/// interrupted.
const INTERRUPT_EXPIRED: &str = "UPDATE assistant.runs
SET status = 'interrupted', ended_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'
    AND deadline_at <= now()";

/// `$1` tenant, `$2` conversation.
const RUNNING: &str = "SELECT 1 AS running FROM assistant.runs
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'";

/// `$1` tenant, `$2` conversation: a run still `running` whose deadline has not passed. The
/// deadline is compared with `statement_timestamp()`, the time after the caller waited for the
/// conversation's row lock, not with `now()`, the transaction's start: a transaction that
/// waited past a deadline sees it passed.
const RUNNING_BEFORE_DEADLINE: &str = "SELECT 1 AS running FROM assistant.runs
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND status = 'running'
    AND deadline_at > statement_timestamp()";

/// `$1` tenant, `$2` run, `$3` conversation, `$4` user, `$5` microseconds until the deadline.
const INSERT_RUN: &str =
    "INSERT INTO assistant.runs (tenant_id, run_id, conversation_id, user_id, deadline_at)
VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, now() + $5::bigint * interval '1 microsecond')
RETURNING run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user, `$4` status, `$5` result code.
const FINISH_RUN: &str = "UPDATE assistant.runs
SET status = $4, result_code = $5, ended_at = now()
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid AND status = 'running'
RETURNING run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user. A second request keeps the first time.
const REQUEST_CANCEL: &str = "UPDATE assistant.runs r
SET cancel_requested_at = coalesce(r.cancel_requested_at, now())
WHERE r.tenant_id = $1::uuid AND r.run_id = $2::uuid AND r.user_id = $3::uuid
    AND r.status = 'running'
    AND EXISTS (SELECT 1 FROM assistant.conversations c
                WHERE c.tenant_id = r.tenant_id AND c.conversation_id = r.conversation_id
                    AND c.deleted_at IS NULL)
RETURNING r.run_id::text AS run_id, r.conversation_id::text AS conversation_id, r.status,
    r.result_code, r.started_at, r.deadline_at, r.ended_at, r.cancel_requested_at";

/// `$1` tenant, `$2` run, `$3` user: the run, unless its conversation is deleted.
const RUN_STATUS: &str =
    "SELECT r.run_id::text AS run_id, r.conversation_id::text AS conversation_id, r.status,
    r.result_code, r.started_at, r.deadline_at, r.ended_at, r.cancel_requested_at
FROM assistant.runs r
WHERE r.tenant_id = $1::uuid AND r.run_id = $2::uuid AND r.user_id = $3::uuid
    AND EXISTS (SELECT 1 FROM assistant.conversations c
                WHERE c.tenant_id = r.tenant_id AND c.conversation_id = r.conversation_id
                    AND c.deleted_at IS NULL)";

/// `$1` tenant, `$2` run, `$3` user: the run, whatever its conversation.
const OWN_RUN: &str =
    "SELECT run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at
FROM assistant.runs
WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND user_id = $3::uuid";

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
    /// Its deadline passed while it was still running: the service stopped mid-question.
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
    /// Stopped at the user's request.
    Cancelled,
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
        status: RunStatus::from_name(&row.try_get::<String, _>("status")?)
            .ok_or(StoreError::Corrupt("a run's status"))?,
        result_code: row.try_get("result_code")?,
        started_at: row.try_get("started_at")?,
        deadline_at: row.try_get("deadline_at")?,
        ended_at: row.try_get("ended_at")?,
        cancel_requested_at: row.try_get("cancel_requested_at")?,
    })
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

/// Whether a question of the conversation is `running` and its deadline has not passed, in a
/// statement (and so a snapshot) of its own after the caller locked the conversation's row: it
/// sees a run committed by whoever held the lock before. It changes nothing; a run past its
/// deadline stays `running` until the next [`start_run`] marks it interrupted.
pub(super) async fn running_before_deadline(
    tx: &mut TenantTransaction,
    tenant: &str,
    conversation: &str,
) -> Result<bool, StoreError> {
    Ok(sqlx::query(RUNNING_BEFORE_DEADLINE)
        .bind(tenant)
        .bind(conversation)
        .fetch_optional(&mut **tx)
        .await?
        .is_some())
}

/// Records how `principal`'s question `run_id` ended. Works whether or not its conversation was
/// deleted meanwhile: the record of the run closes either way (and goes with the conversation
/// when it is purged).
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
    check_tenant(tx, principal.tenant_id)?;
    let (status, code) = match outcome {
        RunOutcome::Answered => (RunStatus::Answered, None),
        RunOutcome::Cancelled => (RunStatus::Cancelled, None),
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
        .fetch_optional(&mut **tx)
        .await?;
    match finished {
        Some(row) => record(&row),
        None => Err(ended_or_missing(tx, principal, run_id, Lookup::Any).await?),
    }
}

/// Records that `principal` asked to stop their running question `run_id`; the chat API stops
/// it and then finishes it as [`RunOutcome::Cancelled`]. Asking again keeps the first time.
///
/// # Errors
///
/// [`StoreError::RunEnded`]; [`StoreError::NoSuchRun`] (also when its conversation is deleted);
/// [`StoreError::WrongTenant`] or a database failure.
pub async fn request_cancel(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
) -> Result<RunRecord, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let requested = sqlx::query(REQUEST_CANCEL)
        .bind(principal.tenant_id.to_string())
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    match requested {
        Some(row) => record(&row),
        None => Err(ended_or_missing(tx, principal, run_id, Lookup::Visible).await?),
    }
}

/// `principal`'s question `run_id`; `None` when there is no such run of theirs or its
/// conversation is deleted.
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

/// Which runs an update of a running run could have found.
#[derive(Clone, Copy)]
enum Lookup {
    /// Every run of the user ([`OWN_RUN`]).
    Any,
    /// Runs of conversations that are not deleted ([`RUN_STATUS`]).
    Visible,
}

/// Why an update of a running run found nothing: it ended, or `lookup` does not find it.
async fn ended_or_missing(
    tx: &mut TenantTransaction,
    principal: Principal,
    run_id: Uuid,
    lookup: Lookup,
) -> Result<StoreError, StoreError> {
    let query = match lookup {
        Lookup::Any => sqlx::query(OWN_RUN),
        Lookup::Visible => sqlx::query(RUN_STATUS),
    };
    let found = query
        .bind(principal.tenant_id.to_string())
        .bind(run_id.to_string())
        .bind(principal.user_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    Ok(if found.is_some() {
        StoreError::RunEnded
    } else {
        StoreError::NoSuchRun
    })
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
}
