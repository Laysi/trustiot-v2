//! The semantic API (Laysi/trustiot-v2#142; services/telemetry-query/README.md, 「語意查詢」):
//! the statistics and the usage of up to [`range::MAX_SERIES`] data points over a range, in
//! buckets of the requested time zone, from the one-minute and one-hour aggregates. The assistant's
//! `telemetry_statistics` and `telemetry_usage` tools send their input here unchanged, and a
//! dashboard (Laysi/trustiot-v2#150) can store that input and send it again as its viewer.
//!
//! Every query runs as the caller's tenant account after [`Accounts::verify`], like the history
//! API's; the tenant comes only from the caller's internal identity, and a request that names
//! anything else (an unknown field such as `tenant_id`) is refused. A device of another tenant
//! has no data.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | A user who has not picked or joined a tenant | 403 `permission_denied` |
//! | A body that is not the JSON object described (an unknown field included) | 400 `invalid_request` (`telemetry.query_invalid`) |
//! | A field that breaks a rule (series, time zone, period, range; a `to` later than [`range::latest_end`]) | 422 `validation_failed` |
//! | The answer would hold more than [`range::MAX_ROWS`] rows | 422 `validation_failed` (`telemetry.too_many_points`) |
//! | The usage would read more than [`range::MAX_MINUTE_CELLS`] one-minute cells | 422 `validation_failed` (`telemetry.query_too_heavy`, detail `parameter`: `series`) |
//! | The range lies wholly before the queryable period | 403 `quota_exceeded` (`telemetry.outside_queryable_period`) |
//! | The tenant's account has not been created yet | 404 `not_found` |
//! | ClickHouse limits, quota, time limit, failure | as the history API |
//! | Otherwise | 200 with [`Statistics`] or [`Usage`] |
//!
//! [`Accounts::verify`]: crate::accounts::Accounts::verify

pub mod range;
pub mod statements;

use std::collections::{BTreeMap, BTreeSet};

use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::Response;
use axum::routing::post;
use chrono::{DateTime, FixedOffset, Utc};
use contract::{ErrorBody, ErrorCode, MessageKey, TenantId};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use service_kit::identity::UserIdentity;
use utoipa::ToSchema;
use uuid::Uuid;

use self::range::{Asked, Interval, Kind, Plan, Refusal, Resolution, Unit};
use crate::history::{
    Failure, OUTSIDE_PERIOD, POINT_INVALID, QUERY_INVALID, QUERY_TOO_HEAVY, RANGE_INVALID, Reading,
    ReadingColumns, Routes, TIME_ZONE_INVALID, TOO_MANY_POINTS, clickhouse_error, error, invalid,
    is_unknown_time_zone, ok, reading, rows, unexpected_answer,
};
use crate::period::Windows;

/// The routes, as the ingress gateway forwards them (no prefix is stripped).
pub const STATISTICS_PATH: &str = "/api/telemetry/v1/statistics";
pub const USAGE_PATH: &str = "/api/telemetry/v1/usage";

/// Longest data point key: the telemetry table's limit; the key's characters are those of
/// [`is_point_key`].
pub const MAX_POINT_LEN: usize = crate::history::MAX_POINT_LEN;

/// The cumulative check (README, 「累計檢查」): a series behaves like a counter when it fell at
/// most this many times in the window read...
pub const COUNTER_MAX_DROPS: u64 = 2;
/// ...or in at most this many percent of the comparisons where it rose.
pub const COUNTER_MAX_DROP_PERCENT: u64 = 2;
/// Fewer comparisons than this cannot confirm a counter: the usage is computed and flagged
/// `too_few_readings`.
pub const MIN_COMPARISONS: u64 = 10;
/// A gap: a spacing between readings of at least this many times the series' median spacing.
pub const GAP_FACTOR: f64 = 1.5;
/// A suspicious step: a positive difference whose rate, over at least the median spacing, is
/// more than this many times the series' median rate of rises...
pub const SUSPICIOUS_FACTOR: f64 = 20.0;
/// ...when the window holds at least this many rises to take the median of.
pub const MIN_RISES_FOR_SUSPICION: u32 = 10;

const SERIES_INVALID: MessageKey = MessageKey::new("telemetry.series_invalid");
const PERIOD_INVALID: MessageKey = MessageKey::new("telemetry.period_invalid");

/// One data point of one device.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeriesKey {
    /// The device.
    pub device_id: Uuid,
    /// The data point key: 1 to 128 letters, digits, `.`, `_` or `-`, starting with a letter or
    /// digit.
    #[schema(
        min_length = 1,
        max_length = 128,
        pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"
    )]
    pub point: String,
}

