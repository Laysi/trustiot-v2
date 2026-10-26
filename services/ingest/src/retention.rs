//! The platform's data retention: raw telemetry (rebuild checklist I2, Laysi/trustiot-v2#92) and
//! the one-minute and one-hour aggregates (Laysi/trustiot-v2#141). The numbers and the reasoning
//! are in docs/architecture/data-retention.md.
//!
//! Raw telemetry (`trustiot.telemetry`) and the batch completion records (`trustiot.batch_commits`)
//! expire this many days after the server received them. The migration Job (tools/migrate) sets
//! both TTLs from its setting `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`, which defaults to
//! [`RAW_RETENTION_DAYS`], and checks the setting with [`RawRetention::from_days`] before it
//! touches anything.
//!
//! The retention must be at least [`MIN_RAW_RETENTION_DAYS`] (owner decision of 2026-10-26),
//! for two reasons. It must be longer than the gateway buffer's maximum age
//! ([`protocol::telemetry::BUFFER_MAX_AGE_DAYS`]): a stream's high-water mark lives in the
//! completion records, so a committed batch whose acknowledgement was lost must come back before
//! its record expires, or it would be written twice. And it must be two days longer than the
//! I1 reconcile's window ([`RECONCILE_WINDOW_DAYS`], services/ingest/src/rollup.rs), so the raw
//! rows the reconcile compares are still there.
//!
//! The aggregate tables (`trustiot.telemetry_1m`, `trustiot.telemetry_1h`) expire by the receive
//! day, in months: [`AGGREGATE_1M_RETENTION_MONTHS`] and [`AGGREGATE_1H_RETENTION_MONTHS`] unless
//! the Job's settings `TRUSTIOT__CLICKHOUSE__AGGREGATE_1M_RETENTION_MONTHS` and
//! `TRUSTIOT__CLICKHOUSE__AGGREGATE_1H_RETENTION_MONTHS` say otherwise, checked together with the
//! raw retention by [`AggregateRetention::new`].
//!
//! The migration Job compiles this file too (`#[path]`), so it uses only std and `protocol`.

use std::fmt;

use protocol::telemetry::BUFFER_MAX_AGE_DAYS;

/// The platform-wide raw data retention in days, counted from the server's receive time.
pub const RAW_RETENTION_DAYS: u32 = 180;

/// The longest retention a setting may ask for: ten years. A larger number is almost certainly a
/// unit mistake (hours or seconds written as days).
pub const MAX_RAW_RETENTION_DAYS: u32 = 3650;

/// How many receive days before today (UTC) the I1 reconcile compares
/// (services/ingest/src/rollup.rs, which uses it from here): one more than the gateway buffer's
/// maximum age (owner decision of 2026-10-26). A gateway resends a batch for at most
/// [`BUFFER_MAX_AGE_DAYS`] days, but a batch first received at 23:59 on a day can be rewritten
/// after the midnight that ends the 30th day after it (the time a resend and the rewrite take,
/// or a gateway clock that drifts): a window of the buffer's age alone would then leave out the
/// first attempt's receive day, and its cells would stay counted twice.
pub const RECONCILE_WINDOW_DAYS: u32 = BUFFER_MAX_AGE_DAYS + 1;

/// The shortest raw data retention a setting may ask for: the reconcile window and two days
/// (33 days; owner decision of 2026-10-26). The completion records expire with the raw rows,
/// and both must outlive the gateway buffer; the two days between the window and the retention
/// keep a reconcile clear of raw rows that expire around midnight, and of TTL merges on a
/// replica whose clock runs ahead.
pub const MIN_RAW_RETENTION_DAYS: u32 = RECONCILE_WINDOW_DAYS + 2;

// The minimum covers the gateway buffer, and the platform value itself must pass the check.
const _: () = assert!(MIN_RAW_RETENTION_DAYS > BUFFER_MAX_AGE_DAYS);
const _: () = assert!(
    RAW_RETENTION_DAYS >= MIN_RAW_RETENTION_DAYS && RAW_RETENTION_DAYS <= MAX_RAW_RETENTION_DAYS
);

