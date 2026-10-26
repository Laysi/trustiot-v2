//! Limits shared by the dashboards' work in one replica, besides the dataset runs' caps
//! ([`super::caps`]):
//!
//! - [`KeyedSlots`]: at most a few at a time per key (a tenant), with a bounded wait for a
//!   slot. The `dashboard` tool's writes take one of their tenant's slot before they run
//!   datasets, so one tenant has one write in its run and check at a time.
//!
//! Each counts in this replica only: N replicas allow N times as much.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// When a table has more keys than this, the idle ones are dropped.
const PRUNE_AT: usize = 1024;

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

#[cfg(test)]
mod tests {
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
}
