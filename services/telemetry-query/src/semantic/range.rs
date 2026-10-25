//! What a semantic request's range becomes before anything is queried: the time zone, the
//! relative period resolved at the request's "now", the table that answers it exactly, the
//! queryable period of that table, and the limits (services/telemetry-query/README.md,
//! 「語意查詢」).
//!
//! Local calendar arithmetic uses the IANA Time Zone Database compiled into the service
//! (`jiff-tzdb`), never the host's, so every replica resolves a period alike. ClickHouse puts
//! readings into the local buckets with its own copy of the database; where the two copies
//! differ (a rule changed recently) the first or last bucket can be partial, and the result is
//! still correct for the readings inside the range.

use std::sync::OnceLock;

use chrono::{DateTime, Months, TimeDelta, Utc};
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::{Timestamp, ToSpan, civil::Date};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::history::AGGREGATED_MONTHS;

/// How much later than the queryable period's start the service starts reading: the database
/// evaluates its own window's `now()` after the service took its "now" (the account check, the
/// request and clock skew), so the service's start must never be earlier than the database's.
pub const PERIOD_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// How far around the range the usage reads, for the reading before the range (the baseline)
/// and for the cumulative check, which judges the data point rather than the range.
pub const LOOKAROUND: TimeDelta = TimeDelta::hours(24);

/// The most series one request names.
pub const MAX_SERIES: usize = 20;
/// The most rows (series × buckets) one answer holds.
pub const MAX_ROWS: usize = 10_000;
/// The most one-minute cells (series × minutes of the window read) one usage request reads: the
/// usage orders every cell of a series to compare neighbours, which holds them in memory.
pub const MAX_MINUTE_CELLS: i64 = 1_000_000;
/// `last_<N>_days` takes 1 to this many days.
pub const MAX_LAST_DAYS: u32 = 366;
/// `last_<N>_hours` takes 1 to this many hours.
pub const MAX_LAST_HOURS: u32 = 72;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// The IANA Time Zone Database compiled into the service.
fn database() -> &'static TimeZoneDatabase {
    static DATABASE: OnceLock<TimeZoneDatabase> = OnceLock::new();
    DATABASE.get_or_init(TimeZoneDatabase::bundled)
}

/// The time zone named `name`: exactly a name of the IANA Time Zone Database compiled in, the
/// list a user's time zone preference is checked against too (services/identity).
pub fn time_zone(name: &str) -> Option<TimeZone> {
    if !jiff_tzdb::available().any(|known| known == name) {
        return None;
    }
    database().get(name).ok()
}

/// A bucket length of the semantic API: whole local minutes, hours, days, Monday weeks and
/// months of the requested time zone, or the whole range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
// Named `BucketInterval` in the spec: the history API's `Interval` is another set, and two
// schemas of one name would silently become one.
#[schema(as = BucketInterval)]
pub enum Interval {
    #[serde(rename = "1m")]
    Minute1,
    #[serde(rename = "5m")]
    Minutes5,
    #[serde(rename = "15m")]
    Minutes15,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "1d")]
    Day1,
    #[serde(rename = "1w")]
    Week1,
    #[serde(rename = "1mo")]
    Month1,
    #[default]
    #[serde(rename = "total")]
    Total,
}

/// How the buckets of an interval are formed in ClickHouse ([`super::statements`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Minutes from the Unix epoch (every offset is a whole number of 15 minutes, so these are
    /// local quarter hours too), this many.
    Minutes(u32),
    Hour,
    Day,
    Week,
    Month,
    Total,
}

impl Interval {
    pub const fn unit(self) -> Unit {
        match self {
            Self::Minute1 => Unit::Minutes(1),
            Self::Minutes5 => Unit::Minutes(5),
            Self::Minutes15 => Unit::Minutes(15),
            Self::Hour1 => Unit::Hour,
            Self::Day1 => Unit::Day,
            Self::Week1 => Unit::Week,
            Self::Month1 => Unit::Month,
            Self::Total => Unit::Total,
        }
    }