/// A semantic request: which series, which range in which time zone, and the bucket length. The
/// tenant is the caller's; nothing else can name one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeriesQuery {
    /// 1 to 20 distinct series.
    #[schema(min_items = 1, max_items = 20)]
    pub series: Vec<SeriesKey>,
    /// An IANA time zone name, such as `Asia/Taipei`: the local hours, days, Monday weeks and
    /// months the buckets and the period follow, and the offset of every time in the answer.
    #[schema(example = "Asia/Taipei")]
    pub tz: String,
    /// A period relative to the request's time: `today`, `yesterday`, `this_week`, `last_week`,
    /// `this_month`, `last_month`, `this_year`, `last_year` (whole local periods, weeks from
    /// Monday), `last_<N>_days` (N local days with today, 1 to 366) or `last_<N>_hours` (up to
    /// now, 1 to 72). Either this or `from` and `to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        pattern = "^(today|yesterday|this_week|last_week|this_month|last_month|this_year|last_year|last_[1-9][0-9]{0,2}_days|last_[1-9][0-9]?_hours)$"
    )]
    pub period: Option<String>,
    /// The range's start, RFC 3339 with an offset (inclusive), rounded down to the minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// The range's end, RFC 3339 with an offset (exclusive), rounded down to the minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    /// The bucket length; `total` (the default) gives one bucket for the whole range.
    #[serde(default)]
    pub interval: Interval,
}

/// The range an answer covers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct QueryRange {
    /// The time zone the buckets follow; every time in the answer has its offset.
    pub tz: String,
    pub interval: Interval,
    /// The range read: `from` inclusive, `to` exclusive. `from` is later than requested when the
    /// request started before the queryable period (`truncated`).
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
    /// Whether `from` was moved to the start of the queryable period.
    pub truncated: bool,
    /// The start of the queryable period of the table read, at the time of the request.
    pub earliest: DateTime<FixedOffset>,
    /// Whether the range ends after the request's time (a period still running, such as
    /// `today`): its last bucket is not complete.
    pub ends_after_now: bool,
    /// The aggregate table read: one-minute or one-hour cells. A minimum's or maximum's time is
    /// the start of its cell.
    pub resolution: Resolution,
    /// When the request was answered.
    pub queried_at: DateTime<FixedOffset>,
}

/// Whether a series had readings in the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataStatus {
    Ok,
    /// No reading in the range: the data is insufficient (資料不足).
    NoData,
}

/// A reading and when it was taken.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TimedReading {
    pub time: DateTime<FixedOffset>,
    pub value: Reading,
}

/// A number and the time it belongs to (a bucket's start, or a reading's time).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TimedValue {
    pub time: DateTime<FixedOffset>,
    pub value: f64,
}

/// One bucket of a series' statistics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StatisticsRow {
    /// The bucket's start.
    pub time: DateTime<FixedOffset>,
    /// Readings in the bucket.
    pub count: u64,
    /// The smallest, largest and mean numeric value (a bool counts as 0 or 1); `null` when the
    /// bucket holds only text readings.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    /// The bucket's latest reading.
    pub last: Reading,
}

/// A series' statistics over the range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SeriesStatistics {
    pub device_id: Uuid,
    pub point: String,
    pub status: DataStatus,
    /// Readings in the range, and how many of them are numeric (float, int, bool).
    pub count: u64,
    pub numeric_count: u64,
    /// The smallest value and the start of the cell it was in.
    pub min: Option<f64>,
    pub min_time: Option<DateTime<FixedOffset>>,
    /// The largest value and the start of the cell it was in.
    pub max: Option<f64>,
    pub max_time: Option<DateTime<FixedOffset>>,
    /// The mean of the numeric readings; for an on/off point, the share of readings that were on.
    pub avg: Option<f64>,
    /// The first and last reading of the range.
    pub first: Option<TimedReading>,
    pub last: Option<TimedReading>,
    /// The buckets with the highest and lowest mean (not for `total`).
    pub highest: Option<TimedValue>,
    pub lowest: Option<TimedValue>,
    /// The buckets with readings, in time order; buckets without readings are left out.
    pub rows: Vec<StatisticsRow>,
}

/// Statistics of each series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Statistics {
    pub range: QueryRange,
    /// One entry per requested series, in the request's order.
    pub series: Vec<SeriesStatistics>,
}

/// Whether a series' usage could be computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    /// No reading in the range: the data is insufficient (資料不足).
    NoData,
    /// The data point is on/off or text: use the statistics.
    NotNumeric,
    /// The data point does not behave like a counter (it falls too often): use the statistics.
    NotCumulative,
}

/// What the cumulative check concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The series behaves like a counter.
    Cumulative,
    /// Too few comparisons to tell: the usage is computed but unconfirmed.
    TooFewReadings,
    /// The series falls too often for a counter.
    NotCumulative,
}

/// The cumulative check over the window read around the range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CumulativeCheck {
    pub verdict: Verdict,
    /// How often the value fell, rose, and was compared with the one before.
    pub drops: u64,
    pub rises: u64,
    pub comparisons: u64,
    /// The window the check read.
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
}

/// A spacing between two readings of at least [`GAP_FACTOR`] times the usual one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Gap {
    /// The reading before the gap and the one after it.
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
}

/// One bucket of a series' usage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UsageRow {
    /// The bucket's start.
    pub time: DateTime<FixedOffset>,
    /// The increase of the counter in the bucket.
    pub usage: f64,
    /// Readings in the bucket.
    pub readings: u64,
    /// Drops counted as a reset or wrap in the bucket.
    pub drops: u64,
    /// The bucket starts after a gap: its usage includes the usage of the gap.
    pub after_gap: bool,
    /// Steps in the bucket much faster than the series usually rises (a misread or a replaced
    /// meter): the usage includes them.
    pub suspicious_steps: u64,
}

