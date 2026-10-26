//! A statistics or usage answer of telemetry-query flattened into a dataset's table: the fixed
//! columns of `contract::api::dashboard::result` for the source and interval, one row per series
//! and bucket (per series for `total`), series in the request's order and buckets in time order.
//!
//! The answer is read into this module's own types, which take only what the table shows, as
//! numbers, booleans, times and closed names; the series must be the request's, in its order,
//! so the only text in the table is the request's own device IDs and data point keys and the
//! readings' text values.

use chrono::{DateTime, FixedOffset, SecondsFormat};
use contract::api::dashboard::result::{ResultColumn, SemanticRange, semantic_columns};
use contract::api::dashboard::spec::{SemanticInterval, SemanticRequest, SourceKind};
use serde::Deserialize;
use serde_json::{Number, Value};
use uuid::Uuid;

/// A flattened answer.
#[derive(Debug, PartialEq)]
pub struct Table {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
    pub range: SemanticRange,
}

/// The answer is not what the request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Mismatch {
    #[error("the answer is not a statistics or usage answer")]
    Unparseable,
    #[error("the answer's series are not the request's, in its order")]
    Series,
    #[error("the answer's interval is not the request's")]
    Interval,
}

/// A reading, as telemetry-query writes it (`history::Reading`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum Reading {
    Float(Option<f64>),
    Int(i64),
    Bool(bool),
    String(String),
}

