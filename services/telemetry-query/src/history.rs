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
//! months. A range that starts earlier is cut to the period's start (`truncated`); per-plan
//! periods come with Laysi/trustiot-v2#453.

use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Months, TimeDelta, Utc};
use contract::{ErrorBody, ErrorCode, MessageKey, TenantId};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::accounts::{AccountError, Accounts, Ensured};
use crate::clickhouse::{QueryError, code};

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

/// The numeric value of a reading (a bool as 0 or 1; NULL for a string), for min, max and avg.
/// Buckets start where `toStartOfInterval` puts them in the time zone `tz`: minutes count from
/// the Unix epoch (every time zone's offset is a whole number of 15 minutes, so 15-minute
/// buckets start on the local quarter hour too), hours and days from the local midnight.
/// `tenant_id` repeats what the account's row policy enforces anyway.
macro_rules! series_statement {
    ($interval:literal) => {
        concat!(
            "SELECT toUnixTimestamp64Milli(toDateTime64(toStartOfInterval(ts, ",
            $interval,
            "({step:UInt32}), {tz:String}), 3)) AS bucket,
  count(), min(n), max(n), avg(n),
  argMax(tuple(toString(value_type), value_float, value_int, value_bool, value_string), ts)
FROM (
  SELECT ts, value_type, value_float, value_int, value_bool, value_string,
    multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int),
      value_type = 'bool', toFloat64(value_bool), CAST(NULL AS Nullable(Float64))) AS n
  FROM trustiot.tenant_telemetry
  WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
    AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
)
GROUP BY bucket ORDER BY bucket LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow"
        )
    };
}

const SERIES_MINUTES: &str = series_statement!("toIntervalMinute");
const SERIES_HOURS: &str = series_statement!("toIntervalHour");
const SERIES_DAYS: &str = series_statement!("toIntervalDay");

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
    /// The state the routes share: the semantic API (`crate::semantic`) uses it too.
    pub fn new(verifier: IdentityVerifier, db: TenantDb, accounts: Accounts) -> Self {
        Self {
            verifier,
            db,
            accounts,
        }
    }
}

/// The service's API: [`SERIES_PATH`] and [`LATEST_PATH`].
pub fn routes(verifier: IdentityVerifier, db: TenantDb, accounts: Accounts) -> Router {
    Router::new()
        .route(SERIES_PATH, get(get_series))
        .route(LATEST_PATH, get(get_latest))
        .with_state(Routes {
            verifier,
            db,
            accounts,
        })
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

/// The start of the queryable period of `interval` at `now`.
fn earliest(interval: Interval, now: DateTime<Utc>) -> DateTime<Utc> {
    if interval == Interval::Raw {
        now - TimeDelta::days(RAW_DAYS)
    } else {
        now.checked_sub_months(Months::new(AGGREGATED_MONTHS))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }
}

/// What a request becomes once checked: the interval and the range within the period.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    interval: Interval,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    truncated: bool,
    earliest: DateTime<Utc>,
}

/// Checks a series request against the rules and the queryable period at `now`.
fn plan(params: &SeriesParams, now: DateTime<Utc>) -> Result<Plan, Box<Response>> {
    if !is_point(&params.point) {
        return Err(Box::new(invalid("point", POINT_INVALID)));
    }
    if !is_time_zone_name(&params.tz) {
        return Err(Box::new(invalid("tz", TIME_ZONE_INVALID)));
    }
    if params.from >= params.to {
        return Err(Box::new(invalid("from", RANGE_INVALID)));
    }
    // The automatic choice is always aggregated, so its period is the aggregated one.
    let period_of = params.interval.unwrap_or(Interval::Day1);
    let earliest = earliest(period_of, now);
    if params.to <= earliest {
        return Err(Box::new(error(
            ErrorBody::new(ErrorCode::QuotaExceeded)
                .with_message_key(OUTSIDE_PERIOD)
                .with_detail("earliest", earliest.to_rfc3339()),
        )));
    }
    let truncated = params.from < earliest;
    let from = params.from.max(earliest);
    let to = params.to;
    let interval = match params.interval {
        Some(interval) => {
            if interval
                .max_buckets(from, to)
                .is_some_and(|buckets| buckets > i64::from(MAX_BUCKETS))
            {
                return Err(Box::new(error(
                    ErrorBody::new(ErrorCode::ValidationFailed)
                        .with_message_key(TOO_MANY_POINTS)
                        .with_detail("parameter", "interval")
                        .with_detail("max", MAX_BUCKETS),
                )));
            }
            interval
        }
        None => Interval::AGGREGATED
            .into_iter()
            .find(|interval| {
                interval
                    .max_buckets(from, to)
                    .is_some_and(|buckets| buckets <= i64::from(MAX_BUCKETS))
            })
            .ok_or_else(|| Box::new(invalid("to", RANGE_INVALID)))?,
    };
    Ok(Plan {
        interval,
        from,
        to,
        truncated,
        earliest,
    })
}

