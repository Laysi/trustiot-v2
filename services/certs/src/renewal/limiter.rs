//! The renewal endpoint's rate limits, in memory per replica: for all hardware IDs together one
//! for each step, checked before anything in the request is trusted, and one per hardware ID,
//! checked only once a redemption's signature holds for the hardware ID's registered key
//! (Laysi/trustiot-v2#801).
//!
//! Each limit is a generic cell rate algorithm (the token bucket written as a "theoretical
//! arrival time"): a limit of `burst` requests and one more every `interval` lets a request
//! through while its theoretical arrival time is at most `interval × (burst − 1)` ahead of now,
//! and moves it `interval` further. A refused request changes nothing.
//!
//! Why the hardware ID's limit waits for the signature: a hardware ID is not a secret (it is
//! printed on the device, and it is the certificate's CN and the MQTT user), so a limit per
//! hardware ID that unauthenticated requests count against is one anyone can use up by naming the
//! ID, and the real gateway then gets 429 for as long as someone sends one request per interval,
//! far below any per-source limit at the ingress. Only the holder of the registered key can use up
//! [`Limiter::admit_verified`]; it bounds how often one key gets a certificate (the ledger and the
//! tenant events grow with it). The global limits bound everything before the signature: the
//! challenge rows, the reads of the registrations and the signature verifications.
//!
//! Each step has a global limit of its own (both `global_burst` and `global_interval`), so a
//! flood of challenge requests, which anyone can send, does not use up the turns of the
//! redemptions: a gateway that holds a challenge still redeems it. The flood still takes the
//! challenges of every other gateway on the replica; README.md, "換發", "規則與取捨", says what is
//! left and where it is stopped.
//!
//! The map of hardware IDs holds only IDs whose signature held. An entry whose time has passed is
//! the same as no entry and is pruned, so the map holds at most the registered gateways that
//! redeemed in the last `gateway_interval × gateway_burst`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
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

    /// Lets a request at `now` through and moves `arrival`, or returns how long to wait before
    /// the next one would be let through, leaving `arrival` as it was.
    fn take(self, arrival: &mut Option<Instant>, now: Instant) -> Result<(), Duration> {
        let at = arrival.map_or(now, |at| at.max(now));
        let wait = at
            .saturating_duration_since(now)
            .saturating_sub(self.tolerance);
        if !wait.is_zero() {
            return Err(wait);
        }
        *arrival = Some(at + self.interval);
        Ok(())
    }
}

#[derive(Debug)]
struct State {
    /// The global limit of the challenge step.
    challenges: Option<Instant>,
    /// The global limit of the renew step.
    renewals: Option<Instant>,
    /// The limits of the hardware IDs whose signature held.
    gateways: HashMap<String, Instant>,
    prune_at: usize,
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

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lets a `step` request at `now` through the step's global limit, or returns how long to
    /// wait before the next one would be. Nothing in the request is trusted yet, so the hardware
    /// ID it names plays no part.
    pub(crate) fn admit(&self, step: Step, now: Instant) -> Result<(), Duration> {
        let mut state = self.state();
        let arrival = match step {
            Step::Challenge => &mut state.challenges,
            Step::Renew => &mut state.renewals,
        };
        self.global.take(arrival, now)
    }

    /// Lets a redemption at `now`, whose signature held for `hardware_id`'s registered key,
    /// through that hardware ID's limit, or returns how long to wait before the next one would be.
    pub(crate) fn admit_verified(&self, hardware_id: &str, now: Instant) -> Result<(), Duration> {
        let mut state = self.state();
        let mut arrival = state.gateways.get(hardware_id).copied();
        self.gateway.take(&mut arrival, now)?;
        if let Some(arrival) = arrival {
            state.gateways.insert(hardware_id.to_owned(), arrival);
        }
        if state.gateways.len() > state.prune_at {
            state.gateways.retain(|_, arrival| *arrival > now);
            state.prune_at = PRUNE_AT.max(state.gateways.len().saturating_mul(2));
        }
        Ok(())
    }