    /// The most buckets `from..to` spans: nominal lengths, one more for a range that does not
    /// start on a boundary and one more for a daylight saving change, as the history API counts.
    fn max_buckets(self, from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
        let range = (to - from).num_seconds().max(0);
        let length = match self.unit() {
            Unit::Total => return 1,
            Unit::Minutes(step) => 60 * i64::from(step),
            Unit::Hour => 3_600,
            Unit::Day => 86_400,
            Unit::Week => 7 * 86_400,
            // The shortest month.
            Unit::Month => 28 * 86_400,
        };
        (range + length - 1) / length + 2
    }
}

/// A relative period, resolved in the request's time zone when the request arrives. Weeks start
/// on Monday (ISO 8601). Calendar periods cover the whole day, week, month or year, so the
/// current one ends after now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
    /// This many local days, today included: from the local midnight `N - 1` days ago to the
    /// next local midnight.
    LastDays(u32),
    /// This many hours up to now, rounded up to the next whole minute.
    LastHours(u32),
}

/// A count of 1 to `max` in plain decimal, without leading zeros or signs.
fn count(text: &str, max: u32) -> Option<u32> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>()
        .ok()
        .filter(|count| (1..=max).contains(count))
}

impl Period {
    /// The accepted spellings: `today`, `yesterday`, `this_week`, `last_week`, `this_month`,
    /// `last_month`, `this_year`, `last_year`, `last_<N>_days` (N 1 to 366) and
    /// `last_<N>_hours` (N 1 to 72).
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "today" => Self::Today,
            "yesterday" => Self::Yesterday,
            "this_week" => Self::ThisWeek,
            "last_week" => Self::LastWeek,
            "this_month" => Self::ThisMonth,
            "last_month" => Self::LastMonth,
            "this_year" => Self::ThisYear,
            "last_year" => Self::LastYear,
            _ => {
                let rest = text.strip_prefix("last_")?;
                if let Some(days) = rest.strip_suffix("_days") {
                    Self::LastDays(count(days, MAX_LAST_DAYS)?)
                } else {
                    Self::LastHours(count(rest.strip_suffix("_hours")?, MAX_LAST_HOURS)?)
                }
            }
        })
    }

    /// The range, `from` inclusive and `to` exclusive, at `now` in `tz`.
    pub fn resolve(
        self,
        now: DateTime<Utc>,
        tz: &TimeZone,
    ) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let now_ts = Timestamp::from_millisecond(now.timestamp_millis()).ok()?;
        let today = now_ts.to_zoned(tz.clone()).date();
        let midnight = |date: Date| -> Option<DateTime<Utc>> {
            let zoned = date.to_zoned(tz.clone()).ok()?;
            DateTime::from_timestamp_millis(zoned.timestamp().as_millisecond())
        };
        let days = |date: Date, n: i64| date.checked_add(n.days()).ok();
        let range = |from: Date, to: Date| Some((midnight(from)?, midnight(to)?));
        let monday = days(today, -i64::from(today.weekday().to_monday_zero_offset()))?;
        let month = today.first_of_month();
        let year = today.first_of_year();
        match self {
            Self::Today => range(today, days(today, 1)?),
            Self::Yesterday => range(days(today, -1)?, today),
            Self::ThisWeek => range(monday, days(monday, 7)?),
            Self::LastWeek => range(days(monday, -7)?, monday),
            Self::ThisMonth => range(month, month.checked_add(1.month()).ok()?),
            Self::LastMonth => range(month.checked_sub(1.month()).ok()?, month),
            Self::ThisYear => range(year, year.checked_add(1.year()).ok()?),
            Self::LastYear => range(year.checked_sub(1.year()).ok()?, year),
            Self::LastDays(n) => range(days(today, 1 - i64::from(n))?, days(today, 1)?),
            Self::LastHours(n) => {
                let to = ceil(now, MINUTE_MS)?;
                Some((to - TimeDelta::hours(i64::from(n)), to))
            }
        }
    }
}

