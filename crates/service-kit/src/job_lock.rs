//! Postgres advisory locks that keep a scheduled job to one replica at a time
//! (docs/architecture/deployment.md: 排程工作用 Postgres advisory lock 互斥).
//!
//! Every replica schedules the same jobs. Before a run, a replica takes a session-level advisory
//! lock on a key derived from the service and job names; the replica that gets it runs the job,
//! the others skip that round. The lock lives exactly as long as the database session that took
//! it, so a crashed or partitioned replica cannot keep it. While a job runs, its session is
//! pinged; if the session dies (and with it the lock), the job is cancelled at its next await
//! point and [`JobLockError::Lost`] is returned, because another replica may already be running
//! it. Jobs must still be idempotent: the lock prevents concurrent runs, not repeated ones.
//!
//! Session-level locks need a session-level connection. Connect directly to Postgres or through
//! a pooler in session mode, never through transaction-mode pooling.

use std::fmt::Display;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::task::Poll;
use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgConnection, PgPool, Postgres};
use tokio::time::MissedTickBehavior;

use crate::shutdown::Shutdown;

/// How often a running job's lock session is pinged by default.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(5);

/// Separates job-lock keys from any other key derivation, and versions this one.
const NAMESPACE: &str = "service-kit/job-lock/v1";

/// The advisory-lock key of `job` in `service`: 64-bit FNV-1a over the namespace and both names.
///
/// The derivation must never change. During a rolling update old and new pods run side by side;
/// if they computed different keys for the same job, both would run it. Different services that
/// share a database get different keys for jobs of the same name.
pub fn lock_key(service: &str, job: &str) -> i64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let bytes = NAMESPACE
        .bytes()
        .chain([0])
        .chain(service.bytes())
        .chain([0])
        .chain(job.bytes());
    let hash = bytes.fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    // Postgres keys are signed; keep all 64 bits.
    i64::from_be_bytes(hash.to_be_bytes())
}

/// Why a locked run did not complete.
#[derive(Debug, thiserror::Error)]
pub enum JobLockError {
    /// The database failed while taking or releasing the lock.
    #[error("database error on the lock of job `{job}`: {source}")]
    Database {
        /// The job name.
        job: String,
        /// The database error.
        source: sqlx::Error,
    },
    /// The lock's session ended while the job ran; another replica may now run it.
    #[error("lost the lock of job `{job}`: its database session ended")]
    Lost {
        /// The job name.
        job: String,
    },
}

/// The lock of one scheduled job.
#[derive(Clone, Debug)]
pub struct JobLock {
    pool: PgPool,
    job: String,
    key: i64,
    heartbeat: Duration,
}

impl JobLock {
    /// The lock of `job` in `service`, taken through connections from `pool`.
    pub fn new(pool: PgPool, service: &str, job: &str) -> Self {
        Self {
            pool,
            job: job.to_owned(),
            key: lock_key(service, job),
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }

    /// Pings the lock's session this often while a job runs. Default 5 s.
    pub fn with_heartbeat(mut self, every: Duration) -> Self {
        self.heartbeat = every;
        self
    }

    /// The job name.
    pub fn job(&self) -> &str {
        &self.job
    }

    /// The advisory-lock key.
    pub fn key(&self) -> i64 {
        self.key
    }

    fn database(&self, source: sqlx::Error) -> JobLockError {
        JobLockError::Database {
            job: self.job.clone(),
            source,
        }
    }

    /// Takes the lock without waiting; `None` when another session holds it. The lock is held
    /// until [`JobLockGuard::release`], or until the guard is dropped, which closes its session.
    pub async fn try_acquire(&self) -> Result<Option<JobLockGuard>, JobLockError> {
        let connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| self.database(error))?;
        // From here on, dropping the guard (also when this future is cancelled mid-query) closes
        // the session instead of returning it to the pool, which releases any lock it holds.
        let mut guard = JobLockGuard {
            connection: Some(connection),
            key: self.key,
            job: self.job.clone(),
        };
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(self.key)
            .fetch_one(guard.session()?)
            .await
            .map_err(|error| self.database(error))?;
        if locked {
            Ok(Some(guard))
        } else {
            guard.return_unlocked();
            Ok(None)
        }
    }

    /// Runs `job` if this replica gets the lock and returns its output; returns `None` without
    /// running it when another replica holds the lock.
    pub async fn run_exclusive<F, Fut, R>(&self, job: F) -> Result<Option<R>, JobLockError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = R>,
    {
        let Some(mut guard) = self.try_acquire().await? else {
            return Ok(None);
        };
        let outcome = tokio::select! {
            output = job() => Ok(output),
            lost = guard.watch(self.heartbeat) => Err(lost),
        };
        match outcome {
            Ok(output) => {
                guard.release().await?;
                Ok(Some(output))
            }
            // Dropping the guard closes the dead session's connection.
            Err(lost) => Err(lost),
        }
    }

