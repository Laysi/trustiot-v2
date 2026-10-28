//! The scheduled purge (Laysi/trustiot-v2#645): conversations deleted more than
//! [`store::DELETED_RETENTION_DAYS`] days ago leave the database for good, with their messages,
//! discarded rounds and runs ([`store::purge_expired`]).
//!
//! Every replica schedules it, and one at a time runs it: a round runs under the service's job
//! lock (`JobLock`, a Postgres advisory lock taken on the service role's pool, as service-kit
//! means that pool for), and a replica that finds the lock held skips the round. A round deletes
//! in batches of [`PURGE_BATCH`] conversations, each in a transaction of its own of the platform
//! role `assistant_platform`, which sees and deletes only expired conversations, until a batch
//! deletes fewer. It then logs one INFO line with how many it deleted, also none; never a title,
//! an ID or anything of the content.
//!
//! A batch waits at most [`store::PURGE_LOCK_TIMEOUT`] for a row another session holds (a
//! restore that started just before the 30 days ended): it then fails with SQLSTATE `55P03`,
//! deletes nothing, and the round stops with one WARN line; the next round tries again.

use std::time::Duration;

use contract::ServiceName;
use service_kit::Context;
use service_kit::identity::ServiceIdentity;
use service_kit::job_lock::JobLock;
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDbError};
use sqlx::PgPool;

use crate::config::Config;
use crate::store::{self, StoreError};

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

/// One round: deletes expired conversations in batches until a batch deletes fewer than
/// [`PURGE_BATCH`], and returns how many it deleted. Logs one INFO line with the number, or one
/// WARN line with the number so far when a row stayed locked (then it returns that number). The
/// caller holds the job lock.
///
/// # Errors
///
/// When a platform transaction cannot start, or a batch or its commit fails otherwise; the
/// batches before it stay deleted.
pub async fn round(platform: &PlatformDb) -> Result<u64, PurgeError> {
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assistant));
    let mut purged = 0_u64;
    loop {
        let mut tx = platform.begin(actor, PURPOSE).await?;
        let batch = match store::purge_expired(&mut tx, PURGE_BATCH).await {
            Ok(batch) => batch,
            Err(failure) => {
                // A rollback that fails leaves nothing behind: the connection is closed then.
                let _ = tx.rollback().await;
                if failure.sqlstate() == Some(store::LOCK_NOT_AVAILABLE) {
                    tracing::warn!(
                        job = JOB,
                        purged,
                        error = %failure,
                        "purge round stopped: an expired conversation stayed locked; the next \
                         round tries again"
                    );
                    return Ok(purged);
                }
                return Err(failure.into());
            }
        };
        tx.commit().await.map_err(StoreError::from)?;
        purged += batch;
        if batch < u64::from(PURGE_BATCH) {
            break;
        }
    }
    tracing::info!(job = JOB, purged, "purged expired conversations");
    Ok(purged)
}