/// `time` rounded down to a multiple of `ms` milliseconds since the epoch.
pub fn floor(time: DateTime<Utc>, ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(time.timestamp_millis().div_euclid(ms) * ms)
}

/// `time` rounded up to a multiple of `ms` milliseconds since the epoch.
pub fn ceil(time: DateTime<Utc>, ms: i64) -> Option<DateTime<Utc>> {
    let millis = time.timestamp_millis();
    let floored = millis.div_euclid(ms) * ms;
    let rounded = if floored == millis {
        floored
    } else {
        floored.checked_add(ms)?
    };
    DateTime::from_timestamp_millis(rounded)
}

/// The aggregate table a request reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum Resolution {
    /// `trustiot.tenant_telemetry_1m`, one-minute cells.
    #[serde(rename = "1m")]
    Minute,
    /// `trustiot.tenant_telemetry_1h`, one-hour cells (UTC hours).
    #[serde(rename = "1h")]
    Hour,
}

impl Resolution {
    /// A cell's length in milliseconds.
    pub const fn cell_ms(self) -> i64 {
        match self {
            Self::Minute => MINUTE_MS,
            Self::Hour => HOUR_MS,
        }
    }
}

/// The start of the queryable period of `table` at `now`: [`AGGREGATED_MONTHS`] calendar months
/// back (a day the month lacks becomes its last day, as ClickHouse's `subtractMonths` does),
/// [`PERIOD_MARGIN`] later, rounded up to the table's cell. Every cell from it on (inclusive)
/// may be read; the database's own window starts at or before it.
pub fn queryable_start(table: Resolution, now: DateTime<Utc>) -> DateTime<Utc> {
    let start = now
        .checked_sub_months(Months::new(AGGREGATED_MONTHS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
        + PERIOD_MARGIN;
    ceil(start, table.cell_ms()).unwrap_or(start)
}

/// Whether every bucket boundary of `from..to` in `tz` falls on a whole UTC hour, so the
/// one-hour table answers exactly: `from` and `to` are whole UTC hours, and every offset `tz`
/// uses in the range is a whole number of hours.
fn whole_hours(tz: &TimeZone, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    let whole = |offset: jiff::tz::Offset| offset.seconds() % 3_600 == 0;
    if from.timestamp_millis() % HOUR_MS != 0 || to.timestamp_millis() % HOUR_MS != 0 {
        return false;
    }
    let (Ok(start), Ok(end)) = (
        Timestamp::from_millisecond(from.timestamp_millis()),
        Timestamp::from_millisecond(to.timestamp_millis()),
    ) else {
        return false;
    };
    whole(tz.to_offset(start))
        && tz
            .following(start)
            .take_while(|transition| transition.timestamp() < end)
            .all(|transition| whole(transition.offset()))
}

/// Which semantic answer is planned: usage also reads around the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Statistics,
    Usage,
}

/// Why a request cannot be planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `period` is not one of the accepted spellings, or both or neither of `period` and
    /// `from`/`to` were given.
    Period,
    /// `from` is not before `to`, or only one of them was given.
    Range,
    /// The range lies wholly before the queryable period, which starts at this time.
    OutsidePeriod(DateTime<Utc>),
    /// The answer would hold more than [`MAX_ROWS`] rows.
    TooManyRows,
    /// The usage would read more than [`MAX_MINUTE_CELLS`] one-minute cells.
    TooManyCells,
}

/// The range as asked: a relative period, or `from` and `to`.
#[derive(Clone, Copy, Debug)]
pub struct Asked<'a> {
    pub period: Option<&'a str>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub interval: Interval,
    pub series: usize,
}

