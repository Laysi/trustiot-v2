//! The memory dataset results take in this replica, counted in bytes (Laysi/trustiot-v2#150;
//! services/assistant/README.md, 「記憶體」): one budget for all tenants, [`RESULT_MEMORY`], of
//! which one tenant holds at most [`RESULT_MEMORY_PER_TENANT`].
//!
//! What draws from it, each for as long as it holds the memory:
//!
//! - a dataset run, from when telemetry-query has answered (its status and headers are in) until
//!   the run's result is dropped: first what the run may read and parse, then only its result's
//!   JSON ([`super::runner`]). The public API's answer holds the result's reservation until the
//!   answer's body has been written or dropped. A run waiting for telemetry-query to answer
//!   holds none of it, only its tenant's slot ([`super::caps`]).
//! - the public API's answers besides: before a run of the API starts, it takes room for its
//!   answer from its tenant's [`ANSWER_MEMORY_PER_TENANT`] ([`ResultMemory::reserve_answer`]),
//!   and the answer keeps the room of its JSON's length for as long as it holds the result. So
//!   the answers whose readers have not taken them yet hold at most that much of one tenant's
//!   memory, and past it only that tenant's new runs wait (and are refused after the wait), not
//!   other tenants' runs or writes. The room is counted apart from the budget: the memory itself
//!   is the result's reservation.
//! - a write of the `dashboard` tool: the JSON of the results it uses, then their values, its
//!   check request body and the verdict it reads, until the write ends
//!   ([`crate::tools::dashboard`]).
//!
//! A reservation waits a bounded time for room and is refused after it; nothing waits without
//! a bound. Waiters are not served in order: one that fits what is free takes it, so a large
//! reservation never holds up smaller ones that fit (it can only be refused after its wait).
//!
//! The sizes reserved are upper estimates of what the structures allocate, from the limits on
//! what is read ([`read_bytes`] and the callers' functions); they are not measured. Laysi/
//! trustiot-v2#694 measures them and tunes the budget.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use contract::TenantId;
use tokio::sync::Notify;
use tokio::time::Instant;

/// The replica's budget for dataset results in memory, in bytes: two tenants' shares.
pub const RESULT_MEMORY: usize = 576 << 20;

/// The most of [`RESULT_MEMORY`] one tenant holds at a time: half, room for the largest write
/// of the `dashboard` tool and the tenant's two largest runs at once (about 272 MiB,
/// `tools::dashboard::tests::a_worst_case_write_fits_a_tenants_share`). So even the largest
/// write can finish on an idle replica, two tenants at their share fill the budget, and one
/// tenant never takes it all.
pub const RESULT_MEMORY_PER_TENANT: usize = RESULT_MEMORY / 2;

/// The most room one tenant's answers of the public API hold at a time
/// ([`ResultMemory::reserve_answer`]): a sixteenth of [`RESULT_MEMORY`], 36 MiB, room for 18
/// answers of the largest result (2 MiB) whose readers have not taken them yet.
pub const ANSWER_MEMORY_PER_TENANT: usize = RESULT_MEMORY / 16;

/// What one heap allocation costs beyond its bytes, counted for each string and vector a parse
/// may make: the allocator's rounding and bookkeeping.
pub const ALLOCATION_BYTES: usize = 32;

/// A JSON value as `serde_json` holds it.
pub const VALUE_BYTES: usize = size_of::<serde_json::Value>();

/// A vector's own fields (pointer, capacity, length) where a vector holds vectors or strings.
pub const VEC_BYTES: usize = size_of::<Vec<u8>>();

/// What reading a body of at most `limit` bytes holds while it is read
/// ([`crate::callee::read_limited`]): the declared length, since the buffer is allocated at that
/// size; nothing when the body declares more than `limit`, since it is refused unread; and twice
/// `limit` when it declares no length, since the buffer then grows by doubling up to `limit` and
/// holds the old and the new buffer while it grows.
#[must_use]
pub fn read_bytes(declared: Option<u64>, limit: usize) -> usize {
    match declared.map(usize::try_from) {
        Some(Ok(length)) if length <= limit => length,
        Some(_) => 0,
        None => limit.saturating_mul(2),
    }
}

/// The largest body that may be read: the declared length when it is within `limit`, `limit`
/// when there is none (nothing when it declares more: the body is refused unread).
#[must_use]
pub fn body_bytes(declared: Option<u64>, limit: usize) -> usize {
    match declared.map(usize::try_from) {
        Some(Ok(length)) if length <= limit => length,
        Some(_) => 0,
        None => limit,
    }
}

/// The replica's budget. Cheap to clone; the clones share it.
#[derive(Clone)]
pub struct ResultMemory {
    shared: Arc<Shared>,
}