/// A series' usage over the range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SeriesUsage {
    pub device_id: Uuid,
    pub point: String,
    pub status: UsageStatus,
    /// The cumulative check; absent without data.
    pub check: Option<CumulativeCheck>,
    /// The usage over the range; absent unless `status` is `ok`.
    pub total: Option<f64>,
    /// Drops in the range, each counted as a reset or wrap that started again from 0: each loses
    /// at most the usage of one sampling interval.
    pub drops: u64,
    /// Readings in the range.
    pub readings: u64,
    /// The first and last numeric reading in the range.
    pub first_reading: Option<TimedValue>,
    pub last_reading: Option<TimedValue>,
    /// Whether a reading before the range was found, so the usage up to the first reading in the
    /// range is counted.
    pub baseline: bool,
    /// Gaps in the range, and the longest.
    pub gaps: u64,
    pub longest_gap: Option<Gap>,
    /// Steps much faster than the series usually rises, and the largest: the total includes them,
    /// and they may be a meter misread.
    pub suspicious_steps: u64,
    pub largest_suspicious_step: Option<TimedValue>,
    /// Cells left out because their first or last value was not a finite number.
    pub skipped_cells: u64,
    /// The buckets with the highest and lowest usage (not for `total`).
    pub highest: Option<TimedValue>,
    pub lowest: Option<TimedValue>,
    /// The buckets with readings, in time order; a bucket without readings is left out (unknown,
    /// not zero), and the usage across a gap is in the first bucket after it.
    pub rows: Vec<UsageRow>,
}

/// Usage of each series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    pub range: QueryRange,
    /// One entry per requested series, in the request's order.
    pub series: Vec<SeriesUsage>,
}

/// The routes: [`STATISTICS_PATH`] and [`USAGE_PATH`].
pub fn routes(state: Routes) -> Router {
    Router::new()
        .route(STATISTICS_PATH, post(post_statistics))
        .route(USAGE_PATH, post(post_usage))
        .with_state(state)
}

/// Whether `point` is a data point key the API accepts: 1 to [`MAX_POINT_LEN`] ASCII letters,
/// digits, `.`, `_` or `-`, starting with a letter or digit (the protocol's keys, and the lab
/// datasets').
pub fn is_point_key(point: &str) -> bool {
    point.len() <= MAX_POINT_LEN
        && point
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && point
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The cumulative check's verdict on a window with `drops` falls and `rises` rises in
/// `comparisons` comparisons.
pub const fn verdict(drops: u64, rises: u64, comparisons: u64) -> Verdict {
    let counter = drops <= COUNTER_MAX_DROPS
        || drops.saturating_mul(100) <= COUNTER_MAX_DROP_PERCENT.saturating_mul(rises);
    if !counter {
        Verdict::NotCumulative
    } else if comparisons < MIN_COMPARISONS {
        Verdict::TooFewReadings
    } else {
        Verdict::Cumulative
    }
}

/// `time` with the offset `tz` has then.
fn local(time: DateTime<Utc>, tz: &TimeZone) -> DateTime<FixedOffset> {
    let offset = jiff::Timestamp::from_millisecond(time.timestamp_millis())
        .map(|instant| tz.to_offset(instant).seconds())
        .unwrap_or(0);
    let offset = FixedOffset::east_opt(offset).unwrap_or_else(|| {
        FixedOffset::east_opt(0).unwrap_or_else(|| unreachable!("UTC is an offset"))
    });
    time.with_timezone(&offset)
}

fn local_millis(millis: i64, tz: &TimeZone) -> Option<DateTime<FixedOffset>> {
    DateTime::from_timestamp_millis(millis).map(|time| local(time, tz))
}

fn local_seconds(seconds: i64, tz: &TimeZone) -> Option<DateTime<FixedOffset>> {
    local_millis(seconds.checked_mul(1000)?, tz)
}

/// A checked request.
struct Checked {
    tz: TimeZone,
    plan: Plan,
    now: DateTime<Utc>,
}

/// The range as `query` asks it.
fn asked(query: &SeriesQuery) -> Asked<'_> {
    Asked {
        period: query.period.as_deref(),
        from: query.from,
        to: query.to,
        interval: query.interval,
        series: query.series.len(),
    }
}

/// Checks `query` against the rules that need neither the account nor its windows (the series,
/// the time zone and the range asked, at `now`), before the account is looked up; the time zone.
fn validate(query: &SeriesQuery, now: DateTime<Utc>) -> Result<TimeZone, Box<Response>> {
    let distinct: BTreeSet<&SeriesKey> = query.series.iter().collect();
    if query.series.is_empty()
        || query.series.len() > range::MAX_SERIES
        || distinct.len() != query.series.len()
    {
        return Err(Box::new(invalid("series", SERIES_INVALID)));
    }
    if !query.series.iter().all(|key| is_point_key(&key.point)) {
        return Err(Box::new(invalid("series", POINT_INVALID)));
    }
    let Some(tz) = range::time_zone(&query.tz) else {
        return Err(Box::new(invalid("tz", TIME_ZONE_INVALID)));
    };
    range::resolve(&asked(query), now, &tz).map_err(|refusal| Box::new(refused(refusal, &tz)))?;
    Ok(tz)
}