/// A raw data retention of [`MIN_RAW_RETENTION_DAYS`] to [`MAX_RAW_RETENTION_DAYS`] days.
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
    /// When `days` is shorter than [`MIN_RAW_RETENTION_DAYS`], or longer than
    /// [`MAX_RAW_RETENTION_DAYS`].
    pub const fn from_days(days: u32) -> Result<Self, RetentionError> {
        if days < MIN_RAW_RETENTION_DAYS {
            return Err(RetentionError::TooShort { days });
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
    /// Shorter than [`MIN_RAW_RETENTION_DAYS`].
    TooShort { days: u32 },
    /// Longer than [`MAX_RAW_RETENTION_DAYS`].
    TooLong { days: u32 },
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { days } => write!(
                f,
                "{days} days is shorter than the {MIN_RAW_RETENTION_DAYS} days required: \
                 completion records (the high-water marks) must outlive the gateway buffer's \
                 maximum age of {BUFFER_MAX_AGE_DAYS} days, or batches a gateway resends would \
                 be written twice, and the raw rows must outlive the I1 reconcile's window of \
                 {RECONCILE_WINDOW_DAYS} receive days by two days, or a reconcile could compare \
                 cells with raw rows that are expiring; use at least {MIN_RAW_RETENTION_DAYS} days"
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

/// How long the one-minute aggregates are kept, in months: 13 (owner decision of 2026-10-25),
/// the shortest the rebuild checklist I2 allows ([`MIN_AGGREGATE_1M_RETENTION_MONTHS`]).
pub const AGGREGATE_1M_RETENTION_MONTHS: u32 = 13;

/// How long the one-hour aggregates are kept, in months: four years (owner decision of
/// 2026-10-25). They carry the long-term trends, month-on-month and year-on-year growth and the
/// monthly carbon report.
pub const AGGREGATE_1H_RETENTION_MONTHS: u32 = 48;

/// The shortest one-minute retention a setting may ask for: 13 months, so that a month can be
/// compared with the same month a year earlier at either resolution (rebuild checklist I2).
/// Time zones with a half- or quarter-hour offset get whole local days only from the one-minute
/// table.
pub const MIN_AGGREGATE_1M_RETENTION_MONTHS: u32 = 13;

/// The longest aggregate retention a setting may ask for: ten years, as for the raw data
/// ([`MAX_RAW_RETENTION_DAYS`]). A larger number is almost certainly a unit mistake (days written
/// as months).
pub const MAX_AGGREGATE_RETENTION_MONTHS: u32 = 120;

/// The fewest days in `k` consecutive calendar months, for `k` from 0 to 11: runs that start in
/// February.
const SHORTEST_MONTH_RUNS: [u32; 12] = [0, 28, 59, 89, 120, 150, 181, 212, 242, 273, 303, 334];

/// The fewest days that `months` months after a date can be, as ClickHouse adds months to a
/// receive day (`received_day + toIntervalMonth(months)`, a day past the end of the target month
/// becoming its last day): 365 for every 12 months, and the shortest run of calendar months for
/// the rest. Checked against ClickHouse 26.8 for every start date from 2000 to 2299 and every
/// number of months up to [`MAX_AGGREGATE_RETENTION_MONTHS`]: never more than the real number of
/// days, and equal to it up to 48 months (Laysi/trustiot-v2#141).
#[must_use]
pub const fn min_days_in_months(months: u32) -> u32 {
    365 * (months / 12) + SHORTEST_MONTH_RUNS[(months % 12) as usize]
}

/// The smallest number of months that never holds fewer than `days` days.
const fn months_covering(days: u32) -> u32 {
    let mut months = 0;
    while min_days_in_months(months) < days {
        months += 1;
    }
    months
}

// The platform values themselves must pass the checks.
const _: () = assert!(
    AGGREGATE_1M_RETENTION_MONTHS >= MIN_AGGREGATE_1M_RETENTION_MONTHS
        && AGGREGATE_1H_RETENTION_MONTHS >= AGGREGATE_1M_RETENTION_MONTHS
        && AGGREGATE_1H_RETENTION_MONTHS <= MAX_AGGREGATE_RETENTION_MONTHS
        && min_days_in_months(AGGREGATE_1M_RETENTION_MONTHS) >= RAW_RETENTION_DAYS
        && min_days_in_months(MAX_AGGREGATE_RETENTION_MONTHS) >= MAX_RAW_RETENTION_DAYS
);

/// One of the two aggregate tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rollup {
    /// `trustiot.telemetry_1m`.
    Minute,
    /// `trustiot.telemetry_1h`.
    Hour,
}

impl Rollup {
    /// How the messages name the table.
    const fn describe(self) -> &'static str {
        match self {
            Self::Minute => "the one-minute aggregates",
            Self::Hour => "the one-hour aggregates",
        }
    }
}

/// The retention of the two aggregate tables, in months, checked against each other and against
/// the raw data retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggregateRetention {
    minute_months: u32,
    hour_months: u32,
}

