//! The caps on dataset runs per tenant (design record 「資料集的執行」): at most
//! [`RUNS_PER_MINUTE`] runs started in any minute and [`CONCURRENT_RUNS`] at a time, counted in
//! this replica. A run that would start a 61st in the minute is refused at once with the time
//! until the oldest leaves the minute; a run that finds both slots taken waits up to
//! [`RunCaps::new`]'s `wait` for one and is refused after it. Opening a dashboard in the browser
//! and the `dashboard` tool share one [`RunCaps`], so a question cannot run around the caps.
//!
//! The caps keep one tenant from using up telemetry-query's quota (120 requests a minute per
//! tenant) with dashboards alone: two replicas together stay within it. They are per replica,
//! so N replicas let a tenant start up to N × 60 runs a minute; telemetry-query's own quota
//! still holds then and answers `rate_limited`
//! ([`super::runner::RunError::QuotaExhausted`]).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use contract::TenantId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The most runs a tenant may start in any 60 seconds, per replica.
pub const RUNS_PER_MINUTE: usize = 60;

/// The most runs of a tenant at a time, per replica.
pub const CONCURRENT_RUNS: usize = 2;

/// How long a run waits for one of its tenant's [`CONCURRENT_RUNS`] slots: a dashboard's page
/// starts all its datasets at once, so a third waits for the first two instead of failing.
pub const CONCURRENT_WAIT: Duration = Duration::from_secs(10);

/// The window [`RUNS_PER_MINUTE`] counts in.
const WINDOW: Duration = Duration::from_secs(60);

/// When the table of tenants has more entries than this, the idle ones are dropped.
const PRUNE_AT: usize = 1024;

/// The time source of the per-minute count: `Instant::now` in the service, a hand-driven clock
/// in tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A run was refused by the caps; try again after `retry_after` (whole seconds, at least one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limited {
    pub retry_after: Duration,
}

/// A run admitted by the caps: it holds one of the tenant's slots until it is dropped.
#[derive(Debug)]
pub struct RunPermit {
    _slot: OwnedSemaphorePermit,
}

/// The caps of one replica, shared by every dataset run in it.
pub struct RunCaps {
    per_minute: usize,
    concurrent: usize,
    wait: Duration,
    clock: Clock,
    tenants: Mutex<HashMap<TenantId, TenantRuns>>,
}

/// One tenant's runs: when the runs of the last minute started, and its slots.
struct TenantRuns {
    started: VecDeque<Instant>,
    slots: Arc<Semaphore>,
}

impl TenantRuns {
    /// Forgets the runs that started a [`WINDOW`] or longer before `now`.
    fn prune(&mut self, now: Instant) {
        while self
            .started
            .front()
            .is_some_and(|started| now.saturating_duration_since(*started) >= WINDOW)
        {
            self.started.pop_front();
        }
    }

    /// Nothing counted and no run holding or waiting for a slot.
    fn idle(&self) -> bool {
        self.started.is_empty() && Arc::strong_count(&self.slots) == 1
    }
}

impl std::fmt::Debug for RunCaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCaps")
            .field("per_minute", &self.per_minute)
            .field("concurrent", &self.concurrent)
            .field("wait", &self.wait)
            .finish_non_exhaustive()
    }
}

impl Default for RunCaps {
    /// The service's caps: [`RUNS_PER_MINUTE`], [`CONCURRENT_RUNS`], [`CONCURRENT_WAIT`], the
    /// system clock.
    fn default() -> Self {
        Self::new(
            RUNS_PER_MINUTE,
            CONCURRENT_RUNS,
            CONCURRENT_WAIT,
            Arc::new(Instant::now),
        )
    }
}

impl RunCaps {
    /// Caps of `per_minute` runs started in any minute and `concurrent` at a time per tenant,
    /// where a run waits up to `wait` for a slot. Both counts are at least one.
    #[must_use]
    pub fn new(per_minute: usize, concurrent: usize, wait: Duration, clock: Clock) -> Self {
        Self {
            per_minute: per_minute.max(1),
            concurrent: concurrent.max(1),
            wait,
            clock,
            tenants: Mutex::new(HashMap::new()),
        }
    }

    /// Admits a run of `tenant`: counts it in the minute, then waits for a slot. A run refused
    /// while waiting for a slot is not counted.
    ///
    /// # Errors
    ///
    /// [`Limited`] when the tenant started [`RUNS_PER_MINUTE`] runs in the last minute (try
    /// again when the oldest leaves it), or no slot came free within the wait (try again in a
    /// second).
    pub async fn admit(&self, tenant: TenantId) -> Result<RunPermit, Limited> {
        let (slots, ticket) = self.count(tenant)?;
        if let Ok(slot) = Arc::clone(&slots).try_acquire_owned() {
            return Ok(RunPermit { _slot: slot });
        }
        if let Ok(Ok(slot)) = tokio::time::timeout(self.wait, slots.acquire_owned()).await {
            return Ok(RunPermit { _slot: slot });
        }
        self.uncount(tenant, ticket);
        Err(Limited {
            retry_after: Duration::from_secs(1),
        })
    }

