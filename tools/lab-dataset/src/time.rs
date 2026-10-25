//! Dataset times and the constant shift that moves a dataset onto the current time.
//!
//! Every time in an index and a segment file is UTC with exactly millisecond precision, written
//! as `2026-01-05T22:00:00.120Z` (RFC 3339, three fractional digits, `Z`). One spelling per
//! instant keeps the files diffable and the content hash meaningful.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// The only accepted layout (chrono format string).
const LAYOUT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// The layout ClickHouse's default (`basic`) parser reads into a `DateTime64(3, 'UTC')` column.
const CLICKHOUSE_LAYOUT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// Parses a dataset time; anything but the canonical spelling is refused.
///
/// # Errors
///
/// When `text` is not `YYYY-MM-DDTHH:MM:SS.mmmZ` or names no valid instant.
pub fn parse(text: &str) -> Result<DateTime<Utc>, String> {
    let naive = NaiveDateTime::parse_from_str(text, LAYOUT)
        .map_err(|_| format!("`{text}` is not a time in the form 2026-01-05T22:00:00.000Z"))?;
    let time = naive.and_utc();
    // The round trip refuses other spellings that chrono's parser accepts.
    if format(time) != text {
        return Err(format!(
            "`{text}` is not a time in the form 2026-01-05T22:00:00.000Z"
        ));
    }
    Ok(time)
}

/// The canonical spelling of a dataset time (milliseconds, `Z`). Sub-millisecond parts, which a
/// dataset time never has, are truncated.
#[must_use]
pub fn format(time: DateTime<Utc>) -> String {
    time.format(LAYOUT).to_string()
}

/// The text a `DateTime64(3, 'UTC')` column takes in `JSONEachRow`.
pub(crate) fn clickhouse(time: DateTime<Utc>) -> String {
    time.format(CLICKHOUSE_LAYOUT).to_string()
}

/// Serde `deserialize_with` for dataset times in the index.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<DateTime<Utc>, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse(&text).map_err(serde::de::Error::custom)
}

/// The earliest and latest instant a `DateTime64(3)` column holds (ClickHouse documents the range
/// as 1900-01-01 to 2299-12-31).
fn clickhouse_range() -> (DateTime<Utc>, DateTime<Utc>) {
    let first = NaiveDate::from_ymd_opt(1900, 1, 1)
        .and_then(|day| day.and_hms_milli_opt(0, 0, 0, 0))
        .map(|naive| naive.and_utc());
    let last = NaiveDate::from_ymd_opt(2299, 12, 31)
        .and_then(|day| day.and_hms_milli_opt(23, 59, 59, 999))
        .map(|naive| naive.and_utc());
    match (first, last) {
        (Some(first), Some(last)) => (first, last),
        _ => (DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC),
    }
}

/// A constant number of milliseconds added to every reading time, receive time and commit time
/// of a dataset when it is loaded. Order, gaps and every difference between two times stay
/// exact; a shift by whole days (see [`TimeShift::ending_before`]) also keeps every reading in
/// the same position within its UTC minute, hour and day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TimeShift {
    millis: i64,
}

impl TimeShift {
    /// No shift: the dataset's own times.
    pub const NONE: Self = Self { millis: 0 };

