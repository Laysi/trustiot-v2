//! The semantic API (Laysi/trustiot-v2#142): statistics and usage of a lab dataset, loaded into
//! ClickHouse as the ingest service writes it, equal what this test computes from the dataset's
//! readings; the cumulative check, the gap and the suspicious-step rules give the documented
//! answers on series written for them; the queryable period cuts and refuses ranges at the edge
//! the service reports; another tenant's devices have no data; and hostile values never reach
//! the SQL as SQL.
//!
//! The dataset is `lab_dataset::index_for_tests()`: the synthetic fixture, or the index that
//! `TRUSTIOT_LAB_DATASET_INDEX` names (the lab recording, Laysi/trustiot-v2#681). Every expected
//! value comes from the dataset's readings through the reference below, which implements the
//! rules of services/telemetry-query/README.md (「語意查詢」) in Rust, cell by cell, without the
//! service's SQL; the fixture's own properties (a reset, a 32-bit wrap, missing reads, gauges,
//! state points) are asserted only when the dataset is the fixture.
//!
//! Needs podman or docker (a TLS Postgres and a ClickHouse); skipped without one unless `CI` is
//! set. One test: the binary shares one product database.

#![allow(
    clippy::print_stderr,
    reason = "prints each compared answer: the evidence `-- --nocapture` shows"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};
use contract::{ServiceName, TenantId, TenantRole};
use jiff::tz::TimeZone;
use lab_dataset::index::PointKind;
use lab_dataset::{Dataset, Loaded, Target, TimeShift, Value as Recorded, index_for_tests};
use serde_json::{Value, json};
use support::{ClickHouseTarget, Service, Stack, wait_account_ready};
use telemetry_query::period::{View, Windows};
use telemetry_query::semantic::{self, SeriesKey, Verdict, statements};
use testkit::identity::TestIssuer;
use uuid::Uuid;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

const STATISTICS: &str = semantic::STATISTICS_PATH;
const USAGE: &str = semantic::USAGE_PATH;

// ---------------------------------------------------------------------------------------------
// The reference: the README's rules over the dataset's readings.

/// One reading of a series: time in ms and value.
#[derive(Clone, Debug)]
struct Point {
    ms: i64,
    value: Recorded,
}

/// The value the aggregates count as a number (a bool as 0 or 1).
fn number(value: &Recorded) -> Option<f64> {
    match value {
        Recorded::Float(v) => Some(*v),
        Recorded::Int(v) => Some(*v as f64),
        Recorded::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        Recorded::String(_) => None,
    }
}

/// The value the usage counts: int or float only, NaN otherwise.
fn counted(value: &Recorded) -> f64 {
    match value {
        Recorded::Float(v) => *v,
        Recorded::Int(v) => *v as f64,
        Recorded::Bool(_) | Recorded::String(_) => f64::NAN,
    }
}

/// One aggregate cell of a series.
#[derive(Clone, Debug)]
struct Cell {
    start: i64,
    readings: u64,
    first: Point,
    last: Point,
    min: Option<f64>,
    max: Option<f64>,
}

impl Cell {
    fn f(&self) -> f64 {
        counted(&self.first.value)
    }
    fn l(&self) -> f64 {
        counted(&self.last.value)
    }
    fn usable(&self) -> bool {
        let (low, high) = (self.min.unwrap_or(f64::NAN), self.max.unwrap_or(f64::NAN));
        [self.f(), self.l(), low, high]
            .iter()
            .all(|v| v.is_finite())
    }
    fn non_numeric(&self) -> bool {
        let text_or_bool = |v: &Recorded| matches!(v, Recorded::Bool(_) | Recorded::String(_));
        text_or_bool(&self.first.value) || text_or_bool(&self.last.value)
    }
}

/// The cells of `points` (in time order) at a cell length of `cell_ms`.
fn cells(points: &[Point], cell_ms: i64) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    for point in points {
        let start = point.ms.div_euclid(cell_ms) * cell_ms;
        let value = number(&point.value);
        match cells.last_mut() {
            Some(cell) if cell.start == start => {
                cell.readings += 1;
                cell.last = point.clone();
                if let Some(value) = value {
                    cell.min = Some(cell.min.map_or(value, |min| min.min(value)));
                    cell.max = Some(cell.max.map_or(value, |max| max.max(value)));
                }
            }
            _ => cells.push(Cell {
                start,
                readings: 1,
                first: point.clone(),
                last: point.clone(),
                min: value,
                max: value,
            }),
        }
    }
    cells
}

/// `quantileExact(0.5)`: the element at position n / 2 of the sorted values; 0 without any.
fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// One bucket of the reference usage.
#[derive(Clone, Debug, Default, PartialEq)]
struct UsageBucket {
    usage: f64,
    readings: u64,
    drops: u64,
    gaps: u64,
    suspicious: u64,
}

/// The reference usage of one series.
#[derive(Debug, Default)]
struct UsageReference {
    buckets: BTreeMap<i64, UsageBucket>,
    drops: u64,
    rises: u64,
    comparisons: u64,
    baseline: bool,
    non_numeric: bool,
    usable: u64,
    longest_gap: Option<(i64, i64)>,
    largest_suspicious: Option<(i64, f64)>,
}