    /// Runs `job` every `period` on whichever replica gets the lock, until `shutdown` begins
    /// stopping. The first round starts at once. A run that is still going when shutdown
    /// begins finishes unless the service's drain timeout cuts it off. Failures are logged.
    ///
    /// A job that panics does not stop the schedule: the panic is caught, one ERROR line names
    /// the job (never the panic payload, which may carry data), the lock is released as after
    /// any other run, and the job runs again next period. The closure is called again after a
    /// panic, so state it shares must not be left half-updated (a poisoned `Mutex`, for
    /// example). A binary built with `panic = "abort"` still exits on a panic.
    pub async fn run_periodically<F, Fut, E>(self, period: Duration, shutdown: Shutdown, mut job: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
        E: Display,
    {
        let mut ticks = tokio::time::interval(period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = shutdown.stopping() => break,
                _ = ticks.tick() => {}
            }
            // The closure runs inside the catch, so a panic while it builds the future is caught
            // too. The run then ends normally and the lock is released with an explicit unlock.
            match self
                .run_exclusive(|| catch_panic(async { job().await }))
                .await
            {
                Ok(Some(Ok(Ok(())))) => tracing::debug!(job = %self.job, "scheduled job finished"),
                Ok(Some(Ok(Err(error)))) => {
                    tracing::warn!(job = %self.job, error = %error, "scheduled job failed");
                }
                // The payload is dropped unread: the panic hook has already logged the panic
                // with its location, and this line must carry nothing a job could put in a
                // panic message.
                Ok(Some(Err(Panicked))) => tracing::error!(
                    job = %self.job,
                    "scheduled job panicked; it runs again next period"
                ),
                Ok(None) => tracing::debug!(
                    job = %self.job,
                    "scheduled job skipped; another replica holds its lock"
                ),
                Err(error) => {
                    tracing::warn!(job = %self.job, error = %error, "scheduled job lock failed");
                }
            }
        }
    }
}

/// A job panicked. The panic payload is deliberately not kept.
struct Panicked;

/// Awaits `future`, turning a panic while it is polled into `Err(Panicked)`.
async fn catch_panic<Fut: Future>(future: Fut) -> Result<Fut::Output, Panicked> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|context| {
        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => {
                drop(payload);
                Poll::Ready(Err(Panicked))
            }
        }
    })
    .await
}

/// A held job lock.
#[derive(Debug)]
pub struct JobLockGuard {
    connection: Option<PoolConnection<Postgres>>,
    key: i64,
    job: String,
}

impl JobLockGuard {
    fn session(&mut self) -> Result<&mut PgConnection, JobLockError> {
        match self.connection.as_mut() {
            Some(connection) => Ok(&mut **connection),
            None => Err(JobLockError::Lost {
                job: self.job.clone(),
            }),
        }
    }

    /// Returns a connection that holds no lock to the pool.
    fn return_unlocked(&mut self) {
        drop(self.connection.take());
    }

    /// Resolves only when the lock's session stops answering pings.
    async fn watch(&mut self, every: Duration) -> JobLockError {
        loop {
            tokio::time::sleep(every).await;
            let alive = match self.session() {
                Ok(session) => session.ping().await.is_ok(),
                Err(lost) => return lost,
            };
            if !alive {
                tracing::warn!(job = %self.job, "job lock session ended while the job ran");
                return JobLockError::Lost {
                    job: self.job.clone(),
                };
            }
        }
    }

    /// Releases the lock and returns the connection to the pool.
    pub async fn release(mut self) -> Result<(), JobLockError> {
        let key = self.key;
        let job = self.job.clone();
        let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(key)
            .fetch_one(self.session()?)
            .await
            .map_err(|source| JobLockError::Database {
                job: job.clone(),
                source,
            })?;
        if !released {
            // The session is alive but does not hold the lock: something released it behind
            // the guard's back. Close the session rather than trust it.
            return Err(JobLockError::Lost { job });
        }
        self.return_unlocked();
        Ok(())
    }
}

impl Drop for JobLockGuard {
    fn drop(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            // Ending the session releases the lock even when an explicit unlock cannot run
            // (panic, cancellation, failed release).
            if tokio::runtime::Handle::try_current().is_ok() {
                connection.close_on_drop();
            } else {
                drop(connection.detach());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::lock_key;

    #[test]
    fn lock_keys_are_stable_across_releases() {
        // Pinned values, cross-checked with an independent FNV-1a implementation on
        // 2026-10-10. A change here means old and new pods disagree during a rollout.
        assert_eq!(
            lock_key("assets", "purge-expired"),
            3_554_545_238_182_866_881
        );
        assert_eq!(
            lock_key("alerts", "evaluate-rules"),
            2_121_025_707_153_906_333
        );
        // Hashes with the top bit set map to negative keys without losing bits.
        assert_eq!(lock_key("knowledge", "reindex"), -849_317_837_940_309_294);
    }

    #[test]
    fn lock_keys_separate_services_and_name_boundaries() {
        assert_ne!(lock_key("assets", "purge"), lock_key("alerts", "purge"));
        assert_ne!(lock_key("ab", "c"), lock_key("a", "bc"));
    }
}
