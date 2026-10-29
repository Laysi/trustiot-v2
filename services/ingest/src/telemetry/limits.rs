//! Fairness between gateways (services/ingest/README.md, "寫入流程"): one gateway, for example
//! a compromised one that floods its topic with replays of committed batches or with garbage,
//! must not take the replica's batch slots, its database connections or its log away from the
//! others.
//!
//! - Each gateway has a token bucket of batches ([`LimitSettings::batches_per_minute`] refill,
//!   [`LimitSettings::burst`] capacity) and at most [`LimitSettings::max_in_progress`] batches
//!   being handled at once. A message over either limit is answered `retry` with
//!   [`super::THROTTLED_RETRY_AFTER_MS`] before anything else is done with it: no binding
//!   lookup, no lock, no ClickHouse statement, and it holds no batch slot.
//! - Each gateway has a log budget too ([`LOG_BURST`] lines, one more every
//!   [`LOG_REFILL`]): the telemetry path's per-message lines about a gateway are dropped once it
//!   is spent, and the next line logged carries how many were (`suppressed_logs`). The metrics
//!   count every message regardless.
//!
//! The state lives in the replica's memory, keyed by the gateway of the topic (the broker's ACL
//! lets a gateway publish only under its own ID, so a gateway cannot spend another's budget).
//! Replicas share a gateway's messages, so a gateway's whole allowance is the replicas' sum.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use protocol::GatewayId;

/// Lines a gateway may log in a row before its log budget is spent.
pub const LOG_BURST: u32 = 5;

/// How long the log budget takes to grow by one line.
pub const LOG_REFILL: Duration = Duration::from_secs(10);

/// The gateways tracked before idle ones are dropped; the bound grows with the gateways that
/// are active.
const PRUNE_AT: usize = 1024;

/// A gateway's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitSettings {
    /// How fast a gateway's bucket refills, in batches per minute.
    pub batches_per_minute: u32,
    /// The bucket's capacity: the batches a gateway may send in a row after a quiet period.
    pub burst: u32,
    /// The batches of one gateway handled at once.
    pub max_in_progress: usize,
}

/// Why a message was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Throttled {
    /// The gateway's bucket is empty.
    Rate,
    /// The gateway has [`LimitSettings::max_in_progress`] batches in hand already.
    InProgress,
}

impl Throttled {
    /// The reason's name in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rate => "rate",
            Self::InProgress => "in_progress",
        }
    }
}

/// A message admitted for its gateway: it counts as in progress until dropped.
#[derive(Debug)]
pub struct InProgress {
    limits: Arc<Inner>,
    gateway: GatewayId,
}

impl Drop for InProgress {
    fn drop(&mut self) {
        let mut state = self.limits.lock();
        if let Some(gateway) = state.gateways.get_mut(&self.gateway) {
            gateway.in_progress = gateway.in_progress.saturating_sub(1);
        }
    }
}

/// Every gateway's buckets. Cheap to clone.
#[derive(Clone, Debug)]
pub struct GatewayLimits(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    settings: LimitSettings,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    gateways: HashMap<GatewayId, Gateway>,
    prune_at: usize,
}

#[derive(Debug)]
struct Gateway {
    /// Batches the gateway may still send, as of `refilled`.
    tokens: f64,
    refilled: Instant,
    in_progress: usize,
    /// Lines the gateway may still log, as of `log_refilled`.
    log_tokens: f64,
    log_refilled: Instant,
    suppressed_logs: u64,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding the lock; a poisoned one still holds consistent numbers.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `tokens` after `elapsed` at `per_second`, at most `capacity`.
fn refilled(tokens: f64, elapsed: Duration, per_second: f64, capacity: f64) -> f64 {
    (tokens + elapsed.as_secs_f64() * per_second).min(capacity)
}

impl GatewayLimits {
    #[must_use]
    pub fn new(settings: LimitSettings) -> Self {
        Self(Arc::new(Inner {
            settings: LimitSettings {
                batches_per_minute: settings.batches_per_minute.max(1),
                burst: settings.burst.max(1),
                max_in_progress: settings.max_in_progress.max(1),
            },
            state: Mutex::new(State {
                gateways: HashMap::new(),
                prune_at: PRUNE_AT,
            }),
        }))
    }