/// Checks `query` against the rules and plans it at `now` for an account whose views keep
/// `windows` (read back when the account was checked).
fn check(
    query: &SeriesQuery,
    kind: Kind,
    windows: Windows,
    now: DateTime<Utc>,
) -> Result<Checked, Box<Response>> {
    let tz = validate(query, now)?;
    let plan = range::plan(&asked(query), kind, windows, now, &tz)
        .map_err(|refusal| Box::new(refused(refusal, &tz)))?;
    Ok(Checked { tz, plan, now })
}

/// The response that refuses a request's range or size.
fn refused(refusal: Refusal, tz: &TimeZone) -> Response {
    match refusal {
        Refusal::Period => invalid("period", PERIOD_INVALID),
        Refusal::Range => invalid("from", RANGE_INVALID),
        Refusal::TooLate(latest) => error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(RANGE_INVALID)
                .with_detail("parameter", "to")
                .with_detail("latest", local(latest, tz).to_rfc3339()),
        ),
        Refusal::OutsidePeriod(earliest) => error(
            ErrorBody::new(ErrorCode::QuotaExceeded)
                .with_message_key(OUTSIDE_PERIOD)
                .with_detail("earliest", local(earliest, tz).to_rfc3339()),
        ),
        Refusal::TooManyRows => error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(TOO_MANY_POINTS)
                .with_detail("parameter", "interval")
                .with_detail("max", range::MAX_ROWS),
        ),
        // Fewer series or a shorter range read fewer cells; the number is not the user's.
        Refusal::TooManyCells => error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(QUERY_TOO_HEAVY)
                .with_detail("parameter", "series"),
        ),
    }
}