impl AggregateRetention {
    /// The platform values, [`AGGREGATE_1M_RETENTION_MONTHS`] and
    /// [`AGGREGATE_1H_RETENTION_MONTHS`].
    pub const PLATFORM: Self = Self {
        minute_months: AGGREGATE_1M_RETENTION_MONTHS,
        hour_months: AGGREGATE_1H_RETENTION_MONTHS,
    };

    /// The one-minute aggregates kept `minute_months` months and the one-hour aggregates
    /// `hour_months` months, with raw data kept as `raw` says.
    ///
    /// # Errors
    ///
    /// When either is longer than [`MAX_AGGREGATE_RETENTION_MONTHS`], the one-minute value is
    /// shorter than [`MIN_AGGREGATE_1M_RETENTION_MONTHS`], the one-hour value is shorter than the
    /// one-minute value, or the one-minute value can hold fewer days than the raw retention
    /// (the one-hour value is then at least as long).
    pub const fn new(
        raw: RawRetention,
        minute_months: u32,
        hour_months: u32,
    ) -> Result<Self, AggregateRetentionError> {
        if minute_months > MAX_AGGREGATE_RETENTION_MONTHS {
            return Err(AggregateRetentionError::TooLong {
                rollup: Rollup::Minute,
                months: minute_months,
            });
        }
        if hour_months > MAX_AGGREGATE_RETENTION_MONTHS {
            return Err(AggregateRetentionError::TooLong {
                rollup: Rollup::Hour,
                months: hour_months,
            });
        }
        if minute_months < MIN_AGGREGATE_1M_RETENTION_MONTHS {
            return Err(AggregateRetentionError::TooShortForYearOnYear {
                months: minute_months,
            });
        }
        if hour_months < minute_months {
            return Err(AggregateRetentionError::HourShorterThanMinute {
                hour_months,
                minute_months,
            });
        }
        if min_days_in_months(minute_months) < raw.days() {
            return Err(AggregateRetentionError::ShorterThanRaw {
                months: minute_months,
                raw_days: raw.days(),
            });
        }
        Ok(Self {
            minute_months,
            hour_months,
        })
    }

    /// The one-minute aggregates' retention in months.
    pub const fn minute_months(self) -> u32 {
        self.minute_months
    }

    /// The one-hour aggregates' retention in months.
    pub const fn hour_months(self) -> u32 {
        self.hour_months
    }
}

impl Default for AggregateRetention {
    fn default() -> Self {
        Self::PLATFORM
    }
}

/// Why an aggregate retention setting is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateRetentionError {
    /// Longer than [`MAX_AGGREGATE_RETENTION_MONTHS`].
    TooLong { rollup: Rollup, months: u32 },
    /// The one-minute value is shorter than [`MIN_AGGREGATE_1M_RETENTION_MONTHS`].
    TooShortForYearOnYear { months: u32 },
    /// The one-hour value is shorter than the one-minute value.
    HourShorterThanMinute {
        hour_months: u32,
        minute_months: u32,
    },
    /// The one-minute value can hold fewer days than the raw data retention.
    ShorterThanRaw { months: u32, raw_days: u32 },
}

