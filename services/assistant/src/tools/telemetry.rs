//! The telemetry tools (Laysi/trustiot-v2#591): a device data point's time series and a device's
//! latest values, read from the telemetry query service's history API
//! (services/telemetry-query/README.md, "API") as the question's user. The service runs every
//! query as the user's tenant account, so a device of another tenant has no data.

use std::sync::Arc;

use chrono::{DateTime, Months, SecondsFormat, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::callee::{Callee, Get};
use crate::model::BoxFuture;
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};

/// The routes, as services/telemetry-query serves them (`history::SERIES_PATH`, `LATEST_PATH`).
pub const SERIES_PATH: &str = "/api/telemetry/v1/devices/{device_id}/series";
pub const LATEST_PATH: &str = "/api/telemetry/v1/devices/{device_id}/latest";

/// Raw readings are queryable this many days back (the service's `RAW_DAYS`).
pub const RAW_DAYS: i64 = 7;
/// Aggregated series are queryable this many months back (the service's `AGGREGATED_MONTHS`).
pub const AGGREGATED_MONTHS: u32 = 3;
/// The longest data point key (the service's `MAX_POINT_LEN`).
pub const MAX_POINT_LEN: usize = 128;
/// The longest time zone name the tool passes on (the service accepts 64).
const MAX_TIME_ZONE_LEN: usize = 64;

/// The most rows of a series handed to the model. The service returns up to 1,500 buckets or
/// 10,000 raw readings; more than this is too much to read, so the tool picks an interval that
/// stays within it, and refuses a raw series over it.
pub const MAX_SERIES_ROWS: usize = 500;
/// The most data points of the latest values handed to the model; the rest are counted.
pub const MAX_LATEST_ROWS: usize = 200;
/// The most data points the latest-value tool may be asked to pick.
const MAX_PICKED_POINTS: usize = 100;

/// The intervals, finest first, with their nominal length in seconds.
const INTERVALS: [(&str, i64); 8] = [
    ("1m", 60),
    ("5m", 300),
    ("15m", 900),
    ("1h", 3_600),
    ("3h", 10_800),
    ("6h", 21_600),
    ("12h", 43_200),
    ("1d", 86_400),
];

const SERIES_REJECTED: &str = "The telemetry service rejected the parameters. Check that \
`point` is a data point key of the device (1 to 128 characters, such as `voltage`), that `tz` is \
an IANA time zone name such as `Asia/Taipei` or `UTC`, that `from` is before `to`, and that the \
range is within the queryable period: raw readings for the last 7 days, aggregated intervals for \
the last 3 months.";

const LATEST_REJECTED: &str = "The telemetry service rejected the request. `device_id` must be \
the device's ID, a UUID such as `0192f6e0-7a3c-7d4e-8f00-0123456789ab`.";

/// A device data point's series.
pub struct TelemetrySeries {
    telemetry: Callee,
}

impl TelemetrySeries {
    pub const NAME: &'static str = "telemetry_series";

    /// The tool, calling the telemetry query service through `telemetry`.
    pub fn new(telemetry: Callee) -> Arc<Self> {
        Arc::new(Self { telemetry })
    }
}

/// The model's input of [`TelemetrySeries`]. No tenant or user: the service takes both from the
/// user's token.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeriesInput {
    device_id: String,
    point: String,
    from: String,
    to: String,
    tz: String,
    #[serde(default)]
    interval: Option<String>,
}

const SERIES_FIELDS: &str = "The input has `device_id`, `point`, `from`, `to` and `tz`, and \
optionally `interval`; nothing else. The organisation and user are always the asking user's.";