impl Reading {
    /// The two cells of a reading: the number (a boolean as 0 or 1) and the text.
    fn cells(&self) -> (Value, Value) {
        match self {
            Self::Float(value) => (value.map_or(Value::Null, number), Value::Null),
            Self::Int(value) => (Value::from(*value), Value::Null),
            Self::Bool(value) => (Value::from(u8::from(*value)), Value::Null),
            Self::String(text) => (Value::Null, Value::from(text.as_str())),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TimedReading {
    time: DateTime<FixedOffset>,
    value: Reading,
}

#[derive(Debug, Deserialize)]
struct TimedValue {
    time: DateTime<FixedOffset>,
    value: f64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DataStatus {
    Ok,
    NoData,
}

impl DataStatus {
    const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoData => "no_data",
        }
    }
}

#[derive(Debug, Deserialize)]
struct StatisticsRow {
    time: DateTime<FixedOffset>,
    count: u64,
    min: Option<f64>,
    max: Option<f64>,
    avg: Option<f64>,
    last: Reading,
}

#[derive(Debug, Deserialize)]
struct SeriesStatistics {
    device_id: Uuid,
    point: String,
    status: DataStatus,
    count: u64,
    numeric_count: u64,
    min: Option<f64>,
    min_time: Option<DateTime<FixedOffset>>,
    max: Option<f64>,
    max_time: Option<DateTime<FixedOffset>>,
    avg: Option<f64>,
    first: Option<TimedReading>,
    last: Option<TimedReading>,
    rows: Vec<StatisticsRow>,
}

#[derive(Debug, Deserialize)]
struct Statistics {
    range: SemanticRange,
    series: Vec<SeriesStatistics>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatus {
    Ok,
    NoData,
    NotNumeric,
    NotCumulative,
}

impl UsageStatus {
    const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoData => "no_data",
            Self::NotNumeric => "not_numeric",
            Self::NotCumulative => "not_cumulative",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Cumulative,
    TooFewReadings,
    NotCumulative,
}

impl Verdict {
    const fn name(self) -> &'static str {
        match self {
            Self::Cumulative => "cumulative",
            Self::TooFewReadings => "too_few_readings",
            Self::NotCumulative => "not_cumulative",
        }
    }
}

#[derive(Debug, Deserialize)]
struct CumulativeCheck {
    verdict: Verdict,
}

#[derive(Debug, Deserialize)]
struct UsageRow {
    time: DateTime<FixedOffset>,
    usage: f64,
    readings: u64,
    drops: u64,
    after_gap: bool,
    suspicious_steps: u64,
}

#[derive(Debug, Deserialize)]
struct SeriesUsage {
    device_id: Uuid,
    point: String,
    status: UsageStatus,
    check: Option<CumulativeCheck>,
    total: Option<f64>,
    drops: u64,
    readings: u64,
    first_reading: Option<TimedValue>,
    last_reading: Option<TimedValue>,
    baseline: bool,
    gaps: u64,
    suspicious_steps: u64,
    skipped_cells: u64,
    rows: Vec<UsageRow>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    range: SemanticRange,
    series: Vec<SeriesUsage>,
}

/// The table of a statistics answer `body` to `request`.
///
/// # Errors
///
/// [`Mismatch`] when the body is not a statistics answer to the request.
pub fn statistics(request: &SemanticRequest, body: &[u8]) -> Result<Table, Mismatch> {
    let answer: Statistics = serde_json::from_slice(body).map_err(|_| Mismatch::Unparseable)?;
    check(
        request,
        &answer.range,
        answer
            .series
            .iter()
            .map(|series| (series.device_id, series.point.as_str())),
    )?;
    let total = answer.range.interval == SemanticInterval::Total;
    let mut rows = Vec::new();
    for series in &answer.series {
        let (device_id, point) = (Value::from(series.device_id.to_string()), &series.point);
        if total {
            let (first, first_text, first_time) = timed_reading(series.first.as_ref());
            let (last, last_text, last_time) = timed_reading(series.last.as_ref());
            rows.push(vec![
                device_id,
                Value::from(point.as_str()),
                Value::from(series.status.name()),
                Value::from(series.count),
                Value::from(series.numeric_count),
                optional_number(series.min),
                optional_time(series.min_time.as_ref()),
                optional_number(series.max),
                optional_time(series.max_time.as_ref()),
                optional_number(series.avg),
                first,
                first_text,
                first_time,
                last,
                last_text,
                last_time,
            ]);
        } else {
            for row in &series.rows {
                let (last, last_text) = row.last.cells();
                rows.push(vec![
                    device_id.clone(),
                    Value::from(point.as_str()),
                    time(&row.time),
                    Value::from(row.count),
                    optional_number(row.min),
                    optional_number(row.max),
                    optional_number(row.avg),
                    last,
                    last_text,
                ]);
            }
        }
    }
    Ok(Table {
        columns: columns(SourceKind::TelemetryStatistics, answer.range.interval),
        rows,
        range: answer.range,
    })
}

/// The table of a usage answer `body` to `request`.
///
/// # Errors
///
/// [`Mismatch`] when the body is not a usage answer to the request.
pub fn usage(request: &SemanticRequest, body: &[u8]) -> Result<Table, Mismatch> {
    let answer: Usage = serde_json::from_slice(body).map_err(|_| Mismatch::Unparseable)?;
    check(
        request,
        &answer.range,
        answer
            .series
            .iter()
            .map(|series| (series.device_id, series.point.as_str())),
    )?;
    let total = answer.range.interval == SemanticInterval::Total;
    let mut rows = Vec::new();
    for series in &answer.series {
        let (device_id, point) = (Value::from(series.device_id.to_string()), &series.point);
        if total {
            let (first_reading, first_reading_time) = timed_value(series.first_reading.as_ref());
            let (last_reading, last_reading_time) = timed_value(series.last_reading.as_ref());
            rows.push(vec![
                device_id,
                Value::from(point.as_str()),
                Value::from(series.status.name()),
                optional_number(series.total),
                series
                    .check
                    .as_ref()
                    .map_or(Value::Null, |check| Value::from(check.verdict.name())),
                Value::from(series.readings),
                Value::from(series.drops),
                Value::from(series.gaps),
                Value::from(series.suspicious_steps),
                Value::from(series.baseline),
                first_reading,
                first_reading_time,
                last_reading,
                last_reading_time,
                Value::from(series.skipped_cells),
            ]);
        } else {
            for row in &series.rows {
                rows.push(vec![
                    device_id.clone(),
                    Value::from(point.as_str()),
                    time(&row.time),
                    number(row.usage),
                    Value::from(row.readings),
                    Value::from(row.drops),
                    Value::from(row.after_gap),
                    Value::from(row.suspicious_steps),
                ]);
            }
        }
    }
    Ok(Table {
        columns: columns(SourceKind::TelemetryUsage, answer.range.interval),
        rows,
        range: answer.range,
    })
}

/// The answer's series must be the request's, in its order, and its interval the request's.
fn check<'a>(
    request: &SemanticRequest,
    range: &SemanticRange,
    series: impl ExactSizeIterator<Item = (Uuid, &'a str)>,
) -> Result<(), Mismatch> {
    if range.interval != request.interval {
        return Err(Mismatch::Interval);
    }
    if series.len() != request.series.len()
        || !series
            .zip(&request.series)
            .all(|((device_id, point), asked)| {
                device_id == *asked.device_id.as_uuid() && point == asked.point
            })
    {
        return Err(Mismatch::Series);
    }
    Ok(())
}

fn columns(source: SourceKind, interval: SemanticInterval) -> Vec<ResultColumn> {
    semantic_columns(source, interval)
        .unwrap_or(&[])
        .iter()
        .map(|column| ResultColumn {
            name: column.name.to_owned(),
            kind: column.kind,
        })
        .collect()
}

/// A finite number; `null` for NaN and the infinities, which JSON cannot hold.
fn number(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn optional_number(value: Option<f64>) -> Value {
    value.map_or(Value::Null, number)
}

/// A time as RFC 3339 with the offset the answer gave, which the runtime reads as written.
fn time(value: &DateTime<FixedOffset>) -> Value {
    Value::from(value.to_rfc3339_opts(SecondsFormat::AutoSi, false))
}

fn optional_time(value: Option<&DateTime<FixedOffset>>) -> Value {
    value.map_or(Value::Null, time)
}

/// The number, text and time cells of a reading that may be absent.
fn timed_reading(reading: Option<&TimedReading>) -> (Value, Value, Value) {
    match reading {
        Some(reading) => {
            let (number, text) = reading.value.cells();
            (number, text, time(&reading.time))
        }
        None => (Value::Null, Value::Null, Value::Null),
    }
}

/// The number and time cells of a value that may be absent.
fn timed_value(value: Option<&TimedValue>) -> (Value, Value) {
    match value {
        Some(value) => (number(value.value), time(&value.time)),
        None => (Value::Null, Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request(interval: &str, series: &[(&str, &str)]) -> SemanticRequest {
        serde_json::from_value(json!({
            "series": series
                .iter()
                .map(|(device_id, point)| json!({"device_id": device_id, "point": point}))
                .collect::<Vec<_>>(),
            "tz": "Asia/Taipei",
            "period": "today",
            "interval": interval,
        }))
        .expect("a request")
    }

    fn range(interval: &str) -> Value {
        json!({
            "tz": "Asia/Taipei",
            "interval": interval,
            "from": "2026-10-26T00:00:00+08:00",
            "to": "2026-10-27T00:00:00+08:00",
            "truncated": false,
            "earliest": "2025-10-26T08:00:00+08:00",
            "ends_after_now": true,
            "resolution": "1h",
            "queried_at": "2026-10-26T09:02:03.456+08:00",
        })
    }

    const DEVICE: &str = "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b";

    fn bucket_answer(device_id: &str, point: &str) -> Value {
        json!({
            "range": range("1h"),
            "series": [{
                "device_id": device_id, "point": point, "status": "ok", "count": 4,
                "numeric_count": 4, "min": 1.0, "min_time": null, "max": 2.0, "max_time": null,
                "avg": 1.5, "first": null, "last": null, "highest": null, "lowest": null,
                "rows": [{
                    "time": "2026-10-26T08:00:00+08:00", "count": 4, "min": 1.0, "max": 2.0,
                    "avg": 1.5, "last": {"type": "bool", "value": true}
                }]
            }],
            "a_field_from_a_newer_service": {"ignored": true}
        })
    }

    #[test]
    fn a_reading_becomes_a_number_or_a_text() {
        assert_eq!(Reading::Float(Some(1.5)).cells(), (json!(1.5), Value::Null));
        assert_eq!(Reading::Float(None).cells(), (Value::Null, Value::Null));
        assert_eq!(Reading::Int(-7).cells(), (json!(-7), Value::Null));
        assert_eq!(Reading::Bool(true).cells(), (json!(1), Value::Null));
        assert_eq!(Reading::Bool(false).cells(), (json!(0), Value::Null));
        assert_eq!(
            Reading::String("auto".to_owned()).cells(),
            (Value::Null, json!("auto"))
        );
        assert_eq!(number(f64::NAN), Value::Null);
        assert_eq!(number(f64::INFINITY), Value::Null);
    }

    #[test]
    fn the_answer_must_be_for_the_requests_series_and_interval() {
        let asked = request("1h", &[(DEVICE, "power")]);
        let body = |answer: &Value| serde_json::to_vec(answer).expect("JSON");
        let table = statistics(&asked, &body(&bucket_answer(DEVICE, "power"))).expect("a table");
        assert_eq!(
            table.rows,
            vec![vec![
                json!(DEVICE),
                json!("power"),
                json!("2026-10-26T08:00:00+08:00"),
                json!(4),
                json!(1.0),
                json!(2.0),
                json!(1.5),
                json!(1),
                Value::Null,
            ]]
        );
        assert_eq!(
            statistics(&asked, &body(&bucket_answer(DEVICE, "energy"))),
            Err(Mismatch::Series)
        );
        assert_eq!(
            statistics(
                &asked,
                &body(&bucket_answer(
                    "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7c",
                    "power"
                ))
            ),
            Err(Mismatch::Series)
        );
        assert_eq!(
            statistics(
                &request("1d", &[(DEVICE, "power")]),
                &body(&bucket_answer(DEVICE, "power"))
            ),
            Err(Mismatch::Interval)
        );
        let mut two = bucket_answer(DEVICE, "power");
        let series = two["series"][0].clone();
        two["series"].as_array_mut().expect("an array").push(series);
        assert_eq!(statistics(&asked, &body(&two)), Err(Mismatch::Series));
        // A status this build does not know, a usage answer, or not JSON.
        let mut unknown = bucket_answer(DEVICE, "power");
        unknown["series"][0]["status"] = json!("partial");
        assert_eq!(
            statistics(&asked, &body(&unknown)),
            Err(Mismatch::Unparseable)
        );
        assert_eq!(
            usage(&asked, &body(&bucket_answer(DEVICE, "power"))),
            Err(Mismatch::Unparseable)
        );
        assert_eq!(statistics(&asked, b"<html>"), Err(Mismatch::Unparseable));
    }
}
