//! Reconnect delays: exponential backoff with full jitter.
//!
//! The n-th failed attempt in a row waits a uniformly random time between [`MIN_DELAY`] and
//! `min(cap, base * 2^n)`. The randomness spreads the reconnects of many gateways after a
//! cloud outage; the floor keeps an unlucky draw from turning into a tight loop. A connection
//! that stayed up for [`STABLE_AFTER`] resets the sequence, so one drop after a long, healthy
//! connection reconnects quickly again.

use std::time::Duration;

/// The shortest delay, whatever the draw: never more than ten attempts a second.
pub const MIN_DELAY: Duration = Duration::from_millis(100);
/// How long a connection must have been up for the next failure to start over at `base`.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);

/// The two numbers of the backoff, from the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffSettings {
    /// The ceiling of the first delay.
    pub base: Duration,
    /// The largest ceiling.
    pub cap: Duration,
}

impl Default for BackoffSettings {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        }
    }
}

/// The state of one backoff sequence.
#[derive(Clone, Debug)]
pub struct Backoff {
    settings: BackoffSettings,
    failures: u32,
}

impl Backoff {
    /// A sequence that has seen no failure yet.
    pub const fn new(settings: BackoffSettings) -> Self {
        Self {
            settings,
            failures: 0,
        }
    }

    /// The ceiling of the next delay: `min(cap, base * 2^failures)`, never below
    /// [`MIN_DELAY`].
    pub fn ceiling(&self) -> Duration {
        let factor = 1_u32.checked_shl(self.failures.min(31)).unwrap_or(u32::MAX);
        self.settings
            .base
            .checked_mul(factor)
            .unwrap_or(self.settings.cap)
            .min(self.settings.cap)
            .max(MIN_DELAY)
    }

    /// Counts a failure and returns how long to wait before the next attempt. `random` is a
    /// uniformly distributed `u64` (the caller's source of randomness, so tests can fix it).
    pub fn next_delay(&mut self, random: u64) -> Duration {
        let ceiling = self.ceiling();
        self.failures = self.failures.saturating_add(1);
        let span = ceiling.saturating_sub(MIN_DELAY);
        let span_nanos = span.as_nanos();
        // `random / 2^64` scaled to the span: uniform, and never above it.
        let offset = span_nanos.saturating_mul(u128::from(random)) >> 64;
        MIN_DELAY + Duration::from_nanos(u64::try_from(offset).unwrap_or(u64::MAX))
    }

    /// Starts the sequence over, after a connection that stayed up for [`STABLE_AFTER`].
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Failures counted since the last reset.
    pub const fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff() -> Backoff {
        Backoff::new(BackoffSettings::default())
    }

    #[test]
    fn ceilings_double_from_the_base_up_to_the_cap() {
        let mut backoff = backoff();
        let mut ceilings = Vec::new();
        for _ in 0..10 {
            ceilings.push(backoff.ceiling().as_secs());
            backoff.next_delay(0);
        }
        assert_eq!(ceilings, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn delays_stay_between_the_floor_and_the_ceiling() {
        let mut backoff = backoff();
        for failures in 0..40 {
            let ceiling = backoff.ceiling();
            for random in [0, 1, u64::MAX / 3, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
                let delay = backoff.clone().next_delay(random);
                assert!(
                    (MIN_DELAY..=ceiling).contains(&delay),
                    "failure {failures}: {delay:?} outside {MIN_DELAY:?}..={ceiling:?}"
                );
            }
            backoff.next_delay(u64::MAX);
        }
        // The extremes of the draw reach the ends of the range.
        let mut fresh = Backoff::new(BackoffSettings::default());
        assert_eq!(fresh.clone().next_delay(0), MIN_DELAY);
        assert!(fresh.next_delay(u64::MAX) > Duration::from_millis(999));
    }

    #[test]
    fn reset_starts_over_at_the_base() {
        let mut backoff = backoff();
        for _ in 0..8 {
            backoff.next_delay(7);
        }
        assert_eq!(backoff.ceiling(), Duration::from_secs(60));
        backoff.reset();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.ceiling(), Duration::from_secs(1));
    }

    #[test]
    fn huge_failure_counts_and_settings_do_not_overflow() {
        let mut backoff = Backoff::new(BackoffSettings {
            base: Duration::from_secs(u64::MAX / 2),
            cap: Duration::from_secs(u64::MAX),
        });
        backoff.failures = u32::MAX;
        assert!(backoff.next_delay(u64::MAX) >= MIN_DELAY);
        assert_eq!(backoff.failures(), u32::MAX);
        // A base below the floor still waits at least the floor.
        let mut tiny = Backoff::new(BackoffSettings {
            base: Duration::from_millis(1),
            cap: Duration::from_millis(1),
        });
        assert_eq!(tiny.next_delay(u64::MAX), MIN_DELAY);
    }
}
