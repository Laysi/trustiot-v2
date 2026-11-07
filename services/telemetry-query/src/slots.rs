//! Each tenant account's queries at once (Laysi/trustiot-v2#1122): the settings profile allows a
//! tenant account [`PER_TENANT`] queries at once (`max_concurrent_queries_for_user`), and
//! ClickHouse refuses the next one with `TOO_MANY_SIMULTANEOUS_QUERIES`. Every API request takes
//! one of its tenant's slots before its first query as the tenant account (the account check of
//! [`crate::accounts::Accounts::verify`]) and holds it until its last one has answered, so the
//! requests one replica serves never run more of a tenant's queries at once than ClickHouse
//! allows; the others wait for a slot, in the order they came.
//!
//! - A tenant has at most [`WAITING_PER_TENANT`] requests waiting; one more is refused at once
//!   ([`SlotError::Busy`]), so one tenant's burst holds a bounded number of requests.
//! - A request waits at most [`MAX_WAIT`]; then it is refused too, before the request's own time
//!   limit (`http.request_timeout`, 30 s by default) would answer 504 `timeout` for a query that
//!   never ran. The caller answers either refusal with 429 `rate_limited`, `details.limit` =
//!   `concurrency`.
//! - A request runs its queries within [`Slot::hold`], and the slot is given back when that
//!   ends. When the request is cancelled instead (its client went away, or its time limit
//!   `http.request_timeout` ended it), ClickHouse may still run its query: it does not cancel a
//!   read-only HTTP query when its client goes away (`cancel_http_readonly_queries_on_client_close`
//!   is off). So a slot whose work was cancelled is given back only after [`LINGER`], the most a
//!   query of the account still runs; otherwise the next request would take the slot, be refused
//!   with `TOO_MANY_SIMULTANEOUS_QUERIES` and spend the account's quota errors on it (2026-11-07
//!   security review).
//!
//! Slots are counted per replica and only for the requests of this service: another replica can
//! still hold one of the account's queries. [`crate::clickhouse::ClickHouse::query_limited`]
//! waits and retries such a refusal a bounded number of times.
//!
//! A tenant's entry exists only while it has a request holding or waiting for a slot: the last one
//! to leave removes it, so the map never holds more tenants than have requests in flight.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use contract::TenantId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A tenant account's queries at once: `max_concurrent_queries_for_user` of the settings profile
/// `trustiot_tenant_reader` (services/ingest/migrations/clickhouse/0004_access.sql). A unit test
/// keeps it equal to [`crate::accounts::LIMITS`], which the account check compares with the
/// account itself.
pub const PER_TENANT: usize = 4;

/// How many more of a tenant's requests may wait for a slot. A question of the assistant sends at
/// most 3 calls at once per replica (its own limit), a dashboard runs at most 2 datasets at once
/// per replica, and each open device page asks for 2 series a minute: 32 leaves room for several
/// users of one tenant at once.
pub const WAITING_PER_TENANT: usize = 32;

/// The longest a request waits for a slot. Below the default request time limit of 30 s, so a
/// request that never got a slot answers with what happened (429, `concurrency`) instead of a
/// timeout. A request that waited and then queried can still reach that limit (504 `timeout`);
/// its slot then stays taken for [`LINGER`].
pub const MAX_WAIT: Duration = Duration::from_secs(10);

/// How long a slot stays taken after the work in it was cancelled: the account's
/// `max_execution_time` of 30 s (a unit test keeps it equal to [`crate::accounts::LIMITS`]),
/// the longest a query that had started still runs, and 5 s for ClickHouse to notice.
pub const LINGER: Duration = Duration::from_secs(35);

/// Why a request got no slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SlotError {
    /// The tenant already had [`PER_TENANT`] requests querying and [`WAITING_PER_TENANT`]
    /// waiting, or the request waited [`MAX_WAIT`] without getting a slot.
    #[error("the tenant has as many queries running and waiting as it may")]
    Busy,
}

/// The slots of every tenant account.
pub struct QuerySlots {
    per_tenant: usize,
    waiting_per_tenant: usize,
    max_wait: Duration,
    linger: Duration,
    tenants: Mutex<BTreeMap<TenantId, Tenant>>,
}

/// A tenant with a request holding or waiting for a slot.
struct Tenant {
    slots: Arc<Semaphore>,
    /// Its requests that have joined and not left: waiting for a slot, or holding one.
    members: usize,
}

impl Default for QuerySlots {
    fn default() -> Self {
        Self::new(PER_TENANT, WAITING_PER_TENANT, MAX_WAIT)
    }
}

