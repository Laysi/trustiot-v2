//! Limits shared by the dashboards' work in one replica, besides the dataset runs' caps
//! ([`super::caps`]):
//!
//! - [`KeyedSlots`]: at most a few at a time per key (a tenant), with a bounded wait for a
//!   slot. The `dashboard` tool's writes take one of their tenant's slot before they run
//!   datasets, so one tenant has one write in its run and check at a time.
//! - [`VersionBudget`]: the versions publishes and restores of the public API add, which are
//!   never removed: at most [`VERSIONS_PER_USER`] per user in any minute, and
//!   [`VERSIONS_PER_TENANT`] per tenant for its saved dashboards and as many for its drafts.
//!
//! Each counts in this replica only: N replicas allow N times as much.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use contract::api::dashboard::public::DashboardKind;
use contract::{TenantId, UserId};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::caps::{Clock, Limited, whole_seconds};
use crate::history::Principal;

/// When a table has more keys than this, the idle ones are dropped.
const PRUNE_AT: usize = 1024;

/// The most versions one user may add by publishing and restoring in any 60 seconds, per
/// replica.
pub const VERSIONS_PER_USER: usize = 10;

/// The most versions the users of one tenant may add to its saved dashboards in any 60
/// seconds, per replica; the tenant's drafts have a budget of the same size of their own.
pub const VERSIONS_PER_TENANT: usize = 30;

/// The window [`VERSIONS_PER_USER`] and [`VERSIONS_PER_TENANT`] count in.
const WINDOW: Duration = Duration::from_secs(60);

/// A slot of a [`KeyedSlots`] or of a process-wide limit: held until dropped.
#[derive(Debug)]
pub struct Slot {
    _permit: OwnedSemaphorePermit,
}

/// Up to `per_key` slots for each key, taken in the order asked for.
#[derive(Debug)]
pub struct KeyedSlots<K> {
    per_key: usize,
    keys: Mutex<HashMap<K, Arc<Semaphore>>>,
}

