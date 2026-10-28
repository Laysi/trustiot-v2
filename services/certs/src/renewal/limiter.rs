//! The renewal endpoint's rate limits, in memory per replica: one per hardware ID, counting the
//! requests of both steps, and one for all hardware IDs together for each step.
//!
//! Each limit is a generic cell rate algorithm (the token bucket written as a "theoretical
//! arrival time"): a limit of `burst` requests and one more every `interval` lets a request
//! through while its theoretical arrival time is at most `interval × (burst − 1)` ahead of now,
//! and moves it `interval` further. A request is let through only when its hardware ID's limit
//! and its step's global limit both allow it, and only then are both moved, so a refused request
//! changes nothing: in particular a flood of hardware IDs refused by a global limit adds no
//! entries. An entry whose time has passed is the same as no entry and is pruned, so the map
//! holds at most the hardware IDs that sent a request in the last
//! `gateway_interval × gateway_burst`, which the global limits bound.
//!
//! Each step has a global limit of its own (both `global_burst` and `global_interval`), so a
//! flood of challenge requests for made-up hardware IDs, which anyone can send, does not use up
//! the turns of the redemptions: a gateway that holds a challenge still redeems it. The flood
//! still takes the challenges of every other gateway on the replica; README.md, "換發", "規則與
//! 取捨", says what is left and where it is stopped.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::config::RenewalLimits;

/// Entries the map may hold before the first pruning; afterwards twice what a pruning kept.
const PRUNE_AT: usize = 1024;

/// The step a request is, which picks its global limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Asking for a challenge.
    Challenge,
    /// Redeeming a challenge.
    Renew,
}

/// One limit: `burst` requests at once, one more every `interval`.
#[derive(Clone, Copy, Debug)]
struct Rate {
    interval: Duration,
    /// `interval × (burst − 1)`: how far ahead of now the arrival time may be.
    tolerance: Duration,
}

impl Rate {
    fn new(burst: u32, interval: Duration) -> Self {
        Self {
            interval,
            tolerance: interval.saturating_mul(burst.saturating_sub(1)),
        }
    }

    /// How long a request at `now` must wait: zero when the limit lets it through.
    fn wait(self, arrival: Option<Instant>, now: Instant) -> Duration {
        let arrival = arrival.map_or(now, |at| at.max(now));
        arrival
            .saturating_duration_since(now)
            .saturating_sub(self.tolerance)
    }

    /// The arrival time after a request at `now` went through.
    fn next(self, arrival: Option<Instant>, now: Instant) -> Instant {
        arrival.map_or(now, |at| at.max(now)) + self.interval
    }
}

#[derive(Debug)]
struct State {
    /// The global limit of the challenge step.
    challenges: Option<Instant>,
    /// The global limit of the renew step.
    renewals: Option<Instant>,
    gateways: HashMap<String, Instant>,
    prune_at: usize,
}

impl State {
    fn global(&mut self, step: Step) -> &mut Option<Instant> {
        match step {
            Step::Challenge => &mut self.challenges,
            Step::Renew => &mut self.renewals,
        }
    }
}

/// The limits of one replica. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub(crate) struct Limiter {
    gateway: Rate,
    global: Rate,
    state: Mutex<State>,
}

impl Limiter {
    pub(crate) fn new(limits: &RenewalLimits) -> Self {
        Self {
            gateway: Rate::new(limits.gateway_burst, limits.gateway_interval),
            global: Rate::new(limits.global_burst, limits.global_interval),
            state: Mutex::new(State {
                challenges: None,
                renewals: None,
                gateways: HashMap::new(),
                prune_at: PRUNE_AT,
            }),
        }
    }