impl AggregateRetentionError {
    /// The table whose setting is refused.
    pub const fn rollup(&self) -> Rollup {
        match self {
            Self::TooLong { rollup, .. } => *rollup,
            Self::TooShortForYearOnYear { .. } | Self::ShorterThanRaw { .. } => Rollup::Minute,
            Self::HourShorterThanMinute { .. } => Rollup::Hour,
        }
    }
}

impl fmt::Display for AggregateRetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { rollup, months } => write!(
                f,
                "{months} months for {} is longer than the {MAX_AGGREGATE_RETENTION_MONTHS} \
                 months (ten years) accepted (is it in another unit?)",
                rollup.describe()
            ),
            Self::TooShortForYearOnYear { months } => write!(
                f,
                "{months} months for the one-minute aggregates is shorter than \
                 {MIN_AGGREGATE_1M_RETENTION_MONTHS} months: the aggregates must reach the same \
                 month a year earlier for year-on-year comparisons (rebuild checklist I2), and \
                 time zones with a half- or quarter-hour offset get whole local days only from \
                 the one-minute table; use at least {MIN_AGGREGATE_1M_RETENTION_MONTHS} months"
            ),
            Self::HourShorterThanMinute {
                hour_months,
                minute_months,
            } => write!(
                f,
                "{hour_months} months for the one-hour aggregates is shorter than the \
                 {minute_months} months of the one-minute aggregates: the one-hour table carries \
                 the long-term trends and reports, so it keeps a period at least as long as the \
                 finer table does; use at least {minute_months} months"
            ),
            Self::ShorterThanRaw { months, raw_days } => write!(
                f,
                "{months} months for the one-minute aggregates can be as short as {} days, \
                 shorter than the raw data retention of {raw_days} days: the aggregates of \
                 readings still kept raw would expire first; use at least {} months",
                min_days_in_months(*months),
                months_covering(*raw_days)
            ),
        }
    }
}

impl std::error::Error for AggregateRetentionError {}