    #[cfg(test)]
    fn entries(&self) -> usize {
        self.state().gateways.len()
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
    fn a_verified_hardware_id_gets_its_burst_then_one_per_interval() {
        let limiter = Limiter::new(&limits((2, 10), (1000, 1)));
        let start = Instant::now();
        assert_eq!(limiter.admit_verified("tg-0001", start), Ok(()));
        assert_eq!(limiter.admit_verified("tg-0001", start), Ok(()));
        assert_eq!(
            limiter.admit_verified("tg-0001", start),
            Err(Duration::from_secs(10))
        );
        // Another hardware ID has its own limit.
        assert_eq!(limiter.admit_verified("tg-0002", start), Ok(()));
        let later = start + Duration::from_secs(4);
        assert_eq!(
            limiter.admit_verified("tg-0001", later),
            Err(Duration::from_secs(6))
        );
        let next = start + Duration::from_secs(10);
        assert_eq!(limiter.admit_verified("tg-0001", next), Ok(()));
        assert!(limiter.admit_verified("tg-0001", next).is_err());
    }

    #[test]
    fn requests_before_the_signature_leave_the_hardware_ids_limit_alone() {
        // Laysi/trustiot-v2#801: however many requests went through the global limits, the
        // holder of the key still has its whole burst.
        let limiter = Limiter::new(&limits((1, 3600), (1000, 1)));
        let start = Instant::now();
        for _ in 0..100 {
            assert_eq!(limiter.admit(Challenge, start), Ok(()));
            assert_eq!(limiter.admit(Renew, start), Ok(()));
        }
        assert_eq!(
            limiter.entries(),
            0,
            "the global limits keep no hardware IDs"
        );
        assert_eq!(limiter.admit_verified("tg-0001", start), Ok(()));
        assert_eq!(
            limiter.admit_verified("tg-0001", start),
            Err(Duration::from_secs(3600))
        );
        // A refusal by the hardware ID's limit leaves the global limits alone.
        let limiter = Limiter::new(&limits((1, 3600), (1, 60)));
        assert_eq!(limiter.admit(Renew, start), Ok(()));
        assert_eq!(limiter.admit_verified("tg-0001", start), Ok(()));
        for _ in 0..10 {
            assert!(limiter.admit_verified("tg-0001", start).is_err());
        }
        assert_eq!(limiter.admit(Challenge, start), Ok(()));
        assert_eq!(
            limiter.admit(Renew, start + Duration::from_secs(60)),
            Ok(())
        );
    }

    #[test]
    fn the_global_limit_holds_and_refusals_change_nothing() {
        let limiter = Limiter::new(&limits((100, 1), (2, 5)));
        let start = Instant::now();
        assert_eq!(limiter.admit(Challenge, start), Ok(()));
        assert_eq!(limiter.admit(Challenge, start), Ok(()));
        for _ in 0..50 {
            assert_eq!(limiter.admit(Challenge, start), Err(Duration::from_secs(5)));
        }
        assert_eq!(
            limiter.admit(Challenge, start + Duration::from_secs(5)),
            Ok(())
        );
    }

    #[test]
    fn a_flood_of_one_step_leaves_the_other_steps_limit_alone() {
        let limiter = Limiter::new(&limits((100, 1), (2, 60)));
        let start = Instant::now();
        // A gateway holds a challenge when a flood of challenge requests begins.
        assert_eq!(limiter.admit(Challenge, start), Ok(()));
        assert_eq!(limiter.admit(Challenge, start), Ok(()));
        for _ in 1..50 {
            assert!(limiter.admit(Challenge, start).is_err());
        }
        // Its redemption has turns of its own.
        assert_eq!(limiter.admit(Renew, start), Ok(()));
        // And a flood of made-up redemptions leaves the challenges as they were.
        assert_eq!(limiter.admit(Renew, start), Ok(()));
        assert!(limiter.admit(Renew, start).is_err());
        let next = start + Duration::from_secs(60);
        assert_eq!(limiter.admit(Challenge, next), Ok(()));
        assert!(limiter.admit(Challenge, next).is_err());
    }

    #[test]
    fn idle_hardware_ids_are_pruned() {
        let limiter = Limiter::new(&limits((1, 1), (1_000_000, 0)));
        let start = Instant::now();
        for n in 0..PRUNE_AT {
            assert_eq!(limiter.admit_verified(&format!("old-{n}"), start), Ok(()));
        }
        assert_eq!(limiter.entries(), PRUNE_AT);
        // One more past the threshold, after every old entry's time has passed, prunes them all.
        let later = start + Duration::from_secs(2);
        assert_eq!(limiter.admit_verified("new", later), Ok(()));
        assert_eq!(limiter.entries(), 1);
        // Entries still within their time are kept.
        let limiter = Limiter::new(&limits((1, 60), (1_000_000, 0)));
        for n in 0..=PRUNE_AT {
            assert_eq!(limiter.admit_verified(&format!("busy-{n}"), start), Ok(()));
        }
        assert_eq!(limiter.entries(), PRUNE_AT + 1);
    }
}
