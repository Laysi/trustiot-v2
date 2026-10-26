//! The history API of device charts (services/telemetry-query/README.md, "API"): a data point's
//! series over a time range, downsampled into buckets that follow the requested time zone, and
//! the latest stored value of each data point of a device. Every query runs as the caller's
//! tenant account, so ClickHouse returns that tenant's rows only (decision 14); the account's
//! settings profile and quota bound each query. Before each query the service checks, logged in
//! as the account, that its tenant, role, `readonly` and query limits are as it creates them
//! ([`Accounts::verify`]), and runs nothing as an account that differs.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | A user who has not picked or joined a tenant | 403 `permission_denied` |
//! | A malformed device ID or query string | 400 `invalid_request` |
//! | A parameter that breaks a rule (time zone, range, interval, data point) | 422 `validation_failed` |
//! | The range lies wholly before the queryable period | 403 `quota_exceeded` |
//! | The tenant's account has not been created yet | 404 `not_found` |
//! | The query reads or holds too much for the account's limits | 422 `validation_failed` (`telemetry.query_too_heavy`) |
//! | The account's quota or concurrency limit is used up | 429 `rate_limited` |
//! | ClickHouse stopped the query at its time limit | 504 `timeout` |
//! | Postgres or ClickHouse failed, or the tenant's account is not as the service creates it (its tenant, role, `readonly` or query limits differ) | 503 `unavailable` |
//! | Otherwise | 200 with [`Series`] or [`Latest`] |
//!
//! The queryable period is fixed for now to the strictest plan's (docs/product/plans.md, Starter):
//! raw readings of the last [`RAW_DAYS`] days, aggregated series of the last [`AGGREGATED_MONTHS`]
//! months. ClickHouse enforces it on every read ([`crate::period`]); the API cuts a range that
//! starts earlier to where the clamp of the view it reads starts ([`Windows::earliest`],
//! `truncated`), which is a little later than the database's window of that view, so the answer
//! holds every reading from `earliest` on. Each view has its own window, so the automatic
//! interval is chosen within the range the views can serve. Per-plan periods come with
//! Laysi/trustiot-v2#453.
//!
//! Raw series and latest values read the raw view. Aggregated series read the aggregates
//! (services/ingest/README.md, 「彙總表」): intervals of minutes the one-minute cells, intervals
//! of hours and days the one-hour cells, which start on UTC hours, so a time zone whose offset is
//! not a whole number of hours (Asia/Kolkata, Australia/Adelaide) reads the one-minute cells
//! instead, with the range cut to their period; ClickHouse itself tells (`toStartOfHour` of a
//! cell in the zone). A bucket is the
//! sum of whole cells: those whose start lies in `[from, to)`, so with a `from` or a `to` that is
//! not on a cell boundary the first bucket starts at the next cell and the last one can hold
//! readings up to a cell after `to`.

use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRef, Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use contract::api::telemetry_query::RunSql;
use contract::client::Endpoint as _;
use contract::{ErrorBody, ErrorCode, MessageKey, TenantId};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::accounts::{AccountError, Accounts, Ensured, Verified};
use crate::clickhouse::{QueryError, code};
use crate::period::{View, Windows};
use crate::semantic::range::latest_end;

/// The routes, as the ingress gateway forwards them (no prefix is stripped).
pub const SERIES_PATH: &str = "/api/telemetry/v1/devices/{device_id}/series";
pub const LATEST_PATH: &str = "/api/telemetry/v1/devices/{device_id}/latest";

/// Raw readings are queryable for this many days back from now.
pub const RAW_DAYS: i64 = 7;
/// Aggregated series are queryable for this many calendar months back from now.
pub const AGGREGATED_MONTHS: u32 = 3;
/// Most buckets one aggregated series returns; the automatic interval is the finest that stays
/// within it.
pub const MAX_BUCKETS: u16 = 1_500;
/// Most readings one raw series returns; a range with more needs an aggregated interval.
pub const MAX_RAW_READINGS: usize = 10_000;
/// Most data points the latest values cover.
pub const MAX_LATEST_POINTS: usize = 1_000;
/// Longest data point key (the telemetry table's `point_length` constraint).
pub const MAX_POINT_LEN: usize = 128;
/// Longest time zone name accepted.
const MAX_TIME_ZONE_LEN: usize = 64;

pub(crate) const QUERY_INVALID: MessageKey = MessageKey::new("telemetry.query_invalid");
const DEVICE_ID_INVALID: MessageKey = MessageKey::new("telemetry.device_id_invalid");
pub(crate) const TIME_ZONE_INVALID: MessageKey = MessageKey::new("telemetry.time_zone_invalid");
pub(crate) const RANGE_INVALID: MessageKey = MessageKey::new("telemetry.range_invalid");
pub(crate) const POINT_INVALID: MessageKey = MessageKey::new("telemetry.point_invalid");
pub(crate) const TOO_MANY_POINTS: MessageKey = MessageKey::new("telemetry.too_many_points");
pub(crate) const OUTSIDE_PERIOD: MessageKey = MessageKey::new("telemetry.outside_queryable_period");
pub(crate) const QUERY_TOO_HEAVY: MessageKey = MessageKey::new("telemetry.query_too_heavy");
const ACCOUNT_NOT_READY: MessageKey = MessageKey::new("telemetry.account_not_ready");

/// The row of the caller's tenant, if its account exists. Row-level security limits the table to
/// that tenant.
const ACCOUNT_READY: &str = "SELECT EXISTS (SELECT 1 FROM telemetry_query.tenant_accounts)";