/// The series as the text of a ClickHouse `Array(Tuple(UUID, String))` parameter: single quotes,
/// with backslashes and single quotes escaped (the keys allow neither, but the encoding does not
/// rely on it). Public for the integration test that binds hostile keys below the API.
pub fn series_parameter(series: &[SeriesKey]) -> String {
    let quoted = |text: &str| format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"));
    let items: Vec<String> = series
        .iter()
        .map(|key| {
            format!(
                "({}, {})",
                quoted(&key.device_id.hyphenated().to_string()),
                quoted(&key.point)
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// The statement for an answer, a table and a unit, and the `step` it takes.
fn statement(kind: Kind, resolution: Resolution, unit: Unit) -> (&'static str, u32) {
    use statements::*;
    let step = match unit {
        Unit::Minutes(step) => step,
        _ => 1,
    };
    let statement = match (kind, resolution, unit) {
        (Kind::Statistics, Resolution::Minute, Unit::Minutes(_)) => STATISTICS_1M_MINUTES,
        (Kind::Statistics, Resolution::Minute, Unit::Hour) => STATISTICS_1M_HOUR,
        (Kind::Statistics, Resolution::Minute, Unit::Day) => STATISTICS_1M_DAY,
        (Kind::Statistics, Resolution::Minute, Unit::Week) => STATISTICS_1M_WEEK,
        (Kind::Statistics, Resolution::Minute, Unit::Month) => STATISTICS_1M_MONTH,
        (Kind::Statistics, Resolution::Minute, Unit::Total) => STATISTICS_1M_TOTAL,
        // The planner never puts minute buckets on the hour table; the hour statement would
        // group by hours, so the minute table is used.
        (Kind::Statistics, Resolution::Hour, Unit::Minutes(_)) => STATISTICS_1M_MINUTES,
        (Kind::Statistics, Resolution::Hour, Unit::Hour) => STATISTICS_1H_HOUR,
        (Kind::Statistics, Resolution::Hour, Unit::Day) => STATISTICS_1H_DAY,
        (Kind::Statistics, Resolution::Hour, Unit::Week) => STATISTICS_1H_WEEK,
        (Kind::Statistics, Resolution::Hour, Unit::Month) => STATISTICS_1H_MONTH,
        (Kind::Statistics, Resolution::Hour, Unit::Total) => STATISTICS_1H_TOTAL,
        (Kind::Usage, Resolution::Minute, Unit::Minutes(_)) => USAGE_1M_MINUTES,
        (Kind::Usage, Resolution::Minute, Unit::Hour) => USAGE_1M_HOUR,
        (Kind::Usage, Resolution::Minute, Unit::Day) => USAGE_1M_DAY,
        (Kind::Usage, Resolution::Minute, Unit::Week) => USAGE_1M_WEEK,
        (Kind::Usage, Resolution::Minute, Unit::Month) => USAGE_1M_MONTH,
        (Kind::Usage, Resolution::Minute, Unit::Total) => USAGE_1M_TOTAL,
        (Kind::Usage, Resolution::Hour, Unit::Minutes(_)) => USAGE_1M_MINUTES,
        (Kind::Usage, Resolution::Hour, Unit::Hour) => USAGE_1H_HOUR,
        (Kind::Usage, Resolution::Hour, Unit::Day) => USAGE_1H_DAY,
        (Kind::Usage, Resolution::Hour, Unit::Week) => USAGE_1H_WEEK,
        (Kind::Usage, Resolution::Hour, Unit::Month) => USAGE_1H_MONTH,
        (Kind::Usage, Resolution::Hour, Unit::Total) => USAGE_1H_TOTAL,
    };
    (statement, step)
}

/// Checks `query`, looks up the caller's tenant and checks its account, plans `query` with the
/// account's windows and runs it as the account: the tenant, the plan and the response body.
/// The rules that need no account refuse a request before anything is looked up.
async fn answer(
    state: &Routes,
    user: &UserIdentity,
    query: &SeriesQuery,
    kind: Kind,
) -> Result<(TenantId, Checked, String), Box<Response>> {
    validate(query, Utc::now())?;
    let tenant = match state.tenant(user).await {
        Ok(tenant) => tenant,
        Err(Failure::Response(response)) => return Err(response),
        Err(Failure::ClickHouse(_)) => unreachable!("tenant() does not query ClickHouse"),
    };
    let windows = match state.verified(tenant).await {
        Ok(windows) => windows,
        Err(Failure::Response(response)) => return Err(response),
        Err(Failure::ClickHouse(failure)) => {
            return Err(Box::new(clickhouse_error(tenant, &failure)));
        }
    };
    // "Now" once the account is checked: the database takes its own later still, which the
    // queryable start's margin covers.
    let checked = check(query, kind, windows, Utc::now())?;
    let body = run(state, tenant, query, kind, &checked).await?;
    Ok((tenant, checked, body))
}

/// Runs the planned statement for `query` as `tenant`'s account, which [`answer`] checked; the
/// response body.
async fn run(
    state: &Routes,
    tenant: TenantId,
    query: &SeriesQuery,
    kind: Kind,
    checked: &Checked,
) -> Result<String, Box<Response>> {
    let plan = &checked.plan;
    let (statement, step) = statement(kind, plan.resolution, plan.unit);
    let tenant_text = tenant.to_string();
    let series = series_parameter(&query.series);
    let seconds = |time: DateTime<Utc>| time.timestamp().to_string();
    let (from, to) = (seconds(plan.from), seconds(plan.to));
    // The window's end may be "now": rounded up to the second, so its cell is in.
    let window_to = (plan.window_to.timestamp()
        + i64::from(plan.window_to.timestamp_subsec_millis() > 0))
    .to_string();
    let window_from = seconds(plan.window_from);
    let step = step.to_string();
    let limit = (range::MAX_ROWS + 1).to_string();
    let (gap_factor, factor, min_rises) = (
        GAP_FACTOR.to_string(),
        SUSPICIOUS_FACTOR.to_string(),
        MIN_RISES_FOR_SUSPICION.to_string(),
    );
    let mut params = vec![
        ("tenant", tenant_text.as_str()),
        ("series", series.as_str()),
        ("from", from.as_str()),
        ("to", to.as_str()),
        ("tz", query.tz.as_str()),
        ("step", step.as_str()),
        ("limit", limit.as_str()),
    ];
    if kind == Kind::Usage {
        params.extend([
            ("window_from", window_from.as_str()),
            ("window_to", window_to.as_str()),
            ("gap_factor", gap_factor.as_str()),
            ("factor", factor.as_str()),
            ("min_rises", min_rises.as_str()),
        ]);
    }
    match state.run_verified(tenant, statement, &params).await {
        Ok(body) => Ok(body),
        Err(Failure::Response(response)) => Err(response),
        Err(Failure::ClickHouse(failure)) if is_unknown_time_zone(&failure) => {
            Err(Box::new(invalid("tz", TIME_ZONE_INVALID)))
        }
        Err(Failure::ClickHouse(failure)) => Err(Box::new(clickhouse_error(tenant, &failure))),
    }
}

fn query_range(query: &SeriesQuery, checked: &Checked) -> QueryRange {
    let (plan, tz) = (&checked.plan, &checked.tz);
    QueryRange {
        tz: query.tz.clone(),
        interval: query.interval,
        from: local(plan.from, tz),
        to: local(plan.to, tz),
        truncated: plan.truncated,
        earliest: local(plan.earliest, tz),
        ends_after_now: plan.ends_after_now,
        resolution: plan.resolution,
        queried_at: local(checked.now, tz),
    }
}

/// The parsed request, or the response that refuses it.
fn parsed(
    body: Result<axum::Json<SeriesQuery>, JsonRejection>,
) -> Result<SeriesQuery, Box<Response>> {
    body.map(|axum::Json(query)| query).map_err(|_| {
        Box::new(error(
            ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(QUERY_INVALID),
        ))
    })
}

/// The statistics of each series over a range, in buckets of the requested time zone, from the
/// one-minute or one-hour aggregates.
#[utoipa::path(
    post,
    path = "/api/telemetry/v1/statistics",
    tag = "telemetry",
    operation_id = "queryTelemetryStatistics",
    request_body = SeriesQuery,
    responses(
        (status = 200, description = "The statistics of each series, in the request's order.", body = Statistics),
        (status = 400, description = "The body is not the JSON object described, or has a field it does not describe (`invalid_request`, `telemetry.query_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`).", body = ErrorBody),
        (status = 404, description = "The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`).", body = ErrorBody),
        (status = 422, description = "A field breaks a rule (`validation_failed`, detail `parameter`: `series`, `tz`, `period`, `from`, or `to` with detail `latest` when it ends too far after now), the answer would have too many rows (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`).", body = ErrorBody),
        (status = 429, description = "The tenant's query quota or concurrency limit is used up (`rate_limited`).", body = ErrorBody),
        (status = 503, description = "Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`).", body = ErrorBody),
        (status = 504, description = "The query ran into its time limit (`timeout`).", body = ErrorBody),
    )
)]
pub(crate) async fn post_statistics(
    State(state): State<Routes>,
    user: UserIdentity,
    body: Result<axum::Json<SeriesQuery>, JsonRejection>,
) -> Response {
    let query = match parsed(body) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let (tenant, checked, body) = match answer(&state, &user, &query, Kind::Statistics).await {
        Ok(answer) => answer,
        Err(response) => return *response,
    };
    let Some(cells) = rows(&body, |cells: StatisticsCells| Some(cells)) else {
        return unexpected_answer(tenant);
    };
    match statistics(&query, &checked, cells) {
        Some(answer) => ok(&answer),
        None => unexpected_answer(tenant),
    }
}

/// The usage of each cumulative series over a range, in buckets of the requested time zone, from
/// the one-minute or one-hour aggregates, after a check that the series behaves like a counter.
#[utoipa::path(
    post,
    path = "/api/telemetry/v1/usage",
    tag = "telemetry",
    operation_id = "queryTelemetryUsage",
    request_body = SeriesQuery,
    responses(
        (status = 200, description = "The usage of each series, in the request's order; a series that is not a counter or not numeric has a `status` that says so and no usage.", body = Usage),
        (status = 400, description = "The body is not the JSON object described, or has a field it does not describe (`invalid_request`, `telemetry.query_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`).", body = ErrorBody),
        (status = 404, description = "The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`).", body = ErrorBody),
        (status = 422, description = "A field breaks a rule (`validation_failed`, detail `parameter`: `series`, `tz`, `period`, `from`, or `to` with detail `latest` when it ends too far after now), the answer would have too many rows (`telemetry.too_many_points`), or the request would read too many one-minute cells (detail `parameter`: `series`) or the query reads or holds too much (`telemetry.query_too_heavy`).", body = ErrorBody),
        (status = 429, description = "The tenant's query quota or concurrency limit is used up (`rate_limited`).", body = ErrorBody),
        (status = 503, description = "Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`).", body = ErrorBody),
        (status = 504, description = "The query ran into its time limit (`timeout`).", body = ErrorBody),
    )
)]
pub(crate) async fn post_usage(
    State(state): State<Routes>,
    user: UserIdentity,
    body: Result<axum::Json<SeriesQuery>, JsonRejection>,
) -> Response {
    let query = match parsed(body) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let (tenant, checked, body) = match answer(&state, &user, &query, Kind::Usage).await {
        Ok(answer) => answer,
        Err(response) => return *response,
    };
    let Some(cells) = rows(&body, |cells: UsageCells| Some(cells)) else {
        return unexpected_answer(tenant);
    };
    match usage(&query, &checked, cells) {
        Some(answer) => ok(&answer),
        None => unexpected_answer(tenant),
    }
}