    /// Counts a run of `tenant` in the minute: its slots and the run's start time.
    fn count(&self, tenant: TenantId) -> Result<(Arc<Semaphore>, Instant), Limited> {
        let mut tenants = self.tenants.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        if tenants.len() > PRUNE_AT {
            tenants.retain(|_, runs| {
                runs.prune(now);
                !runs.idle()
            });
        }
        let concurrent = self.concurrent;
        let runs = tenants.entry(tenant).or_insert_with(|| TenantRuns {
            started: VecDeque::new(),
            slots: Arc::new(Semaphore::new(concurrent)),
        });
        runs.prune(now);
        if runs.started.len() >= self.per_minute {
            let oldest = runs.started.front().copied().unwrap_or(now);
            let left = WINDOW.saturating_sub(now.saturating_duration_since(oldest));
            return Err(Limited {
                retry_after: whole_seconds(left),
            });
        }
        runs.started.push_back(now);
        Ok((Arc::clone(&runs.slots), now))
    }

    /// Takes back the count of a run that never got a slot.
    fn uncount(&self, tenant: TenantId, ticket: Instant) {
        let mut tenants = self.tenants.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(runs) = tenants.get_mut(&tenant)
            && let Some(position) = runs.started.iter().rposition(|started| *started == ticket)
        {
            runs.started.remove(position);
        }
    }
}

/// `duration` rounded up to whole seconds, at least one: what `Retry-After` says.
fn whole_seconds(duration: Duration) -> Duration {
    // The standard library has `impl From<bool> for u64` (stable since Rust 1.28): `true` is 1,
    // `false` is 0. The unit test `retry_after_is_whole_seconds_and_at_least_one` covers it.
    let seconds = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    Duration::from_secs(seconds.max(1))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A clock that moves only when the test says so.
    fn manual_clock() -> (Clock, Arc<AtomicU64>) {
        let start = Instant::now();
        let elapsed_ms = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&elapsed_ms);
        let clock: Clock =
            Arc::new(move || start + Duration::from_millis(shared.load(Ordering::SeqCst)));
        (clock, elapsed_ms)
    }

    fn tenant() -> TenantId {
        TenantId::from_uuid(uuid::Uuid::now_v7())
    }

    #[tokio::test]
    async fn the_61st_run_in_a_minute_waits_for_the_oldest_to_leave_it() {
        let (clock, elapsed_ms) = manual_clock();
        let caps = RunCaps::new(RUNS_PER_MINUTE, CONCURRENT_RUNS, Duration::ZERO, clock);
        let (busy, other) = (tenant(), tenant());
        for run in 0..u64::try_from(RUNS_PER_MINUTE).expect("fits") {
            // Ten runs a second, each finished before the next starts.
            elapsed_ms.store(100 * run, Ordering::SeqCst);
            drop(caps.admit(busy).await.expect("within the cap"));
        }
        // 5.9 s after the first run: the first leaves the minute 54.1 s later.
        assert_eq!(
            caps.admit(busy).await.expect_err("the 61st run"),
            Limited {
                retry_after: Duration::from_secs(55)
            }
        );
        // Another tenant runs meanwhile.
        drop(caps.admit(other).await.expect("another tenant's run"));
        // A refused run is not counted: when the first leaves the minute, one more fits.
        elapsed_ms.store(60_000, Ordering::SeqCst);
        drop(
            caps.admit(busy)
                .await
                .expect("the first run left the minute"),
        );
        assert!(caps.admit(busy).await.is_err(), "the second is still in it");
        elapsed_ms.store(60_100, Ordering::SeqCst);
        drop(caps.admit(busy).await.expect("the second left the minute"));
    }

    #[tokio::test]
    async fn a_third_concurrent_run_waits_for_a_slot_and_is_refused_after_the_wait() {
        let (clock, _) = manual_clock();
        let caps = RunCaps::new(RUNS_PER_MINUTE, CONCURRENT_RUNS, Duration::ZERO, clock);
        let busy = tenant();
        let first = caps.admit(busy).await.expect("a slot");
        let second = caps.admit(busy).await.expect("a slot");
        assert_eq!(
            caps.admit(busy).await.expect_err("both slots taken"),
            Limited {
                retry_after: Duration::from_secs(1)
            }
        );
        // Another tenant has its own slots.
        let _other = caps.admit(tenant()).await.expect("another tenant's slot");
        drop(first);
        let _third = caps.admit(busy).await.expect("a slot came free");
        drop(second);
        // The refused run was not counted: 3 runs in the minute, not 4.
        let tenants = caps.tenants.lock().expect("not poisoned");
        assert_eq!(tenants.get(&busy).expect("counted").started.len(), 3);
    }

    #[tokio::test]
    async fn a_waiting_run_gets_the_slot_that_comes_free() {
        let (clock, _) = manual_clock();
        let caps = Arc::new(RunCaps::new(
            RUNS_PER_MINUTE,
            1,
            Duration::from_secs(3600),
            clock,
        ));
        let busy = tenant();
        let first = caps.admit(busy).await.expect("a slot");
        let waiting = tokio::spawn({
            let caps = Arc::clone(&caps);
            async move { caps.admit(busy).await.map(drop) }
        });
        // The waiter cannot finish while the slot is held (the test's runtime runs it now, on
        // this thread); it does once the slot is released.
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "the slot is still held");
        drop(first);
        waiting
            .await
            .expect("the waiting run")
            .expect("it got the slot");
    }

    #[test]
    fn retry_after_is_whole_seconds_and_at_least_one() {
        assert_eq!(whole_seconds(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(
            whole_seconds(Duration::from_millis(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            whole_seconds(Duration::from_millis(1_001)),
            Duration::from_secs(2)
        );
        assert_eq!(
            whole_seconds(Duration::from_secs(54)),
            Duration::from_secs(54)
        );
    }
}