impl Tool for TelemetrySeries {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Reads one data point of one device over a time range, as the asking user. \
                 Returns JSON with the range actually queried (`from` is moved to the start of \
                 the queryable period when the request started earlier, and then `truncated` is \
                 true), the interval used, and `rows`: one row per bucket with readings, in time \
                 order, as [bucket start (UTC), reading count, min, max, average, last reading]. \
                 Buckets without readings are left out; min, max and average are null for text \
                 readings. With interval `raw`, each row is one reading. Queryable period: raw \
                 readings for the last {RAW_DAYS} days, aggregated intervals for the last \
                 {AGGREGATED_MONTHS} months. At most {MAX_SERIES_ROWS} rows: without `interval` \
                 the tool picks the finest interval that fits; a raw series with more readings \
                 is refused, so ask for an interval or a shorter range. Use telemetry_latest \
                 first to see which data points a device has."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "device_id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The device's ID, a UUID."
                    },
                    "point": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_POINT_LEN,
                        "description": "The data point key, such as `voltage`."
                    },
                    "from": {
                        "type": "string",
                        "format": "date-time",
                        "description": "Start of the range (inclusive), RFC 3339 with an offset, such as `2026-10-22T00:00:00+08:00`."
                    },
                    "to": {
                        "type": "string",
                        "format": "date-time",
                        "description": "End of the range (exclusive), RFC 3339 with an offset."
                    },
                    "tz": {
                        "type": "string",
                        "maxLength": MAX_TIME_ZONE_LEN,
                        "description": "IANA time zone whose local hours and midnights the hour and day buckets start at: the user's, such as `Asia/Taipei`. Use `UTC` when unknown."
                    },
                    "interval": {
                        "type": "string",
                        "enum": ["raw", "1m", "5m", "15m", "1h", "3h", "6h", "12h", "1d"],
                        "description": "Bucket length, or `raw` for every reading. Leave it out to get the finest interval with at most 500 buckets."
                    }
                },
                "required": ["device_id", "point", "from", "to", "tz"],
                "additionalProperties": false
            }),
        }
    }

    fn narrowing_hint(&self) -> &str {
        "Ask for a coarser interval (such as 1h or 1d) or a shorter range."
    }

    fn call(
        self: Arc<Self>,
        context: ToolContext,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: SeriesInput = serde_json::from_value(input)
                .map_err(|_| ToolError::Input(SERIES_FIELDS.into()))?;
            let request = SeriesRequest::check(&input, Utc::now())?;
            let path = device_path(SERIES_PATH, request.device_id);
            let from = rfc3339(request.from);
            let to = rfc3339(request.to);
            let series: Series = self
                .telemetry
                .get(
                    &context,
                    Get {
                        tool: Self::NAME,
                        path: &path,
                        query: &[
                            ("point", input.point.as_str()),
                            ("from", &from),
                            ("to", &to),
                            ("tz", input.tz.as_str()),
                            ("interval", request.interval),
                        ],
                        rejected_input: SERIES_REJECTED,
                    },
                )
                .await?;
            if series.device_id != request.device_id {
                return Err(ToolError::Failed);
            }
            if series.points.len() > MAX_SERIES_ROWS {
                return Err(ToolError::Input(format!(
                    "The series has {} readings in this range, more than the {MAX_SERIES_ROWS} \
                     this tool returns. Ask for an aggregated interval (such as 1h) or a shorter \
                     range.",
                    series.points.len()
                )));
            }
            let rows: Vec<Value> = series
                .points
                .iter()
                .map(|point| {
                    json!([
                        point.time,
                        point.count,
                        point.min,
                        point.max,
                        point.avg,
                        point.last.to_json()
                    ])
                })
                .collect();
            let result = json!({
                "device_id": series.device_id,
                "point": series.point,
                "tz": series.tz,
                "interval": series.interval,
                "from": series.from,
                "to": series.to,
                "truncated": series.truncated,
                "earliest": series.earliest,
                "columns": ["time", "count", "min", "max", "avg", "last"],
                "rows": rows,
            });
            Ok(ToolOutput::text(result.to_string()))
        })
    }
}

/// A series request after the tool's own checks, which give the model a precise message before
/// anything is sent.
#[derive(Debug, PartialEq, Eq)]
struct SeriesRequest {
    device_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    interval: &'static str,
}

