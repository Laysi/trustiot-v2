//! The queryable period (rebuild checklist B3, J3): how far back a tenant's queries reach in each
//! of the three tenant views, and where a service's own clamp starts so that it never asks for
//! rows the database hides.
//!
//! ClickHouse enforces the period itself (services/ingest/migrations/clickhouse/
//! 0011_queryable_period.sql): each tenant account carries one window per table as a `CONST`
//! setting, and the row policies drop every row before `now − window`, inclusive, with `now`
//! taken from the ClickHouse server's clock at the start of the query. The service writes the
//! windows ([`crate::accounts`]) and reads them back before each query; a service that cuts a
//! range to the period ([`Windows::earliest`]) starts [`PERIOD_MARGIN`] later than the window and
//! rounds up to the view's cell, so its clamp is never wider than the database's filter as long
//! as the service's clock and ClickHouse's differ by less than the margin. Its response then
//! holds every row from its reported start on.
//!
//! The windows are the strictest plan's (docs/product/plans.md) until per-plan periods come with
//! Laysi/trustiot-v2#453: [`Windows::PLATFORM`].

use chrono::{DateTime, Months, TimeDelta, Timelike as _, Utc};

use crate::history::{AGGREGATED_MONTHS, RAW_DAYS};

/// How much later than the window a service's clamp starts: the time a query takes to reach
/// ClickHouse plus the difference between the two clocks must stay below it.
pub const PERIOD_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// Clock differences above this are logged when an account is checked (the margin still covers
/// them up to [`PERIOD_MARGIN`]).
pub const CLOCK_SKEW_WARNING: TimeDelta = TimeDelta::seconds(60);

/// The longest raw window the account settings accept (0011_queryable_period.sql caps the
/// arithmetic at it).
pub const MAX_RAW_DAYS: u32 = 36_500;
/// The longest aggregate window the account settings accept.
pub const MAX_MONTHS: u32 = 1_200;

/// One of the three tenant views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// `trustiot.tenant_telemetry`: raw readings, window in days.
    Raw,
    /// `trustiot.tenant_telemetry_1m`: one-minute cells, window in calendar months.
    Minute,
    /// `trustiot.tenant_telemetry_1h`: one-hour cells, window in calendar months.
    Hour,
}

impl View {
    /// The view's qualified name.
    #[must_use]
    pub const fn table(self) -> &'static str {
        match self {
            Self::Raw => "trustiot.tenant_telemetry",
            Self::Minute => "trustiot.tenant_telemetry_1m",
            Self::Hour => "trustiot.tenant_telemetry_1h",
        }
    }

    /// The account setting that holds the view's window.
    #[must_use]
    pub const fn setting(self) -> &'static str {
        match self {
            Self::Raw => "SQL_queryable_raw_days",
            Self::Minute => "SQL_queryable_1m_months",
            Self::Hour => "SQL_queryable_1h_months",
        }
    }
}

/// A tenant account's windows: one per table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Raw readings: whole days back from now.
    pub raw_days: u32,
    /// One-minute cells: calendar months back from now.
    pub minute_months: u32,
    /// One-hour cells: calendar months back from now.
    pub hour_months: u32,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "RAW_DAYS is a small positive constant, checked by the assertion next to it"
)]
const PLATFORM_RAW_DAYS: u32 = {
    assert!(RAW_DAYS > 0 && RAW_DAYS <= MAX_RAW_DAYS as i64);
    RAW_DAYS as u32
};

impl Windows {
    /// The fixed windows until per-plan periods (Laysi/trustiot-v2#453): the history API's
    /// [`RAW_DAYS`] for raw readings and [`AGGREGATED_MONTHS`] for both aggregates.
    pub const PLATFORM: Self = Self::new(PLATFORM_RAW_DAYS, AGGREGATED_MONTHS, AGGREGATED_MONTHS);

    /// Windows of `raw_days` days and `minute_months`, `hour_months` months.
    ///
    /// # Panics
    ///
    /// When a window is 0 or above what the account settings accept; in a constant, at compile
    /// time.
    #[must_use]
    pub const fn new(raw_days: u32, minute_months: u32, hour_months: u32) -> Self {
        assert!(raw_days > 0 && raw_days <= MAX_RAW_DAYS);
        assert!(minute_months > 0 && minute_months <= MAX_MONTHS);
        assert!(hour_months > 0 && hour_months <= MAX_MONTHS);
        Self {
            raw_days,
            minute_months,
            hour_months,
        }
    }

    /// Windows read back from an account, when each is in range.
    #[must_use]
    pub fn checked(raw_days: u32, minute_months: u32, hour_months: u32) -> Option<Self> {
        ((1..=MAX_RAW_DAYS).contains(&raw_days)
            && (1..=MAX_MONTHS).contains(&minute_months)
            && (1..=MAX_MONTHS).contains(&hour_months))
        .then_some(Self {
            raw_days,
            minute_months,
            hour_months,
        })
    }