struct Shared {
    total: usize,
    per_tenant: usize,
    answers_per_tenant: usize,
    state: Mutex<State>,
    /// Woken whenever memory is released.
    released: Notify,
}

#[derive(Default)]
struct State {
    used: usize,
    /// What each tenant holds; tenants that hold nothing are left out.
    tenants: HashMap<TenantId, usize>,
    /// The room each tenant's answers hold; tenants that hold none are left out.
    answers: HashMap<TenantId, usize>,
}

/// Memory reserved for a tenant, released when this is dropped.
pub struct Reservation {
    shared: Arc<Shared>,
    tenant: TenantId,
    bytes: usize,
}

/// Room for a tenant's answers ([`ResultMemory::reserve_answer`]), given back when this is
/// dropped.
pub struct AnswerRoom {
    shared: Arc<Shared>,
    tenant: TenantId,
    bytes: usize,
}

/// What a reservation counts against.
#[derive(Clone, Copy)]
enum Pool {
    /// The budget and the tenant's share.
    Memory,
    /// The tenant's room for answers.
    Answers,
}

impl Default for ResultMemory {
    /// The service's budget: [`RESULT_MEMORY`], [`RESULT_MEMORY_PER_TENANT`],
    /// [`ANSWER_MEMORY_PER_TENANT`].
    fn default() -> Self {
        Self::with_answers(
            RESULT_MEMORY,
            RESULT_MEMORY_PER_TENANT,
            ANSWER_MEMORY_PER_TENANT,
        )
    }
}

impl fmt::Debug for ResultMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultMemory")
            .field("total", &self.shared.total)
            .field("per_tenant", &self.shared.per_tenant)
            .field("answers_per_tenant", &self.shared.answers_per_tenant)
            .field("used", &self.used())
            .finish()
    }
}