    /// Lets a `step` request of `hardware_id` at `now` through, or returns how long to wait
    /// before the next one would be: the longer of the two limits' waits.
    pub(crate) fn check(
        &self,
        step: Step,
        hardware_id: &str,
        now: Instant,
    ) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let own = state.gateways.get(hardware_id).copied();
        let global = *state.global(step);
        let wait = self
            .global
            .wait(global, now)
            .max(self.gateway.wait(own, now));
        if !wait.is_zero() {
            return Err(wait);
        }
        *state.global(step) = Some(self.global.next(global, now));
        let next = self.gateway.next(own, now);
        state.gateways.insert(hardware_id.to_owned(), next);
        if state.gateways.len() > state.prune_at {
            state.gateways.retain(|_, arrival| *arrival > now);
            state.prune_at = PRUNE_AT.max(state.gateways.len().saturating_mul(2));
        }
        Ok(())
    }

    #[cfg(test)]
    fn entries(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .gateways
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::Step::{Challenge, Renew};
    use super::*;

    fn limits(gateway: (u32, u64), global: (u32, u64)) -> RenewalLimits {
        RenewalLimits {
            gateway_burst: gateway.0,
            gateway_interval: Duration::from_secs(gateway.1),
            global_burst: global.0,
            global_interval: Duration::from_secs(global.1),
            ..RenewalLimits::default()
        }
    }

    #[test]
    fn a_hardware_id_gets_its_burst_then_one_per_interval_over_both_steps() {
        let limiter = Limiter::new(&limits((3, 10), (1000, 1)));
        let start = Instant::now();
        assert_eq!(limiter.check(Challenge, "tg-0001", start), Ok(()));
        assert_eq!(limiter.check(Renew, "tg-0001", start), Ok(()));
        assert_eq!(limiter.check(Challenge, "tg-0001", start), Ok(()));
        for step in [Challenge, Renew] {
            assert_eq!(
                limiter.check(step, "tg-0001", start),
                Err(Duration::from_secs(10))
            );
        }
        // Another hardware ID has its own limit.
        assert_eq!(limiter.check(Challenge, "tg-0002", start), Ok(()));
        let later = start + Duration::from_secs(4);
        assert_eq!(
            limiter.check(Renew, "tg-0001", later),
            Err(Duration::from_secs(6))
        );
        let next = start + Duration::from_secs(10);
        assert_eq!(limiter.check(Renew, "tg-0001", next), Ok(()));
        assert!(limiter.check(Challenge, "tg-0001", next).is_err());
    }

    #[test]
    fn the_global_limit_holds_across_hardware_ids_and_refusals_change_nothing() {
        let limiter = Limiter::new(&limits((100, 1), (2, 5)));
        let start = Instant::now();
        assert_eq!(limiter.check(Challenge, "tg-0001", start), Ok(()));
        assert_eq!(limiter.check(Challenge, "tg-0002", start), Ok(()));
        for n in 0..50 {
            assert_eq!(
                limiter.check(Challenge, &format!("flood-{n}"), start),
                Err(Duration::from_secs(5))
            );
        }
        assert_eq!(limiter.entries(), 2, "refused requests add no entries");
        assert_eq!(
            limiter.check(Challenge, "tg-0003", start + Duration::from_secs(5)),
            Ok(())
        );
    }

    #[test]
    fn a_flood_of_one_step_leaves_the_other_steps_limit_alone() {
        let limiter = Limiter::new(&limits((100, 1), (2, 60)));
        let start = Instant::now();
        // A gateway holds a challenge when a flood of challenge requests begins.
        assert_eq!(limiter.check(Challenge, "tg-0001", start), Ok(()));
        assert_eq!(limiter.check(Challenge, "flood-0", start), Ok(()));
        for n in 1..50 {
            assert!(
                limiter
                    .check(Challenge, &format!("flood-{n}"), start)
                    .is_err()
            );
        }
        // Its redemption has turns of its own.
        assert_eq!(limiter.check(Renew, "tg-0001", start), Ok(()));
        // And a flood of made-up redemptions leaves the challenges as they were.
        assert_eq!(limiter.check(Renew, "made-up-0", start), Ok(()));
        assert!(limiter.check(Renew, "made-up-1", start).is_err());
        let next = start + Duration::from_secs(60);
        assert_eq!(limiter.check(Challenge, "tg-0002", next), Ok(()));
        assert!(limiter.check(Challenge, "tg-0003", next).is_err());
    }

    #[test]
    fn a_refusal_by_the_hardware_id_does_not_use_up_the_global_limit() {
        let limiter = Limiter::new(&limits((1, 60), (2, 60)));
        let start = Instant::now();
        assert_eq!(limiter.check(Challenge, "tg-0001", start), Ok(()));
        for _ in 0..10 {
            assert!(limiter.check(Challenge, "tg-0001", start).is_err());
        }
        assert_eq!(limiter.check(Challenge, "tg-0002", start), Ok(()));
    }

    #[test]
    fn idle_hardware_ids_are_pruned() {
        let limiter = Limiter::new(&limits((1, 1), (1_000_000, 0)));
        let start = Instant::now();
        for n in 0..PRUNE_AT {
            assert_eq!(limiter.check(Challenge, &format!("old-{n}"), start), Ok(()));
        }
        assert_eq!(limiter.entries(), PRUNE_AT);
        // One more past the threshold, after every old entry's time has passed, prunes them all.
        let later = start + Duration::from_secs(2);
        assert_eq!(limiter.check(Renew, "new", later), Ok(()));
        assert_eq!(limiter.entries(), 1);
        // Entries still within their time are kept.
        let limiter = Limiter::new(&limits((1, 60), (1_000_000, 0)));
        for n in 0..=PRUNE_AT {
            assert_eq!(
                limiter.check(Challenge, &format!("busy-{n}"), start),
                Ok(())
            );
        }
        assert_eq!(limiter.entries(), PRUNE_AT + 1);
    }
}