/// A statistics row as ClickHouse writes it ([`statements`]): device, point, bucket start
/// (seconds), readings, numeric readings, sum, minimum and its cell (seconds), maximum and its
/// cell, first and last reading (time in ms, then the typed value).
#[derive(Debug, Deserialize)]
struct StatisticsCells(
    String,
    String,
    i64,
    u64,
    u64,
    Option<f64>,
    Option<f64>,
    Option<i64>,
    Option<f64>,
    Option<i64>,
    (i64, String, Option<f64>, i64, bool, String),
    (i64, String, Option<f64>, i64, bool, String),
);

/// A usage row as ClickHouse writes it ([`statements`]).
#[derive(Debug, Deserialize)]
struct UsageCells {
    device: String,
    point: String,
    bucket: i64,
    usage: Option<f64>,
    readings: u64,
    drops: u64,
    skipped: u64,
    non_numeric: u64,
    usable: u64,
    gaps: u64,
    longest_gap_ms: i64,
    longest_gap_from: i64,
    suspicious: u64,
    inside_suspicious: Option<f64>,
    inside_suspicious_at: i64,
    across_suspicious: Option<f64>,
    across_suspicious_at: i64,
    first_at: i64,
    first: Option<f64>,
    last_at: i64,
    last: Option<f64>,
    check_drops: u64,
    check_rises: u64,
    check_comparisons: u64,
    window_first: i64,
}

