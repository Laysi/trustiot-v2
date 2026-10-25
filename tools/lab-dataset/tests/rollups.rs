//! The one-minute and one-hour aggregates of the synthetic fixture (Laysi/trustiot-v2#141):
//! loaded into a throwaway ClickHouse with the product's ClickHouse schema, every cell a tenant
//! reads through its aggregate views equals the cell this test computes from the dataset's
//! readings (count, numeric count, sum, minimum, maximum, first and last reading with its typed
//! value), the uncommitted batch and the backlog included; the energy queries of
//! services/ingest/src/rollup.rs give the increase of the three cumulative points (the meter
//! cleared to zero, the 32-bit pulse counter that wraps, the press counter) from both views; and
//! the reconcile of every loaded batch writes nothing, since the views counted each batch once.
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`, like `clickhouse_load.rs`.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "test helpers unwrap and print like the tests themselves (clippy.toml allows both in \
              tests); the printed summary is what `-- --nocapture` shows as evidence"
)]

/// The ClickHouse container and its HTTP client, shared with the ingest service's schema tests.
#[path = "../../../services/ingest/tests/common/mod.rs"]
mod common;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use ingest::rollup::{ENERGY_1H, ENERGY_1M, RECONCILE_1H, RECONCILE_1M, ReconcileParams};
use lab_dataset::{Dataset, Loaded, Reading, Target, Value, synthetic_index};
use uuid::Uuid;

use self::common::{Account, ClickHouse};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// A reading as a cell's first or last: time in ms, type, float, int, bool, string.
type Typed = (i64, String, f64, i64, bool, String);

/// A cell computed from the dataset; ties at the earliest or latest time keep every candidate.
#[derive(Debug, Default)]
struct Expected {
    readings: u64,
    numeric: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    firsts: Vec<Typed>,
    lasts: Vec<Typed>,
}

fn typed(at: DateTime<Utc>, value: &Value) -> Typed {
    let ms = at.timestamp_millis();
    match value {
        Value::Float(v) => (ms, "float".to_owned(), *v, 0, false, String::new()),
        Value::Int(v) => (ms, "int".to_owned(), 0.0, *v, false, String::new()),
        Value::Bool(v) => (ms, "bool".to_owned(), 0.0, 0, *v, String::new()),
        Value::String(v) => (ms, "string".to_owned(), 0.0, 0, false, v.clone()),
    }
}

/// The value the aggregates count as a number, converted as toFloat64 converts it.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Float(v) => Some(*v),
        Value::Int(v) => Some(*v as f64),
        Value::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        Value::String(_) => None,
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// The shifted reading time of a reading.
fn at(loaded: &Loaded, reading: &Reading) -> DateTime<Utc> {
    loaded.shift().apply(reading.ts).unwrap()
}

/// The cells of the dataset, keyed by device ID, point and bucket start in seconds.
fn expected_cells(
    loaded: &Loaded,
    readings: &[Reading],
    bucket_ms: i64,
) -> BTreeMap<(String, String, i64), Expected> {
    let mut cells: BTreeMap<(String, String, i64), Expected> = BTreeMap::new();
    for reading in readings {
        let ts = at(loaded, reading);
        let bucket = ts.timestamp_millis().div_euclid(bucket_ms) * bucket_ms / 1000;
        let device = loaded.device(&reading.device).unwrap().to_string();
        let cell = cells
            .entry((device, reading.point.clone(), bucket))
            .or_default();
        cell.readings += 1;
        if let Some(value) = number(&reading.value) {
            cell.numeric += 1;
            cell.sum += value;
            cell.min = Some(cell.min.map_or(value, |min| min.min(value)));
            cell.max = Some(cell.max.map_or(value, |max| max.max(value)));
        }
        let this = typed(ts, &reading.value);
        match cell.firsts.first().map(|first| first.0) {
            Some(ms) if ms < this.0 => {}
            Some(ms) if ms == this.0 => cell.firsts.push(this.clone()),
            _ => cell.firsts = vec![this.clone()],
        }
        match cell.lasts.first().map(|last| last.0) {
            Some(ms) if ms > this.0 => {}
            Some(ms) if ms == this.0 => cell.lasts.push(this),
            _ => cell.lasts = vec![this],
        }
    }
    cells
}

fn parse_typed(fields: &[&str]) -> Typed {
    (
        fields[0].parse().unwrap(),
        fields[1].to_owned(),
        fields[2].parse().unwrap(),
        fields[3].parse().unwrap(),
        fields[4] == "true",
        fields[5].to_owned(),
    )
}

fn optional(field: &str) -> Option<f64> {
    (field != "\\N").then(|| field.parse().unwrap())
}

/// Compares a view's cells, as the tenant reads them, with the dataset's; the problems found and
/// the number of cells.
async fn compare(
    server: &ClickHouse,
    tenant: &Account,
    view: &str,
    expected: &BTreeMap<(String, String, i64), Expected>,
) -> (Vec<String>, usize) {
    let answer = server
        .query(
            tenant,
            &format!(
                "SELECT toString(device_id), point, toUnixTimestamp(bucket), readings,
  numeric_readings, value_sum, value_min, value_max,
  toUnixTimestamp64Milli(first.ts), first.value_type, first.value_float, first.value_int,
  first.value_bool, first.value_string,
  toUnixTimestamp64Milli(last.ts), last.value_type, last.value_float, last.value_int,
  last.value_bool, last.value_string
FROM {view} FORMAT TSV"
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{view}: {}", error.body));
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for line in answer.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let key = (
            fields[0].to_owned(),
            fields[1].to_owned(),
            fields[2].parse::<i64>().unwrap(),
        );
        let Some(want) = expected.get(&key) else {
            problems.push(format!("{view} {key:?}: not in the dataset: {line}"));
            continue;
        };
        seen.insert(key.clone());
        let same = fields[3].parse::<u64>().unwrap() == want.readings
            && fields[4].parse::<u64>().unwrap() == want.numeric
            && close(fields[5].parse().unwrap(), want.sum)
            && optional(fields[6]) == want.min
            && optional(fields[7]) == want.max
            && want.firsts.contains(&parse_typed(&fields[8..14]))
            && want.lasts.contains(&parse_typed(&fields[14..20]));
        if !same {
            problems.push(format!("{view} {key:?}: expected {want:?}, got {line}"));
        }
    }
    for key in expected.keys().filter(|key| !seen.contains(*key)) {
        problems.push(format!("{view} {key:?}: missing"));
    }
    (problems, seen.len())
}

/// A cumulative point's increase by consecutive readings: a drop is a wrap when the counter has
/// a wrap value, otherwise a reset to the new reading.
fn increase(values: &[f64], wraps_at: f64) -> f64 {
    values
        .windows(2)
        .map(|pair| {
            let (previous, next) = (pair[0], pair[1]);
            if next >= previous {
                next - previous
            } else if wraps_at > 0.0 {
                next + wraps_at - previous
            } else {
                next
            }
        })
        .sum()
}

fn floor(at: DateTime<Utc>, ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(at.timestamp_millis().div_euclid(ms) * ms)
        .single()
        .unwrap()
}

fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn password() -> String {
    Uuid::new_v4().simple().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_aggregates_of_the_fixture_equal_its_readings() {
    let Some(server) = ClickHouse::start_throwaway().await else {
        return;
    };
    for migration in migrate::registry::PRODUCT.clickhouse {
        let statements = migrate::clickhouse::split_statements(migration.sql)
            .unwrap_or_else(|error| panic!("{}: {error}", migration.name));
        for statement in statements {
            server
                .admin(&statement)
                .await
                .unwrap_or_else(|error| panic!("{}: {statement}\n{}", migration.name, error.body));
        }
    }

    let dataset = Dataset::open(synthetic_index()).unwrap();
    let writer = lab_dataset::ClickHouse::new(
        &server.url,
        &server.admin.user,
        &server.admin.password,
        None,
    )
    .unwrap();
    let shift = dataset
        .shift_ending_before(Utc::now(), TimeDelta::days(1))
        .unwrap();
    let tenant = Uuid::new_v4();
    let loaded = dataset
        .load(&writer, &Target::new(tenant, Uuid::new_v4()), shift)
        .await
        .unwrap();
    assert_eq!(
        (loaded.rows, loaded.batches, loaded.committed_batches),
        (316, 16, 15)
    );

    // A tenant account as telemetry-query creates one, and the ingest service's role.
    let reader = Account {
        user: "rollup_fixture_tenant".to_owned(),
        password: password(),
    };
    server
        .admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}' \
             SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '{tenant}' CONST",
            reader.user, reader.password
        ))
        .await;
    server
        .admin_ok(&format!("GRANT trustiot_tenant_reader TO {}", reader.user))
        .await;
    let ingest = Account {
        user: "rollup_fixture_ingest".to_owned(),
        password: password(),
    };
    server
        .admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}'",
            ingest.user, ingest.password
        ))
        .await;
    server
        .admin_ok(&format!("GRANT trustiot_ingest TO {}", ingest.user))
        .await;

    // Every cell of both views equals the dataset's.
    let readings = dataset.readings().unwrap();
    for (view, bucket_ms) in [
        ("trustiot.tenant_telemetry_1m", MINUTE_MS),
        ("trustiot.tenant_telemetry_1h", HOUR_MS),
    ] {
        let expected = expected_cells(&loaded, &readings, bucket_ms);
        let (problems, cells) = compare(&server, &reader, view, &expected).await;
        assert!(
            problems.is_empty(),
            "{view} differs from the dataset:\n{}",
            problems.join("\n")
        );
        let counted: u64 = expected.values().map(|cell| cell.readings).sum();
        println!(
            "{view}: {cells} cells equal the dataset's, counting {counted} of its {} readings \
             (the uncommitted batch and the backlog included)",
            readings.len()
        );
        assert_eq!(counted, loaded.rows);
    }

    // The energy of the cumulative points, from both views, equals the consecutive differences.
    for (device, point, wraps_at) in [
        ("meter-1", "energy_kwh", 0.0),
        ("meter-2", "pulses", 4_294_967_296.0),
        ("press-1", "cycle_count", 0.0),
    ] {
        let mut series: Vec<(DateTime<Utc>, f64)> = readings
            .iter()
            .filter(|reading| reading.device == device && reading.point == point)
            .map(|reading| (at(&loaded, reading), number(&reading.value).unwrap()))
            .collect();
        series.sort_by_key(|(ts, _)| *ts);
        let values: Vec<f64> = series.iter().map(|(_, value)| *value).collect();
        let truth = increase(&values, wraps_at);
        let from = floor(series[0].0, HOUR_MS);
        let to = floor(series[series.len() - 1].0, HOUR_MS) + TimeDelta::hours(1);
        let device_id = loaded.device(device).unwrap().to_string();
        let from_text = from.format("%Y-%m-%d %H:%M:%S").to_string();
        let to_text = to.format("%Y-%m-%d %H:%M:%S").to_string();
        let wraps_text = wraps_at.to_string();
        let mut answers = Vec::new();
        for statement in [ENERGY_1M, ENERGY_1H] {
            let answer: f64 = server
                .query_with(
                    &reader,
                    statement,
                    &[
                        ("param_device", device_id.as_str()),
                        ("param_point", point),
                        ("param_from", from_text.as_str()),
                        ("param_to", to_text.as_str()),
                        ("param_wraps_at", wraps_text.as_str()),
                    ],
                )
                .await
                .unwrap_or_else(|error| panic!("energy of {device}/{point}: {}", error.body))
                .trim()
                .parse()
                .unwrap();
            assert!(
                close(answer, truth),
                "{device}/{point}: {answer}, expected {truth}"
            );
            answers.push(answer);
        }
        println!(
            "energy {device}/{point} over {} readings: consecutive differences {truth}, \
             one-minute view {}, one-hour view {}",
            values.len(),
            answers[0],
            answers[1]
        );
    }

    // The reconcile of every loaded batch, as the ingest service would run it after a rewrite,
    // finds nothing to correct.
    let mut batches: BTreeMap<(String, u64), Vec<&Reading>> = BTreeMap::new();
    for reading in &readings {
        batches
            .entry((reading.stream.clone(), reading.batch_seq))
            .or_default()
            .push(reading);
    }
    for ((stream, seq), rows) in &batches {
        let series: BTreeSet<(String, String)> = rows
            .iter()
            .map(|reading| {
                (
                    loaded.device(&reading.device).unwrap().to_string(),
                    reading.point.clone(),
                )
            })
            .collect();
        let params = ReconcileParams {
            tenant: tenant.to_string(),
            gateway: loaded.gateway.to_string(),
            stream: loaded.stream(stream).unwrap().to_string(),
            series: series.into_iter().collect(),
            first_ts: time(
                rows.iter()
                    .map(|reading| at(&loaded, reading))
                    .min()
                    .unwrap(),
            ),
            last_ts: time(
                rows.iter()
                    .map(|reading| at(&loaded, reading))
                    .max()
                    .unwrap(),
            ),
        };
        let pairs = params.to_query_pairs();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        for statement in [RECONCILE_1M, RECONCILE_1H] {
            let answer = server.query_with_summary(&ingest, statement, &pairs).await;
            if let Err(error) = &answer.result {
                panic!("reconcile of {stream}/{seq}: {}", error.body);
            }
            assert_eq!(
                answer.written_rows,
                Some(0),
                "the reconcile of {stream}/{seq} corrected cells the views wrote"
            );
        }
    }
    println!(
        "reconcile: {} batches checked, both statements wrote 0 rows for each",
        batches.len()
    );
}