/// Maps a ClickHouse failure of a tenant query to the response, and logs it without its text.
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

    /// Runs `statement` as `tenant`'s account, after checking the account is as the service
    /// creates it ([`Accounts::verify`]: its tenant, role, `readonly` and query limits). When
    /// ClickHouse refuses the account's password (a new password generation of the service's
    /// account, or a changed account), the check sets it again first. An account that differs
    /// in anything else is refused with 503, without running the statement: its limits or its
    /// tenant may not hold.
    pub(crate) async fn query(
        &self,
        tenant: TenantId,
        statement: &str,
        params: &[(&str, &str)],
    ) -> Result<String, Failure> {
        match self.accounts.verify(tenant).await {
            Ok(Ensured::Ready) => {}
            Ok(Ensured::PasswordSet) => {
                tracing::info!(tenant_id = %tenant, "set the tenant account's password again");
            }
            Err(AccountError::ClickHouse { source, .. }) if !source.is_authentication() => {
                return Err(Failure::ClickHouse(source));
            }
            Err(failure) => {
                tracing::error!(
                    tenant_id = %tenant,
                    error = %failure,
                    "the tenant's ClickHouse account is not as the service creates it, or refused                      its password and could not be repaired; no query runs as it"
                );
                return Err(Failure::Response(Box::new(error(ErrorBody::new(
                    ErrorCode::Unavailable,
                )))));
            }
        }
        let account = self.accounts.account(tenant);
        self.accounts
            .clickhouse()
            .query(account.credentials(), statement, params, None)
            .await
            .map_err(Failure::ClickHouse)
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
    let plan = match plan(&params, Utc::now()) {
        Ok(plan) => plan,
        Err(response) => return *response,
    };
    let tenant = match state.tenant(&user).await {
        Ok(tenant) => tenant,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(_)) => unreachable!("tenant() does not query ClickHouse"),
    };
    let (statement, step, limit) = match plan.interval.bucket() {
        None => (RAW_SERIES, 0, MAX_RAW_READINGS + 1),
        Some((Unit::Minute, step)) => (SERIES_MINUTES, step, usize::from(MAX_BUCKETS)),
        Some((Unit::Hour, step)) => (SERIES_HOURS, step, usize::from(MAX_BUCKETS)),
        Some((Unit::Day, step)) => (SERIES_DAYS, step, usize::from(MAX_BUCKETS)),
    };
    let (tenant_text, device_text) = (tenant.to_string(), device.to_string());
    let (from, to) = (
        plan.from.timestamp_millis().to_string(),
        plan.to.timestamp_millis().to_string(),
    );
    let (step, limit) = (step.to_string(), limit.to_string());
    let mut query_params = vec![
        ("tenant", tenant_text.as_str()),
        ("device", device_text.as_str()),
        ("point", params.point.as_str()),
        ("from", from.as_str()),
        ("to", to.as_str()),
        ("tz", params.tz.as_str()),
        ("limit", limit.as_str()),
    ];
    if plan.interval != Interval::Raw {
        query_params.push(("step", step.as_str()));
    }
    let body = match state.query(tenant, statement, &query_params).await {
        Ok(body) => body,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(failure)) if is_unknown_time_zone(&failure) => {
            return invalid("tz", TIME_ZONE_INVALID);
        }
        Err(Failure::ClickHouse(failure)) => return clickhouse_error(tenant, &failure),
    };
    let points = if plan.interval == Interval::Raw {
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
    } else {
        rows(
            &body,
            |(millis, count, min, max, avg, last): (
                i64,
                u64,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                ReadingColumns,
            )| {
                Some(SeriesPoint {
                    time: utc_millis(millis)?,
                    count,
                    min,
                    max,
                    avg,
                    last: reading(last)?,
                })
            },
        )
    };
    let Some(points) = points else {
        return unexpected_answer(tenant);
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
    let earliest = earliest(Interval::Raw, Utc::now());
    let (tenant_text, device_text) = (tenant.to_string(), device.to_string());
    let from = earliest.timestamp_millis().to_string();
    let limit = (MAX_LATEST_POINTS + 1).to_string();
    let params = [
        ("tenant", tenant_text.as_str()),
        ("device", device_text.as_str()),
        ("from", from.as_str()),
        ("limit", limit.as_str()),
    ];
    let body = match state.query(tenant, LATEST, &params).await {
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
            ("2026-07-21T12:00:00Z", Interval::Hours3),
        ] {
            let plan = plan(&params(from, NOW, None), now).expect("a plan");
            assert_eq!(plan.interval, interval, "{from}");
            assert!(!plan.truncated, "{from}");
        }
    }

    #[test]
    fn ranges_are_cut_to_the_queryable_period() {
        let now = at(NOW);
        // Raw: seven days.
        let raw = plan(
            &params("2026-10-01T00:00:00Z", NOW, Some(Interval::Hours6)),
            now,
        )
        .expect("aggregated reaches back three months");
        assert!(!raw.truncated);
        let raw = plan(
            &params(
                "2026-10-13T00:00:00Z",
                "2026-10-15T00:00:00Z",
                Some(Interval::Raw),
            ),
            now,
        )
        .expect("partly inside");
        assert!(raw.truncated);
        assert_eq!(raw.from, at("2026-10-14T12:00:00Z"));
        assert_eq!(raw.earliest, at("2026-10-14T12:00:00Z"));
        // Wholly before the raw period: refused with the period's start.
        assert_eq!(
            status(plan(
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
        let aggregated = plan(
            &params("2026-01-01T00:00:00Z", NOW, Some(Interval::Day1)),
            now,
        )
        .expect("partly inside");
        assert!(aggregated.truncated);
        assert_eq!(aggregated.from, at("2026-07-21T12:00:00Z"));
        assert_eq!(
            status(plan(
                &params("2026-01-01T00:00:00Z", "2026-07-21T12:00:00Z", None),
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
            assert_eq!(status(plan(&bad_tz, now)), 422, "{tz}");
        }
        for tz in [
            "UTC",
            "Asia/Taipei",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+8",
        ] {
            bad_tz.tz = tz.to_owned();
            assert!(plan(&bad_tz, now).is_ok(), "{tz}");
        }
        let mut bad_point = params("2026-10-21T00:00:00Z", NOW, None);
        for point in [String::new(), "p".repeat(129)] {
            bad_point.point = point;
            assert_eq!(status(plan(&bad_point, now)), 422);
        }
        assert_eq!(
            status(plan(&params(NOW, NOW, None), now)),
            422,
            "an empty range"
        );
        assert_eq!(
            status(plan(
                &params("2026-09-21T12:00:00Z", NOW, Some(Interval::Minute1)),
                now
            )),
            422,
            "a month of minutes has too many buckets"
        );
        assert_eq!(
            status(plan(
                &params("2026-10-21T00:00:00Z", "3000-01-01T00:00:00Z", None),
                now
            )),
            422,
            "no interval fits"
        );
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
            SERIES_MINUTES,
            SERIES_HOURS,
            SERIES_DAYS,
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