impl SeriesRequest {
    fn check(input: &SeriesInput, now: DateTime<Utc>) -> Result<Self, ToolError> {
        let device_id = parse_device_id(&input.device_id)?;
        if input.point.is_empty() || input.point.len() > MAX_POINT_LEN {
            return Err(ToolError::Input(
                "`point` must be a data point key of 1 to 128 characters, such as `voltage`."
                    .into(),
            ));
        }
        if input.tz.is_empty() || input.tz.len() > MAX_TIME_ZONE_LEN {
            return Err(ToolError::Input(
                "`tz` must be an IANA time zone name, such as `Asia/Taipei` or `UTC`.".into(),
            ));
        }
        let from = parse_time("from", &input.from)?;
        let to = parse_time("to", &input.to)?;
        if from >= to {
            return Err(ToolError::Input("`from` must be before `to`.".into()));
        }
        let requested = match input.interval.as_deref() {
            None => None,
            Some("raw") => Some("raw"),
            Some(other) => Some(
                INTERVALS
                    .iter()
                    .find(|(name, _)| *name == other)
                    .map(|(name, _)| *name)
                    .ok_or_else(|| {
                        ToolError::Input(
                            "`interval` must be one of raw, 1m, 5m, 15m, 1h, 3h, 6h, 12h, 1d, or \
                             left out."
                                .into(),
                        )
                    })?,
            ),
        };
        let earliest = earliest(requested == Some("raw"), now);
        if to <= earliest {
            return Err(ToolError::Input(format!(
                "The range ends before the queryable period, which starts at {} for {}. Ask for \
                 a later range.",
                rfc3339(earliest),
                if requested == Some("raw") {
                    "raw readings (the last 7 days)"
                } else {
                    "aggregated intervals (the last 3 months)"
                }
            )));
        }
        let start = from.max(earliest);
        let interval = match requested {
            Some("raw") => "raw",
            Some(name) => {
                let seconds = INTERVALS
                    .iter()
                    .find(|(interval, _)| *interval == name)
                    .map_or(86_400, |(_, seconds)| *seconds);
                if max_buckets(seconds, start, to) > MAX_SERIES_ROWS {
                    return Err(ToolError::Input(format!(
                        "Interval {name} gives more than {MAX_SERIES_ROWS} buckets over this \
                         range. Ask for a coarser interval or a shorter range, or leave \
                         `interval` out to get the finest one that fits."
                    )));
                }
                name
            }
            None => INTERVALS
                .iter()
                .find(|(_, seconds)| max_buckets(*seconds, start, to) <= MAX_SERIES_ROWS)
                .map_or("1d", |(name, _)| *name),
        };
        Ok(Self {
            device_id,
            from,
            to,
            interval,
        })
    }
}

/// The most buckets `from..to` spans at an interval of `seconds`, as the service counts them:
/// one more for a range that does not start on a bucket boundary, one more for a daylight saving
/// change.
fn max_buckets(seconds: i64, from: DateTime<Utc>, to: DateTime<Utc>) -> usize {
    let range = (to - from).num_seconds().max(0);
    usize::try_from((range + seconds - 1) / seconds + 2).unwrap_or(usize::MAX)
}

/// The start of the queryable period at `now`, as the service computes it.
fn earliest(raw: bool, now: DateTime<Utc>) -> DateTime<Utc> {
    if raw {
        now - TimeDelta::days(RAW_DAYS)
    } else {
        now.checked_sub_months(Months::new(AGGREGATED_MONTHS))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }
}

/// A device's latest values.
pub struct TelemetryLatest {
    telemetry: Callee,
}

impl TelemetryLatest {
    pub const NAME: &'static str = "telemetry_latest";