fn split_reading(
    (millis, kind, float, int, boolean, text): (i64, String, Option<f64>, i64, bool, String),
) -> Option<(i64, Reading)> {
    let columns: ReadingColumns = (kind, float, int, boolean, text);
    Some((millis, reading(columns)?))
}

/// The key ClickHouse writes a series under.
fn key_text(key: &SeriesKey) -> (String, String) {
    (key.device_id.hyphenated().to_string(), key.point.clone())
}

/// The highest and lowest of `values` (time, value), the first of equals.
fn extremes(
    values: impl Iterator<Item = (DateTime<FixedOffset>, f64)>,
) -> (Option<TimedValue>, Option<TimedValue>) {
    let mut highest: Option<TimedValue> = None;
    let mut lowest: Option<TimedValue> = None;
    for (time, value) in values {
        if highest.as_ref().is_none_or(|best| value > best.value) {
            highest = Some(TimedValue { time, value });
        }
        if lowest.as_ref().is_none_or(|best| value < best.value) {
            lowest = Some(TimedValue { time, value });
        }
    }
    (highest, lowest)
}

/// The statistics answer from the rows; `None` for a row that cannot be read.
fn statistics(
    query: &SeriesQuery,
    checked: &Checked,
    cells: Vec<StatisticsCells>,
) -> Option<Statistics> {
    let tz = &checked.tz;
    let mut by_series: BTreeMap<(String, String), Vec<StatisticsCells>> = BTreeMap::new();
    for cell in cells {
        by_series
            .entry((cell.0.clone(), cell.1.clone()))
            .or_default()
            .push(cell);
    }
    let mut series = Vec::with_capacity(query.series.len());
    for key in &query.series {
        let cells = by_series.remove(&key_text(key)).unwrap_or_default();
        let mut entry = SeriesStatistics {
            device_id: key.device_id,
            point: key.point.clone(),
            status: if cells.is_empty() {
                DataStatus::NoData
            } else {
                DataStatus::Ok
            },
            count: 0,
            numeric_count: 0,
            min: None,
            min_time: None,
            max: None,
            max_time: None,
            avg: None,
            first: None,
            last: None,
            highest: None,
            lowest: None,
            rows: Vec::with_capacity(cells.len()),
        };
        let mut sum = 0.0;
        let mut first: Option<(i64, Reading)> = None;
        let mut last: Option<(i64, Reading)> = None;
        for StatisticsCells(
            _,
            _,
            bucket,
            count,
            numeric,
            value_sum,
            min,
            min_at,
            max,
            max_at,
            first_cell,
            last_cell,
        ) in cells
        {
            let time = local_seconds(bucket, tz)?;
            entry.count += count;
            entry.numeric_count += numeric;
            // A bucket without numeric readings adds nothing; one whose sum is not finite (a NaN
            // or infinite reading) leaves the series without an average.
            if numeric > 0 {
                sum += value_sum.unwrap_or(f64::NAN);
            }
            if let Some(min) = min
                && entry.min.is_none_or(|best| min < best)
            {
                entry.min = Some(min);
                entry.min_time = Some(local_seconds(min_at?, tz)?);
            }
            if let Some(max) = max
                && entry.max.is_none_or(|best| max > best)
            {
                entry.max = Some(max);
                entry.max_time = Some(local_seconds(max_at?, tz)?);
            }
            let first_cell = split_reading(first_cell)?;
            if first.as_ref().is_none_or(|(at, _)| first_cell.0 < *at) {
                first = Some(first_cell);
            }
            let last_cell = split_reading(last_cell)?;
            let row_last = last_cell.1.clone();
            if last.as_ref().is_none_or(|(at, _)| last_cell.0 > *at) {
                last = Some(last_cell);
            }
            let avg = (numeric > 0).then(|| value_sum.unwrap_or(f64::NAN) / numeric as f64);
            entry.rows.push(StatisticsRow {
                time,
                count,
                min,
                max,
                avg: avg.filter(|avg| avg.is_finite()),
                last: row_last,
            });
        }
        if entry.numeric_count > 0 && sum.is_finite() {
            entry.avg = Some(sum / entry.numeric_count as f64);
        }
        let timed = |reading: Option<(i64, Reading)>| -> Option<Option<TimedReading>> {
            match reading {
                None => Some(None),
                Some((at, value)) => Some(Some(TimedReading {
                    time: local_millis(at, tz)?,
                    value,
                })),
            }
        };
        entry.first = timed(first)?;
        entry.last = timed(last)?;
        if query.interval != Interval::Total {
            (entry.highest, entry.lowest) = extremes(
                entry
                    .rows
                    .iter()
                    .filter_map(|row| Some((row.time, row.avg?))),
            );
        }
        series.push(entry);
    }
    Some(Statistics {
        range: query_range(query, checked),
        series,
    })
}