/// An aggregated series from the cells of `$view`: per bucket, the readings, the smallest and
/// largest numeric value (a bool as 0 or 1), the mean of the numeric values (the sum of the
/// cells' sums over the sum of their numeric readings; NULL without one), the latest reading,
/// and whether a cell does not start on a `$cell_start` boundary in the time zone `tz`, which
/// means the zone's offset is not a whole number of the view's cells and the buckets cannot be
/// built from them. Buckets start where `toStartOfInterval` puts them in `tz`: minutes count from
/// the Unix epoch (every time zone's offset is a whole number of 15 minutes, so 15-minute
/// buckets start on the local quarter hour too), hours and days from the local midnight. A cell
/// counts when its start lies in `[from, to)`. `tenant_id` repeats what the account's row
/// policy enforces anyway.
macro_rules! cells_statement {
    ($view:literal, $interval:literal, $cell_start:literal) => {
        concat!(
            "SELECT toUnixTimestamp64Milli(toDateTime64(toStartOfInterval(bucket, ",
            $interval,
            "({step:UInt32}), {tz:String}), 3)) AS start,
  sum(readings), min(value_min), max(value_max),
  if(sum(numeric_readings) = 0, NULL, sum(value_sum) / sum(numeric_readings)),
  argMax(tuple(toString(last.value_type), last.value_float, last.value_int, last.value_bool,
    last.value_string), last.ts),
  max(",
            $cell_start,
            "(bucket, {tz:String}) != bucket)
FROM ",
            $view,
            "
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND bucket >= fromUnixTimestamp64Milli({from:Int64})
  AND bucket < fromUnixTimestamp64Milli({to:Int64})
GROUP BY start ORDER BY start LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow"
        )
    };
}

const MINUTE_CELLS_MINUTES: &str = cells_statement!(
    "trustiot.tenant_telemetry_1m",
    "toIntervalMinute",
    "toStartOfMinute"
);
const MINUTE_CELLS_HOURS: &str = cells_statement!(
    "trustiot.tenant_telemetry_1m",
    "toIntervalHour",
    "toStartOfMinute"
);
const MINUTE_CELLS_DAYS: &str = cells_statement!(
    "trustiot.tenant_telemetry_1m",
    "toIntervalDay",
    "toStartOfMinute"
);
const HOUR_CELLS_HOURS: &str = cells_statement!(
    "trustiot.tenant_telemetry_1h",
    "toIntervalHour",
    "toStartOfHour"
);
const HOUR_CELLS_DAYS: &str = cells_statement!(
    "trustiot.tenant_telemetry_1h",
    "toIntervalDay",
    "toStartOfHour"
);

/// Raw readings in time order. `{tz}` is checked here too, so a raw series refuses an unknown
/// time zone like an aggregated one.
const RAW_SERIES: &str = "SELECT toUnixTimestamp64Milli(ts), toString(value_type), value_float,
  value_int, value_bool, value_string
FROM trustiot.tenant_telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
  AND toTimeZone(ts, {tz:String}) IS NOT NULL
ORDER BY ts LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow";

/// The latest reading of each data point of a device within the raw period.
const LATEST: &str = "SELECT point, toUnixTimestamp64Milli(max(ts)),
  argMax(tuple(toString(value_type), value_float, value_int, value_bool, value_string), ts)
FROM trustiot.tenant_telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID}
  AND ts >= fromUnixTimestamp64Milli({from:Int64})
GROUP BY point ORDER BY point LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow";

/// How readings are grouped: `raw` (no grouping) or a bucket length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum Interval {
    #[serde(rename = "raw")]
    Raw,
    #[serde(rename = "1m")]
    Minute1,
    #[serde(rename = "5m")]
    Minutes5,
    #[serde(rename = "15m")]
    Minutes15,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "3h")]
    Hours3,
    #[serde(rename = "6h")]
    Hours6,
    #[serde(rename = "12h")]
    Hours12,
    #[serde(rename = "1d")]
    Day1,
}

/// The unit a bucket length counts in, which decides how buckets align ([`series_statement`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Minute,
    Hour,
    Day,
}

impl Interval {
    /// The aggregated intervals, finest first: the automatic choice takes the first that fits.
    pub const AGGREGATED: [Self; 8] = [
        Self::Minute1,
        Self::Minutes5,
        Self::Minutes15,
        Self::Hour1,
        Self::Hours3,
        Self::Hours6,
        Self::Hours12,
        Self::Day1,
    ];

    /// The bucket's unit and how many of them; `None` for raw.
    fn bucket(self) -> Option<(Unit, u32)> {
        match self {
            Self::Raw => None,
            Self::Minute1 => Some((Unit::Minute, 1)),
            Self::Minutes5 => Some((Unit::Minute, 5)),
            Self::Minutes15 => Some((Unit::Minute, 15)),
            Self::Hour1 => Some((Unit::Hour, 1)),
            Self::Hours3 => Some((Unit::Hour, 3)),
            Self::Hours6 => Some((Unit::Hour, 6)),
            Self::Hours12 => Some((Unit::Hour, 12)),
            Self::Day1 => Some((Unit::Day, 1)),
        }
    }

    /// The nominal bucket length in seconds (a local day can be 23 or 25 hours).
    fn seconds(self) -> Option<i64> {
        self.bucket().map(|(unit, step)| {
            let unit = match unit {
                Unit::Minute => 60,
                Unit::Hour => 3_600,
                Unit::Day => 86_400,
            };
            unit * i64::from(step)
        })
    }

    /// How many buckets `from..to` spans at most: one more than the length allows, for a range
    /// that does not start on a bucket boundary, and one more for a daylight saving change.
    fn max_buckets(self, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<i64> {
        let seconds = self.seconds()?;
        let range = (to - from).num_seconds().max(0);
        // The range is never negative and a bucket at least a minute.
        Some((range + seconds - 1) / seconds + 2)
    }

    /// The view the interval reads first: raw readings, the one-minute cells for minutes, the
    /// one-hour cells for hours and days (the one-minute cells when the time zone is not whole
    /// hours from UTC).
    fn view(self) -> View {
        match self.bucket() {
            None => View::Raw,
            Some((Unit::Minute, _)) => View::Minute,
            Some((Unit::Hour | Unit::Day, _)) => View::Hour,
        }
    }
}

/// The statement of an aggregated series of `unit` from `view`'s cells.
fn cells_statement(view: View, unit: Unit) -> &'static str {
    match (view, unit) {
        (View::Hour, Unit::Hour) => HOUR_CELLS_HOURS,
        (View::Hour, Unit::Day) => HOUR_CELLS_DAYS,
        (_, Unit::Hour) => MINUTE_CELLS_HOURS,
        (_, Unit::Day) => MINUTE_CELLS_DAYS,
        (_, Unit::Minute) => MINUTE_CELLS_MINUTES,
    }
}

/// One reading's value, as its type stored it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Reading {
    /// A floating-point reading; `null` for NaN or an infinity.
    Float(Option<f64>),
    /// An integer reading.
    Int(i64),
    /// A boolean reading.
    Bool(bool),
    /// A text reading.
    String(String),
}

/// One bucket of a series (for `raw`, one reading).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SeriesPoint {
    /// The bucket's start (for `raw`, the reading's time), UTC.
    pub time: DateTime<Utc>,
    /// How many readings the bucket holds.
    pub count: u64,
    /// The smallest numeric value (a bool counts as 0 or 1); `null` when the bucket holds only
    /// text readings.
    pub min: Option<f64>,
    /// The largest numeric value.
    pub max: Option<f64>,
    /// The mean of the numeric values.
    pub avg: Option<f64>,
    /// The bucket's latest reading.
    pub last: Reading,
}

/// A data point's series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Series {
    /// The device.
    pub device_id: Uuid,
    /// The data point.
    pub point: String,
    /// The time zone the buckets follow.
    pub tz: String,
    /// The interval used: the requested one, or the automatic choice.
    pub interval: Interval,
    /// The range queried, `from` inclusive and `to` exclusive, UTC; `from` is later than
    /// requested when the request started before the queryable period.
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Whether `from` was moved to the start of the queryable period.
    pub truncated: bool,
    /// The start of the queryable period for this interval at the time of the request.
    pub earliest: DateTime<Utc>,
    /// The buckets with readings, in time order; buckets without readings are left out.
    pub points: Vec<SeriesPoint>,
}