    /// The window's value for `view`, as the account setting holds it.
    #[must_use]
    pub const fn get(self, view: View) -> u32 {
        match view {
            View::Raw => self.raw_days,
            View::Minute => self.minute_months,
            View::Hour => self.hour_months,
        }
    }

    /// Where a service's clamp of `view` starts at `now` (the service's clock): the window's
    /// start plus [`PERIOD_MARGIN`], rounded up to a whole millisecond (raw) or to the view's
    /// cell, a UTC minute or hour. Every row and cell at or after it is in the database's window
    /// as long as ClickHouse's clock is less than the margin ahead of `now`.
    #[must_use]
    pub fn earliest(self, view: View, now: DateTime<Utc>) -> DateTime<Utc> {
        let start = match view {
            View::Raw => now - TimeDelta::days(i64::from(self.raw_days)),
            View::Minute | View::Hour => now
                .checked_sub_months(Months::new(self.get(view)))
                .unwrap_or(DateTime::<Utc>::MIN_UTC),
        };
        let start = start + PERIOD_MARGIN;
        match view {
            View::Raw => ceil(start, TimeDelta::milliseconds(1)),
            View::Minute => ceil(start, TimeDelta::minutes(1)),
            View::Hour => ceil(start, TimeDelta::hours(1)),
        }
    }
}

/// `time` rounded up to a multiple of `unit` since the Unix epoch (`unit` divides a day).
fn ceil(time: DateTime<Utc>, unit: TimeDelta) -> DateTime<Utc> {
    let unit_nanos = unit.num_nanoseconds().unwrap_or(i64::MAX);
    let since_midnight = time.num_seconds_from_midnight();
    let into =
        (i64::from(since_midnight) * 1_000_000_000 + i64::from(time.nanosecond())) % unit_nanos;
    if into == 0 {
        time
    } else {
        time + TimeDelta::nanoseconds(unit_nanos - into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("an RFC 3339 time")
    }

    #[test]
    fn the_clamp_starts_after_the_window_by_the_margin_rounded_up_to_the_cell() {
        let windows = Windows::PLATFORM;
        assert_eq!(windows, Windows::new(7, 3, 3));
        let now = at("2026-10-21T12:34:56.789123Z");
        assert_eq!(
            windows.earliest(View::Raw, now),
            at("2026-10-14T12:39:56.790Z"),
            "seven days back, five minutes later, a whole millisecond"
        );
        assert_eq!(
            windows.earliest(View::Minute, now),
            at("2026-07-21T12:40:00Z")
        );
        assert_eq!(
            windows.earliest(View::Hour, now),
            at("2026-07-21T13:00:00Z")
        );
        // Already on a boundary after the margin: unchanged.
        assert_eq!(
            windows.earliest(View::Hour, at("2026-10-21T11:55:00Z")),
            at("2026-07-21T12:00:00Z")
        );
    }

    #[test]
    fn months_keep_the_day_or_take_the_last_day_of_a_shorter_month() {
        // As ClickHouse's subtraction of months does (2026-05-31 − 3 months = 2026-02-28).
        let windows = Windows::new(7, 3, 1);
        assert_eq!(
            windows.earliest(View::Minute, at("2026-05-31T12:00:00Z")),
            at("2026-02-28T12:05:00Z")
        );
        assert_eq!(
            windows.earliest(View::Minute, at("2028-05-31T12:00:00Z")),
            at("2028-02-29T12:05:00Z")
        );
        assert_eq!(
            windows.earliest(View::Hour, at("2026-03-31T00:30:00Z")),
            at("2026-02-28T01:00:00Z")
        );
    }

    #[test]
    fn the_clamp_is_never_earlier_than_the_window_a_little_later() {
        // ClickHouse evaluates the window when the query starts, after the service took `now`:
        // up to the margin later, its window still starts no later than the clamp.
        let windows = Windows::new(7, 3, 3);
        let mut now = at("2026-01-28T23:50:00Z");
        while now < at("2026-04-02T00:00:00Z") {
            for view in [View::Raw, View::Minute, View::Hour] {
                let clamp = windows.earliest(view, now);
                let later = now + PERIOD_MARGIN;
                let database = match view {
                    View::Raw => later - TimeDelta::days(7),
                    View::Minute | View::Hour => {
                        later.checked_sub_months(Months::new(3)).expect("in range")
                    }
                };
                assert!(database <= clamp, "{view:?} at {now}: {database} > {clamp}");
            }
            now += TimeDelta::minutes(37);
        }
    }

    #[test]
    fn windows_outside_the_settings_range_are_not_accepted() {
        assert_eq!(Windows::checked(7, 3, 3), Some(Windows::PLATFORM));
        assert_eq!(Windows::checked(0, 3, 3), None);
        assert_eq!(Windows::checked(7, 0, 3), None);
        assert_eq!(Windows::checked(7, 3, MAX_MONTHS + 1), None);
        assert_eq!(Windows::checked(MAX_RAW_DAYS + 1, 3, 3), None);
        assert_eq!(View::Hour.setting(), "SQL_queryable_1h_months");
        assert_eq!(View::Minute.table(), "trustiot.tenant_telemetry_1m");
    }
}