/// A request's range once planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub resolution: Resolution,
    pub unit: Unit,
    /// The range read: `from` inclusive (the requested start, or the queryable period's start
    /// when the request started earlier), `to` exclusive.
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// The requested start was moved to the queryable period's start.
    pub truncated: bool,
    /// The queryable period's start of the table read.
    pub earliest: DateTime<Utc>,
    /// The range ends after the request's "now" (a period still running, such as `today`).
    pub ends_after_now: bool,
    /// What the usage reads: the range and [`LOOKAROUND`] on each side, within the queryable
    /// period and not past now (or `to`, when that is later).
    pub window_from: DateTime<Utc>,
    pub window_to: DateTime<Utc>,
}

/// Plans a request at `now` in `tz`.
pub fn plan(
    asked: &Asked<'_>,
    kind: Kind,
    now: DateTime<Utc>,
    tz: &TimeZone,
) -> Result<Plan, Refusal> {
    let (from, to) = match (asked.period, asked.from, asked.to) {
        (Some(period), None, None) => Period::parse(period)
            .and_then(|period| period.resolve(now, tz))
            .ok_or(Refusal::Period)?,
        (None, Some(from), Some(to)) => (
            floor(from, MINUTE_MS).ok_or(Refusal::Range)?,
            floor(to, MINUTE_MS).ok_or(Refusal::Range)?,
        ),
        (Some(_), _, _) | (None, None, None) => return Err(Refusal::Period),
        (None, _, _) => return Err(Refusal::Range),
    };
    if from >= to {
        return Err(Refusal::Range);
    }
    let minute_start = queryable_start(Resolution::Minute, now);
    if to <= minute_start {
        return Err(Refusal::OutsidePeriod(minute_start));
    }
    let hour_start = queryable_start(Resolution::Hour, now);
    let hourly = !matches!(asked.interval.unit(), Unit::Minutes(_))
        && to > hour_start
        && whole_hours(tz, from.max(hour_start), to);
    let resolution = if hourly {
        Resolution::Hour
    } else {
        Resolution::Minute
    };
    let earliest = queryable_start(resolution, now);
    let start = from.max(earliest);
    let series = i64::try_from(asked.series).unwrap_or(i64::MAX);
    let rows = series.saturating_mul(asked.interval.max_buckets(start, to));
    if rows > i64::try_from(MAX_ROWS).unwrap_or(i64::MAX) {
        return Err(Refusal::TooManyRows);
    }
    let window_from = (start - LOOKAROUND).max(earliest);
    let window_to = to.max((to + LOOKAROUND).min(now));
    if kind == Kind::Usage && resolution == Resolution::Minute {
        let minutes = (window_to - window_from).num_minutes() + 1;
        if series.saturating_mul(minutes) > MAX_MINUTE_CELLS {
            return Err(Refusal::TooManyCells);
        }
    }
    Ok(Plan {
        resolution,
        unit: asked.interval.unit(),
        from: start,
        to,
        truncated: from < earliest,
        earliest,
        ends_after_now: to > now,
        window_from: if kind == Kind::Usage {
            window_from
        } else {
            start
        },
        window_to: if kind == Kind::Usage { window_to } else { to },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("an RFC 3339 time")
    }

    fn tz(name: &str) -> TimeZone {
        time_zone(name).expect("a known time zone")
    }

    fn resolved(period: &str, now: &str, zone: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        Period::parse(period)
            .expect("a period")
            .resolve(at(now), &tz(zone))
            .expect("resolves")
    }

    #[test]
    fn time_zones_are_the_compiled_in_names_exactly() {
        for name in ["UTC", "Asia/Taipei", "Asia/Kolkata", "America/New_York"] {
            assert!(time_zone(name).is_some(), "{name}");
        }
        for name in [
            "",
            "asia/taipei",
            "Asia/Taipei ",
            "UTC') --",
            "../UTC",
            "Mars/Olympus",
        ] {
            assert!(time_zone(name).is_none(), "{name}");
        }
    }

    #[test]
    fn periods_parse_only_their_spellings() {
        assert_eq!(Period::parse("last_7_days"), Some(Period::LastDays(7)));
        assert_eq!(Period::parse("last_366_days"), Some(Period::LastDays(366)));
        assert_eq!(Period::parse("last_72_hours"), Some(Period::LastHours(72)));
        assert_eq!(Period::parse("this_week"), Some(Period::ThisWeek));
        for bad in [
            "",
            "Today",
            "last_0_days",
            "last_07_days",
            "last_367_days",
            "last_73_hours",
            "last_-1_days",
            "last_+1_days",
            "last_1_weeks",
            "last__days",
            "last_7_days ",
            "next_week",
        ] {
            assert_eq!(Period::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn calendar_periods_follow_the_local_calendar_with_monday_weeks() {
        // Sunday 2026-10-25 23:30 in Taipei (15:30 UTC).
        let now = "2026-10-25T15:30:00Z";
        let taipei = "Asia/Taipei";
        assert_eq!(
            resolved("today", now, taipei),
            (at("2026-10-24T16:00:00Z"), at("2026-10-25T16:00:00Z"))
        );
        assert_eq!(
            resolved("yesterday", now, taipei),
            (at("2026-10-23T16:00:00Z"), at("2026-10-24T16:00:00Z"))
        );
        // The week of a Sunday started on the Monday before it.
        assert_eq!(
            resolved("this_week", now, taipei),
            (at("2026-10-18T16:00:00Z"), at("2026-10-25T16:00:00Z"))
        );
        assert_eq!(
            resolved("last_week", now, taipei),
            (at("2026-10-11T16:00:00Z"), at("2026-10-18T16:00:00Z"))
        );
        assert_eq!(
            resolved("this_month", now, taipei),
            (at("2026-09-30T16:00:00Z"), at("2026-10-31T16:00:00Z"))
        );
        assert_eq!(
            resolved("last_month", now, taipei),
            (at("2026-08-31T16:00:00Z"), at("2026-09-30T16:00:00Z"))
        );
        assert_eq!(
            resolved("this_year", now, taipei),
            (at("2025-12-31T16:00:00Z"), at("2026-12-31T16:00:00Z"))
        );
        assert_eq!(
            resolved("last_year", now, taipei),
            (at("2024-12-31T16:00:00Z"), at("2025-12-31T16:00:00Z"))
        );
        // Seven local days, today included.
        assert_eq!(
            resolved("last_7_days", now, taipei),
            (at("2026-10-18T16:00:00Z"), at("2026-10-25T16:00:00Z"))
        );
        // The same instant is already 00:30 on Monday 2026-10-26 in Tokyo: a new week there.
        assert_eq!(
            resolved("this_week", now, "Asia/Tokyo"),
            (at("2026-10-25T15:00:00Z"), at("2026-11-01T15:00:00Z"))
        );
        // Rolling hours end at the next whole minute.
        assert_eq!(
            resolved("last_24_hours", "2026-10-25T15:30:20Z", taipei),
            (at("2026-10-24T15:31:00Z"), at("2026-10-25T15:31:00Z"))
        );
    }

    #[test]
    fn local_days_across_a_daylight_saving_change_have_their_own_length() {
        // New York leaves daylight saving time on Sunday 2026-11-01: that day has 25 hours.
        let (from, to) = resolved("yesterday", "2026-11-02T15:00:00Z", "America/New_York");
        assert_eq!(from, at("2026-11-01T04:00:00Z"));
        assert_eq!(to, at("2026-11-02T05:00:00Z"));
        assert_eq!((to - from).num_hours(), 25);
    }

    #[test]
    fn the_queryable_period_starts_later_than_the_databases_rounded_up_to_the_cell() {
        let now = at("2026-10-26T10:17:42.500Z");
        // Three months back is 2026-07-26 10:17:42.5; five minutes later 10:22:42.5.
        assert_eq!(
            queryable_start(Resolution::Minute, now),
            at("2026-07-26T10:23:00Z")
        );
        assert_eq!(
            queryable_start(Resolution::Hour, now),
            at("2026-07-26T11:00:00Z")
        );
        // A day the month lacks becomes its last day, as ClickHouse's subtractMonths does.
        assert_eq!(
            queryable_start(Resolution::Minute, at("2026-05-31T12:00:00Z")),
            at("2026-02-28T12:05:00Z")
        );
        // Already on a boundary: stays there.
        assert_eq!(
            queryable_start(Resolution::Hour, at("2026-10-26T09:55:00Z")),
            at("2026-07-26T10:00:00Z")
        );
        // Never earlier than now minus three months, the database's window.
        for now in [
            now,
            at("2026-03-31T23:59:59.999Z"),
            at("2026-01-01T00:00:00Z"),
        ] {
            let database = now
                .checked_sub_months(Months::new(AGGREGATED_MONTHS))
                .expect("in range");
            for table in [Resolution::Minute, Resolution::Hour] {
                assert!(
                    queryable_start(table, now) >= database + PERIOD_MARGIN,
                    "{now}"
                );
            }
        }
    }

    fn asked(
        period: Option<&'static str>,
        range: Option<(&str, &str)>,
        interval: Interval,
        series: usize,
    ) -> Asked<'static> {
        Asked {
            period,
            from: range.map(|(from, _)| at(from)),
            to: range.map(|(_, to)| at(to)),
            interval,
            series,
        }
    }

    const NOW: &str = "2026-10-26T10:17:42Z";

    fn planned(asked: &Asked<'_>, kind: Kind, zone: &str) -> Result<Plan, Refusal> {
        plan(asked, kind, at(NOW), &tz(zone))
    }

    #[test]
    fn whole_hour_ranges_in_whole_hour_zones_read_the_hour_table() {
        let days = asked(Some("last_7_days"), None, Interval::Day1, 3);
        let taipei = planned(&days, Kind::Usage, "Asia/Taipei").expect("a plan");
        assert_eq!(taipei.resolution, Resolution::Hour);
        assert_eq!(taipei.from, at("2026-10-19T16:00:00Z"));
        assert_eq!(taipei.to, at("2026-10-26T16:00:00Z"));
        assert!(taipei.ends_after_now);
        assert!(!taipei.truncated);
        // The usage reads a day before and up to now, since the range ends later.
        assert_eq!(taipei.window_from, at("2026-10-18T16:00:00Z"));
        assert_eq!(taipei.window_to, at("2026-10-26T16:00:00Z"));
        // Half-hour zones need the minute table.
        let kolkata = planned(&days, Kind::Usage, "Asia/Kolkata").expect("a plan");
        assert_eq!(kolkata.resolution, Resolution::Minute);
        assert_eq!(kolkata.from, at("2026-10-19T18:30:00Z"));
        // So do minute intervals and ranges off the hour.
        let minutes = asked(Some("today"), None, Interval::Minutes15, 1);
        assert_eq!(
            planned(&minutes, Kind::Statistics, "UTC")
                .expect("a plan")
                .resolution,
            Resolution::Minute
        );
        let rolling = asked(Some("last_24_hours"), None, Interval::Hour1, 1);
        assert_eq!(
            planned(&rolling, Kind::Statistics, "UTC")
                .expect("a plan")
                .resolution,
            Resolution::Minute
        );
        // A range ending in the past reads a day after it, for the check.
        let past = asked(
            None,
            Some(("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z")),
            Interval::Total,
            1,
        );
        let plan = planned(&past, Kind::Usage, "UTC").expect("a plan");
        assert_eq!(plan.resolution, Resolution::Hour);
        assert_eq!(plan.window_from, at("2026-09-30T00:00:00Z"));
        assert_eq!(plan.window_to, at("2026-10-03T00:00:00Z"));
        assert!(!plan.ends_after_now);
        // Statistics read only the range.
        let plan = planned(&past, Kind::Statistics, "UTC").expect("a plan");
        assert_eq!((plan.window_from, plan.window_to), (plan.from, plan.to));
    }

    #[test]
    fn ranges_are_cut_to_the_queryable_period_or_refused() {
        let year = asked(Some("this_year"), None, Interval::Month1, 1);
        let plan = planned(&year, Kind::Usage, "Asia/Taipei").expect("a plan");
        assert_eq!(plan.resolution, Resolution::Hour);
        assert!(plan.truncated);
        assert_eq!(plan.from, at("2026-07-26T11:00:00Z"));
        assert_eq!(plan.earliest, plan.from);
        assert_eq!(plan.window_from, plan.from, "no lookback before the period");
        let minute = asked(Some("this_year"), None, Interval::Day1, 1);
        let plan = planned(&minute, Kind::Statistics, "Asia/Kolkata").expect("a plan");
        assert_eq!(plan.resolution, Resolution::Minute);
        assert_eq!(plan.from, at("2026-07-26T10:23:00Z"));
        let before = asked(
            None,
            Some(("2026-01-01T00:00:00Z", "2026-07-26T10:23:00Z")),
            Interval::Total,
            1,
        );
        assert_eq!(
            planned(&before, Kind::Usage, "UTC"),
            Err(Refusal::OutsidePeriod(at("2026-07-26T10:23:00Z")))
        );
        // Ending inside the first hour of the period: the minute table still has it.
        let edge = asked(
            None,
            Some(("2026-07-01T00:00:00Z", "2026-07-26T11:00:00Z")),
            Interval::Total,
            1,
        );
        let plan = planned(&edge, Kind::Usage, "UTC").expect("a plan");
        assert_eq!(plan.resolution, Resolution::Minute);
        assert_eq!(plan.from, at("2026-07-26T10:23:00Z"));
    }

    #[test]
    fn the_range_and_the_limits_are_checked() {
        let both = Asked {
            period: Some("today"),
            ..asked(
                None,
                Some(("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z")),
                Interval::Total,
                1,
            )
        };
        assert_eq!(planned(&both, Kind::Usage, "UTC"), Err(Refusal::Period));
        assert_eq!(
            planned(&asked(None, None, Interval::Total, 1), Kind::Usage, "UTC"),
            Err(Refusal::Period)
        );
        assert_eq!(
            planned(
                &asked(Some("fortnight"), None, Interval::Total, 1),
                Kind::Usage,
                "UTC"
            ),
            Err(Refusal::Period)
        );
        let only_from = Asked {
            to: None,
            ..asked(
                None,
                Some(("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z")),
                Interval::Total,
                1,
            )
        };
        assert_eq!(planned(&only_from, Kind::Usage, "UTC"), Err(Refusal::Range));
        // Within the same minute: empty once floored.
        let empty = asked(
            None,
            Some(("2026-10-01T00:00:10Z", "2026-10-01T00:00:50Z")),
            Interval::Total,
            1,
        );
        assert_eq!(planned(&empty, Kind::Usage, "UTC"), Err(Refusal::Range));
        // 20 series of 15-minute buckets over a week: 20 × 674 rows.
        let rows = asked(Some("last_7_days"), None, Interval::Minutes15, 20);
        assert_eq!(
            planned(&rows, Kind::Statistics, "UTC"),
            Err(Refusal::TooManyRows)
        );
        // 14 series fit (14 × 674 = 9,436).
        let rows = asked(Some("last_7_days"), None, Interval::Minutes15, 14);
        assert!(planned(&rows, Kind::Statistics, "UTC").is_ok());
        // Usage on the minute table: 20 series over 60 days and the day before them.
        let cells = asked(Some("last_60_days"), None, Interval::Day1, 20);
        assert_eq!(
            planned(&cells, Kind::Usage, "Asia/Kolkata"),
            Err(Refusal::TooManyCells)
        );
        assert!(planned(&cells, Kind::Statistics, "Asia/Kolkata").is_ok());
        assert!(
            planned(&cells, Kind::Usage, "Asia/Taipei").is_ok(),
            "the hour table"
        );
    }
}