    /// A shift of `millis` milliseconds (negative moves the dataset into the past).
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self { millis }
    }

    /// The shift in milliseconds.
    #[must_use]
    pub const fn millis(self) -> i64 {
        self.millis
    }

    /// The largest shift that is a whole multiple of `align` and moves `end` to `at` or earlier.
    /// With `end` the dataset's last receive time ([`crate::Dataset::end`]), `at` the current
    /// time and `align` one day, the loaded data ends within the last day and no row lies in the
    /// future.
    ///
    /// # Errors
    ///
    /// When `align` is not a positive whole number of milliseconds, or the shift overflows.
    pub fn ending_before(
        end: DateTime<Utc>,
        at: DateTime<Utc>,
        align: TimeDelta,
    ) -> Result<Self, String> {
        let step = align.num_milliseconds();
        if step <= 0 || TimeDelta::milliseconds(step) != align {
            return Err("the alignment must be a positive whole number of milliseconds".to_owned());
        }
        // Whole milliseconds, rounded so that `end + shift <= at` holds exactly: `at` (often the
        // current time, with sub-millisecond digits) down, `end` up.
        let at_millis = at.timestamp_millis();
        let end_millis = end
            .timestamp_millis()
            .checked_add(i64::from(
                !end.timestamp_subsec_nanos().is_multiple_of(1_000_000),
            ))
            .ok_or_else(|| "the shift overflows".to_owned())?;
        let distance = at_millis
            .checked_sub(end_millis)
            .ok_or_else(|| "the shift overflows".to_owned())?;
        let millis = distance
            .div_euclid(step)
            .checked_mul(step)
            .ok_or_else(|| "the shift overflows".to_owned())?;
        Ok(Self { millis })
    }

    /// `time` moved by this shift.
    ///
    /// # Errors
    ///
    /// When the result falls outside what a ClickHouse `DateTime64(3)` column holds.
    pub fn apply(self, time: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        let (first, last) = clickhouse_range();
        time.checked_add_signed(TimeDelta::milliseconds(self.millis))
            .filter(|moved| (first..=last).contains(moved))
            .ok_or_else(|| {
                format!(
                    "{} shifted by {} ms falls outside 1900-01-01 to 2299-12-31",
                    format(time),
                    self.millis
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_times_round_trip() {
        let time = parse("2026-01-05T22:00:00.120Z").unwrap();
        assert_eq!(time.timestamp_millis(), 1_767_650_400_120);
        assert_eq!(format(time), "2026-01-05T22:00:00.120Z");
        assert_eq!(clickhouse(time), "2026-01-05 22:00:00.120");
    }

    #[test]
    fn other_spellings_are_refused() {
        for text in [
            "2026-01-05T22:00:00Z",
            "2026-01-05T22:00:00.1Z",
            "2026-01-05T22:00:00.1200Z",
            "2026-01-05T22:00:00.120+00:00",
            "2026-01-05 22:00:00.120Z",
            "2026-01-05t22:00:00.120z",
            "2026-02-30T22:00:00.120Z",
            "2026-01-05T24:00:00.000Z",
            " 2026-01-05T22:00:00.120Z",
            "",
        ] {
            assert!(parse(text).is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn a_shift_keeps_every_difference() {
        let a = parse("2026-01-05T22:00:00.120Z").unwrap();
        let b = parse("2026-01-06T01:41:30.000Z").unwrap();
        let shift = TimeShift::from_millis(-123_456_789);
        let (sa, sb) = (shift.apply(a).unwrap(), shift.apply(b).unwrap());
        assert_eq!(sb - sa, b - a);
        assert_eq!(sa.timestamp_millis() - a.timestamp_millis(), -123_456_789);
    }

    #[test]
    fn ending_before_aligns_to_whole_steps() {
        let end = parse("2026-01-06T01:55:02.500Z").unwrap();
        let at = parse("2026-10-25T08:00:00.000Z").unwrap();
        let shift = TimeShift::ending_before(end, at, TimeDelta::days(1)).unwrap();
        assert_eq!(shift.millis() % 86_400_000, 0);
        let moved = shift.apply(end).unwrap();
        assert!(moved <= at);
        assert!(at - moved < TimeDelta::days(1));
        assert_eq!(format(moved), "2026-10-25T01:55:02.500Z");

        // A dataset that ends after `at` moves back.
        let back = TimeShift::ending_before(at, end, TimeDelta::hours(1)).unwrap();
        assert!(back.millis() < 0);
        assert!(back.apply(at).unwrap() <= end);

        // Sub-millisecond instants: the end never lands after `at`, even by a fraction of a
        // millisecond (Utc::now() carries nanoseconds).
        for (end_offset, at_offset) in [(0, -500), (0, -1), (-999_999, -1_000_000), (-1, -2)] {
            let end_ns = end + TimeDelta::nanoseconds(end_offset);
            let at_ns = end + TimeDelta::nanoseconds(at_offset);
            let shift = TimeShift::ending_before(end_ns, at_ns, TimeDelta::days(1)).unwrap();
            let moved = end_ns + TimeDelta::milliseconds(shift.millis());
            assert!(moved <= at_ns, "{end_offset} ns, {at_offset} ns: {shift:?}");
            assert_eq!(shift, TimeShift::from_millis(-86_400_000));
        }

        // Exactly on a step: no further step back.
        let exact = TimeShift::ending_before(end, end, TimeDelta::days(1)).unwrap();
        assert_eq!(exact, TimeShift::NONE);

        assert!(TimeShift::ending_before(end, at, TimeDelta::zero()).is_err());
        assert!(TimeShift::ending_before(end, at, TimeDelta::microseconds(1500)).is_err());
    }

    #[test]
    fn a_shift_outside_clickhouse_range_is_refused() {
        let time = parse("2026-01-05T22:00:00.000Z").unwrap();
        assert!(TimeShift::from_millis(i64::MAX).apply(time).is_err());
        assert!(
            TimeShift::from_millis(-(130 * 365 * 86_400_000))
                .apply(time)
                .is_err()
        );
        assert!(
            TimeShift::from_millis(280 * 365 * 86_400_000)
                .apply(time)
                .is_err()
        );
    }
}