/// The README's usage rules over the cells of the window, summed into the buckets of the cells
/// that start in `from..to`.
fn usage_reference(
    window: &[Cell],
    (from, to): (i64, i64),
    bucket: &dyn Fn(i64) -> i64,
) -> UsageReference {
    struct Step {
        inside: f64,
        across: f64,
        inside_compared: bool,
        inside_drop: bool,
        inside_rise: bool,
        across_compared: bool,
        across_drop: bool,
        across_rise: bool,
        spacing: i64,
        prev_last: i64,
    }
    let steps: Vec<Step> = window
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let (f, l) = (cell.f(), cell.l());
            let (low, high) = (cell.min.unwrap_or(f64::NAN), cell.max.unwrap_or(f64::NAN));
            let previous = index.checked_sub(1).map(|i| &window[i]);
            let prev_l = previous.map_or(f64::NAN, Cell::l);
            let inside_compared = cell.usable() && cell.readings > 1;
            let inside_drop = inside_compared && (l < high || low < f);
            let across_compared = cell.usable() && prev_l.is_finite();
            let across_drop = across_compared && f < prev_l;
            Step {
                inside: match (inside_compared, inside_drop) {
                    (false, _) => 0.0,
                    (true, true) => high - f + l,
                    (true, false) => l - f,
                },
                across: match (across_compared, across_drop) {
                    (false, _) => 0.0,
                    (true, true) => f,
                    (true, false) => f - prev_l,
                },
                inside_compared,
                inside_drop,
                inside_rise: inside_compared && !inside_drop && l > f,
                across_compared,
                across_drop,
                across_rise: across_compared && f > prev_l,
                spacing: cell.first.ms - previous.map_or(0, |p| p.last.ms),
                prev_last: previous.map_or(0, |p| p.last.ms),
            }
        })
        .collect();
    let count = |test: &dyn Fn(&Step) -> bool| steps.iter().filter(|s| test(s)).count() as u64;
    let rises: Vec<f64> = steps
        .iter()
        .filter(|s| s.across_rise)
        .map(|s| s.across * 1000.0 / s.spacing.max(1) as f64)
        .collect();
    let rate_basis = rises.len() as u64;
    let median_rate = median(rises);
    let median_spacing = median(
        steps
            .iter()
            .filter(|s| s.across_compared)
            .map(|s| s.spacing as f64)
            .collect(),
    );
    let mut reference = UsageReference {
        drops: count(&|s| s.inside_drop) + count(&|s| s.across_drop),
        rises: count(&|s| s.inside_rise) + count(&|s| s.across_rise),
        comparisons: count(&|s| s.inside_compared) + count(&|s| s.across_compared),
        baseline: window.first().is_some_and(|cell| cell.start < from),
        ..UsageReference::default()
    };
    let enough = rate_basis >= u64::from(semantic::MIN_RISES_FOR_SUSPICION);
    let suspicious = |amount: f64, duration: f64| {
        enough
            && amount > 0.0
            && amount * 1000.0 / duration.max(median_spacing).max(1.0)
                > semantic::SUSPICIOUS_FACTOR * median_rate
    };
    for (cell, step) in window.iter().zip(&steps) {
        if cell.start < from || cell.start >= to {
            continue;
        }
        reference.non_numeric |= cell.non_numeric();
        reference.usable += u64::from(cell.usable());
        let entry = reference.buckets.entry(bucket(cell.start)).or_default();
        entry.usage += step.inside + step.across;
        entry.readings += cell.readings;
        entry.drops += u64::from(step.inside_drop) + u64::from(step.across_drop);
        if step.across_compared && step.spacing as f64 >= semantic::GAP_FACTOR * median_spacing {
            entry.gaps += 1;
            if reference
                .longest_gap
                .is_none_or(|(_, ms)| step.spacing > ms)
            {
                reference.longest_gap = Some((step.prev_last, step.spacing));
            }
        }
        for (amount, duration, at) in [
            (
                step.inside,
                (cell.last.ms - cell.first.ms) as f64,
                cell.last.ms,
            ),
            (step.across, step.spacing as f64, cell.first.ms),
        ] {
            if suspicious(amount, duration) {
                entry.suspicious += 1;
                if reference
                    .largest_suspicious
                    .is_none_or(|(_, best)| amount > best)
                {
                    reference.largest_suspicious = Some((at, amount));
                }
            }
        }
    }
    reference
}

/// The usage by consecutive readings, a drop counting as a restart from 0: what the cells give
/// wherever each holds at most one drop and stays below its peak after it.
fn reading_usage(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let (previous, next) = (counted(&pair[0].value), counted(&pair[1].value));
            if next >= previous {
                next - previous
            } else {
                next
            }
        })
        .sum()
}

/// One bucket of the reference statistics.
#[derive(Clone, Debug, Default)]
struct StatisticsBucket {
    count: u64,
    numeric: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    last: Option<Point>,
}

fn statistics_reference(
    points: &[Point],
    (from, to): (i64, i64),
    bucket: &dyn Fn(i64) -> i64,
) -> BTreeMap<i64, StatisticsBucket> {
    let mut buckets: BTreeMap<i64, StatisticsBucket> = BTreeMap::new();
    for point in points.iter().filter(|p| p.ms >= from && p.ms < to) {
        let entry = buckets.entry(bucket(point.ms)).or_default();
        entry.count += 1;
        if let Some(value) = number(&point.value) {
            entry.numeric += 1;
            entry.sum += value;
            entry.min = Some(entry.min.map_or(value, |min| min.min(value)));
            entry.max = Some(entry.max.map_or(value, |max| max.max(value)));
        }
        if entry.last.as_ref().is_none_or(|last| point.ms >= last.ms) {
            entry.last = Some(point.clone());
        }
    }
    buckets
}