impl QuerySlots {
    /// `per_tenant` slots for each tenant, up to `waiting_per_tenant` more of a tenant's requests
    /// waiting, each for at most `max_wait`; a slot whose work was cancelled stays taken for
    /// [`LINGER`].
    ///
    /// # Panics
    ///
    /// When `per_tenant` is 0: no request could ever query.
    #[must_use]
    pub fn new(per_tenant: usize, waiting_per_tenant: usize, max_wait: Duration) -> Self {
        assert!(per_tenant > 0, "a tenant runs at least one query");
        Self {
            per_tenant,
            waiting_per_tenant,
            max_wait,
            linger: LINGER,
            tenants: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same, with a slot whose work was cancelled taken for `linger` instead of [`LINGER`].
    #[cfg(test)]
    fn lingering(mut self, linger: Duration) -> Self {
        self.linger = linger;
        self
    }

    /// The map, also after a panic elsewhere while it was locked: no code panics while holding
    /// it, and every change to it is complete before the lock is released.
    fn tenants(&self) -> MutexGuard<'_, BTreeMap<TenantId, Tenant>> {
        self.tenants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Joins `tenant`'s requests.
    fn join(self: &Arc<Self>, tenant: TenantId) -> Result<Member, SlotError> {
        let mut tenants = self.tenants();
        let entry = tenants.entry(tenant).or_insert_with(|| Tenant {
            slots: Arc::new(Semaphore::new(self.per_tenant)),
            members: 0,
        });
        if entry.members >= self.per_tenant + self.waiting_per_tenant {
            return Err(SlotError::Busy);
        }
        entry.members += 1;
        Ok(Member {
            slots: Arc::clone(self),
            tenant,
            semaphore: Arc::clone(&entry.slots),
        })
    }

    /// Waits for one of `tenant`'s slots, in the order the requests came.
    ///
    /// # Errors
    ///
    /// [`SlotError::Busy`] when the tenant already has as many requests in flight as it may, or
    /// no slot was free within the longest wait.
    pub async fn acquire(self: &Arc<Self>, tenant: TenantId) -> Result<Slot, SlotError> {
        let member = self.join(tenant)?;
        let semaphore = Arc::clone(&member.semaphore);
        match tokio::time::timeout(self.max_wait, semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Slot {
                permit: Some(permit),
                member: Some(member),
                linger: self.linger,
                working: false,
            }),
            // Timed out; a closed semaphore cannot happen (this module never closes one), and is
            // refused the same way rather than let the query run without a slot.
            Ok(Err(_)) | Err(_) => Err(SlotError::Busy),
        }
    }

    /// The tenants with a request in flight, and how many each has (holding or waiting).
    #[cfg(test)]
    fn in_flight(&self) -> BTreeMap<TenantId, usize> {
        self.tenants()
            .iter()
            .map(|(tenant, entry)| (*tenant, entry.members))
            .collect()
    }
}

/// A request that has joined its tenant's: counted until it is dropped, waiting or holding.
struct Member {
    slots: Arc<QuerySlots>,
    tenant: TenantId,
    /// The tenant's semaphore while the entry exists. A member keeps the entry in the map, so
    /// every member of a tenant waits on the same semaphore.
    semaphore: Arc<Semaphore>,
}

impl Drop for Member {
    fn drop(&mut self) {
        let mut tenants = self.slots.tenants();
        if let Some(entry) = tenants.get_mut(&self.tenant) {
            entry.members = entry.members.saturating_sub(1);
            if entry.members == 0 {
                tenants.remove(&self.tenant);
            }
        }
    }
}

/// One of a tenant's slots, given back when it is dropped; [`LINGER`] later when it is dropped
/// while the work of [`Slot::hold`] runs. The slot is given back before the membership, so the
/// entry leaves the map only after its last slot is back.
#[must_use = "the slot is given back when it is dropped"]
pub struct Slot {
    /// `None` only while dropping.
    permit: Option<OwnedSemaphorePermit>,
    /// `None` only while dropping.
    member: Option<Member>,
    linger: Duration,
    /// [`Slot::hold`]'s work has started and not ended: dropped now, it was cancelled.
    working: bool,
}

impl Slot {
    /// Runs `work`, the request's queries as the tenant account, within the slot, and gives the
    /// slot back when the work ends. When the request is cancelled while `work` runs, the slot
    /// is given back [`LINGER`] later: ClickHouse may still be running the query.
    pub async fn hold<F: Future>(mut self, work: F) -> F::Output {
        self.working = true;
        let output = work.await;
        self.working = false;
        output
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (permit, member) = (self.permit.take(), self.member.take());
        if !self.working {
            drop(permit);
            drop(member);
            return;
        }
        // Outside a runtime there is nothing to wait with: given back now.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            drop(permit);
            drop(member);
            return;
        };
        if let Some(member) = &member {
            tracing::info!(
                tenant_id = %member.tenant,
                linger_s = self.linger.as_secs(),
                "a request was cancelled while it queried; its query slot stays taken while \
                 ClickHouse may still run the query"
            );
        }
        let linger = self.linger;
        runtime.spawn(async move {
            tokio::time::sleep(linger).await;
            drop(permit);
            drop(member);
        });
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `future` is still pending after one poll: a zero timeout polls the future before
    /// it looks at the clock.
    async fn is_pending<F: Future + Unpin>(future: &mut F) -> bool {
        tokio::time::timeout(Duration::ZERO, future).await.is_err()
    }