/// The latest value of one data point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LatestPoint {
    /// The data point.
    pub point: String,
    /// When the reading was taken, UTC.
    pub time: DateTime<Utc>,
    /// The reading.
    pub value: Reading,
}

/// The latest value of each data point of a device that has a reading in the raw period.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Latest {
    /// The device.
    pub device_id: Uuid,
    /// The start of the period searched (the raw period), UTC.
    pub earliest: DateTime<Utc>,
    /// One entry per data point, ordered by data point.
    pub points: Vec<LatestPoint>,
}

/// The query string of a series.
#[derive(Clone, Debug, Deserialize)]
pub struct SeriesParams {
    /// The data point key (1 to 128 characters).
    pub point: String,
    /// The range's start, RFC 3339 (inclusive). Percent-encode a `+` offset, or use `Z`.
    pub from: DateTime<Utc>,
    /// The range's end, RFC 3339 (exclusive).
    pub to: DateTime<Utc>,
    /// An IANA time zone name, for example `Asia/Taipei`: hour and day buckets start at its
    /// local hours and midnights.
    pub tz: String,
    /// `raw`, `1m`, `5m`, `15m`, `1h`, `3h`, `6h`, `12h` or `1d`; without it, the finest
    /// interval that gives at most 1500 buckets.
    pub interval: Option<Interval>,
}

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
    accounts: Accounts,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

impl Routes {
    /// The state the routes share: the semantic API (`crate::semantic`) and the internal SQL
    /// endpoint (`crate::sql_api`) use it too.
    pub fn new(verifier: IdentityVerifier, db: TenantDb, accounts: Accounts) -> Self {
        Self {
            verifier,
            db,
            accounts,
        }
    }
}

/// The service's API: [`SERIES_PATH`] and [`LATEST_PATH`], and the internal SQL endpoint
/// ([`crate::sql_api`]).
pub fn routes(verifier: IdentityVerifier, db: TenantDb, accounts: Accounts) -> Router {
    Router::new()
        .route(SERIES_PATH, get(get_series))
        .route(LATEST_PATH, get(get_latest))
        .route(
            RunSql::ROUTE,
            post(crate::sql_api::run_sql)
                .layer(DefaultBodyLimit::max(crate::sql_api::MAX_REQUEST_BYTES)),
        )
        .with_state(Routes::new(verifier, db, accounts))
}

/// An error response.
pub(crate) fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

/// A rejected query parameter.
pub(crate) fn invalid(parameter: &'static str, key: MessageKey) -> Response {
    error(
        ErrorBody::new(ErrorCode::ValidationFailed)
            .with_message_key(key)
            .with_detail("parameter", parameter),
    )
}