    /// The tool, calling the telemetry query service through `telemetry`.
    pub fn new(telemetry: Callee) -> Arc<Self> {
        Arc::new(Self { telemetry })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatestInput {
    device_id: String,
    #[serde(default)]
    points: Option<Vec<String>>,
}

const LATEST_FIELDS: &str = "The input has `device_id`, and optionally `points` (a list of data \
point keys); nothing else. The organisation and user are always the asking user's.";

impl Tool for TelemetryLatest {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Reads the latest reading of each data point of one device, as the asking user: \
                 what the device reports now, and which data points it has. Only data points \
                 with a reading in the last {RAW_DAYS} days are listed. Returns JSON with `rows` \
                 ordered by data point, each [data point key, reading time (UTC), value], and \
                 `total`, the number of data points found. At most {MAX_LATEST_ROWS} rows; when \
                 `total` is larger, pass `points` to choose which ones."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "device_id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The device's ID, a UUID."
                    },
                    "points": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": MAX_POINT_LEN},
                        "maxItems": MAX_PICKED_POINTS,
                        "description": "Only these data point keys. Leave it out for all."
                    }
                },
                "required": ["device_id"],
                "additionalProperties": false
            }),
        }
    }

    fn narrowing_hint(&self) -> &str {
        "Pass `points` with fewer data point keys."
    }

    fn call(
        self: Arc<Self>,
        context: ToolContext,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: LatestInput = serde_json::from_value(input)
                .map_err(|_| ToolError::Input(LATEST_FIELDS.into()))?;
            let device_id = parse_device_id(&input.device_id)?;
            if let Some(points) = &input.points
                && (points.is_empty() || points.len() > MAX_PICKED_POINTS)
            {
                return Err(ToolError::Input(format!(
                    "`points` must list 1 to {MAX_PICKED_POINTS} data point keys, or be left out."
                )));
            }
            let path = device_path(LATEST_PATH, device_id);
            let latest: Latest = self
                .telemetry
                .get(
                    &context,
                    Get {
                        tool: Self::NAME,
                        path: &path,
                        query: &[],
                        rejected_input: LATEST_REJECTED,
                    },
                )
                .await?;
            if latest.device_id != device_id {
                return Err(ToolError::Failed);
            }
            let picked: Vec<&LatestPoint> = latest
                .points
                .iter()
                .filter(|point| {
                    input
                        .points
                        .as_ref()
                        .is_none_or(|keys| keys.contains(&point.point))
                })
                .collect();
            let rows: Vec<Value> = picked
                .iter()
                .take(MAX_LATEST_ROWS)
                .map(|point| json!([point.point, point.time, point.value.to_json()]))
                .collect();
            let mut result = json!({
                "device_id": latest.device_id,
                "earliest": latest.earliest,
                "columns": ["point", "time", "value"],
                "rows": rows,
                "total": picked.len(),
            });
            if picked.len() > MAX_LATEST_ROWS {
                result["note"] = json!(format!(
                    "Only the first {MAX_LATEST_ROWS} of {} data points are shown; pass `points` \
                     for the others.",
                    picked.len()
                ));
            }
            Ok(ToolOutput::text(result.to_string()))
        })
    }
}

fn parse_device_id(text: &str) -> Result<Uuid, ToolError> {
    Uuid::try_parse(text).map_err(|_| {
        ToolError::Input(
            "`device_id` must be the device's ID, a UUID such as \
             `0192f6e0-7a3c-7d4e-8f00-0123456789ab`."
                .into(),
        )
    })
}

fn parse_time(name: &str, text: &str) -> Result<DateTime<Utc>, ToolError> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| {
            ToolError::Input(format!(
                "`{name}` must be an RFC 3339 time with an offset, such as \
                 `2026-10-22T00:00:00+08:00` or `2026-10-21T16:00:00Z`."
            ))
        })
}

/// UTC with `Z`, the form the service's query string takes without percent-encoding a `+`.
fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// The route with the device's ID, in the canonical hyphenated form (only hex digits and `-`).
fn device_path(route: &str, device_id: Uuid) -> String {
    route.replace("{device_id}", &device_id.hyphenated().to_string())
}

/// A reading, as the service writes it (`history::Reading`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum Reading {
    Float(Option<f64>),
    Int(i64),
    Bool(bool),
    String(String),
}

impl Reading {
    fn to_json(&self) -> Value {
        match self {
            Self::Float(value) => json!(value),
            Self::Int(value) => json!(value),
            Self::Bool(value) => json!(value),
            Self::String(value) => json!(value),
        }
    }
}

/// The fields of the service's `history::Series` the tool hands on. Unknown fields are ignored,
/// so the service may add some.
#[derive(Debug, Deserialize)]
struct Series {
    device_id: Uuid,
    point: String,
    tz: String,
    interval: String,
    from: String,
    to: String,
    truncated: bool,
    earliest: String,
    points: Vec<SeriesPoint>,
}

#[derive(Debug, Deserialize)]
struct SeriesPoint {
    time: String,
    count: u64,
    min: Option<f64>,
    max: Option<f64>,
    avg: Option<f64>,
    last: Reading,
}

/// The fields of the service's `history::Latest`.
#[derive(Debug, Deserialize)]
struct Latest {
    device_id: Uuid,
    earliest: String,
    points: Vec<LatestPoint>,
}

