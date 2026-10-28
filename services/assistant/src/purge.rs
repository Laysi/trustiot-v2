//! The scheduled purge (Laysi/trustiot-v2#645): conversations deleted more than
//! [`store::DELETED_RETENTION_DAYS`] days ago leave the database for good, with their messages,
//! discarded rounds and runs ([`store::purge_expired`]).
//!
//! Every replica schedules it, and one at a time runs it: a round runs under the service's job
//! lock (`JobLock`, a Postgres advisory lock taken on the service role's pool, as service-kit
//! means that pool for), and a replica that finds the lock held skips the round. A round deletes
//! in batches of [`PURGE_BATCH`] conversations, each in a transaction of its own of the platform
//! role `assistant_platform`, which sees and deletes only expired conversations, until a batch
//! deletes fewer.
//!
//! A row another session holds does not hold up the others. A batch waits at most
//! [`store::PURGE_LOCK_TIMEOUT`] for a lock, then fails with SQLSTATE `55P03` and deletes nothing;
//! the round then deletes that batch's conversations one at a time ([`store::purge_one`], each in
//! its own transaction, waiting at most [`store::PURGE_ROW_LOCK_TIMEOUT`]), skips the ones that
//! stay locked, and leaves them out of its later batches. A skipped conversation stays until a
//! round finds it unlocked, normally the next one.
//!
//! A round logs one line with how many conversations it deleted (`purged`, also none) and how
//! many stayed locked (`skipped`): INFO, or WARN when it skipped any. Never a title, an ID, a
//! tenant or anything of the content.

use std::time::Duration;

use contract::ServiceName;
use service_kit::Context;
use service_kit::identity::ServiceIdentity;
use service_kit::job_lock::JobLock;
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDbError};
use sqlx::PgPool;

use crate::config::Config;
use crate::store::{self, ExpiredConversation, StoreError};

/// The job's name in the job lock and the logs.
pub const JOB: &str = "purge";

/// Conversations one transaction deletes. Each takes its messages, discarded rounds and runs
/// with it (up to some thousand rows), so a batch stays a short transaction that holds few
/// locks, and a round that stops keeps what the batches before it deleted.
pub const PURGE_BATCH: u32 = 100;

/// Why the platform transactions are opened, as `platform database access` logs it.
pub const PURPOSE: &str = "purge expired conversations";

/// Why a round failed: the class of the failure only (a [`StoreError`] carries no database text).
#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    /// No platform transaction could start: the role could not log in, or its session was
    /// refused.
    #[error("could not start a platform transaction: {0}")]
    Begin(String),
    /// A batch or its commit failed.
    #[error("{0}")]
    Store(#[from] StoreError),
}

impl From<TenantDbError> for PurgeError {
    fn from(failure: TenantDbError) -> Self {
        match failure {
            // The class only: sqlx's text can carry values.
            TenantDbError::Database(error) => Self::Begin(StoreError::from(error).to_string()),
            TenantDbError::UnsafeSession(_) => Self::Begin("unsafe database session".into()),
            _ => Self::Begin("database error".into()),
        }
    }
}

/// Schedules the purge every `interval` on this replica, until the service stops. The first
/// round starts at once.
pub fn spawn(
    context: &Context<Config>,
    lock_pool: PgPool,
    platform: PlatformDb,
    interval: Duration,
) {
    let lock = JobLock::new(lock_pool, "assistant", JOB);
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        lock.run_periodically(interval, shutdown, move || {
            let platform = platform.clone();
            async move { round(&platform).await.map(|_| ()) }
        })
        .await;
    });
}

/// What one round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Round {
    /// Conversations it deleted.
    pub purged: u64,
    /// Expired conversations it left because another session held them longer than
    /// [`store::PURGE_ROW_LOCK_TIMEOUT`]; a later round deletes them.
    pub skipped: u64,
}

/// What the one-at-a-time fallback of a batch did.
#[derive(Default)]
struct OneAtATime {
    purged: u64,
    locked: Vec<ExpiredConversation>,
}

/// One round: deletes expired conversations in batches until a batch deletes fewer than
/// [`PURGE_BATCH`] or none are left but the skipped ones, and logs one line with what it did
/// (see the module documentation). The caller holds the job lock.
///
/// # Errors
///
/// When a platform transaction cannot start, or a batch, a single deletion or a commit fails
/// otherwise than by waiting for a lock; what was deleted before stays deleted.
pub async fn round(platform: &PlatformDb) -> Result<Round, PurgeError> {
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assistant));
    let mut purged = 0_u64;
    let mut skipped: Vec<ExpiredConversation> = Vec::new();
    loop {
        let mut tx = platform.begin(actor, PURPOSE).await?;
        match store::purge_expired(&mut tx, PURGE_BATCH, &skipped).await {
            Ok(batch) => {
                tx.commit().await.map_err(StoreError::from)?;
                purged += batch;
                if batch < u64::from(PURGE_BATCH) {
                    break;
                }
            }
            Err(failure) if failure.sqlstate() == Some(store::LOCK_NOT_AVAILABLE) => {
                // A rollback that fails leaves nothing behind: the connection is closed then.
                let _ = tx.rollback().await;
                let done = one_at_a_time(platform, actor, &skipped).await?;
                purged += done.purged;
                let progress = done.purged > 0 || !done.locked.is_empty();
                skipped.extend(done.locked);
                if !progress {
                    // The batch's candidates were gone by the time they were tried one by
                    // one (restored or deleted meanwhile); the next round starts afresh.
                    break;
                }
            }
            Err(failure) => {
                let _ = tx.rollback().await;
                return Err(failure.into());
            }
        }
    }
    let done = Round {
        purged,
        skipped: u64::try_from(skipped.len()).unwrap_or(u64::MAX),
    };
    if done.skipped > 0 {
        tracing::warn!(
            job = JOB,
            purged = done.purged,
            skipped = done.skipped,
            "purged expired conversations; some stayed locked and wait for the next round"
        );
    } else {
        tracing::info!(
            job = JOB,
            purged = done.purged,
            skipped = 0_u64,
            "purged expired conversations"
        );
    }
    Ok(done)
}

/// Deletes the conversations the failed batch would have deleted, one per transaction, and
/// returns how many it deleted and which stayed locked.
async fn one_at_a_time(
    platform: &PlatformDb,
    actor: PlatformActor,
    skipped: &[ExpiredConversation],
) -> Result<OneAtATime, PurgeError> {
    let mut tx = platform.begin(actor, PURPOSE).await?;
    let candidates = store::expired_conversations(&mut tx, PURGE_BATCH, skipped).await;
    let _ = tx.rollback().await;
    let mut done = OneAtATime::default();
    for conversation in candidates? {
        let mut tx = platform.begin(actor, PURPOSE).await?;
        match store::purge_one(&mut tx, conversation).await {
            Ok(deleted) => {
                tx.commit().await.map_err(StoreError::from)?;
                done.purged += u64::from(deleted);
            }
            Err(failure) if failure.sqlstate() == Some(store::LOCK_NOT_AVAILABLE) => {
                let _ = tx.rollback().await;
                done.locked.push(conversation);
            }
            Err(failure) => {
                let _ = tx.rollback().await;
                return Err(failure.into());
            }
        }
    }
    Ok(done)
}