pub(crate) fn ok<T: Serialize>(value: &T) -> Response {
    ([(CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
}

/// Whether `tz` looks like an IANA time zone name; ClickHouse decides whether it knows it.
fn is_time_zone_name(tz: &str) -> bool {
    !tz.is_empty()
        && tz.len() <= MAX_TIME_ZONE_LEN
        && !tz.starts_with('/')
        && !tz.contains("..")
        && tz
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_+-".contains(&byte))
}

/// Whether `point` is a data point key the telemetry table can hold.
fn is_point(point: &str) -> bool {
    (1..=MAX_POINT_LEN).contains(&point.len())
}

/// What a request becomes once checked: the interval, the view it reads, and the range within
/// that view's period.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    interval: Interval,
    view: View,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    truncated: bool,
    /// Where the clamp of `view` starts ([`Windows::earliest`]).
    earliest: DateTime<Utc>,
}

/// The refusal of a range that lies wholly before the period starting at `earliest`.
fn outside_period(earliest: DateTime<Utc>) -> Box<Response> {
    Box::new(error(
        ErrorBody::new(ErrorCode::QuotaExceeded)
            .with_message_key(OUTSIDE_PERIOD)
            .with_detail("earliest", earliest.to_rfc3339()),
    ))
}

/// Whether `interval` gives at most [`MAX_BUCKETS`] buckets over `from..to` (raw readings are
/// counted after the query instead).
fn fits(interval: Interval, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    interval
        .max_buckets(from, to)
        .is_none_or(|buckets| buckets <= i64::from(MAX_BUCKETS))
}

/// Checks a series request against the rules that need neither the account nor the time.
fn validate(params: &SeriesParams) -> Result<(), Box<Response>> {
    if !is_point(&params.point) {
        return Err(Box::new(invalid("point", POINT_INVALID)));
    }
    if !is_time_zone_name(&params.tz) {
        return Err(Box::new(invalid("tz", TIME_ZONE_INVALID)));
    }
    if params.from >= params.to {
        return Err(Box::new(invalid("from", RANGE_INVALID)));
    }
    Ok(())
}

/// Checks a series request against the rules and the queryable period of an account with
/// `windows` at `now`. Each view has its own window (one plan may keep hourly cells longer
/// than minute cells), so a range is cut to the window of the view its interval reads.
fn plan(
    params: &SeriesParams,
    windows: Windows,
    now: DateTime<Utc>,
) -> Result<Plan, Box<Response>> {
    validate(params)?;
    // Before any arithmetic on the request's times.
    if !ends_in_time(params.to, now) {
        return Err(Box::new(invalid("to", RANGE_INVALID)));
    }
    match params.interval {
        Some(interval) => plan_interval(params, interval, interval.view(), windows, now),
        None => plan_automatic(params, windows, now),
    }
}

/// Whether a range ending at `to` ends no later than the semantic API's ranges may
/// ([`latest_end`]): [`MAX_AHEAD`] after `now`, and never past the last second of the cells'
/// `bucket`, a ClickHouse `DateTime` that wraps silently after 2106 (an empty answer although
/// data exists). chrono reads years up to +262142, so this comes before any arithmetic on `to`.
///
/// [`MAX_AHEAD`]: crate::semantic::range::MAX_AHEAD
fn ends_in_time(to: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    to <= latest_end(now)
}

/// The plan of `interval` read from `view`: the range cut to that view's period. Also the plan
/// of an hour or day interval that falls back to the one-minute cells, for a time zone that is
/// not whole hours from UTC.
fn plan_interval(
    params: &SeriesParams,
    interval: Interval,
    view: View,
    windows: Windows,
    now: DateTime<Utc>,
) -> Result<Plan, Box<Response>> {
    let earliest = windows.earliest(view, now);
    if params.to <= earliest {
        return Err(outside_period(earliest));
    }
    let from = params.from.max(earliest);
    if !fits(interval, from, params.to) {
        return Err(Box::new(error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(TOO_MANY_POINTS)
                .with_detail("parameter", "interval")
                .with_detail("max", MAX_BUCKETS),
        )));
    }
    Ok(Plan {
        interval,
        view,
        from,
        to: params.to,
        truncated: params.from < earliest,
        earliest,
    })
}

/// The automatic interval: the range cut as little as the two aggregates' periods allow, then
/// the finest aggregated interval whose view holds that whole range and which gives at most
/// [`MAX_BUCKETS`] buckets over it. When no interval fits from the earlier of the two starts,
/// the range starts at the later one instead (a shorter range fits finer intervals).
fn plan_automatic(
    params: &SeriesParams,
    windows: Windows,
    now: DateTime<Utc>,
) -> Result<Plan, Box<Response>> {
    let (minute, hour) = (
        windows.earliest(View::Minute, now),
        windows.earliest(View::Hour, now),
    );
    let (sooner, later) = (minute.min(hour), minute.max(hour));
    if params.to <= sooner {
        return Err(outside_period(sooner));
    }
    for start in [sooner, later] {
        let from = params.from.max(start);
        if params.to <= from {
            break;
        }
        let chosen = Interval::AGGREGATED.into_iter().find(|interval| {
            windows.earliest(interval.view(), now) <= from && fits(*interval, from, params.to)
        });
        if let Some(interval) = chosen {
            let view = interval.view();
            return Ok(Plan {
                interval,
                view,
                from,
                to: params.to,
                truncated: params.from < from,
                earliest: windows.earliest(view, now),
            });
        }
    }
    Err(Box::new(invalid("to", RANGE_INVALID)))
}

/// Maps a ClickHouse failure of a tenant query, or of the account check before one, to the
/// response, and logs it without its text.
pub(crate) fn clickhouse_error(tenant: TenantId, failure: &QueryError) -> Response {
    let (code, key) = match failure.code() {
        Some(code::TIMEOUT_EXCEEDED) => (ErrorCode::Timeout, None),
        Some(code::QUOTA_EXCEEDED | code::TOO_MANY_SIMULTANEOUS_QUERIES) => {
            (ErrorCode::RateLimited, None)
        }
        Some(code::MEMORY_LIMIT_EXCEEDED | code::TOO_MANY_ROWS | code::TOO_MANY_ROWS_OR_BYTES) => {
            (ErrorCode::ValidationFailed, Some(QUERY_TOO_HEAVY))
        }
        _ => (ErrorCode::Unavailable, None),
    };
    tracing::warn!(
        tenant_id = %tenant,
        error_kind = failure.kind(),
        "a telemetry query failed"
    );
    let body = ErrorBody::new(code);
    error(match key {
        Some(key) => body.with_message_key(key),
        None => body,
    })
}

/// Whether ClickHouse refused the time zone.
pub(crate) fn is_unknown_time_zone(failure: &QueryError) -> bool {
    matches!(failure, QueryError::Failed { code: Some(code::BAD_ARGUMENTS), body, .. }
        if body.contains("Cannot load time zone"))
}

/// Why a request could not be served; each becomes a response.
pub(crate) enum Failure {
    Response(Box<Response>),
    ClickHouse(QueryError),
}

impl Routes {
    /// The tenant accounts and the ClickHouse client.
    pub(crate) fn accounts(&self) -> &Accounts {
        &self.accounts
    }

    /// The caller's tenant, once its account exists.
    pub(crate) async fn tenant(&self, user: &UserIdentity) -> Result<TenantId, Failure> {
        let Some(tenant) = user.tenant_id() else {
            return Err(Failure::Response(Box::new(error(ErrorBody::new(
                ErrorCode::PermissionDenied,
            )))));
        };
        let ready = async {
            let mut tx = self.db.begin(user).await?;
            let ready: bool = sqlx::query_scalar(ACCOUNT_READY)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, TenantDbError>(ready)
        };
        match ready.await {
            Ok(true) => Ok(tenant),
            Ok(false) => Err(Failure::Response(Box::new(error(
                ErrorBody::new(ErrorCode::NotFound).with_message_key(ACCOUNT_NOT_READY),
            )))),
            Err(TenantDbError::NoTenant) => Err(Failure::Response(Box::new(error(
                ErrorBody::new(ErrorCode::PermissionDenied),
            )))),
            Err(failure) => {
                tracing::error!(
                    tenant_id = %tenant,
                    error_kind = error_kind(&failure),
                    "could not read whether the tenant's account exists"
                );
                Err(Failure::Response(Box::new(error(ErrorBody::new(
                    ErrorCode::Unavailable,
                )))))
            }
        }
    }

    /// Checks `tenant`'s account before a query runs as it ([`Accounts::verify`]: its tenant,
    /// role, `readonly` and query limits), and returns the windows the database applies to its
    /// queries. When ClickHouse refuses the account's password (a new password generation of
    /// the service's account, or a changed account), the check sets it again first; missing or
    /// other windows are set too. An account that differs in anything else is refused with 503,
    /// and nothing may run as it: its limits or its tenant may not hold.
    pub(crate) async fn verified(&self, tenant: TenantId) -> Result<Windows, Failure> {
        match self.accounts.verify(tenant).await {
            Ok(Verified {
                ensured,
                windows,
                windows_set: _,
            }) => {
                if ensured == Ensured::PasswordSet {
                    tracing::info!(tenant_id = %tenant, "set the tenant account's password again");
                }
                Ok(windows)
            }
            Err(AccountError::ClickHouse { source, .. }) if !source.is_authentication() => {
                Err(Failure::ClickHouse(source))
            }
            Err(failure) => {
                tracing::error!(
                    tenant_id = %tenant,
                    error = %failure,
                    "the tenant's ClickHouse account is not as the service creates it, or refused \
                     its password and could not be repaired; no query runs as it"
                );
                Err(Failure::Response(Box::new(error(ErrorBody::new(
                    ErrorCode::Unavailable,
                )))))
            }
        }
    }

    /// Runs `statement` as `tenant`'s account, which [`Routes::verified`] checked for this
    /// request.
    pub(crate) async fn run_verified(
        &self,
        tenant: TenantId,
        statement: &str,
        params: &[(&str, &str)],
    ) -> Result<String, Failure> {
        let account = self.accounts.account(tenant);
        self.accounts
            .clickhouse()
            .query(account.credentials(), statement, params, None)
            .await
            .map_err(Failure::ClickHouse)
    }
}

/// The texts of a series statement's query parameters that come from its plan.
struct PlanTexts {
    from: String,
    to: String,
    step: Option<String>,
    limit: String,
}

impl PlanTexts {
    fn new(plan: &Plan, limit: usize) -> Self {
        Self {
            from: plan.from.timestamp_millis().to_string(),
            to: plan.to.timestamp_millis().to_string(),
            step: plan.interval.bucket().map(|(_, step)| step.to_string()),
            limit: limit.to_string(),
        }
    }

    /// The query parameters of the series statement.
    fn params<'a>(
        &'a self,
        tenant: &'a str,
        device: &'a str,
        request: &'a SeriesParams,
    ) -> Vec<(&'a str, &'a str)> {
        let mut params = vec![
            ("tenant", tenant),
            ("device", device),
            ("point", request.point.as_str()),
            ("from", self.from.as_str()),
            ("to", self.to.as_str()),
            ("tz", request.tz.as_str()),
            ("limit", self.limit.as_str()),
        ];
        if let Some(step) = &self.step {
            params.push(("step", step.as_str()));
        }
        params
    }
}

