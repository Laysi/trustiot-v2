//! When to try a request again: exponential backoff with random jitter, bounded by a number of
//! attempts and by a total time.
//!
//! The delay before attempt `n + 1` is drawn uniformly from the upper half of
//! `min(cap, initial × 2^(n − 1))` ("equal jitter"): attempts of several workstations that failed
//! together spread out, and no delay is shorter than half its step. No new attempt starts once
//! the attempt limit is reached, or when its delay would end past the total time limit; an
//! attempt that has started runs to its own timeout (`--timeout`).

use std::time::Duration;

use crate::Error;

/// The registration's retry limits; [`Backoff::default`] is what `register` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The step before the second attempt.
    pub initial: Duration,
    /// The largest step.
    pub cap: Duration,
    /// Attempts in all, the first included; at least 1.
    pub max_attempts: u32,
    /// No attempt starts later than this after the first one started.
    pub max_total: Duration,
}

/// The default initial step.
pub const DEFAULT_INITIAL: Duration = Duration::from_secs(1);
/// The default cap of a step.
pub const DEFAULT_CAP: Duration = Duration::from_secs(30);
/// The default number of attempts.
pub const DEFAULT_ATTEMPTS: u32 = 8;
/// The default total time limit.
pub const DEFAULT_TOTAL: Duration = Duration::from_secs(300);
/// The most attempts `--attempts` allows.
pub const MAX_ATTEMPTS: u32 = 20;
/// The longest `--retry-for` allows, in seconds.
pub const MAX_TOTAL_SECS: u64 = 3600;

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: DEFAULT_INITIAL,
            cap: DEFAULT_CAP,
            max_attempts: DEFAULT_ATTEMPTS,
            max_total: DEFAULT_TOTAL,
        }
    }
}

impl Backoff {
    /// The step before attempt `next` (2 or more): `min(cap, initial × 2^(next − 2))`.
    #[must_use]
    pub fn step(&self, next: u32) -> Duration {
        let doublings = next.saturating_sub(2).min(31);
        self.initial
            .saturating_mul(1_u32 << doublings)
            .min(self.cap)
    }

    /// The delay before attempt `next` (2 or more), after `elapsed` since the first attempt
    /// started; `None` when no further attempt may start.
    ///
    /// # Errors
    ///
    /// When the operating system gives no randomness for the jitter.
    pub fn delay(&self, next: u32, elapsed: Duration) -> Result<Option<Duration>, Error> {
        if next > self.max_attempts.max(1) {
            return Ok(None);
        }
        let step = self.step(next);
        let delay = jitter(step)?;
        if elapsed.saturating_add(delay) > self.max_total {
            return Ok(None);
        }
        Ok(Some(delay))
    }
}

/// A duration drawn uniformly from `[step / 2, step]`, to the microsecond.
fn jitter(step: Duration) -> Result<Duration, Error> {
    let half = step / 2;
    let span = u64::try_from((step - half).as_micros()).unwrap_or(u64::MAX);
    if span == 0 {
        return Ok(step);
    }
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|_| Error::Failed("the operating system gave no randomness".to_owned()))?;
    let offset = u64::from_le_bytes(bytes) % span.saturating_add(1);
    Ok(half + Duration::from_micros(offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_double_up_to_the_cap() {
        let backoff = Backoff::default();
        let steps: Vec<u64> = (2..=9).map(|next| backoff.step(next).as_secs()).collect();
        assert_eq!(steps, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff.step(200), DEFAULT_CAP, "no overflow");
    }

    #[test]
    fn delays_stay_in_the_upper_half_of_their_step() {
        let backoff = Backoff::default();
        for next in 2..=DEFAULT_ATTEMPTS {
            let step = backoff.step(next);
            for _ in 0..200 {
                let delay = backoff
                    .delay(next, Duration::ZERO)
                    .expect("random")
                    .expect("allowed");
                assert!(delay >= step / 2 && delay <= step, "{delay:?} of {step:?}");
            }
        }
    }

    #[test]
    fn the_attempt_and_time_limits_stop_further_attempts() {
        let backoff = Backoff::default();
        assert!(
            backoff
                .delay(DEFAULT_ATTEMPTS, Duration::ZERO)
                .expect("random")
                .is_some()
        );
        assert_eq!(
            backoff
                .delay(DEFAULT_ATTEMPTS + 1, Duration::ZERO)
                .expect("random"),
            None
        );
        // The delay before the third attempt is at least 1 s: it would end past 300 s.
        assert_eq!(
            backoff
                .delay(3, Duration::from_secs(299) + Duration::from_millis(1))
                .expect("random"),
            None
        );
        let once = Backoff {
            max_attempts: 1,
            ..Backoff::default()
        };
        assert_eq!(once.delay(2, Duration::ZERO).expect("random"), None);
        let no_time = Backoff {
            max_total: Duration::ZERO,
            ..Backoff::default()
        };
        assert_eq!(no_time.delay(2, Duration::ZERO).expect("random"), None);
    }
}