#[derive(Debug, Deserialize)]
struct LatestPoint {
    point: String,
    time: String,
    value: Reading,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(from: &str, to: &str, interval: Option<&str>) -> SeriesInput {
        SeriesInput {
            device_id: "0192f6e0-7a3c-7d4e-8f00-0123456789ab".into(),
            point: "voltage".into(),
            from: from.into(),
            to: to.into(),
            tz: "Asia/Taipei".into(),
            interval: interval.map(str::to_owned),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-23T12:00:00Z")
            .expect("a time")
            .with_timezone(&Utc)
    }

    fn checked(from: &str, to: &str, interval: Option<&str>) -> Result<SeriesRequest, ToolError> {
        SeriesRequest::check(&input(from, to, interval), now())
    }

    #[test]
    fn without_an_interval_the_finest_that_fits_is_picked() {
        // One day: 1440 minutes and 288 five-minute buckets, so 5m.
        let day = checked(
            "2026-10-22T12:00:00+08:00",
            "2026-10-23T12:00:00+08:00",
            None,
        )
        .expect("valid");
        assert_eq!(day.interval, "5m");
        assert_eq!(
            rfc3339(day.from),
            "2026-10-22T04:00:00Z",
            "times go on in UTC"
        );
        // 91 days: 3h gives 730 buckets, 6h 366.
        let quarter = checked("2026-07-24T00:00:00Z", "2026-10-23T00:00:00Z", None).expect("valid");
        assert_eq!(quarter.interval, "6h");
        // A range that starts before the period is cut to it before counting: years would need
        // 1d, the period's 91.5 days fit 6h.
        let long = checked("2020-01-01T00:00:00Z", "2026-10-23T00:00:00Z", None).expect("valid");
        assert_eq!(long.interval, "6h");
        assert_eq!(
            rfc3339(long.from),
            "2020-01-01T00:00:00Z",
            "`from` goes on as asked"
        );
    }

    #[test]
    fn a_requested_interval_must_fit_and_the_range_must_reach_the_period() {
        let error = checked("2026-10-20T00:00:00Z", "2026-10-23T00:00:00Z", Some("1m"))
            .expect_err("4320 buckets");
        assert!(matches!(error, ToolError::Input(message) if message.contains("coarser")));
        assert_eq!(
            checked("2026-10-20T00:00:00Z", "2026-10-23T00:00:00Z", Some("raw"))
                .expect("raw has no bucket count")
                .interval,
            "raw"
        );
        let error = checked("2026-10-01T00:00:00Z", "2026-10-10T00:00:00Z", Some("raw"))
            .expect_err("before the raw period");
        assert!(
            matches!(error, ToolError::Input(message) if message.contains("2026-10-16T12:00:00Z"))
        );
        let error = checked("2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z", None)
            .expect_err("before the aggregated period");
        assert!(
            matches!(error, ToolError::Input(message) if message.contains("2026-07-23T12:00:00Z"))
        );
    }

    #[test]
    fn malformed_input_gets_a_message_that_says_how_to_fix_it() {
        for (input, needle) in [
            (
                SeriesInput {
                    device_id: "floor-3".into(),
                    ..input("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z", None)
                },
                "UUID",
            ),
            (input("yesterday", "2026-10-23T00:00:00Z", None), "RFC 3339"),
            (
                input("2026-10-23T00:00:00Z", "2026-10-22T00:00:00Z", None),
                "before `to`",
            ),
            (
                input("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z", Some("2h")),
                "one of raw",
            ),
            (
                SeriesInput {
                    point: String::new(),
                    ..input("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z", None)
                },
                "data point key",
            ),
            (
                SeriesInput {
                    tz: "x".repeat(65),
                    ..input("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z", None)
                },
                "IANA",
            ),
        ] {
            let error = SeriesRequest::check(&input, now()).expect_err("invalid");
            assert!(
                matches!(&error, ToolError::Input(message) if message.contains(needle)),
                "{error:?} lacks {needle}"
            );
        }
    }

    #[test]
    fn the_device_id_is_put_in_the_path_in_canonical_form() {
        let id = Uuid::try_parse("0192F6E07A3C7D4E8F000123456789AB").expect("a UUID");
        assert_eq!(
            device_path(SERIES_PATH, id),
            "/api/telemetry/v1/devices/0192f6e0-7a3c-7d4e-8f00-0123456789ab/series"
        );
    }
}