/// One `JSONCompactEachRow` line of a series bucket or a latest value: the reading's type and
/// its value columns.
pub(crate) type ReadingColumns = (String, Option<f64>, i64, bool, String);

pub(crate) fn reading((kind, float, int, boolean, text): ReadingColumns) -> Option<Reading> {
    match kind.as_str() {
        "float" => Some(Reading::Float(float)),
        "int" => Some(Reading::Int(int)),
        "bool" => Some(Reading::Bool(boolean)),
        "string" => Some(Reading::String(text)),
        _ => None,
    }
}

fn utc_millis(millis: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(millis)
}

/// The numeric value of a raw reading, as the aggregated series computes it.
fn numeric(value: &Reading) -> Option<f64> {
    match value {
        Reading::Float(value) => *value,
        // Rounds above 2^53 as ClickHouse's toFloat64 does in the aggregated series.
        Reading::Int(value) => Some(*value as f64),
        Reading::Bool(value) => Some(f64::from(u8::from(*value))),
        Reading::String(_) => None,
    }
}

/// What may be logged of a Postgres failure: never the database's message text.
fn error_kind(error: &TenantDbError) -> String {
    match error {
        TenantDbError::Database(sqlx::Error::Database(database)) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        TenantDbError::Database(sqlx::Error::PoolTimedOut) => "pool timed out".to_owned(),
        TenantDbError::Database(_) => "database client".to_owned(),
        other => other.to_string(),
    }
}

/// Parses each line with `row`; `None` for a line ClickHouse should not have written.
pub(crate) fn rows<T, R: serde::de::DeserializeOwned>(
    body: &str,
    row: impl Fn(R) -> Option<T>,
) -> Option<Vec<T>> {
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<R>(line).ok().and_then(&row))
        .collect()
}

pub(crate) fn unexpected_answer(tenant: TenantId) -> Response {
    tracing::error!(tenant_id = %tenant, "ClickHouse answered in an unexpected form");
    error(ErrorBody::new(ErrorCode::Internal))
}

/// Runs a series statement as `tenant`'s verified account; a refusal becomes its response.
async fn series_body(
    state: &Routes,
    tenant: TenantId,
    statement: &str,
    params: &[(&str, &str)],
) -> Result<String, Box<Response>> {
    match state.run_verified(tenant, statement, params).await {
        Ok(body) => Ok(body),
        Err(Failure::Response(response)) => Err(response),
        Err(Failure::ClickHouse(failure)) if is_unknown_time_zone(&failure) => {
            Err(Box::new(invalid("tz", TIME_ZONE_INVALID)))
        }
        Err(Failure::ClickHouse(failure)) => Err(Box::new(clickhouse_error(tenant, &failure))),
    }
}

/// A raw series: one point per reading.
async fn raw_series(
    state: &Routes,
    tenant: TenantId,
    params: &[(&str, &str)],
) -> Result<Vec<SeriesPoint>, Box<Response>> {
    let body = series_body(state, tenant, RAW_SERIES, params).await?;
    rows(
        &body,
        |(millis, kind, float, int, boolean, text): (
            i64,
            String,
            Option<f64>,
            i64,
            bool,
            String,
        )| {
            let last = reading((kind, float, int, boolean, text))?;
            let value = numeric(&last);
            Some(SeriesPoint {
                time: utc_millis(millis)?,
                count: 1,
                min: value,
                max: value,
                avg: value,
                last,
            })
        },
    )
    .ok_or_else(|| Box::new(unexpected_answer(tenant)))
}

/// One line of [`cells_statement!`]: the bucket's start, readings, min, max, mean, the latest
/// reading, and whether a cell is off the time zone's boundaries.
type CellsLine = (
    i64,
    u64,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    ReadingColumns,
    u8,
);

/// An aggregated series from the cells of an aggregate, and whether a cell did not start on the
/// time zone's boundary of the view's cell (the series is then wrong and must not be served).
async fn cells_series(
    state: &Routes,
    tenant: TenantId,
    statement: &str,
    params: &[(&str, &str)],
) -> Result<(Vec<SeriesPoint>, bool), Box<Response>> {
    let body = series_body(state, tenant, statement, params).await?;
    let lines = rows(
        &body,
        |(millis, count, min, max, avg, last, misaligned): CellsLine| {
            Some((
                SeriesPoint {
                    time: utc_millis(millis)?,
                    count,
                    min,
                    max,
                    avg,
                    last: reading(last)?,
                },
                misaligned != 0,
            ))
        },
    )
    .ok_or_else(|| Box::new(unexpected_answer(tenant)))?;
    let misaligned = lines.iter().any(|(_, misaligned)| *misaligned);
    Ok((
        lines.into_iter().map(|(point, _)| point).collect(),
        misaligned,
    ))
}