impl fmt::Debug for Reservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reservation")
            .field("tenant", &self.tenant)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl fmt::Debug for AnswerRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnswerRoom")
            .field("tenant", &self.tenant)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl ResultMemory {
    /// A budget of `total` bytes, of which one tenant holds at most `per_tenant` (at most
    /// `total`), and whose answers are limited by that share only: tests use small ones.
    #[must_use]
    pub fn new(total: usize, per_tenant: usize) -> Self {
        Self::with_answers(total, per_tenant, per_tenant)
    }

    /// [`ResultMemory::new`], where one tenant's answers hold at most `answers_per_tenant` of
    /// room (at most the share).
    #[must_use]
    pub fn with_answers(total: usize, per_tenant: usize, answers_per_tenant: usize) -> Self {
        let per_tenant = per_tenant.min(total);
        Self {
            shared: Arc::new(Shared {
                total,
                per_tenant,
                answers_per_tenant: answers_per_tenant.min(per_tenant),
                state: Mutex::new(State::default()),
                released: Notify::new(),
            }),
        }
    }

    /// The whole budget.
    #[must_use]
    pub fn total(&self) -> usize {
        self.shared.total
    }

    /// The most one tenant holds.
    #[must_use]
    pub fn per_tenant(&self) -> usize {
        self.shared.per_tenant
    }

    /// The most room one tenant's answers hold.
    #[must_use]
    pub fn answers_per_tenant(&self) -> usize {
        self.shared.answers_per_tenant
    }

    /// What all tenants hold now.
    #[must_use]
    pub fn used(&self) -> usize {
        self.shared.lock().used
    }

    /// What `tenant` holds now.
    #[must_use]
    pub fn held_by(&self, tenant: TenantId) -> usize {
        self.shared
            .lock()
            .tenants
            .get(&tenant)
            .copied()
            .unwrap_or(0)
    }

    /// The room `tenant`'s answers hold now.
    #[must_use]
    pub fn answers_held_by(&self, tenant: TenantId) -> usize {
        self.shared
            .lock()
            .answers
            .get(&tenant)
            .copied()
            .unwrap_or(0)
    }

    /// Reserves `bytes` for `tenant`, waiting up to `wait` for them to fit both the budget and
    /// the tenant's share; `None` after the wait, at once when `bytes` can never fit the share.
    pub async fn reserve(
        &self,
        tenant: TenantId,
        bytes: usize,
        wait: Duration,
    ) -> Option<Reservation> {
        self.take(Pool::Memory, tenant, bytes, wait)
            .await
            .then(|| Reservation {
                shared: Arc::clone(&self.shared),
                tenant,
                bytes,
            })
    }

    /// Takes `bytes` of room for an answer of `tenant`'s, waiting up to `wait` for them to fit
    /// the room its answers hold ([`ResultMemory::answers_per_tenant`]); `None` after the wait,
    /// at once when `bytes` can never fit. Only the tenant's own answers give room back, so
    /// other tenants never wait here. The room is not memory: the answer's memory is its
    /// result's [`Reservation`].
    pub async fn reserve_answer(
        &self,
        tenant: TenantId,
        bytes: usize,
        wait: Duration,
    ) -> Option<AnswerRoom> {
        self.take(Pool::Answers, tenant, bytes, wait)
            .await
            .then(|| AnswerRoom {
                shared: Arc::clone(&self.shared),
                tenant,
                bytes,
            })
    }

    /// Takes `bytes` of `pool` for `tenant` within `wait`: whether it did.
    async fn take(&self, pool: Pool, tenant: TenantId, bytes: usize, wait: Duration) -> bool {
        let most = match pool {
            Pool::Memory => self.shared.per_tenant,
            Pool::Answers => self.shared.answers_per_tenant,
        };
        if bytes > most {
            return false;
        }
        let deadline = Instant::now() + wait;
        loop {
            // Registered before the attempt, so a release between the attempt and the wait
            // still wakes this one.
            let released = self.shared.released.notified();
            let mut released = std::pin::pin!(released);
            released.as_mut().enable();
            if self.shared.try_take(pool, tenant, bytes) {
                return true;
            }
            if tokio::time::timeout_at(deadline, released).await.is_err() {
                return false;
            }
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes `bytes` of `pool` for `tenant` when they fit: the budget and the tenant's share,
    /// or the tenant's room for answers.
    fn try_take(&self, pool: Pool, tenant: TenantId, bytes: usize) -> bool {
        let mut state = self.lock();
        match pool {
            Pool::Memory => {
                let held = state.tenants.get(&tenant).copied().unwrap_or(0);
                if state.used.saturating_add(bytes) > self.total
                    || held.saturating_add(bytes) > self.per_tenant
                {
                    return false;
                }
                state.used += bytes;
                if bytes > 0 {
                    state.tenants.insert(tenant, held + bytes);
                }
            }
            Pool::Answers => {
                let held = state.answers.get(&tenant).copied().unwrap_or(0);
                if held.saturating_add(bytes) > self.answers_per_tenant {
                    return false;
                }
                if bytes > 0 {
                    state.answers.insert(tenant, held + bytes);
                }
            }
        }
        true
    }

    /// Gives back `bytes` of `tenant`'s in `pool` and wakes the waiters.
    fn release(&self, pool: Pool, tenant: TenantId, bytes: usize) {
        if bytes == 0 {
            return;
        }
        {
            let mut state = self.lock();
            let held_by = match pool {
                Pool::Memory => {
                    state.used = state.used.saturating_sub(bytes);
                    &mut state.tenants
                }
                Pool::Answers => &mut state.answers,
            };
            if let Some(held) = held_by.get_mut(&tenant) {
                *held = held.saturating_sub(bytes);
                if *held == 0 {
                    held_by.remove(&tenant);
                }
            }
        }
        self.released.notify_waiters();
    }
}

impl Reservation {
    /// The bytes reserved.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Keeps only `bytes` of the reservation (nothing changes when it holds less) and releases
    /// the rest at once.
    pub fn shrink_to(&mut self, bytes: usize) {
        if bytes < self.bytes {
            let released = self.bytes - bytes;
            self.bytes = bytes;
            self.shared.release(Pool::Memory, self.tenant, released);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.shared.release(Pool::Memory, self.tenant, self.bytes);
    }
}

impl AnswerRoom {
    /// The bytes of room taken.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Keeps only `bytes` of the room (nothing changes when it holds less) and gives the rest
    /// back at once.
    pub fn shrink_to(&mut self, bytes: usize) {
        if bytes < self.bytes {
            let released = self.bytes - bytes;
            self.bytes = bytes;
            self.shared.release(Pool::Answers, self.tenant, released);
        }
    }
}

impl Drop for AnswerRoom {
    fn drop(&mut self) {
        self.shared.release(Pool::Answers, self.tenant, self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::from_uuid(uuid::Uuid::now_v7())
    }

    #[test]
    fn a_read_holds_the_declared_length_or_twice_the_limit() {
        assert_eq!(read_bytes(Some(1000), 4096), 1000);
        assert_eq!(read_bytes(Some(4096), 4096), 4096);
        assert_eq!(read_bytes(Some(4097), 4096), 0, "refused unread");
        assert_eq!(read_bytes(None, 4096), 8192);
        assert_eq!(body_bytes(Some(1000), 4096), 1000);
        assert_eq!(body_bytes(Some(4097), 4096), 0);
        assert_eq!(body_bytes(None, 4096), 4096);
    }

    #[tokio::test]
    async fn one_tenant_holds_at_most_its_share_and_others_use_the_rest() {
        let memory = ResultMemory::new(100, 60);
        let (busy, other) = (tenant(), tenant());
        let first = memory
            .reserve(busy, 40, Duration::ZERO)
            .await
            .expect("within the share");
        let _second = memory
            .reserve(busy, 20, Duration::ZERO)
            .await
            .expect("up to the share");
        assert!(
            memory.reserve(busy, 1, Duration::ZERO).await.is_none(),
            "past the share, with room left in the budget"
        );
        assert!(
            memory
                .reserve(busy, 61, Duration::from_secs(3600))
                .await
                .is_none(),
            "more than a share never fits: refused without waiting"
        );
        assert_eq!(memory.held_by(busy), 60);
        let _others = memory
            .reserve(other, 40, Duration::ZERO)
            .await
            .expect("another tenant takes the rest");
        assert!(
            memory.reserve(other, 1, Duration::ZERO).await.is_none(),
            "the budget is full"
        );
        assert_eq!(memory.used(), 100);
        drop(first);
        assert_eq!(memory.used(), 60);
        assert_eq!(memory.held_by(busy), 20);
        let _again = memory
            .reserve(other, 20, Duration::ZERO)
            .await
            .expect("room again");
    }

    #[tokio::test]
    async fn a_waiting_reservation_gets_what_is_released_and_shrinking_releases_the_rest() {
        let memory = ResultMemory::new(100, 100);
        let (first_tenant, second_tenant) = (tenant(), tenant());
        let mut held = memory
            .reserve(first_tenant, 100, Duration::ZERO)
            .await
            .expect("the whole budget");
        let waiting = tokio::spawn({
            let memory = memory.clone();
            async move {
                memory
                    .reserve(second_tenant, 30, Duration::from_secs(3600))
                    .await
                    .map(|reservation| reservation.bytes())
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "nothing was released yet");
        held.shrink_to(70);
        assert_eq!(held.bytes(), 70);
        assert_eq!(
            waiting.await.expect("the waiting reservation"),
            Some(30),
            "it got what the shrink released"
        );
        held.shrink_to(80);
        assert_eq!(held.bytes(), 70, "shrinking never grows a reservation");
        drop(held);
        assert_eq!(memory.used(), 0, "every reservation was released");
        assert_eq!(memory.held_by(first_tenant), 0);
    }

    #[tokio::test]
    async fn a_tenants_answers_hold_at_most_their_room_and_other_tenants_are_not_held_up() {
        let memory = ResultMemory::with_answers(1000, 500, 100);
        assert_eq!(memory.answers_per_tenant(), 100);
        let (busy, other) = (tenant(), tenant());
        let mut first = memory
            .reserve_answer(busy, 60, Duration::ZERO)
            .await
            .expect("room for an answer");
        let _second = memory
            .reserve_answer(busy, 40, Duration::ZERO)
            .await
            .expect("up to the room");
        assert_eq!(memory.answers_held_by(busy), 100);
        assert_eq!(memory.used(), 0, "room is not memory");
        assert!(
            memory
                .reserve_answer(busy, 1, Duration::from_millis(1))
                .await
                .is_none(),
            "the tenant's answers hold all their room: its next one waits, then is refused"
        );
        assert!(
            memory
                .reserve_answer(busy, 101, Duration::from_secs(3600))
                .await
                .is_none(),
            "more than the room never fits: refused without waiting"
        );
        let _others = memory
            .reserve_answer(other, 100, Duration::ZERO)
            .await
            .expect("another tenant's answers have room of their own");
        let _memory = memory
            .reserve(busy, 500, Duration::ZERO)
            .await
            .expect("the tenant's share of memory is apart from its answers' room");
        let _others_memory = memory
            .reserve(other, 500, Duration::ZERO)
            .await
            .expect("and so is another tenant's");

        // A waiting answer gets what an answer of its tenant gives back.
        let waiting = tokio::spawn({
            let memory = memory.clone();
            async move {
                memory
                    .reserve_answer(busy, 30, Duration::from_secs(3600))
                    .await
                    .map(|room| room.bytes())
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "no room was given back yet");
        first.shrink_to(30);
        assert_eq!(
            waiting.await.expect("the waiting answer"),
            Some(30),
            "it got the room the shrink gave back"
        );
        // The waiter's room went with its task; only the second answer's is left.
        drop(first);
        assert_eq!(memory.answers_held_by(busy), 40);
    }

    #[tokio::test]
    async fn a_reservation_that_does_not_fit_in_its_wait_is_refused_and_holds_nothing() {
        let memory = ResultMemory::new(100, 100);
        let _held = memory
            .reserve(tenant(), 90, Duration::ZERO)
            .await
            .expect("room");
        let waiter = tenant();
        assert!(
            memory
                .reserve(waiter, 20, Duration::from_millis(1))
                .await
                .is_none()
        );
        assert_eq!(memory.held_by(waiter), 0);
        assert_eq!(memory.used(), 90);
    }
}
