//! The platform's raw data retention (rebuild checklist I2, Laysi/trustiot-v2#92; the numbers and
//! the reasoning are in docs/architecture/data-retention.md).
//!
//! Raw telemetry (`trustiot.telemetry`) and the batch completion records (`trustiot.batch_commits`)
//! expire this many days after the server received them. The migration Job (tools/migrate) sets
//! both TTLs from its setting `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`, which defaults to
//! [`RAW_RETENTION_DAYS`], and checks the setting with [`RawRetention::from_days`] before it
//! touches anything.
//!
//! The retention must be longer than the gateway buffer's maximum age
//! ([`protocol::telemetry::BUFFER_MAX_AGE_DAYS`]): a stream's high-water mark lives in the
//! completion records, so a committed batch whose acknowledgement was lost must come back before
//! its record expires, or it would be written twice.
//!
//! The migration Job compiles this file too (`#[path]`), so it uses only std and `protocol`.

use std::fmt;

use protocol::telemetry::BUFFER_MAX_AGE_DAYS;

/// The platform-wide raw data retention in days, counted from the server's receive time.
pub const RAW_RETENTION_DAYS: u32 = 180;

/// The longest retention a setting may ask for: ten years. A larger number is almost certainly a
/// unit mistake (hours or seconds written as days).
pub const MAX_RAW_RETENTION_DAYS: u32 = 3650;

// The platform value itself must pass the check.
const _: () = assert!(
    RAW_RETENTION_DAYS > BUFFER_MAX_AGE_DAYS && RAW_RETENTION_DAYS <= MAX_RAW_RETENTION_DAYS
);

/// A raw data retention that is longer than the gateway buffer's maximum age.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawRetention {
    days: u32,
}

impl RawRetention {
    /// The platform value, [`RAW_RETENTION_DAYS`].
    pub const PLATFORM: Self = Self {
        days: RAW_RETENTION_DAYS,
    };

    /// A retention of `days` days.
    ///
    /// # Errors
    ///
    /// When `days` is not longer than the gateway buffer's maximum age, or longer than
    /// [`MAX_RAW_RETENTION_DAYS`].
    pub const fn from_days(days: u32) -> Result<Self, RetentionError> {
        if days <= BUFFER_MAX_AGE_DAYS {
            return Err(RetentionError::NotLongerThanBuffer { days });
        }
        if days > MAX_RAW_RETENTION_DAYS {
            return Err(RetentionError::TooLong { days });
        }
        Ok(Self { days })
    }

    /// The retention in days.
    pub const fn days(self) -> u32 {
        self.days
    }
}

impl Default for RawRetention {
    fn default() -> Self {
        Self::PLATFORM
    }
}

/// Why a raw data retention setting is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionError {
    /// Not longer than the gateway buffer's maximum age.
    NotLongerThanBuffer { days: u32 },
    /// Longer than [`MAX_RAW_RETENTION_DAYS`].
    TooLong { days: u32 },
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLongerThanBuffer { days } => write!(
                f,
                "{days} days is not longer than the gateway buffer's maximum age of \
                 {BUFFER_MAX_AGE_DAYS} days: completion records (the high-water marks) would \
                 expire before a gateway stops resending, and resent batches would be written \
                 twice; use at least {} days",
                BUFFER_MAX_AGE_DAYS + 1
            ),
            Self::TooLong { days } => write!(
                f,
                "{days} days is longer than the {MAX_RAW_RETENTION_DAYS} days accepted (is it in \
                 another unit?)"
            ),
        }
    }
}

impl std::error::Error for RetentionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_value_is_longer_than_the_buffer() {
        assert_eq!(RawRetention::default().days(), RAW_RETENTION_DAYS);
        assert_eq!(
            RawRetention::from_days(RAW_RETENTION_DAYS),
            Ok(RawRetention::PLATFORM)
        );
    }

    #[test]
    fn a_retention_up_to_the_buffer_age_is_refused() {
        for days in [0, 1, BUFFER_MAX_AGE_DAYS - 1, BUFFER_MAX_AGE_DAYS] {
            let error = RawRetention::from_days(days).expect_err("not longer than the buffer");
            assert_eq!(error, RetentionError::NotLongerThanBuffer { days });
            assert!(
                error
                    .to_string()
                    .contains(&format!("maximum age of {BUFFER_MAX_AGE_DAYS} days")),
                "{error}"
            );
        }
        assert_eq!(
            RawRetention::from_days(BUFFER_MAX_AGE_DAYS + 1).map(RawRetention::days),
            Ok(BUFFER_MAX_AGE_DAYS + 1)
        );
    }

    #[test]
    fn an_implausibly_long_retention_is_refused() {
        assert!(RawRetention::from_days(MAX_RAW_RETENTION_DAYS).is_ok());
        assert_eq!(
            RawRetention::from_days(MAX_RAW_RETENTION_DAYS + 1),
            Err(RetentionError::TooLong {
                days: MAX_RAW_RETENTION_DAYS + 1
            })
        );
        assert!(RawRetention::from_days(u32::MAX).is_err());
    }
}