/// A data point's series over a time range: raw readings, or buckets of an interval that
/// follow the time zone `tz`, within the queryable period.
#[utoipa::path(
    get,
    path = "/api/telemetry/v1/devices/{device_id}/series",
    tag = "telemetry",
    operation_id = "getTelemetrySeries",
    params(
        ("device_id" = Uuid, Path, description = "The device."),
        ("point" = String, Query, description = "The data point key (1 to 128 characters)."),
        ("from" = DateTime<Utc>, Query, description = "The range's start, RFC 3339 (inclusive). Percent-encode a `+` offset, or use `Z`."),
        ("to" = DateTime<Utc>, Query, description = "The range's end, RFC 3339 (exclusive)."),
        ("tz" = String, Query, description = "An IANA time zone name, for example `Asia/Taipei`: hour and day buckets start at its local hours and midnights."),
        ("interval" = Option<Interval>, Query, description = "`raw`, `1m`, `5m`, `15m`, `1h`, `3h`, `6h`, `12h` or `1d`; without it, the finest interval that gives at most 1500 buckets."),
    ),
    responses(
        (status = 200, description = "The series.", body = Series),
        (status = 400, description = "A malformed device ID or query string (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant (`permission_denied`), or the range lies wholly before the queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, detail `earliest`).", body = ErrorBody),
        (status = 404, description = "The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`).", body = ErrorBody),
        (status = 422, description = "A parameter breaks a rule (`validation_failed`, detail `parameter`), the series would have too many points (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`).", body = ErrorBody),
        (status = 429, description = "The tenant's query quota or concurrency limit is used up (`rate_limited`).", body = ErrorBody),
        (status = 503, description = "Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`).", body = ErrorBody),
        (status = 504, description = "The query ran into its time limit (`timeout`).", body = ErrorBody),
    )
)]
async fn get_series(
    State(state): State<Routes>,
    user: UserIdentity,
    device: Result<Path<Uuid>, PathRejection>,
    params: Result<Query<SeriesParams>, QueryRejection>,
) -> Response {
    let Ok(Path(device)) = device else {
        return error(
            ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(DEVICE_ID_INVALID),
        );
    };
    let Ok(Query(params)) = params else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(QUERY_INVALID));
    };
    if let Err(response) = validate(&params) {
        return *response;
    }
    let tenant = match state.tenant(&user).await {
        Ok(tenant) => tenant,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(_)) => unreachable!("tenant() does not query ClickHouse"),
    };
    let windows = match state.verified(tenant).await {
        Ok(windows) => windows,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(failure)) => return clickhouse_error(tenant, &failure),
    };
    let now = Utc::now();
    let mut plan = match plan(&params, windows, now) {
        Ok(plan) => plan,
        Err(response) => return *response,
    };
    let (tenant_text, device_text) = (tenant.to_string(), device.to_string());
    let points = match plan.interval.bucket() {
        None => {
            let texts = PlanTexts::new(&plan, MAX_RAW_READINGS + 1);
            let query_params = texts.params(&tenant_text, &device_text, &params);
            raw_series(&state, tenant, &query_params).await
        }
        Some((unit, _)) => {
            let texts = PlanTexts::new(&plan, usize::from(MAX_BUCKETS));
            let query_params = texts.params(&tenant_text, &device_text, &params);
            match cells_series(
                &state,
                tenant,
                cells_statement(plan.view, unit),
                &query_params,
            )
            .await
            {
                // The one-hour cells start on UTC hours and the time zone's hours do not: the
                // one-minute cells instead, with the range cut to their own period.
                Ok((_, true)) if plan.view == View::Hour => {
                    match plan_interval(&params, plan.interval, View::Minute, windows, now) {
                        Ok(minute) => {
                            plan = minute;
                            let texts = PlanTexts::new(&plan, usize::from(MAX_BUCKETS));
                            let query_params = texts.params(&tenant_text, &device_text, &params);
                            cells_series(
                                &state,
                                tenant,
                                cells_statement(View::Minute, unit),
                                &query_params,
                            )
                            .await
                        }
                        Err(response) => Err(response),
                    }
                }
                other => other,
            }
            .and_then(|(points, misaligned)| {
                if misaligned {
                    // A time zone whose offset is not a whole number of minutes: no real zone
                    // within the aggregates' retention has one.
                    Err(Box::new(unexpected_answer(tenant)))
                } else {
                    Ok(points)
                }
            })
        }
    };
    let points = match points {
        Ok(points) => points,
        Err(response) => return *response,
    };
    if plan.interval == Interval::Raw && points.len() > MAX_RAW_READINGS {
        return error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(TOO_MANY_POINTS)
                .with_detail("parameter", "interval")
                .with_detail("max", MAX_RAW_READINGS),
        );
    }
    ok(&Series {
        device_id: device,
        point: params.point,
        tz: params.tz,
        interval: plan.interval,
        from: plan.from,
        to: plan.to,
        truncated: plan.truncated,
        earliest: plan.earliest,
        points,
    })
}

