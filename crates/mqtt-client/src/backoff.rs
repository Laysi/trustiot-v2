//! The delay before a reconnect: exponential, capped, with jitter.

use std::time::Duration;

use crate::{RECONNECT_DELAY_MAX, RECONNECT_DELAY_MIN};

/// The delay after `failures` failed attempts in a row (0 for the first reconnect), with
/// `jitter` between 0 and 1: the ceiling doubles from [`RECONNECT_DELAY_MIN`] up to
/// [`RECONNECT_DELAY_MAX`], and the delay is between half the ceiling and the ceiling ("equal
/// jitter"), so replicas that lost the broker together do not come back together, and no delay
/// is shorter than half the minimum.
pub(crate) fn delay(failures: u32, jitter: f64) -> Duration {
    let ceiling = RECONNECT_DELAY_MIN
        .saturating_mul(1_u32 << failures.min(16))
        .min(RECONNECT_DELAY_MAX);
    let half = ceiling / 2;
    // `mul_f64` panics on NaN; `clamp` keeps NaN, so it is replaced first.
    let jitter = if jitter.is_nan() { 0.5 } else { jitter };
    half + half.mul_f64(jitter.clamp(0.0, 1.0))
}

/// A random number between 0 and 1 from the operating system, or 0.5 when it has none.
pub(crate) fn jitter() -> f64 {
    let mut bytes = [0_u8; 4];
    match getrandom::fill(&mut bytes) {
        Ok(()) => f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX),
        Err(_) => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_doubles_up_to_the_maximum_and_stays_within_the_jitter_band() {
        assert_eq!(delay(0, 0.0), RECONNECT_DELAY_MIN / 2);
        assert_eq!(delay(0, 1.0), RECONNECT_DELAY_MIN);
        assert_eq!(delay(1, 1.0), RECONNECT_DELAY_MIN * 2);
        assert_eq!(delay(3, 1.0), RECONNECT_DELAY_MIN * 8);
        for failures in [10, 16, 17, 1000, u32::MAX] {
            assert_eq!(delay(failures, 1.0), RECONNECT_DELAY_MAX, "{failures}");
            assert_eq!(delay(failures, 0.0), RECONNECT_DELAY_MAX / 2, "{failures}");
        }
        for _ in 0..1000 {
            let value = delay(2, jitter());
            assert!(value >= RECONNECT_DELAY_MIN * 2 && value <= RECONNECT_DELAY_MIN * 4);
        }
        // Out-of-range jitter cannot leave the band.
        assert_eq!(delay(0, 7.0), RECONNECT_DELAY_MIN);
        assert_eq!(delay(0, -1.0), RECONNECT_DELAY_MIN / 2);
    }
}