/// The start of the bucket of `interval` in `tz` that holds `ms`, as the statements place it.
fn bucket_of(interval: &str, tz: &TimeZone, from: i64, ms: i64) -> i64 {
    let instant = jiff::Timestamp::from_millisecond(ms).expect("a time");
    let midnight = |date: jiff::civil::Date| {
        date.to_zoned(tz.clone())
            .expect("a local midnight")
            .timestamp()
            .as_millisecond()
    };
    let minutes = |n: i64| ms.div_euclid(n * MINUTE_MS) * n * MINUTE_MS;
    match interval {
        "1m" => minutes(1),
        "5m" => minutes(5),
        "15m" => minutes(15),
        "1h" => {
            let offset = i64::from(tz.to_offset(instant).seconds()) * 1000;
            (ms + offset).div_euclid(HOUR_MS) * HOUR_MS - offset
        }
        "1d" => midnight(instant.to_zoned(tz.clone()).date()),
        "1w" => {
            let date = instant.to_zoned(tz.clone()).date();
            let back = i64::from(date.weekday().to_monday_zero_offset());
            midnight(
                date.checked_sub(jiff::Span::new().days(back))
                    .expect("a date"),
            )
        }
        "1mo" => midnight(instant.to_zoned(tz.clone()).date().first_of_month()),
        "total" => from,
        other => panic!("no interval {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Comparing the answers.

fn ms(value: &Value) -> i64 {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .unwrap_or_else(|| panic!("not a time: {value}"))
        .timestamp_millis()
}

fn rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .expect("a time")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Every start (ms) the platform's window of `view` has at some time from `low` to `high`, a
/// second apart and both ends included. The start changes at most once a minute, and it is not
/// monotonic across a month end (crate::period), so a range of the two is not enough.
fn queryable_starts(view: View, low: DateTime<Utc>, high: DateTime<Utc>) -> BTreeSet<i64> {
    let mut starts = BTreeSet::new();
    let mut time = low;
    while time < high {
        starts.insert(Windows::PLATFORM.earliest(view, time).timestamp_millis());
        time += TimeDelta::seconds(1);
    }
    starts.insert(Windows::PLATFORM.earliest(view, high).timestamp_millis());
    starts
}

fn close(actual: &Value, expected: f64, what: &str) {
    let actual = actual
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: {actual}"));
    assert!(
        (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
        "{what}: {actual} != {expected}"
    );
}

fn reading_json(value: &Recorded) -> Value {
    match value {
        Recorded::Float(v) => json!({"type": "float", "value": v}),
        Recorded::Int(v) => json!({"type": "int", "value": v}),
        Recorded::Bool(v) => json!({"type": "bool", "value": v}),
        Recorded::String(v) => json!({"type": "string", "value": v}),
    }
}

/// A series of the dataset: its device key and ID, data point and kind, and readings.
#[derive(Clone, Debug)]
struct Series {
    device: String,
    id: Uuid,
    point: String,
    kind: PointKind,
    points: Vec<Point>,
}

impl Series {
    fn key(&self) -> Value {
        json!({"device_id": self.id, "point": self.point})
    }
    fn name(&self) -> String {
        format!("{}/{}", self.device, self.point)
    }
}

/// The dataset's series as loaded for one tenant, readings in time order and shifted.
fn series_of(dataset: &Dataset, loaded: &Loaded) -> Vec<Series> {
    let readings = dataset.readings().expect("the dataset's readings");
    let mut series = Vec::new();
    for device in &dataset.index().devices {
        for point in &device.points {
            let mut points: Vec<Point> = readings
                .iter()
                .filter(|r| r.device == device.key && r.point == point.key)
                .map(|r| Point {
                    ms: loaded
                        .shift()
                        .apply(r.ts)
                        .expect("shifted")
                        .timestamp_millis(),
                    value: r.value.clone(),
                })
                .collect();
            points.sort_by_key(|p| p.ms);
            series.push(Series {
                device: device.key.clone(),
                id: loaded.device(&device.key).expect("a device ID"),
                point: point.key.clone(),
                kind: point.kind,
                points,
            });
        }
    }
    series
}

struct Fixture {
    stack: Stack,
    service: Service,
    issuer: TestIssuer,
}

impl Fixture {
    async fn post(&self, path: &str, token: &str, body: &Value) -> Value {
        let (status, text) = self
            .service
            .post(path, Some(token), &body.to_string())
            .await;
        if status != 200 {
            let log = self.service.stdout.text();
            let tail: Vec<&str> = log.lines().rev().take(5).collect();
            panic!(
                "POST {path} {body}: {status} {text}\nservice log (newest first):\n{}",
                tail.join("\n")
            );
        }
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    async fn status(&self, path: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
        let (status, text) = self.service.post(path, token, &body.to_string()).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn tenant(&self, name: &str) -> (TenantId, String) {
        let tenant = self.stack.create_tenant(&self.issuer, name).await;
        let token =
            self.issuer
                .tenant_token(ServiceName::TelemetryQuery, tenant, TenantRole::Viewer);
        wait_account_ready(&self.service, &token, tenant).await;
        (tenant, token)
    }

    async fn load(&self, dataset: &Dataset, tenant: TenantId, shift: TimeShift) -> Loaded {
        let writer = lab_dataset::ClickHouse::new(
            &self.stack.clickhouse.url,
            &self.stack.clickhouse.admin.user,
            &self.stack.clickhouse.admin.password,
            None,
        )
        .expect("a ClickHouse writer");
        dataset
            .load(
                &writer,
                &Target::new(*tenant.as_uuid(), Uuid::new_v4()),
                shift,
            )
            .await
            .expect("load the dataset")
    }
}

/// A request body over `from..to`.
fn body(series: &[&Series], tz: &str, (from, to): (i64, i64), interval: &str) -> Value {
    json!({
        "series": series.iter().map(|s| s.key()).collect::<Vec<_>>(),
        "tz": tz,
        "from": rfc3339(from),
        "to": rfc3339(to),
        "interval": interval,
    })
}

/// Compares a statistics answer with the reference.
fn assert_statistics(answer: &Value, series: &[&Series], tz: &TimeZone, interval: &str) {
    let (from, to) = (ms(&answer["range"]["from"]), ms(&answer["range"]["to"]));
    let what = format!("{} {interval}", answer["range"]["tz"]);
    for (entry, series) in answer["series"]
        .as_array()
        .expect("series")
        .iter()
        .zip(series)
    {
        let name = format!("{what} {}", series.name());
        assert_eq!(entry["device_id"], json!(series.id), "{name}");
        let expected = statistics_reference(&series.points, (from, to), &|ms| {
            bucket_of(interval, tz, from, ms)
        });
        let rows = entry["rows"].as_array().expect("rows");
        if expected.is_empty() {
            assert_eq!(entry["status"], "no_data", "{name}");
            continue;
        }
        assert_eq!(entry["status"], "ok", "{name}");
        assert_eq!(rows.len(), expected.len(), "{name}: buckets\n{entry}");
        for (row, (start, bucket)) in rows.iter().zip(&expected) {
            assert_eq!(ms(&row["time"]), *start, "{name}: bucket start");
            assert_eq!(row["count"], json!(bucket.count), "{name} at {start}");
            match (bucket.min, bucket.max) {
                (Some(min), Some(max)) => {
                    close(&row["min"], min, &format!("{name} min"));
                    close(&row["max"], max, &format!("{name} max"));
                    close(
                        &row["avg"],
                        bucket.sum / bucket.numeric as f64,
                        &format!("{name} avg"),
                    );
                }
                _ => assert!(row["min"].is_null() && row["avg"].is_null(), "{name}"),
            }
            let last = bucket.last.as_ref().expect("a reading");
            assert_eq!(
                row["last"],
                reading_json(&last.value),
                "{name} last at {start}"
            );
        }
        let count: u64 = expected.values().map(|b| b.count).sum();
        let numeric: u64 = expected.values().map(|b| b.numeric).sum();
        assert_eq!(entry["count"], json!(count), "{name}");
        assert_eq!(entry["numeric_count"], json!(numeric), "{name}");
        if numeric > 0 {
            let sum: f64 = expected.values().map(|b| b.sum).sum();
            close(&entry["avg"], sum / numeric as f64, &format!("{name} avg"));
            let min = expected
                .values()
                .filter_map(|b| b.min)
                .fold(f64::INFINITY, f64::min);
            let max = expected
                .values()
                .filter_map(|b| b.max)
                .fold(f64::NEG_INFINITY, f64::max);
            close(&entry["min"], min, &format!("{name} min"));
            close(&entry["max"], max, &format!("{name} max"));
            // The time of a minimum or maximum is the start of its cell in the table read:
            // compared where only one reading has that value.
            let cell_ms = if answer["range"]["resolution"] == "1h" {
                HOUR_MS
            } else {
                MINUTE_MS
            };
            for (field, value) in [("min_time", min), ("max_time", max)] {
                let at: Vec<i64> = series
                    .points
                    .iter()
                    .filter(|p| p.ms >= from && p.ms < to)
                    .filter(|p| number(&p.value) == Some(value))
                    .map(|p| p.ms.div_euclid(cell_ms) * cell_ms)
                    .collect();
                if let [only] = at.as_slice() {
                    assert_eq!(ms(&entry[field]), *only, "{name} {field}");
                }
            }
            // The buckets with the highest and lowest mean, where one bucket has it.
            if interval != "total" {
                let means: Vec<(i64, f64)> = expected
                    .iter()
                    .filter(|(_, b)| b.numeric > 0)
                    .map(|(start, b)| (*start, b.sum / b.numeric as f64))
                    .collect();
                let top = means.iter().map(|m| m.1).fold(f64::NEG_INFINITY, f64::max);
                let bottom = means.iter().map(|m| m.1).fold(f64::INFINITY, f64::min);
                for (field, extreme) in [("highest", top), ("lowest", bottom)] {
                    let at: Vec<i64> = means
                        .iter()
                        .filter(|m| (m.1 - extreme).abs() <= 1e-9 * extreme.abs().max(1.0))
                        .map(|m| m.0)
                        .collect();
                    if let [only] = at.as_slice() {
                        assert_eq!(ms(&entry[field]["time"]), *only, "{name} {field}");
                        close(&entry[field]["value"], extreme, &format!("{name} {field}"));
                    }
                }
            }
        }
        let inside: Vec<&Point> = series
            .points
            .iter()
            .filter(|p| p.ms >= from && p.ms < to)
            .collect();
        let (first, last) = (inside[0], inside[inside.len() - 1]);
        assert_eq!(ms(&entry["first"]["time"]), first.ms, "{name} first");
        assert_eq!(
            entry["first"]["value"],
            reading_json(&first.value),
            "{name} first"
        );
        assert_eq!(ms(&entry["last"]["time"]), last.ms, "{name} last");
        assert_eq!(
            entry["last"]["value"],
            reading_json(&last.value),
            "{name} last"
        );
        eprintln!(
            "statistics {name}: {} buckets, {count} readings, equal to the dataset",
            rows.len()
        );
    }
}

/// The reference usage of `series` for an answer: the cells of the table the answer read,
/// within the window the answer's check reports.
fn reference_for(
    answer: &Value,
    entry: &Value,
    series: &Series,
    tz: &TimeZone,
    interval: &str,
) -> UsageReference {
    let (from, to) = (ms(&answer["range"]["from"]), ms(&answer["range"]["to"]));
    let cell_ms = if answer["range"]["resolution"] == "1h" {
        HOUR_MS
    } else {
        MINUTE_MS
    };
    let all = cells(&series.points, cell_ms);
    let (window_from, window_to) = if entry["check"].is_null() {
        (from, to)
    } else {
        (ms(&entry["check"]["from"]), ms(&entry["check"]["to"]))
    };
    let window: Vec<Cell> = all
        .into_iter()
        .filter(|cell| cell.start >= window_from && cell.start < window_to)
        .collect();
    usage_reference(&window, (from, to), &|start| {
        bucket_of(interval, tz, from, start)
    })
}

/// Compares a usage answer with the reference; returns each series' status.
fn assert_usage(answer: &Value, series: &[&Series], tz: &TimeZone, interval: &str) -> Vec<String> {
    let what = format!(
        "{} {interval} {}",
        answer["range"]["tz"], answer["range"]["resolution"]
    );
    let mut statuses = Vec::new();
    for (entry, series) in answer["series"]
        .as_array()
        .expect("series")
        .iter()
        .zip(series)
    {
        let name = format!("{what} {}", series.name());
        assert_eq!(entry["device_id"], json!(series.id), "{name}");
        let reference = reference_for(answer, entry, series, tz, interval);
        let status = entry["status"].as_str().expect("a status").to_owned();
        statuses.push(status.clone());
        if reference.buckets.is_empty() {
            assert_eq!(status, "no_data", "{name}");
            continue;
        }
        let verdict = semantic::verdict(reference.drops, reference.rises, reference.comparisons);
        let check = &entry["check"];
        assert_eq!(
            check["drops"],
            json!(reference.drops),
            "{name}: check drops\n{entry}"
        );
        assert_eq!(
            check["rises"],
            json!(reference.rises),
            "{name}: check rises"
        );
        assert_eq!(
            check["comparisons"],
            json!(reference.comparisons),
            "{name}: comparisons"
        );
        assert_eq!(check["verdict"], json!(verdict), "{name}: verdict");
        assert_eq!(
            entry["baseline"],
            json!(reference.baseline),
            "{name}: baseline"
        );
        let expected_status = if reference.non_numeric {
            "not_numeric"
        } else if reference.usable == 0 {
            "no_data"
        } else if verdict == Verdict::NotCumulative {
            "not_cumulative"
        } else {
            "ok"
        };
        assert_eq!(status, expected_status, "{name}");
        if status != "ok" {
            assert!(
                entry["total"].is_null() && entry["rows"] == json!([]),
                "{name}"
            );
            eprintln!(
                "usage {name}: {status} (check: {} drops, {} rises, {} comparisons)",
                reference.drops, reference.rises, reference.comparisons
            );
            continue;
        }
        let rows = entry["rows"].as_array().expect("rows");
        assert_eq!(
            rows.len(),
            reference.buckets.len(),
            "{name}: buckets\n{entry}"
        );
        let mut total = 0.0;
        for (row, (start, bucket)) in rows.iter().zip(&reference.buckets) {
            assert_eq!(ms(&row["time"]), *start, "{name}: bucket start");
            close(
                &row["usage"],
                bucket.usage,
                &format!("{name} usage at {}", rfc3339(*start)),
            );
            assert_eq!(row["readings"], json!(bucket.readings), "{name} at {start}");
            assert_eq!(row["drops"], json!(bucket.drops), "{name} drops at {start}");
            assert_eq!(
                row["after_gap"],
                json!(bucket.gaps > 0),
                "{name} gap at {start}"
            );
            assert_eq!(
                row["suspicious_steps"],
                json!(bucket.suspicious),
                "{name} at {start}"
            );
            total += bucket.usage;
        }
        close(&entry["total"], total, &format!("{name} total"));
        let gaps: u64 = reference.buckets.values().map(|b| b.gaps).sum();
        let drops: u64 = reference.buckets.values().map(|b| b.drops).sum();
        let suspicious: u64 = reference.buckets.values().map(|b| b.suspicious).sum();
        assert_eq!(entry["gaps"], json!(gaps), "{name}");
        assert_eq!(entry["drops"], json!(drops), "{name}");
        assert_eq!(entry["suspicious_steps"], json!(suspicious), "{name}");
        match reference.longest_gap {
            None => assert!(entry["longest_gap"].is_null(), "{name}"),
            Some((start, length)) => {
                assert_eq!(ms(&entry["longest_gap"]["from"]), start, "{name}");
                assert_eq!(ms(&entry["longest_gap"]["to"]), start + length, "{name}");
            }
        }
        match reference.largest_suspicious {
            None => assert!(entry["largest_suspicious_step"].is_null(), "{name}"),
            Some((at, amount)) => {
                assert_eq!(ms(&entry["largest_suspicious_step"]["time"]), at, "{name}");
                close(&entry["largest_suspicious_step"]["value"], amount, &name);
            }
        }
        eprintln!(
            "usage {name}: {total} over {} buckets, {drops} drops, {gaps} gaps, {suspicious} \
             suspicious steps, check {} drops / {} rises / {} comparisons ({verdict:?}), \
             baseline {}; equal to the reference",
            rows.len(),
            reference.drops,
            reference.rises,
            reference.comparisons,
            reference.baseline
        );
    }
    statuses
}

// ---------------------------------------------------------------------------------------------
// Series written for the rules.

/// Readings of one data point written as the ingest service writes them, as the admin.
const INSERT_SERIES: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
  batch_seq, device_id, point, ts, received_at, value_type, value_float, value_int, value_bool,
  value_string)
SELECT {tenant:UUID}, {gateway:UUID}, {stream:UUID}, 1, {device:UUID}, {point:String},
  fromUnixTimestamp64Milli(reading.1), now64(3), 'float', reading.2, 0, false, ''
FROM (SELECT arrayJoin({readings:Array(Tuple(Int64, Float64))}) AS reading)";

/// A counter rising by 2 every 5 minutes for 15 hours from `start`, starting at 1000, changed by
/// `change` (the reading's index and value; `None` leaves the reading out).
fn counter(start: i64, change: impl Fn(usize, f64) -> Option<f64>) -> Vec<Point> {
    (0..180)
        .filter_map(|index| {
            let value = 1000.0 + 2.0 * index as f64;
            change(index, value).map(|value| Point {
                ms: start + i64::try_from(index).expect("small") * 5 * MINUTE_MS,
                value: Recorded::Float(value),
            })
        })
        .collect()
}

async fn insert(stack: &Stack, tenant: TenantId, device: Uuid, point: &str, points: &[Point]) {
    let readings: Vec<String> = points
        .iter()
        .map(|p| format!("({}, {})", p.ms, counted(&p.value)))
        .collect();
    let readings = format!("[{}]", readings.join(", "));
    let (tenant, device) = (tenant.to_string(), device.to_string());
    let (gateway, stream) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    stack
        .clickhouse
        .query_with(
            &stack.clickhouse.admin,
            INSERT_SERIES,
            &[
                ("param_tenant", &tenant),
                ("param_gateway", &gateway),
                ("param_stream", &stream),
                ("param_device", &device),
                ("param_point", point),
                ("param_readings", &readings),
            ],
        )
        .await
        .unwrap_or_else(|error| panic!("insert {point}: {}", error.body));
}

// ---------------------------------------------------------------------------------------------

/// The service's statements run directly as a tenant's account, with the parameters the service
/// would bind and the given tenant, series and time zone.
struct Direct<'a> {
    fixture: &'a Fixture,
    account: &'a support::clickhouse::Account,
    full: (i64, i64),
}

impl Direct<'_> {
    /// The answers of the statistics and the usage statement by local day.
    async fn rows(&self, tenant: TenantId, series: &str, tz: &str) -> Vec<Result<String, String>> {
        let seconds = |ms: i64| (ms / 1000).to_string();
        let (tenant, from, to) = (
            tenant.to_string(),
            seconds(self.full.0),
            seconds(self.full.1),
        );
        let (window_from, window_to) =
            (seconds(self.full.0 - DAY_MS), seconds(self.full.1 + DAY_MS));
        let params = [
            ("param_tenant", tenant.as_str()),
            ("param_series", series),
            ("param_from", from.as_str()),
            ("param_to", to.as_str()),
            ("param_tz", tz),
            ("param_step", "1"),
            ("param_limit", "10001"),
            ("param_window_from", window_from.as_str()),
            ("param_window_to", window_to.as_str()),
            ("param_gap_factor", "1.5"),
            ("param_factor", "20"),
            ("param_min_rises", "10"),
        ];
        let mut answers = Vec::new();
        for statement in [statements::STATISTICS_1M_DAY, statements::USAGE_1M_DAY] {
            let answer = self
                .fixture
                .stack
                .clickhouse
                .query_with(self.account, statement, &params)
                .await;
            answers.push(answer.map_err(|error| error.body));
        }
        answers
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_semantic_api_answers_the_dataset_within_the_period_and_the_tenant() {
    let Some(stack) = Stack::start().await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(
        &stack.product,
        &ClickHouseTarget {
            url: &stack.clickhouse.url,
            password: &stack.service_password,
        },
        &issuer.jwks(),
    )
    .await;
    let fixture = Fixture {
        stack,
        service,
        issuer,
    };
    let dataset = Dataset::open(index_for_tests()).expect("the dataset");
    let is_fixture = std::env::var_os(lab_dataset::INDEX_ENV).is_none();
    eprintln!(
        "dataset {} version {} ({})",
        dataset.index().name,
        dataset.index().version,
        if is_fixture {
            "the synthetic fixture"
        } else {
            "from TRUSTIOT_LAB_DATASET_INDEX"
        }
    );

    // The same dataset for tenants A and B, each with its own device IDs, ending within the
    // last day (whole days, so every reading keeps its place in its UTC minute and hour).
    let (tenant_a, token_a) = fixture.tenant("Plant A").await;
    let (tenant_b, token_b) = fixture.tenant("Plant B").await;
    let shift = dataset
        .shift_ending_before(Utc::now(), TimeDelta::days(1))
        .expect("a shift");
    let loaded_a = fixture.load(&dataset, tenant_a, shift).await;
    let loaded_b = fixture.load(&dataset, tenant_b, shift).await;
    let series_a = series_of(&dataset, &loaded_a);
    let series_b = series_of(&dataset, &loaded_b);
    let start = series_a
        .iter()
        .filter_map(|s| s.points.first())
        .map(|p| p.ms)
        .min()
        .expect("readings");
    let end = series_a
        .iter()
        .filter_map(|s| s.points.last())
        .map(|p| p.ms)
        .max()
        .expect("readings");
    let full = (
        start.div_euclid(HOUR_MS) * HOUR_MS,
        end.div_euclid(HOUR_MS) * HOUR_MS + HOUR_MS,
    );
    // From the second hour on: the first bucket's usage counts from the reading before it.
    let later = (full.0 + HOUR_MS, full.1);

    // 1. The latest values (the history API the latest-value tool reads): each data point's
    // last reading, the uncommitted batch included.
    let devices: BTreeMap<&str, Uuid> =
        series_a.iter().map(|s| (s.device.as_str(), s.id)).collect();
    for (device, id) in &devices {
        let latest = fixture
            .service
            .get_ok(&format!("/api/telemetry/v1/devices/{id}/latest"), &token_a)
            .await;
        for series in series_a.iter().filter(|s| s.device == *device) {
            let last = series.points.last().expect("a reading");
            let point = latest["points"]
                .as_array()
                .and_then(|points| points.iter().find(|p| p["point"] == series.point.as_str()))
                .unwrap_or_else(|| panic!("{} in {latest}", series.name()));
            assert_eq!(ms(&point["time"]), last.ms, "{}", series.name());
            assert_eq!(
                point["value"],
                reading_json(&last.value),
                "{}",
                series.name()
            );
        }
        eprintln!("latest {device}: equal to the dataset's last readings");
    }

    // 2. Statistics of every series, in chunks of the API's limit, per time zone and interval:
    // UTC and Asia/Taipei read the hour table, Asia/Kolkata (UTC+05:30) the minute table.
    let all: Vec<&Series> = series_a.iter().collect();
    for tz_name in ["UTC", "Asia/Taipei", "Asia/Kolkata"] {
        let tz = TimeZone::get(tz_name).expect("a time zone");
        for interval in ["15m", "1h", "1d", "1w", "1mo", "total"] {
            for chunk in all.chunks(semantic::range::MAX_SERIES) {
                let range = if interval == "15m" {
                    (full.0, (full.0 + 6 * HOUR_MS).min(full.1))
                } else {
                    full
                };
                let answer = fixture
                    .post(STATISTICS, &token_a, &body(chunk, tz_name, range, interval))
                    .await;
                let hourly = tz_name != "Asia/Kolkata" && interval != "15m";
                assert_eq!(
                    answer["range"]["resolution"],
                    if hourly { "1h" } else { "1m" }
                );
                assert_eq!(answer["range"]["truncated"], false);
                assert_statistics(&answer, chunk, &tz, interval);
            }
        }
    }

    // 3. Usage of every series: counters get their usage, gauges and state points are refused
    // with a status; the reading before the range is the baseline from the second hour on.
    for tz_name in ["UTC", "Asia/Taipei", "Asia/Kolkata"] {
        let tz = TimeZone::get(tz_name).expect("a time zone");
        for (interval, range) in [
            ("1h", full),
            ("1h", later),
            ("1d", full),
            ("1w", full),
            ("1mo", full),
            ("total", full),
            ("15m", (full.0, (full.0 + 6 * HOUR_MS).min(full.1))),
        ] {
            for chunk in all.chunks(semantic::range::MAX_SERIES) {
                let answer = fixture
                    .post(USAGE, &token_a, &body(chunk, tz_name, range, interval))
                    .await;
                assert_usage(&answer, chunk, &tz, interval);
            }
        }
    }

    // The fixture's own properties, from the minute table (Asia/Kolkata) and the hour table.
    if is_fixture {
        let find = |key: &str| all.iter().copied().find(|s| s.name() == key).expect(key);
        for tz_name in ["Asia/Kolkata", "Asia/Taipei"] {
            let answer = fixture
                .post(USAGE, &token_a, &body(&all, tz_name, full, "total"))
                .await;
            let entry = |key: &str| {
                answer["series"]
                    .as_array()
                    .and_then(|series| {
                        series.iter().find(|e| {
                            format!("{}", find(key).id)
                                == e["device_id"].as_str().unwrap_or_default()
                                && e["point"] == find(key).point.as_str()
                        })
                    })
                    .cloned()
                    .unwrap_or_else(|| panic!("{key}"))
            };
            for (key, status) in [
                ("meter-1/energy_kwh", "ok"),
                ("meter-2/pulses", "ok"),
                ("press-1/cycle_count", "ok"),
                ("meter-1/power_kw", "not_cumulative"),
                ("meter-2/power_kw", "not_cumulative"),
                ("press-1/running", "not_numeric"),
                ("press-1/mode", "not_numeric"),
            ] {
                assert_eq!(entry(key)["status"], status, "{tz_name} {key}");
            }
            // The reset and the wrap: one drop each, counters, nothing suspicious. The four
            // hours give the hour table 7 comparisons: too few to confirm, counted all the same.
            let verdict = if answer["range"]["resolution"] == "1m" {
                "cumulative"
            } else {
                "too_few_readings"
            };
            for key in ["meter-1/energy_kwh", "meter-2/pulses"] {
                let entry = entry(key);
                assert_eq!(entry["drops"], 1, "{tz_name} {key}");
                assert_eq!(entry["check"]["verdict"], verdict, "{tz_name} {key}");
                assert_eq!(entry["suspicious_steps"], 0, "{tz_name} {key}");
                // Each drop loses at most one sampling interval: here the readings are exact.
                close(&entry["total"], reading_usage(&find(key).points), key);
            }
            // The wrap: the drop rule loses less than one interval's usage against the
            // wrap-aware increase.
            let pulses = find("meter-2/pulses");
            let wrapped: f64 = pulses
                .points
                .windows(2)
                .map(|pair| {
                    let (a, b) = (counted(&pair[0].value), counted(&pair[1].value));
                    if b >= a {
                        b - a
                    } else {
                        b + 4_294_967_296.0 - a
                    }
                })
                .sum();
            let total = entry("meter-2/pulses")["total"].as_f64().expect("a total");
            let interval_usage = wrapped / (pulses.points.len() - 1) as f64;
            assert!(
                wrapped - total > 0.0 && wrapped - total < 2.0 * interval_usage,
                "{wrapped} {total}"
            );
            // The press's missed polls: one gap of 35 minutes, the usage exact.
            let cycles = entry("press-1/cycle_count");
            assert!(cycles["gaps"].as_u64() >= Some(1), "{cycles}");
            let gap = &cycles["longest_gap"];
            assert_eq!(
                ms(&gap["to"]) - ms(&gap["from"]),
                35 * MINUTE_MS,
                "{cycles}"
            );
            close(
                &cycles["total"],
                reading_usage(&find("press-1/cycle_count").points),
                "cycles",
            );
            eprintln!(
                "fixture, {tz_name}: energy_kwh {} with 1 reset, pulses {} with 1 wrap \
                 (wrap-aware {wrapped}), cycle_count {} across a 35-minute gap; both gauges \
                 not_cumulative, running and mode not_numeric",
                entry("meter-1/energy_kwh")["total"],
                total,
                cycles["total"]
            );
        }
    }

    // 4. The tenant: tenant A asking for tenant B's devices gets no data; a field naming a
    // tenant is refused; no token is refused.
    let others: Vec<&Series> = series_b.iter().collect();
    for chunk in others.chunks(semantic::range::MAX_SERIES) {
        for path in [STATISTICS, USAGE] {
            let answer = fixture
                .post(path, &token_a, &body(chunk, "UTC", full, "total"))
                .await;
            for entry in answer["series"].as_array().expect("series") {
                assert_eq!(entry["status"], "no_data", "{path}: {entry}");
            }
            // Tenant B reads its own.
            let own = fixture
                .post(path, &token_b, &body(chunk, "UTC", full, "total"))
                .await;
            assert!(
                own["series"]
                    .as_array()
                    .expect("series")
                    .iter()
                    .any(|e| e["status"] == "ok")
            );
        }
    }
    let mut naming = body(&all[..1], "UTC", full, "total");
    naming["tenant_id"] = json!(tenant_b.to_string());
    for path in [STATISTICS, USAGE] {
        let (status, error) = fixture.status(path, Some(&token_a), &naming).await;
        assert_eq!(
            (status, error["code"].as_str()),
            (400, Some("invalid_request")),
            "{path}"
        );
        let (status, _) = fixture
            .status(path, None, &body(&all[..1], "UTC", full, "total"))
            .await;
        assert_eq!(status, 401, "{path}");
    }
    eprintln!("tenant: A gets no data for B's devices, a tenant field is refused (400)");

    // 5. Injection: hostile values are refused before any query...
    let hostile = [
        json!({"series": [{"device_id": all[0].id, "point": "energy_kwh') OR 1=1 --"}], "tz": "UTC", "period": "today"}),
        json!({"series": [{"device_id": all[0].id, "point": "x' UNION SELECT 1 --"}], "tz": "UTC", "period": "today"}),
        json!({"series": [{"device_id": "1 OR 1=1", "point": "energy_kwh"}], "tz": "UTC", "period": "today"}),
        json!({"series": [all[0].key()], "tz": "UTC') OR 1=1 --", "period": "today"}),
        json!({"series": [all[0].key()], "tz": "UTC", "period": "today' OR '1'='1"}),
        json!({"series": [all[0].key()], "tz": "UTC", "period": "today", "interval": "1h') --"}),
        json!({"series": [all[0].key()], "tz": "UTC", "from": "2026-01-01' --", "to": "2026-01-02"}),
    ];
    for request in &hostile {
        for path in [STATISTICS, USAGE] {
            let (status, error) = fixture.status(path, Some(&token_a), request).await;
            assert!(
                matches!(status, 400 | 422),
                "{path} {request}: {status} {error}"
            );
        }
    }
    // An end too far after now, including a year serde reads but chrono cannot add to, is a 422
    // on `to`, not a panic turned into a 500.
    for to in ["+262142-12-31T23:59:00Z", "2106-02-08T00:00:00Z"] {
        let request = json!({"series": [all[0].key()], "tz": "UTC", "from": "2026-01-01T00:00:00Z", "to": to});
        for path in [STATISTICS, USAGE] {
            let (status, error) = fixture.status(path, Some(&token_a), &request).await;
            assert_eq!(status, 422, "{path} {to}: {error}");
            assert_eq!(error["details"]["parameter"], "to", "{path} {to}");
        }
    }
    // ...and below the API, the statements bind them as values: run as tenant A's account, a
    // hostile key matches no row, a hostile time zone is an error, and naming tenant B in the
    // statement's own filter reads nothing: the account's views hold tenant A's rows only.
    let account = fixture.stack.tenant_account(tenant_a);
    let energy = all
        .iter()
        .copied()
        .find(|s| s.kind == PointKind::Cumulative)
        .unwrap_or(all[0]);
    let key = |series: &Series, point: &str| {
        semantic::series_parameter(&[SeriesKey {
            device_id: series.id,
            point: point.to_owned(),
        }])
    };
    let plain = key(energy, &energy.point);
    let ask = Direct {
        fixture: &fixture,
        account: &account,
        full,
    };
    for body in ask.rows(tenant_a, &plain, "UTC").await {
        let body = body.unwrap_or_else(|error| panic!("the plain key: {error}"));
        assert!(body.lines().count() >= 1, "rows for the plain key");
    }
    for point in [
        format!("{}') OR 1=1 --", energy.point),
        format!("{}' OR point != '", energy.point),
        "x'), ('00000000-0000-0000-0000-000000000000', 'y".to_owned(),
    ] {
        for body in ask.rows(tenant_a, &key(energy, &point), "UTC").await {
            let body = body.unwrap_or_else(|error| panic!("{point}: {error}"));
            assert_eq!(body.trim(), "", "{point} matched rows: {body}");
        }
    }
    let foreign = key(&series_b[0], &series_b[0].point);
    for tenant in [tenant_a, tenant_b] {
        for body in ask.rows(tenant, &foreign, "UTC").await {
            let body = body.unwrap_or_else(|error| panic!("tenant B's device: {error}"));
            assert_eq!(
                body.trim(),
                "",
                "tenant B's device through tenant A's account"
            );
        }
    }
    for body in ask.rows(tenant_a, &plain, "UTC') OR 1=1 --").await {
        assert!(body.is_err(), "a hostile time zone is an unknown time zone");
    }
    eprintln!(
        "injection: hostile values refused by the API (400/422), an end after 2106 or in year +262142 a 422 on `to`; bound as values below it, they \
         match no row or fail; tenant B's rows are not in tenant A's account"
    );

    // 6. The queryable period, on a tenant whose copy of the dataset straddles its start: the
    // answer starts at the period's start (truncated), the usage starts there without a
    // baseline, and a range wholly before it is refused with its start.
    let (tenant_c, token_c) = fixture.tenant("Plant C").await;
    let now = Utc::now();
    // The tenant's account carries the platform's windows (crate::period), which the service
    // reads back before each query.
    let edge = Windows::PLATFORM.earliest(View::Hour, now);
    let shift = TimeShift::ending_before(
        dataset.end(),
        edge + TimeDelta::hours(2),
        TimeDelta::hours(1),
    )
    .expect("a shift");
    let loaded_c = fixture.load(&dataset, tenant_c, shift).await;
    let series_c = series_of(&dataset, &loaded_c);
    let mine: Vec<&Series> = series_c.iter().collect();
    let c_start = mine
        .iter()
        .filter_map(|s| s.points.first())
        .map(|p| p.ms)
        .min()
        .expect("readings");
    let c_end = mine
        .iter()
        .filter_map(|s| s.points.last())
        .map(|p| p.ms)
        .max()
        .expect("readings");
    let c_full = (
        c_start.div_euclid(HOUR_MS) * HOUR_MS,
        c_end.div_euclid(HOUR_MS) * HOUR_MS + HOUR_MS,
    );
    for tz_name in ["UTC", "Asia/Kolkata"] {
        let tz = TimeZone::get(tz_name).expect("a time zone");
        for chunk in mine.chunks(semantic::range::MAX_SERIES) {
            let answer = fixture
                .post(USAGE, &token_c, &body(chunk, tz_name, c_full, "1h"))
                .await;
            let earliest = ms(&answer["range"]["earliest"]);
            assert_eq!(answer["range"]["truncated"], true, "{answer}");
            assert_eq!(ms(&answer["range"]["from"]), earliest);
            // The start the service reported is the account's window for the table it read
            // (`Windows::earliest`: three months back at five minutes later, rounded up to the
            // table's cell) at a time between our two clock readings: the one-hour table in
            // UTC, the one-minute table in a half-hour zone.
            let view = if tz_name == "UTC" {
                View::Hour
            } else {
                View::Minute
            };
            assert_eq!(
                answer["range"]["resolution"],
                if view == View::Hour { "1h" } else { "1m" },
                "{answer}"
            );
            let reported = queryable_starts(view, now, Utc::now());
            assert!(
                reported.contains(&earliest),
                "{earliest} is not a start of the window between the two clock readings: {reported:?}"
            );
            for entry in answer["series"].as_array().expect("series") {
                assert_eq!(entry["baseline"], false, "{entry}");
                if !entry["check"].is_null() {
                    assert_eq!(
                        ms(&entry["check"]["from"]),
                        earliest,
                        "no lookback before the period"
                    );
                }
            }
            assert_usage(&answer, chunk, &tz, "1h");
            let answer = fixture
                .post(STATISTICS, &token_c, &body(chunk, tz_name, c_full, "1h"))
                .await;
            assert_eq!(answer["range"]["truncated"], true);
            assert_statistics(&answer, chunk, &tz, "1h");
            eprintln!(
                "period {tz_name}: cut at {} ({} table), earlier readings left out",
                answer["range"]["earliest"], answer["range"]["resolution"]
            );
        }
        let before = (
            c_full.0 - DAY_MS,
            ms(&json!(rfc3339(c_full.0))) - DAY_MS / 2,
        );
        let before = (
            before.0,
            before.1.min(
                Windows::PLATFORM
                    .earliest(View::Minute, Utc::now())
                    .timestamp_millis()
                    - HOUR_MS,
            ),
        );
        for path in [STATISTICS, USAGE] {
            let (status, error) = fixture
                .status(
                    path,
                    Some(&token_c),
                    &body(&mine[..1], tz_name, before, "total"),
                )
                .await;
            assert_eq!(status, 403, "{path}: {error}");
            assert_eq!(error["message_key"], "telemetry.outside_queryable_period");
            assert!(error["details"]["earliest"].is_string(), "{error}");
        }
    }

    // 7. The rules on series written for them (tenant D): a 0 read and back, a torn 32-bit read
    // and a replaced meter are flagged as suspicious and still counted; a long gap at the usual
    // rate is not; a gauge idling at 0 with three bursts is not a counter.
    let (tenant_d, token_d) = fixture.tenant("Plant D").await;
    let device = Uuid::new_v4();
    let start = (Utc::now().timestamp_millis() - 2 * DAY_MS).div_euclid(DAY_MS) * DAY_MS;
    let cases: Vec<(&str, Vec<Point>)> = vec![
        (
            "misread_zero",
            counter(start, |i, v| Some(if i == 90 { 0.0 } else { v })),
        ),
        (
            "torn_read",
            counter(start, |i, v| Some(if i == 90 { 70_000.0 } else { v })),
        ),
        (
            "replaced_meter",
            counter(start, |i, v| {
                Some(if i >= 90 {
                    5_000.0 + 2.0 * (i - 90) as f64
                } else {
                    v
                })
            }),
        ),
        (
            "long_gap",
            counter(start, |i, v| (!(60..132).contains(&i)).then_some(v)),
        ),
        (
            "idle_bursts",
            counter(start, |i, _| {
                Some(if [30, 90, 150].contains(&i) {
                    50.0
                } else {
                    0.0
                })
            }),
        ),
    ];
    let mut written = Vec::new();
    for (point, points) in &cases {
        insert(&fixture.stack, tenant_d, device, point, points).await;
        written.push(Series {
            device: "rules".into(),
            id: device,
            point: (*point).to_owned(),
            kind: PointKind::Cumulative,
            points: points.clone(),
        });
    }
    let written: Vec<&Series> = written.iter().collect();
    // The raw period of the latest values: a data point whose only reading is 8 days old is
    // left out, while the aggregates still hold it.
    let stale_at = Utc::now().timestamp_millis() - 8 * DAY_MS;
    let stale = vec![Point {
        ms: stale_at,
        value: Recorded::Float(42.0),
    }];
    insert(&fixture.stack, tenant_d, device, "stale", &stale).await;
    let latest = fixture
        .service
        .get_ok(
            &format!("/api/telemetry/v1/devices/{device}/latest"),
            &token_d,
        )
        .await;
    let listed: Vec<&str> = latest["points"]
        .as_array()
        .expect("points")
        .iter()
        .filter_map(|point| point["point"].as_str())
        .collect();
    assert!(!listed.contains(&"stale"), "{latest}");
    assert_eq!(listed.len(), cases.len(), "{latest}");
    let stale_series = Series {
        device: "rules".into(),
        id: device,
        point: "stale".into(),
        kind: PointKind::Gauge,
        points: stale,
    };
    let answer = fixture
        .post(
            STATISTICS,
            &token_d,
            &body(
                &[&stale_series],
                "UTC",
                (stale_at - DAY_MS, stale_at + DAY_MS),
                "total",
            ),
        )
        .await;
    assert_eq!(answer["series"][0]["count"], 1, "{answer}");
    eprintln!(
        "period: the latest values leave out a data point last read 8 days ago (raw period, 7 \
         days); its statistics from the aggregates still hold it"
    );
    let range = (start, start + 16 * HOUR_MS);
    for tz_name in ["Asia/Kolkata", "UTC"] {
        let tz = TimeZone::get(tz_name).expect("a time zone");
        let answer = fixture
            .post(USAGE, &token_d, &body(&written, tz_name, range, "total"))
            .await;
        let statuses = assert_usage(&answer, &written, &tz, "total");
        let entry = |point: &str| {
            answer["series"]
                .as_array()
                .and_then(|s| s.iter().find(|e| e["point"] == point))
                .cloned()
                .expect(point)
        };
        assert_eq!(statuses[4], "not_cumulative", "idle_bursts");
        assert_eq!(entry("idle_bursts")["check"]["drops"], 3);
        assert_eq!(entry("long_gap")["suspicious_steps"], 0);
        assert_eq!(entry("long_gap")["gaps"], 1);
        let minute = answer["range"]["resolution"] == "1m";
        for (point, flagged, value) in [
            ("misread_zero", 1, 1_182.0),
            ("torn_read", 2, 68_822.0),
            ("replaced_meter", 1, 3_822.0),
        ] {
            let entry = entry(point);
            assert_eq!(entry["status"], "ok", "{point}");
            if minute {
                // Each reading is its own cell: the steps are the readings', the total exact.
                assert_eq!(entry["suspicious_steps"], flagged, "{point}: {entry}");
                close(&entry["largest_suspicious_step"]["value"], value, point);
                let points = &cases.iter().find(|(p, _)| *p == point).expect("a case").1;
                close(&entry["total"], reading_usage(points), point);
            } else {
                assert!(
                    entry["suspicious_steps"].as_u64() >= Some(1),
                    "{point}: {entry}"
                );
            }
        }
        eprintln!(
            "rules, {tz_name} ({} table): misread_zero {}, torn_read {}, replaced_meter {} \
             suspicious steps; long_gap 0 with 1 gap; idle_bursts not_cumulative",
            answer["range"]["resolution"],
            entry("misread_zero")["suspicious_steps"],
            entry("torn_read")["suspicious_steps"],
            entry("replaced_meter")["suspicious_steps"]
        );
    }
}