/// The latest value of each data point of a device, from the readings of the raw period.
#[utoipa::path(
    get,
    path = "/api/telemetry/v1/devices/{device_id}/latest",
    tag = "telemetry",
    operation_id = "getTelemetryLatest",
    params(("device_id" = Uuid, Path, description = "The device.")),
    responses(
        (status = 200, description = "The latest values; empty when the device has no reading in the raw period, or is not the tenant's.", body = Latest),
        (status = 400, description = "A malformed device ID (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "The user has not picked or joined a tenant (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "The tenant's account has not been created yet (`not_found`, `telemetry.account_not_ready`).", body = ErrorBody),
        (status = 422, description = "The device has more data points than one answer holds (`telemetry.too_many_points`), or the query reads or holds too much (`telemetry.query_too_heavy`).", body = ErrorBody),
        (status = 429, description = "The tenant's query quota or concurrency limit is used up (`rate_limited`).", body = ErrorBody),
        (status = 503, description = "Postgres or ClickHouse failed, or the tenant's ClickHouse account is not as the service creates it (`unavailable`).", body = ErrorBody),
        (status = 504, description = "The query ran into its time limit (`timeout`).", body = ErrorBody),
    )
)]
async fn get_latest(
    State(state): State<Routes>,
    user: UserIdentity,
    device: Result<Path<Uuid>, PathRejection>,
) -> Response {
    let Ok(Path(device)) = device else {
        return error(
            ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(DEVICE_ID_INVALID),
        );
    };
    let tenant = match state.tenant(&user).await {
        Ok(tenant) => tenant,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(_)) => unreachable!("tenant() does not query ClickHouse"),
    };
    let windows = match state.verified(tenant).await {
        Ok(windows) => windows,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(failure)) => return clickhouse_error(tenant, &failure),
    };
    let earliest = windows.earliest(View::Raw, Utc::now());
    let (tenant_text, device_text) = (tenant.to_string(), device.to_string());
    let from = earliest.timestamp_millis().to_string();
    let limit = (MAX_LATEST_POINTS + 1).to_string();
    let params = [
        ("tenant", tenant_text.as_str()),
        ("device", device_text.as_str()),
        ("from", from.as_str()),
        ("limit", limit.as_str()),
    ];
    let body = match state.run_verified(tenant, LATEST, &params).await {
        Ok(body) => body,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(failure)) => return clickhouse_error(tenant, &failure),
    };
    let points = rows(
        &body,
        |(point, millis, value): (String, i64, ReadingColumns)| {
            Some(LatestPoint {
                point,
                time: utc_millis(millis)?,
                value: reading(value)?,
            })
        },
    );
    let Some(points) = points else {
        return unexpected_answer(tenant);
    };
    if points.len() > MAX_LATEST_POINTS {
        return error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_message_key(TOO_MANY_POINTS)
                .with_detail("max", MAX_LATEST_POINTS),
        );
    }
    ok(&Latest {
        device_id: device,
        earliest,
        points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("an RFC 3339 time")
    }

    fn params(from: &str, to: &str, interval: Option<Interval>) -> SeriesParams {
        SeriesParams {
            point: "temp".to_owned(),
            from: at(from),
            to: at(to),
            tz: "Asia/Taipei".to_owned(),
            interval,
        }
    }

    const NOW: &str = "2026-10-21T12:00:00Z";

    fn plan_at(params: &SeriesParams, now: DateTime<Utc>) -> Result<Plan, Box<Response>> {
        plan(params, Windows::PLATFORM, now)
    }

    #[test]
    fn intervals_read_the_view_of_their_cells() {
        assert_eq!(Interval::Raw.view(), View::Raw);
        for interval in [Interval::Minute1, Interval::Minutes5, Interval::Minutes15] {
            assert_eq!(interval.view(), View::Minute, "{interval:?}");
        }
        for interval in [
            Interval::Hour1,
            Interval::Hours3,
            Interval::Hours6,
            Interval::Hours12,
            Interval::Day1,
        ] {
            assert_eq!(interval.view(), View::Hour, "{interval:?}");
        }
        assert_eq!(cells_statement(View::Hour, Unit::Day), HOUR_CELLS_DAYS);
        assert_eq!(cells_statement(View::Minute, Unit::Day), MINUTE_CELLS_DAYS);
        assert_eq!(cells_statement(View::Hour, Unit::Hour), HOUR_CELLS_HOURS);
        assert_eq!(
            cells_statement(View::Minute, Unit::Hour),
            MINUTE_CELLS_HOURS
        );
        assert_eq!(
            cells_statement(View::Minute, Unit::Minute),
            MINUTE_CELLS_MINUTES
        );
        for statement in [HOUR_CELLS_HOURS, HOUR_CELLS_DAYS] {
            assert!(statement.contains("FROM trustiot.tenant_telemetry_1h\n"));
            assert!(statement.contains("max(toStartOfHour(bucket, {tz:String}) != bucket)"));
        }
        for statement in [MINUTE_CELLS_MINUTES, MINUTE_CELLS_HOURS, MINUTE_CELLS_DAYS] {
            assert!(statement.contains("FROM trustiot.tenant_telemetry_1m\n"));
            assert!(statement.contains("max(toStartOfMinute(bucket, {tz:String}) != bucket)"));
        }
    }

    fn status(result: Result<Plan, Box<Response>>) -> u16 {
        result.expect_err("refused").status().as_u16()
    }

    #[test]
    fn the_automatic_interval_is_the_finest_within_the_bucket_limit() {
        let now = at(NOW);
        for (from, interval) in [
            ("2026-10-21T00:00:00Z", Interval::Minute1),
            ("2026-10-20T12:00:00Z", Interval::Minute1),
            ("2026-10-19T12:00:00Z", Interval::Minutes5),
            ("2026-10-14T12:00:00Z", Interval::Minutes15),
            ("2026-09-21T12:00:00Z", Interval::Hour1),
            ("2026-07-21T13:00:00Z", Interval::Hours3),
        ] {
            let plan = plan_at(&params(from, NOW, None), now).expect("a plan");
            assert_eq!(plan.interval, interval, "{from}");
            assert!(!plan.truncated, "{from}");
        }
    }

    #[test]
    fn ranges_are_cut_to_the_queryable_period() {
        let now = at(NOW);
        // Raw: seven days.
        let raw = plan_at(
            &params("2026-10-01T00:00:00Z", NOW, Some(Interval::Hours6)),
            now,
        )
        .expect("aggregated reaches back three months");
        assert!(!raw.truncated);
        let raw = plan_at(
            &params(
                "2026-10-13T00:00:00Z",
                "2026-10-15T00:00:00Z",
                Some(Interval::Raw),
            ),
            now,
        )
        .expect("partly inside");
        assert!(raw.truncated);
        assert_eq!(
            raw.from,
            at("2026-10-14T12:05:00Z"),
            "seven days and the margin"
        );
        assert_eq!(raw.earliest, at("2026-10-14T12:05:00Z"));
        // Wholly before the raw period: refused with the period's start.
        assert_eq!(
            status(plan_at(
                &params(
                    "2026-10-01T00:00:00Z",
                    "2026-10-14T12:00:00Z",
                    Some(Interval::Raw)
                ),
                now
            )),
            403
        );
        // Aggregated: three calendar months.
        let aggregated = plan_at(
            &params("2026-01-01T00:00:00Z", NOW, Some(Interval::Day1)),
            now,
        )
        .expect("partly inside");
        assert!(aggregated.truncated);
        assert_eq!(
            aggregated.from,
            at("2026-07-21T13:00:00Z"),
            "three months and the margin, on the next whole hour"
        );
        assert_eq!(
            status(plan_at(
                &params("2026-01-01T00:00:00Z", "2026-07-21T12:00:00Z", None),
                now
            )),
            403
        );
    }

    #[test]
    fn each_interval_is_cut_to_the_period_of_the_view_it_reads() {
        let now = at(NOW);
        // Minutes read the one-minute cells: their clamp is on a whole minute.
        let minutes = plan_at(
            &params(
                "2026-07-21T00:00:00Z",
                "2026-07-25T00:00:00Z",
                Some(Interval::Minutes15),
            ),
            now,
        )
        .expect("partly inside");
        assert_eq!(minutes.view, View::Minute);
        assert!(minutes.truncated);
        assert_eq!(minutes.from, at("2026-07-21T12:05:00Z"));
        assert_eq!(minutes.earliest, at("2026-07-21T12:05:00Z"));
        // A plan that keeps hours longer than minutes: an hour interval reaches back to the
        // hours' period, a minute interval only to the minutes'.
        let windows = Windows::new(7, 13, 36);
        let hours = plan(
            &params(
                "2024-01-01T00:00:00Z",
                "2024-01-03T00:00:00Z",
                Some(Interval::Hour1),
            ),
            windows,
            now,
        )
        .expect("inside the hours' period");
        assert_eq!((hours.view, hours.truncated), (View::Hour, false));
        assert_eq!(hours.earliest, at("2023-10-21T13:00:00Z"));
        assert_eq!(
            status(plan(
                &params(
                    "2024-01-01T00:00:00Z",
                    "2024-01-03T00:00:00Z",
                    Some(Interval::Minutes15)
                ),
                windows,
                now
            )),
            403,
            "before the minutes' period"
        );
        // The automatic interval prefers the whole range at a coarser interval to a finer one
        // over the part the minute cells still hold.
        let automatic = plan(
            &params("2024-10-21T12:00:00Z", "2025-09-21T22:00:00Z", None),
            windows,
            now,
        )
        .expect("inside the hours' period");
        assert_eq!(
            (automatic.interval, automatic.view, automatic.truncated),
            (Interval::Hours6, View::Hour, false)
        );
        assert_eq!(automatic.from, at("2024-10-21T12:00:00Z"));
        // Within both periods: the finest interval, from the minute cells.
        let recent =
            plan(&params("2026-10-21T00:00:00Z", NOW, None), windows, now).expect("recent");
        assert_eq!(
            (recent.interval, recent.view),
            (Interval::Minute1, View::Minute)
        );
        // Wholly before both periods: refused with the earlier start.
        let refused = plan(
            &params("2020-01-01T00:00:00Z", "2020-02-01T00:00:00Z", None),
            windows,
            now,
        )
        .expect_err("refused");
        assert_eq!(refused.status().as_u16(), 403);
    }

    #[test]
    fn the_fallback_to_minute_cells_is_cut_to_their_period() {
        // A time zone off the whole hours reads the one-minute cells: with a shorter minute
        // window, the hour interval's range is cut again, or refused when wholly before it.
        let now = at(NOW);
        let windows = Windows::new(7, 1, 3);
        let request = params("2026-08-21T00:00:00Z", NOW, Some(Interval::Hours3));
        let hours = plan(&request, windows, now).expect("inside the hours' period");
        assert_eq!((hours.view, hours.truncated), (View::Hour, false));
        let minutes = plan_interval(&request, hours.interval, View::Minute, windows, now)
            .expect("partly inside the minutes' period");
        assert_eq!(minutes.view, View::Minute);
        assert!(minutes.truncated);
        assert_eq!(minutes.from, at("2026-09-21T12:05:00Z"));
        assert_eq!(minutes.earliest, minutes.from);
        let before = params(
            "2026-08-21T00:00:00Z",
            "2026-09-01T00:00:00Z",
            Some(Interval::Hours3),
        );
        assert!(plan(&before, windows, now).is_ok());
        assert_eq!(
            status(plan_interval(
                &before,
                Interval::Hours3,
                View::Minute,
                windows,
                now
            )),
            403
        );
    }

    #[test]
    fn rules_of_the_parameters() {
        let now = at(NOW);
        let mut bad_tz = params("2026-10-21T00:00:00Z", NOW, None);
        for tz in [
            "",
            "/etc/passwd",
            "../UTC",
            "UTC'",
            "Asia/Taipei;",
            &"A".repeat(65),
        ] {
            bad_tz.tz = tz.to_owned();
            assert_eq!(status(plan_at(&bad_tz, now)), 422, "{tz}");
        }
        for tz in [
            "UTC",
            "Asia/Taipei",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+8",
        ] {
            bad_tz.tz = tz.to_owned();
            assert!(plan_at(&bad_tz, now).is_ok(), "{tz}");
        }
        let mut bad_point = params("2026-10-21T00:00:00Z", NOW, None);
        for point in [String::new(), "p".repeat(129)] {
            bad_point.point = point;
            assert_eq!(status(plan_at(&bad_point, now)), 422);
        }
        assert_eq!(
            status(plan_at(&params(NOW, NOW, None), now)),
            422,
            "an empty range"
        );
        assert_eq!(
            status(plan_at(
                &params("2026-09-21T12:00:00Z", NOW, Some(Interval::Minute1)),
                now
            )),
            422,
            "a month of minutes has too many buckets"
        );
        // A century of cells, in both aggregates, has more days than buckets.
        assert_eq!(
            status(plan(
                &params("1950-01-01T00:00:00Z", NOW, None),
                Windows::new(7, 1_200, 1_200),
                now
            )),
            422,
            "no interval fits"
        );
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        serde_json::from_slice(&bytes).expect("a JSON body")
    }

    #[tokio::test]
    async fn a_range_ends_at_most_max_ahead_after_now() {
        let now = at(NOW);
        // chrono's RFC 3339 reads a signed year of any length, up to +262142: such a `to` is
        // refused before any arithmetic on it, so nothing overflows.
        for to in [
            "+262142-12-31T23:59:00Z",
            "2106-02-08T00:00:00Z",
            "2027-10-23T12:00:00.001Z",
        ] {
            let request: SeriesParams = serde_json::from_value(serde_json::json!({
                "point": "temp",
                "from": "2026-10-21T00:00:00Z",
                "to": to,
                "tz": "UTC",
            }))
            .expect("the query string parses");
            for interval in [None, Some(Interval::Raw), Some(Interval::Day1)] {
                let request = SeriesParams {
                    interval,
                    ..request.clone()
                };
                let refused = plan_at(&request, now).expect_err(to);
                assert_eq!(refused.status().as_u16(), 422, "{to}");
            }
        }
        let refused = plan_at(
            &params("2026-10-21T00:00:00Z", "2106-02-08T00:00:00Z", None),
            now,
        )
        .expect_err("past the cells' DateTime");
        let body = body_json(*refused).await;
        assert_eq!(body["message_key"], "telemetry.range_invalid");
        assert_eq!(body["details"]["parameter"], "to");
        // 367 days ahead (the semantic API's `MAX_AHEAD`) still plans.
        assert!(plan_at(&params(NOW, "2027-10-23T12:00:00Z", None), now).is_ok());
        assert!(ends_in_time(at("2027-10-23T12:00:00Z"), now));
        assert!(!ends_in_time(at("2027-10-23T12:00:00.001Z"), now));
        assert!(!ends_in_time(DateTime::<Utc>::MAX_UTC, now));
        // Never past the cells' last second, 2106-02-07T06:28:15Z, whatever the clock says.
        let late = at("2106-01-01T00:00:00Z");
        assert!(ends_in_time(at("2106-02-07T06:28:15Z"), late));
        assert!(!ends_in_time(at("2106-02-07T06:28:16Z"), late));
    }

    #[test]
    fn readings_keep_their_type() {
        assert_eq!(
            serde_json::to_value(Reading::Float(Some(1.5))).expect("json"),
            serde_json::json!({"type": "float", "value": 1.5})
        );
        assert_eq!(
            serde_json::to_value(Reading::Float(None)).expect("json"),
            serde_json::json!({"type": "float", "value": null})
        );
        assert_eq!(
            serde_json::to_value(Reading::String("on".to_owned())).expect("json"),
            serde_json::json!({"type": "string", "value": "on"})
        );
        assert_eq!(
            reading(("int".to_owned(), None, 7, false, String::new())),
            Some(Reading::Int(7))
        );
        assert_eq!(
            reading(("other".to_owned(), None, 0, false, String::new())),
            None
        );
        assert_eq!(numeric(&Reading::Bool(true)), Some(1.0));
        assert_eq!(numeric(&Reading::String("x".to_owned())), None);
    }

    #[test]
    fn statements_use_parameters_only() {
        for statement in [
            MINUTE_CELLS_MINUTES,
            MINUTE_CELLS_HOURS,
            MINUTE_CELLS_DAYS,
            HOUR_CELLS_HOURS,
            HOUR_CELLS_DAYS,
            RAW_SERIES,
            LATEST,
        ] {
            assert!(statement.contains("{tenant:UUID}"), "{statement}");
            assert!(statement.contains("{device:UUID}"), "{statement}");
            assert!(
                statement.contains("trustiot.tenant_telemetry"),
                "{statement}"
            );
            assert!(!statement.contains("SETTINGS"), "{statement}");
            assert!(!statement.contains("PREWHERE"), "{statement}");
        }
    }
}