    /// The slots a tenant account gets are the queries its settings profile allows at once.
    #[test]
    fn a_tenant_gets_as_many_slots_as_its_account_queries_at_once() {
        let profile = crate::accounts::LIMITS
            .iter()
            .find(|(name, _)| *name == "max_concurrent_queries_for_user")
            .map(|(_, value)| value.parse::<usize>().expect("a number"));
        assert_eq!(profile, Some(PER_TENANT));
        assert!(MAX_WAIT < service_kit::http::HttpConfig::default().request_timeout);
        let execution = crate::accounts::LIMITS
            .iter()
            .find(|(name, _)| *name == "max_execution_time")
            .map(|(_, value)| Duration::from_secs(value.parse().expect("seconds")));
        assert_eq!(
            execution.map(|time| time + Duration::from_secs(5)),
            Some(LINGER)
        );
    }

    #[tokio::test]
    async fn a_tenants_requests_beyond_its_slots_wait_and_others_do_not() {
        let slots = Arc::new(QuerySlots::new(2, 1, Duration::from_secs(3600)));
        let (tenant, other) = (TenantId::generate(), TenantId::generate());
        let first = slots.acquire(tenant).await.expect("a slot");
        let second = slots.acquire(tenant).await.expect("a slot");
        let mut third = Box::pin(slots.acquire(tenant));
        assert!(is_pending(&mut third).await, "the third waits for a slot");
        assert_eq!(
            slots.acquire(tenant).await.err(),
            Some(SlotError::Busy),
            "a fourth exceeds the waiting requests"
        );
        let _others = slots.acquire(other).await.expect("another tenant's slot");
        drop(first);
        let third = third.await.expect("the third gets the slot given back");
        assert_eq!(slots.in_flight().get(&tenant), Some(&2));
        drop((second, third));
        assert_eq!(slots.in_flight().get(&tenant), None, "the entry is removed");
    }

    #[tokio::test]
    async fn a_cancelled_wait_leaves_and_a_dropped_slot_is_given_back() {
        let slots = Arc::new(QuerySlots::new(1, 4, Duration::from_secs(3600)));
        let tenant = TenantId::generate();
        let held = slots.acquire(tenant).await.expect("a slot");
        let mut waiting = Box::pin(slots.acquire(tenant));
        assert!(is_pending(&mut waiting).await);
        assert_eq!(slots.in_flight().get(&tenant), Some(&2));
        drop(waiting);
        assert_eq!(slots.in_flight().get(&tenant), Some(&1));
        drop(held);
        assert_eq!(slots.in_flight().get(&tenant), None);
        let _again = slots.acquire(tenant).await.expect("the slot is free again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waits_at_most_the_longest_wait() {
        let slots = Arc::new(QuerySlots::new(1, 4, Duration::from_secs(10)));
        let tenant = TenantId::generate();
        let _held = slots.acquire(tenant).await.expect("a slot");
        assert_eq!(slots.acquire(tenant).await.err(), Some(SlotError::Busy));
        assert_eq!(
            slots.in_flight().get(&tenant),
            Some(&1),
            "the refused one left"
        );
    }

    /// A slot whose work ended is given back at once; one whose work was cancelled stays taken
    /// for the linger, while ClickHouse may still run its query, and then is given back.
    #[tokio::test(start_paused = true)]
    async fn a_slot_whose_work_was_cancelled_stays_taken_for_the_linger() {
        let linger = Duration::from_secs(35);
        let slots = Arc::new(QuerySlots::new(1, 4, Duration::from_secs(3600)).lingering(linger));
        let tenant = TenantId::generate();

        let done = slots
            .acquire(tenant)
            .await
            .expect("a slot")
            .hold(async { 7 })
            .await;
        assert_eq!(done, 7);
        assert_eq!(slots.in_flight().get(&tenant), None, "given back at once");

        let slot = slots.acquire(tenant).await.expect("a slot");
        let mut work = Box::pin(slot.hold(std::future::pending::<()>()));
        assert!(is_pending(&mut work).await);
        drop(work);
        let started = tokio::time::Instant::now();
        let again = slots
            .acquire(tenant)
            .await
            .expect("the slot after the linger");
        assert!(
            started.elapsed() >= linger,
            "the slot came back after {:?}",
            started.elapsed()
        );
        drop(again);
        assert_eq!(slots.in_flight().get(&tenant), None, "the entry is removed");
    }

    /// A slot dropped before its work started (an early answer) is given back at once.
    #[tokio::test(start_paused = true)]
    async fn a_slot_dropped_without_work_is_given_back_at_once() {
        let slots = Arc::new(QuerySlots::new(1, 4, Duration::from_secs(3600)));
        let tenant = TenantId::generate();
        drop(slots.acquire(tenant).await.expect("a slot"));
        let started = tokio::time::Instant::now();
        let _again = slots.acquire(tenant).await.expect("a slot");
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