/// The usage answer from the rows; `None` for a row that cannot be read.
fn usage(query: &SeriesQuery, checked: &Checked, cells: Vec<UsageCells>) -> Option<Usage> {
    let tz = &checked.tz;
    let plan = &checked.plan;
    let mut by_series: BTreeMap<(String, String), Vec<UsageCells>> = BTreeMap::new();
    for cell in cells {
        by_series
            .entry((cell.device.clone(), cell.point.clone()))
            .or_default()
            .push(cell);
    }
    let mut series = Vec::with_capacity(query.series.len());
    for key in &query.series {
        let cells = by_series.remove(&key_text(key)).unwrap_or_default();
        series.push(series_usage(key, query.interval, plan, tz, cells)?);
    }
    Some(Usage {
        range: query_range(query, checked),
        series,
    })
}

fn series_usage(
    key: &SeriesKey,
    interval: Interval,
    plan: &Plan,
    tz: &TimeZone,
    cells: Vec<UsageCells>,
) -> Option<SeriesUsage> {
    let mut entry = SeriesUsage {
        device_id: key.device_id,
        point: key.point.clone(),
        status: UsageStatus::NoData,
        check: None,
        total: None,
        drops: 0,
        readings: 0,
        first_reading: None,
        last_reading: None,
        baseline: false,
        gaps: 0,
        longest_gap: None,
        suspicious_steps: 0,
        largest_suspicious_step: None,
        skipped_cells: 0,
        highest: None,
        lowest: None,
        rows: Vec::new(),
    };
    let Some(any) = cells.first() else {
        return Some(entry);
    };
    entry.check = Some(CumulativeCheck {
        verdict: verdict(any.check_drops, any.check_rises, any.check_comparisons),
        drops: any.check_drops,
        rises: any.check_rises,
        comparisons: any.check_comparisons,
        from: local(plan.window_from, tz),
        to: local(plan.window_to, tz),
    });
    entry.baseline = any.window_first < plan.from.timestamp();
    let non_numeric: u64 = cells.iter().map(|cell| cell.non_numeric).sum();
    let usable: u64 = cells.iter().map(|cell| cell.usable).sum();
    entry.readings = cells.iter().map(|cell| cell.readings).sum();
    entry.skipped_cells = cells.iter().map(|cell| cell.skipped).sum();
    entry.status = if non_numeric > 0 {
        UsageStatus::NotNumeric
    } else if usable == 0 {
        UsageStatus::NoData
    } else if entry
        .check
        .as_ref()
        .is_some_and(|check| check.verdict == Verdict::NotCumulative)
    {
        UsageStatus::NotCumulative
    } else {
        UsageStatus::Ok
    };
    if entry.status != UsageStatus::Ok {
        return Some(entry);
    }
    let mut total = 0.0;
    let mut longest: Option<(i64, i64)> = None;
    let mut largest: Option<(i64, f64)> = None;
    let mut first: Option<(i64, f64)> = None;
    let mut last: Option<(i64, f64)> = None;
    for cell in &cells {
        let usage = cell.usage.unwrap_or(0.0);
        total += usage;
        entry.drops += cell.drops;
        entry.gaps += cell.gaps;
        entry.suspicious_steps += cell.suspicious;
        if cell.gaps > 0 && longest.is_none_or(|(_, ms)| cell.longest_gap_ms > ms) {
            longest = Some((cell.longest_gap_from, cell.longest_gap_ms));
        }
        if cell.suspicious > 0 {
            for (amount, at) in [
                (cell.inside_suspicious, cell.inside_suspicious_at),
                (cell.across_suspicious, cell.across_suspicious_at),
            ] {
                if let Some(amount) = amount.filter(|amount| *amount > 0.0)
                    && largest.is_none_or(|(_, best)| amount > best)
                {
                    largest = Some((at, amount));
                }
            }
        }
        if cell.usable > 0 {
            if let Some(value) = cell.first
                && first.is_none_or(|(at, _)| cell.first_at < at)
            {
                first = Some((cell.first_at, value));
            }
            if let Some(value) = cell.last
                && last.is_none_or(|(at, _)| cell.last_at > at)
            {
                last = Some((cell.last_at, value));
            }
        }
        entry.rows.push(UsageRow {
            time: local_seconds(cell.bucket, tz)?,
            usage,
            readings: cell.readings,
            drops: cell.drops,
            after_gap: cell.gaps > 0,
            suspicious_steps: cell.suspicious,
        });
    }
    let timed = |value: Option<(i64, f64)>| -> Option<Option<TimedValue>> {
        match value {
            None => Some(None),
            Some((at, value)) => Some(Some(TimedValue {
                time: local_millis(at, tz)?,
                value,
            })),
        }
    };
    entry.total = Some(total);
    entry.first_reading = timed(first)?;
    entry.last_reading = timed(last)?;
    entry.largest_suspicious_step = timed(largest)?;
    entry.longest_gap = match longest {
        None => None,
        Some((from, ms)) => Some(Gap {
            from: local_millis(from, tz)?,
            to: local_millis(from.checked_add(ms)?, tz)?,
        }),
    };
    if interval != Interval::Total {
        (entry.highest, entry.lowest) =
            extremes(entry.rows.iter().map(|row| (row.time, row.usage)));
    }
    Some(entry)
}

#[cfg(test)]
mod tests;
