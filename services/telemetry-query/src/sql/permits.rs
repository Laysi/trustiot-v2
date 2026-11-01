//! The permits a check's thread holds: one of the process's, and one of its tenant's, so one
//! tenant cannot take every thread (Laysi/trustiot-v2#150, decided 2026-10-26).
//!
//! A check first joins its tenant ([`Permits::join`]), which refuses when the tenant already
//! has its running and waiting checks; then it waits for one of the tenant's permits, and only
//! then for one of the process's. A tenant's checks beyond its own permits wait without holding
//! a process permit, so the other tenants still get the remaining ones. Every permit and the
//! membership move into the check's thread ([`Held`]) and are given back when the work returns,
//! also when the caller stopped waiting.
//!
//! A tenant's entry exists only while it has a member, running or waiting: the last member to
//! leave removes it, so the map never holds more tenants than have checks in flight.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use contract::TenantId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};

use super::CheckError;

/// The process's and each tenant's permits for check threads.
pub struct Permits {
    /// The threads the process may run at once.
    all: Semaphore,
    /// The threads one tenant may run at once.
    per_tenant: usize,
    /// How many more of one tenant's checks may wait for its permits.
    waiting_per_tenant: usize,
    /// The tenants with a check in flight.
    tenants: Mutex<BTreeMap<TenantId, Tenant>>,
}

/// A tenant with a check in flight.
struct Tenant {
    permits: Arc<Semaphore>,
    /// Its checks that have joined and not left: waiting for a permit, or running.
    members: usize,
}

impl Permits {
    /// `all` threads for the process, `per_tenant` of them for one tenant, and up to
    /// `waiting_per_tenant` more of a tenant's checks waiting.
    ///
    /// # Panics
    ///
    /// When `per_tenant` is 0 or not below `all`: a tenant could then take every thread, or
    /// none could run. Evaluated at compile time for a `static`.
    pub const fn new(all: usize, per_tenant: usize, waiting_per_tenant: usize) -> Self {
        assert!(
            0 < per_tenant && per_tenant < all,
            "a tenant runs at least one check and leaves one for the others"
        );
        Self {
            all: Semaphore::const_new(all),
            per_tenant,
            waiting_per_tenant,
            tenants: Mutex::new(BTreeMap::new()),
        }
    }

    /// The map, also after a panic elsewhere while it was locked: no code panics while holding
    /// it, and every change to it is complete before the lock is released.
    fn tenants(&self) -> MutexGuard<'_, BTreeMap<TenantId, Tenant>> {
        self.tenants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Joins `tenant`'s checks.
    ///
    /// # Errors
    ///
    /// [`CheckError::Busy`] when the tenant already has `per_tenant + waiting_per_tenant`
    /// checks in flight.
    fn join(&'static self, tenant: TenantId) -> Result<Member, CheckError> {
        let mut tenants = self.tenants();
        let entry = tenants.entry(tenant).or_insert_with(|| Tenant {
            permits: Arc::new(Semaphore::new(self.per_tenant)),
            members: 0,
        });
        if entry.members >= self.per_tenant + self.waiting_per_tenant {
            return Err(CheckError::Busy);
        }
        entry.members += 1;
        Ok(Member {
            permits: self,
            tenant,
            semaphore: Arc::clone(&entry.permits),
        })
    }

    /// Waits for one of `tenant`'s permits and then one of the process's.
    ///
    /// # Errors
    ///
    /// [`CheckError::Busy`] as [`Self::join`], and [`CheckError::Internal`] when a semaphore is
    /// closed, which this module never does.
    pub(super) async fn acquire(&'static self, tenant: TenantId) -> Result<Held, CheckError> {
        let member = self.join(tenant)?;
        let tenant_permit = Arc::clone(&member.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| CheckError::Internal("the tenant's check permits are closed"))?;
        let all = self
            .all
            .acquire()
            .await
            .map_err(|_| CheckError::Internal("the check's permits are closed"))?;
        Ok(Held {
            _all: all,
            _tenant: tenant_permit,
            _member: member,
        })
    }

    /// The process's permits not taken now.
    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.all.available_permits()
    }

    /// The tenants with a check in flight, and how many each has (running or waiting).
    #[cfg(test)]
    pub(super) fn in_flight(&self) -> BTreeMap<TenantId, usize> {
        self.tenants()
            .iter()
            .map(|(tenant, entry)| (*tenant, entry.members))
            .collect()
    }
}

/// A check that has joined its tenant's: counted until it is dropped, whether it is waiting or
/// running.
struct Member {
    permits: &'static Permits,
    tenant: TenantId,
    /// The tenant's semaphore while the entry exists. A member keeps the entry in the map, so
    /// every member of a tenant waits on the same semaphore.
    semaphore: Arc<Semaphore>,
}

impl Drop for Member {
    fn drop(&mut self) {
        let mut tenants = self.permits.tenants();
        if let Some(entry) = tenants.get_mut(&self.tenant) {
            entry.members = entry.members.saturating_sub(1);
            if entry.members == 0 {
                tenants.remove(&self.tenant);
            }
        }
    }
}

/// The permits of a running check, given back when it is dropped. The fields drop in order:
/// the process's permit, the tenant's, then the membership, so the entry leaves the map only
/// after its last permit is back.
pub(super) struct Held {
    _all: SemaphorePermit<'static>,
    _tenant: OwnedSemaphorePermit,
    _member: Member,
}