impl<K: Eq + Hash + Copy> KeyedSlots<K> {
    /// `per_key` slots for each key, at least one.
    #[must_use]
    pub fn new(per_key: usize) -> Self {
        Self {
            per_key: per_key.max(1),
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// One of `key`'s slots, waiting up to `wait` for one to come free; `None` after the wait.
    pub async fn take(&self, key: K, wait: Duration) -> Option<Slot> {
        let slots = {
            let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
            if keys.len() > PRUNE_AT {
                // Only the table holds an idle key's slots: no slot taken and nobody waiting.
                keys.retain(|_, slots| Arc::strong_count(slots) > 1);
            }
            let per_key = self.per_key;
            Arc::clone(
                keys.entry(key)
                    .or_insert_with(|| Arc::new(Semaphore::new(per_key))),
            )
        };
        take(slots, wait).await
    }
}

/// One of `slots`, waiting up to `wait` for one to come free; `None` after the wait.
pub async fn take(slots: Arc<Semaphore>, wait: Duration) -> Option<Slot> {
    if let Ok(permit) = Arc::clone(&slots).try_acquire_owned() {
        return Some(Slot { _permit: permit });
    }
    match tokio::time::timeout(wait, slots.acquire_owned()).await {
        Ok(Ok(permit)) => Some(Slot { _permit: permit }),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The versions publishes and restores added in the last minute, per user and per tenant.
///
/// The store asks ([`super::store::publish`], [`super::store::restore`]) inside the request's
/// transaction once every other check passed, just before it writes: only a version about to
/// be written counts. A request refused for who sent it (a viewer's publish, someone else's
/// draft) or for the dashboard's state (a conflict, an archived dashboard, a restore that
/// changes nothing) counts nothing, so it cannot use up a budget others share; a request this
/// budget refuses counts nothing either. A version counted whose transaction then fails stays
/// counted.
///
/// A tenant's saved dashboards and its drafts have budgets of their own: a viewer, who adds
/// versions only to their own drafts, never takes from what editors publish and restore.
pub struct VersionBudget {
    per_user: usize,
    per_tenant: usize,
    clock: Clock,
    started: Mutex<Started>,
}

/// When the counted versions were added, by user and by tenant and kind of dashboard.
#[derive(Default)]
struct Started {
    users: HashMap<(TenantId, UserId), VecDeque<Instant>>,
    tenants: HashMap<(TenantId, DashboardKind), VecDeque<Instant>>,
}

impl std::fmt::Debug for VersionBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionBudget")
            .field("per_user", &self.per_user)
            .field("per_tenant", &self.per_tenant)
            .finish_non_exhaustive()
    }
}

impl Default for VersionBudget {
    /// The service's budget: [`VERSIONS_PER_USER`], [`VERSIONS_PER_TENANT`], the system clock.
    fn default() -> Self {
        Self::new(
            VERSIONS_PER_USER,
            VERSIONS_PER_TENANT,
            Arc::new(Instant::now),
        )
    }
}

impl VersionBudget {
    /// A budget of `per_user` versions per user and `per_tenant` per tenant and kind of
    /// dashboard in any minute, each at least one.
    #[must_use]
    pub fn new(per_user: usize, per_tenant: usize, clock: Clock) -> Self {
        Self {
            per_user: per_user.max(1),
            per_tenant: per_tenant.max(1),
            clock,
            started: Mutex::new(Started::default()),
        }
    }

    /// Counts a version `principal` is about to add to a dashboard of `kind` against both the
    /// user's budget and the tenant's for that kind, or against neither.
    ///
    /// # Errors
    ///
    /// [`Limited`] when the user, or the tenant for that kind, added its budget in the last
    /// minute, with the time until both have room again.
    pub fn admit(&self, principal: Principal, kind: DashboardKind) -> Result<(), Limited> {
        let mut started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        if started.users.len() > PRUNE_AT || started.tenants.len() > PRUNE_AT {
            started.users.retain(|_, times| {
                forget_old(times, now);
                !times.is_empty()
            });
            started.tenants.retain(|_, times| {
                forget_old(times, now);
                !times.is_empty()
            });
        }
        let Started { users, tenants } = &mut *started;
        let user = users
            .entry((principal.tenant_id, principal.user_id))
            .or_default();
        let tenant = tenants.entry((principal.tenant_id, kind)).or_default();
        forget_old(user, now);
        forget_old(tenant, now);
        let wait =
            room_after(user, self.per_user, now).max(room_after(tenant, self.per_tenant, now));
        if wait > Duration::ZERO {
            return Err(Limited {
                retry_after: whole_seconds(wait),
            });
        }
        user.push_back(now);
        tenant.push_back(now);
        Ok(())
    }
}

/// Forgets the requests that started a [`WINDOW`] or longer before `now`.
fn forget_old(times: &mut VecDeque<Instant>, now: Instant) {
    while times
        .front()
        .is_some_and(|started| now.saturating_duration_since(*started) >= WINDOW)
    {
        times.pop_front();
    }
}

/// How long until `times` (of the last minute) has room for one more within `budget`: zero
/// when it has room now.
fn room_after(times: &VecDeque<Instant>, budget: usize, now: Instant) -> Duration {
    if times.len() < budget {
        return Duration::ZERO;
    }
    // The request that must leave the minute first for one more to fit.
    let leaving = times.len() - budget;
    times.get(leaving).map_or(Duration::ZERO, |started| {
        WINDOW.saturating_sub(now.saturating_duration_since(*started))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[tokio::test]
    async fn each_key_has_its_own_slots_and_a_waiter_gets_the_one_that_comes_free() {
        let slots = Arc::new(KeyedSlots::new(1));
        let first = slots.take(1_u8, Duration::ZERO).await.expect("a slot");
        assert!(
            slots.take(1, Duration::ZERO).await.is_none(),
            "the key's slot is taken"
        );
        let _other = slots
            .take(2, Duration::ZERO)
            .await
            .expect("another key's slot");
        let waiting = tokio::spawn({
            let slots = Arc::clone(&slots);
            async move { slots.take(1, Duration::from_secs(3600)).await.map(drop) }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "the slot is still held");
        drop(first);
        waiting
            .await
            .expect("the waiter")
            .expect("it got the slot that came free");
    }

    #[tokio::test]
    async fn idle_keys_are_dropped_from_a_large_table() {
        let slots = KeyedSlots::new(1);
        let held = slots.take(u32::MAX, Duration::ZERO).await.expect("a slot");
        for key in 0..=u32::try_from(PRUNE_AT).expect("fits") {
            drop(slots.take(key, Duration::ZERO).await.expect("a slot"));
        }
        let keys = slots.keys.lock().expect("not poisoned");
        assert!(keys.len() <= 2, "{} keys kept", keys.len());
        assert!(
            keys.contains_key(&u32::MAX),
            "a key whose slot is held stays"
        );
        drop(keys);
        drop(held);
    }

    /// A clock that moves only when the test says so.
    fn manual_clock() -> (Clock, Arc<AtomicU64>) {
        let start = Instant::now();
        let elapsed_ms = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&elapsed_ms);
        let clock: Clock =
            Arc::new(move || start + Duration::from_millis(shared.load(Ordering::SeqCst)));
        (clock, elapsed_ms)
    }

    fn user_of(tenant: TenantId) -> Principal {
        Principal {
            tenant_id: tenant,
            user_id: UserId::from_uuid(uuid::Uuid::now_v7()),
        }
    }

    const SAVED: DashboardKind = DashboardKind::Saved;

    #[test]
    fn a_user_and_a_tenant_each_have_a_minutes_budget() {
        let (clock, elapsed_ms) = manual_clock();
        let budget = VersionBudget::new(VERSIONS_PER_USER, VERSIONS_PER_TENANT, clock);
        let tenant = TenantId::generate();
        let alice = user_of(tenant);
        for request in 0..u64::try_from(VERSIONS_PER_USER).expect("fits") {
            // One a second.
            elapsed_ms.store(1_000 * request, Ordering::SeqCst);
            budget
                .admit(alice, SAVED)
                .expect("within the user's budget");
        }
        // 9.5 s after her first: it leaves the minute 50.5 s later.
        elapsed_ms.store(9_500, Ordering::SeqCst);
        assert_eq!(
            budget.admit(alice, SAVED),
            Err(Limited {
                retry_after: Duration::from_secs(51)
            })
        );
        // Others of the tenant use the rest of the tenant's budget.
        let others: Vec<Principal> = (0..2).map(|_| user_of(tenant)).collect();
        for other in &others {
            for _ in 0..VERSIONS_PER_USER {
                budget.admit(*other, SAVED).expect("within both budgets");
            }
        }
        let late = user_of(tenant);
        assert_eq!(
            budget.admit(late, SAVED),
            Err(Limited {
                retry_after: Duration::from_secs(51)
            }),
            "the tenant's first request leaves the minute first"
        );
        // The tenant's drafts have a budget of their own.
        budget
            .admit(late, DashboardKind::Draft)
            .expect("a version of a draft");
        // Another tenant is not held back.
        budget
            .admit(user_of(TenantId::generate()), SAVED)
            .expect("another tenant");
        // Refused requests were not counted: when Alice's first leaves the minute, one more of
        // the tenant fits, hers or another's.
        elapsed_ms.store(60_000, Ordering::SeqCst);
        budget
            .admit(late, SAVED)
            .expect("Alice's first left the minute");
        assert!(
            budget.admit(alice, SAVED).is_err(),
            "the tenant is full again"
        );
        elapsed_ms.store(61_000, Ordering::SeqCst);
        budget
            .admit(alice, SAVED)
            .expect("her second left the minute");
        assert_eq!(
            budget.admit(alice, SAVED),
            Err(Limited {
                retry_after: Duration::from_secs(1)
            }),
            "her third leaves it in a second"
        );
    }

    #[test]
    fn the_wait_is_until_both_budgets_have_room() {
        let (clock, elapsed_ms) = manual_clock();
        let budget = VersionBudget::new(2, 3, clock);
        let tenant = TenantId::generate();
        let (alice, bob) = (user_of(tenant), user_of(tenant));
        budget.admit(bob, SAVED).expect("bob's first");
        elapsed_ms.store(10_000, Ordering::SeqCst);
        budget.admit(alice, SAVED).expect("alice's first");
        elapsed_ms.store(20_000, Ordering::SeqCst);
        budget.admit(alice, SAVED).expect("alice's second");
        // Alice's budget has room at 70 s, the tenant's at 60 s: she waits for her own.
        assert_eq!(
            budget.admit(alice, SAVED),
            Err(Limited {
                retry_after: Duration::from_secs(50)
            })
        );
        // Bob has room of his own, but the tenant has none until 60 s.
        assert_eq!(
            budget.admit(bob, SAVED),
            Err(Limited {
                retry_after: Duration::from_secs(40)
            })
        );
    }

    #[test]
    fn idle_users_and_tenants_are_dropped_from_large_tables() {
        let (clock, elapsed_ms) = manual_clock();
        let budget = VersionBudget::new(1, 1, clock);
        for _ in 0..=PRUNE_AT {
            budget
                .admit(user_of(TenantId::generate()), SAVED)
                .expect("a new tenant");
        }
        elapsed_ms.store(60_000, Ordering::SeqCst);
        let recent = user_of(TenantId::generate());
        budget.admit(recent, SAVED).expect("a new tenant");
        let started = budget.started.lock().expect("not poisoned");
        assert_eq!(started.users.len(), 1, "only the recent user is kept");
        assert_eq!(started.tenants.len(), 1, "only the recent tenant is kept");
    }
}