/// The raw and aggregate retention the migration Job applies, checked together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    pub raw: RawRetention,
    pub aggregates: AggregateRetention,
}

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

    /// The owner decision of 2026-10-26: a reconcile window of 31 receive days, one more than
    /// the 30-day gateway buffer, and a raw retention of at least 33 days.
    #[test]
    fn the_minimum_is_the_reconcile_window_and_two_days() {
        assert_eq!(BUFFER_MAX_AGE_DAYS, 30);
        assert_eq!(RECONCILE_WINDOW_DAYS, 31);
        assert_eq!(MIN_RAW_RETENTION_DAYS, 33);
    }

    #[test]
    fn a_retention_below_the_minimum_is_refused() {
        for days in [
            0,
            1,
            BUFFER_MAX_AGE_DAYS,
            BUFFER_MAX_AGE_DAYS + 1,
            MIN_RAW_RETENTION_DAYS - 1,
        ] {
            let error = RawRetention::from_days(days).expect_err("below the minimum");
            assert_eq!(error, RetentionError::TooShort { days });
            let text = error.to_string();
            for reason in [
                format!("maximum age of {BUFFER_MAX_AGE_DAYS} days"),
                format!("reconcile's window of {RECONCILE_WINDOW_DAYS} receive days"),
                format!("use at least {MIN_RAW_RETENTION_DAYS} days"),
            ] {
                assert!(text.contains(&reason), "{days}: {text}");
            }
        }
        assert!(RawRetention::from_days(32).is_err());
        assert_eq!(
            RawRetention::from_days(33).map(RawRetention::days),
            Ok(MIN_RAW_RETENTION_DAYS)
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
    #[test]
    fn the_platform_aggregate_values_pass() {
        assert_eq!(AGGREGATE_1M_RETENTION_MONTHS, 13);
        assert_eq!(AGGREGATE_1H_RETENTION_MONTHS, 48);
        assert_eq!(
            AggregateRetention::new(RawRetention::PLATFORM, 13, 48),
            Ok(AggregateRetention::PLATFORM)
        );
        assert_eq!(AggregateRetention::default(), AggregateRetention::PLATFORM);
        assert_eq!(
            Retention::default(),
            Retention {
                raw: RawRetention::PLATFORM,
                aggregates: AggregateRetention::PLATFORM
            }
        );
    }

    /// Spot values of the bound, against counted calendars: 2025-01-31 plus one month is
    /// 2025-02-28 (28 days), 2025-02-01 plus 13 months is 2026-03-01 (393 days), 2025-03-01 plus
    /// 48 months is 2029-03-01 (1461 days, with 2028-02-29; the bound counts 1460).
    #[test]
    fn months_hold_at_least_the_bound_in_days() {
        assert_eq!(min_days_in_months(0), 0);
        assert_eq!(min_days_in_months(1), 28);
        assert_eq!(min_days_in_months(11), 334);
        assert_eq!(min_days_in_months(12), 365);
        assert_eq!(min_days_in_months(13), 393);
        assert_eq!(min_days_in_months(48), 1460);
        assert_eq!(min_days_in_months(120), 3650);
        for months in 1..=MAX_AGGREGATE_RETENTION_MONTHS {
            assert!(min_days_in_months(months) > min_days_in_months(months - 1));
            assert!(min_days_in_months(months) >= 28 * months);
            assert!(min_days_in_months(months) <= 31 * months);
        }
        assert_eq!(months_covering(0), 0);
        assert_eq!(months_covering(180), 6);
        assert_eq!(months_covering(394), 14);
        assert_eq!(months_covering(MAX_RAW_RETENTION_DAYS), 120);
    }

    #[test]
    fn each_aggregate_rule_refuses_with_its_reason() {
        let raw = RawRetention::PLATFORM;
        let cases = [
            (
                AggregateRetention::new(raw, 121, 121),
                AggregateRetentionError::TooLong {
                    rollup: Rollup::Minute,
                    months: 121,
                },
                Rollup::Minute,
                "another unit",
            ),
            (
                AggregateRetention::new(raw, 13, 395),
                AggregateRetentionError::TooLong {
                    rollup: Rollup::Hour,
                    months: 395,
                },
                Rollup::Hour,
                "another unit",
            ),
            (
                AggregateRetention::new(raw, 12, 48),
                AggregateRetentionError::TooShortForYearOnYear { months: 12 },
                Rollup::Minute,
                "year-on-year",
            ),
            (
                AggregateRetention::new(raw, 24, 13),
                AggregateRetentionError::HourShorterThanMinute {
                    hour_months: 13,
                    minute_months: 24,
                },
                Rollup::Hour,
                "use at least 24 months",
            ),
            (
                AggregateRetention::new(
                    RawRetention::from_days(394).expect("a valid raw retention"),
                    13,
                    48,
                ),
                AggregateRetentionError::ShorterThanRaw {
                    months: 13,
                    raw_days: 394,
                },
                Rollup::Minute,
                "as short as 393 days, shorter than the raw data retention of 394 days",
            ),
        ];
        for (result, expected, rollup, reason) in cases {
            let error = result.expect_err("refused");
            assert_eq!(error, expected);
            assert_eq!(error.rollup(), rollup);
            assert!(error.to_string().contains(reason), "{error}");
        }
        assert!(
            AggregateRetentionError::ShorterThanRaw {
                months: 13,
                raw_days: 394
            }
            .to_string()
            .contains("use at least 14 months")
        );
    }

    #[test]
    fn valid_aggregate_settings_are_kept() {
        let longest_raw =
            RawRetention::from_days(MAX_RAW_RETENTION_DAYS).expect("a valid raw retention");
        for (raw, minute, hour) in [
            (RawRetention::PLATFORM, 13, 13),
            (RawRetention::PLATFORM, 24, 60),
            (RawRetention::PLATFORM, 120, 120),
            (longest_raw, 120, 120),
            (
                RawRetention::from_days(393).expect("a valid raw retention"),
                13,
                48,
            ),
        ] {
            let retention = AggregateRetention::new(raw, minute, hour)
                .unwrap_or_else(|error| panic!("{minute}/{hour}: {error}"));
            assert_eq!(
                (retention.minute_months(), retention.hour_months()),
                (minute, hour)
            );
        }
        assert!(AggregateRetention::new(longest_raw, 119, 120).is_err());
    }
}
