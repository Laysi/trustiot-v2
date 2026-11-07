//! How many tool calls of one tenant run at once against a service whose tenant accounts limit
//! their queries at once (Laysi/trustiot-v2#1122). The telemetry query service runs every query as
//! the tenant's ClickHouse account, which allows 4 at once (`max_concurrent_queries_for_user`);
//! a step of the model may ask for 8 telemetry calls, and every question of the tenant on this
//! replica shares the account. [`crate::tools::ToolRegistry::run`] takes one of the tenant's slots
//! before it starts a call's time limit and gives it back when the call ends, so the calls beyond
//! the slots wait (each call's task asks for its slot when it first runs, so not necessarily in
//! the order the model wrote them), and their wait does not count toward their own time limit
//! (the question's deadline still ends them).
//!
//! The slots are per replica and only for the tools: dashboard dataset runs have their own limit
//! (`crate::dashboards::caps`), and device pages and other replicas query the same account. The
//! telemetry query service queues a tenant's requests to the account's limit itself, so this only
//! keeps one question from filling that queue and leaves a slot of the account for the tenant's
//! other users.
//!
//! A tenant's entry exists only while one of its calls holds or waits for a slot.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use contract::TenantId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The telemetry tool calls one tenant runs at once on one replica: one fewer than the tenant
/// account's 4 queries at once (services/telemetry-query/README.md, settings profile
/// `trustiot_tenant_reader`), so a question never fills the account alone.
pub const TELEMETRY_CALLS_PER_TENANT: usize = 3;

/// Each tenant's slots for the calls to one service.
pub struct TenantSlots {
    per_tenant: usize,
    tenants: Mutex<BTreeMap<TenantId, Tenant>>,
}

struct Tenant {
    slots: Arc<Semaphore>,
    /// Calls holding or waiting for one of its slots.
    members: usize,
}

impl fmt::Debug for TenantSlots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantSlots")
            .field("per_tenant", &self.per_tenant)
            .finish_non_exhaustive()
    }
}

impl TenantSlots {
    /// `per_tenant` calls of each tenant at once.
    ///
    /// # Panics
    ///
    /// When `per_tenant` is 0: no call could run.
    #[must_use]
    pub fn new(per_tenant: usize) -> Self {
        assert!(per_tenant > 0, "a tenant runs at least one call");
        Self {
            per_tenant,
            tenants: Mutex::new(BTreeMap::new()),
        }
    }

    /// The calls of one tenant that run at once.
    #[must_use]
    pub fn per_tenant(&self) -> usize {
        self.per_tenant
    }

    /// The map, also after a panic elsewhere while it was locked: no code panics while holding
    /// it, and every change to it is complete before the lock is released.
    fn tenants(&self) -> MutexGuard<'_, BTreeMap<TenantId, Tenant>> {
        self.tenants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for one of `tenant`'s slots, in the order the waits first ran. Dropping the future while
    /// it waits leaves the queue; the slot is given back when the [`Slot`] is dropped.
    pub async fn acquire(self: &Arc<Self>, tenant: TenantId) -> Slot {
        let member = {
            let mut tenants = self.tenants();
            let entry = tenants.entry(tenant).or_insert_with(|| Tenant {
                slots: Arc::new(Semaphore::new(self.per_tenant)),
                members: 0,
            });
            entry.members += 1;
            Member {
                slots: Arc::clone(self),
                tenant,
                semaphore: Arc::clone(&entry.slots),
            }
        };
        let permit = Arc::clone(&member.semaphore).acquire_owned().await;
        Slot {
            // This module never closes a semaphore; were it closed, the call runs without a slot
            // rather than never.
            _permit: permit.ok(),
            _member: member,
        }
    }

    /// The tenants with a call holding or waiting for a slot, and how many each has.
    #[cfg(test)]
    fn in_flight(&self) -> BTreeMap<TenantId, usize> {
        self.tenants()
            .iter()
            .map(|(tenant, entry)| (*tenant, entry.members))
            .collect()
    }
}

/// A call that has joined its tenant's: counted until it is dropped, waiting or holding.
struct Member {
    slots: Arc<TenantSlots>,
    tenant: TenantId,
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

/// One of a tenant's slots, given back when it is dropped; the slot drops before the
/// membership, so the entry leaves the map only after its last slot is back.
#[must_use = "the slot is given back when it is dropped"]
pub struct Slot {
    _permit: Option<OwnedSemaphorePermit>,
    _member: Member,
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Whether `future` is still pending after one poll: a zero timeout polls the future before
    /// it looks at the clock.
    async fn is_pending<F: Future + Unpin>(future: &mut F) -> bool {
        tokio::time::timeout(Duration::ZERO, future).await.is_err()
    }

    #[tokio::test]
    async fn a_tenants_calls_beyond_its_slots_wait_in_order_and_others_do_not() {
        let slots = Arc::new(TenantSlots::new(1));
        let (tenant, other) = (TenantId::generate(), TenantId::generate());
        let first = slots.acquire(tenant).await;
        let mut second = Box::pin(slots.acquire(tenant));
        let mut third = Box::pin(slots.acquire(tenant));
        assert!(is_pending(&mut second).await);
        assert!(is_pending(&mut third).await);
        let _other = slots.acquire(other).await;
        drop(first);
        assert!(is_pending(&mut third).await, "the second asked first");
        let second = second.await;
        drop(second);
        let third = third.await;
        assert_eq!(slots.in_flight().get(&tenant), Some(&1));
        drop(third);
        assert_eq!(slots.in_flight().get(&tenant), None);
    }

    #[tokio::test]
    async fn a_cancelled_wait_leaves_the_queue() {
        let slots = Arc::new(TenantSlots::new(1));
        let tenant = TenantId::generate();
        let held = slots.acquire(tenant).await;
        let mut waiting = Box::pin(slots.acquire(tenant));
        assert!(is_pending(&mut waiting).await);
        drop(waiting);
        assert_eq!(slots.in_flight().get(&tenant), Some(&1));
        drop(held);
        assert_eq!(slots.in_flight().get(&tenant), None);
        let _again = slots.acquire(tenant).await;
    }
}