    /// The settings in force.
    #[must_use]
    pub fn settings(&self) -> LimitSettings {
        self.0.settings
    }

    fn rate_per_second(&self) -> f64 {
        f64::from(self.0.settings.batches_per_minute) / 60.0
    }

    fn log_per_second() -> f64 {
        1.0 / LOG_REFILL.as_secs_f64()
    }

    /// The gateway's state brought up to `now`, created full on first sight.
    fn gateway<'a>(
        &self,
        state: &'a mut State,
        gateway: &GatewayId,
        now: Instant,
    ) -> &'a mut Gateway {
        if !state.gateways.contains_key(gateway) && state.gateways.len() >= state.prune_at {
            self.prune(state, now);
        }
        let burst = f64::from(self.0.settings.burst);
        let entry = state.gateways.entry(gateway.clone()).or_insert(Gateway {
            tokens: burst,
            refilled: now,
            in_progress: 0,
            log_tokens: f64::from(LOG_BURST),
            log_refilled: now,
            suppressed_logs: 0,
        });
        let rate = self.rate_per_second();
        entry.tokens = refilled(
            entry.tokens,
            now.saturating_duration_since(entry.refilled),
            rate,
            burst,
        );
        entry.refilled = now;
        entry.log_tokens = refilled(
            entry.log_tokens,
            now.saturating_duration_since(entry.log_refilled),
            Self::log_per_second(),
            f64::from(LOG_BURST),
        );
        entry.log_refilled = now;
        entry
    }

    /// Drops the gateways that are idle: nothing in progress and both buckets full by `now`. The
    /// count of lines suppressed for such a gateway goes with it (the metrics still counted every
    /// message), so a gateway that spent its log budget and went quiet is not kept for good.
    fn prune(&self, state: &mut State, now: Instant) {
        let (rate, burst) = (self.rate_per_second(), f64::from(self.0.settings.burst));
        state.gateways.retain(|_, gateway| {
            let tokens = refilled(
                gateway.tokens,
                now.saturating_duration_since(gateway.refilled),
                rate,
                burst,
            );
            let log_tokens = refilled(
                gateway.log_tokens,
                now.saturating_duration_since(gateway.log_refilled),
                Self::log_per_second(),
                f64::from(LOG_BURST),
            );
            gateway.in_progress > 0 || tokens < burst || log_tokens < f64::from(LOG_BURST)
        });
        state.prune_at = PRUNE_AT.max(state.gateways.len().saturating_mul(2));
    }

    /// Admits a message of `gateway` arriving at `now`, or says why not. An admitted message
    /// takes a token and counts as in progress until the returned guard is dropped; a refused one
    /// takes nothing.
    ///
    /// # Errors
    ///
    /// [`Throttled`] when the gateway has its most batches in progress or an empty bucket.
    pub fn admit(&self, gateway: &GatewayId, now: Instant) -> Result<InProgress, Throttled> {
        let max_in_progress = self.0.settings.max_in_progress;
        let mut state = self.0.lock();
        let entry = self.gateway(&mut state, gateway, now);
        if entry.in_progress >= max_in_progress {
            return Err(Throttled::InProgress);
        }
        if entry.tokens < 1.0 {
            return Err(Throttled::Rate);
        }
        entry.tokens -= 1.0;
        entry.in_progress += 1;
        drop(state);
        Ok(InProgress {
            limits: Arc::clone(&self.0),
            gateway: gateway.clone(),
        })
    }

    /// Whether a line about `gateway` may be logged at `now`: `Some` with the lines suppressed
    /// since the last one logged (and spends one from the budget), `None` when the budget is spent
    /// (and counts the line as suppressed).
    #[must_use]
    pub fn log_allowed(&self, gateway: &GatewayId, now: Instant) -> Option<u64> {
        let mut state = self.0.lock();
        let entry = self.gateway(&mut state, gateway, now);
        if entry.log_tokens < 1.0 {
            entry.suppressed_logs = entry.suppressed_logs.saturating_add(1);
            return None;
        }
        entry.log_tokens -= 1.0;
        Some(std::mem::take(&mut entry.suppressed_logs))
    }

    /// How many gateways are tracked.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.0.lock().gateways.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway(name: &str) -> GatewayId {
        GatewayId::new(name).unwrap()
    }

    fn limits(batches_per_minute: u32, burst: u32, max_in_progress: usize) -> GatewayLimits {
        GatewayLimits::new(LimitSettings {
            batches_per_minute,
            burst,
            max_in_progress,
        })
    }

    #[test]
    fn a_gateway_gets_its_burst_then_its_rate() {
        let limits = limits(60, 3, 100);
        let (a, b) = (gateway("gw-a"), gateway("gw-b"));
        let start = Instant::now();
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(limits.admit(&a, start).expect("within the burst"));
        }
        assert_eq!(limits.admit(&a, start).unwrap_err(), Throttled::Rate);
        // Another gateway has its own bucket.
        held.push(limits.admit(&b, start).expect("b's own bucket"));
        // One a second at 60 a minute.
        let later = start + Duration::from_millis(1_000);
        held.push(limits.admit(&a, later).expect("refilled by one"));
        assert_eq!(limits.admit(&a, later).unwrap_err(), Throttled::Rate);
        // Never above the burst, however long the gateway was quiet.
        let much_later = start + Duration::from_secs(3_600);
        for _ in 0..3 {
            held.push(limits.admit(&a, much_later).expect("full again"));
        }
        assert_eq!(limits.admit(&a, much_later).unwrap_err(), Throttled::Rate);
    }

    #[test]
    fn batches_in_progress_are_capped_and_a_refusal_costs_nothing() {
        let limits = limits(60, 10, 2);
        let a = gateway("gw-a");
        let now = Instant::now();
        let first = limits.admit(&a, now).unwrap();
        let second = limits.admit(&a, now).unwrap();
        for _ in 0..20 {
            assert_eq!(limits.admit(&a, now).unwrap_err(), Throttled::InProgress);
        }
        drop(first);
        let third = limits.admit(&a, now).expect("a slot is free again");
        drop((second, third));
        // Two refused admissions took no token: 10 - 3 = 7 are left.
        for _ in 0..7 {
            drop(limits.admit(&a, now).expect("tokens left"));
        }
        assert_eq!(limits.admit(&a, now).unwrap_err(), Throttled::Rate);
    }

    #[test]
    fn logs_are_limited_per_gateway_and_report_what_was_suppressed() {
        let limits = limits(60, 10, 2);
        let (a, b) = (gateway("gw-a"), gateway("gw-b"));
        let start = Instant::now();
        for _ in 0..LOG_BURST {
            assert_eq!(limits.log_allowed(&a, start), Some(0));
        }
        for _ in 0..7 {
            assert_eq!(limits.log_allowed(&a, start), None);
        }
        assert_eq!(limits.log_allowed(&b, start), Some(0), "b's own budget");
        let later = start + LOG_REFILL;
        assert_eq!(limits.log_allowed(&a, later), Some(7));
        assert_eq!(limits.log_allowed(&a, later), None);
    }

    #[test]
    fn idle_gateways_are_dropped_and_busy_ones_kept() {
        let limits = limits(600, 10, 4);
        let start = Instant::now();
        let busy = gateway("gw-busy");
        let held = limits.admit(&busy, start).unwrap();
        // A gateway that spent its log budget, had lines suppressed, and went quiet.
        let quiet = gateway("gw-quiet");
        while limits.log_allowed(&quiet, start).is_some() {}
        assert_eq!(limits.log_allowed(&quiet, start), None);
        for index in 2..PRUNE_AT {
            drop(
                limits
                    .admit(&gateway(&format!("gw-{index}")), start)
                    .unwrap(),
            );
        }
        assert_eq!(limits.tracked(), PRUNE_AT);
        // A minute later every bucket is full again; the next new gateway prunes the idle ones,
        // the quiet one with suppressed lines included.
        let later = start + Duration::from_secs(60);
        drop(limits.admit(&gateway("gw-new"), later).unwrap());
        assert_eq!(limits.tracked(), 2, "the busy gateway and the new one");
        assert_eq!(limits.log_allowed(&quiet, later), Some(0), "tracked anew");
        drop(held);
        assert!(limits.admit(&busy, later).is_ok());
    }
}
