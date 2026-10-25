//! The one-minute and one-hour aggregates of a lab dataset (Laysi/trustiot-v2#141): loaded into
//! a throwaway ClickHouse with the product's ClickHouse schema, every cell a tenant reads through
//! its aggregate views equals the cell this test computes from the dataset's readings (count,
//! numeric count, sum, minimum, maximum, first and last reading with its typed value), the
//! uncommitted batches and the backlog included; for every cumulative point of the index (with
//! its `wraps_at`), the energy queries of services/ingest/src/rollup.rs give, from both views,
//! the answer this test computes with the same formula from the dataset, which equals the sum of
//! consecutive differences wherever every bucket meets the formula's condition (otherwise the
//! difference is printed); and the reconcile of every loaded batch writes nothing, since the
//! views counted each batch once.
//!
//! The dataset is `lab_dataset::index_for_tests()`: the synthetic fixture, or the index that
//! `TRUSTIOT_LAB_DATASET_INDEX` names (for example the lab recording, Laysi/trustiot-v2#681).
//! Every expected value comes from the dataset and its index, none from the fixture.
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`, like `clickhouse_load.rs`. All SQL
//! is static: values and passwords travel as query parameters (tools/sql-check).

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
use ingest::rollup::{
    ENERGY_1H, ENERGY_1M, RECONCILE_1H, RECONCILE_1M, ReconcileParams, stream_settings,
};
use lab_dataset::index::PointKind;
use lab_dataset::{Dataset, Loaded, Reading, Target, Value, index_for_tests};
use uuid::Uuid;

use self::common::{Account, ClickHouse};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// The tenant the dataset is loaded for.
const TENANT: &str = "00000000-0000-4000-8000-000000014109";
/// A tenant account as telemetry-query creates one (0004_access.sql), for [`TENANT`]: ClickHouse
/// 26.8 takes no query parameter in a `SETTINGS` value, so the tenant ID is a literal.
const READER: &str = "rollup_fixture_tenant";
const CREATE_READER: &str = "CREATE USER rollup_fixture_tenant
IDENTIFIED WITH sha256_password BY {password:String}
DEFAULT ROLE trustiot_tenant_reader
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '00000000-0000-4000-8000-000000014109' CONST";
/// The ingest service's account: the ingest role, nothing else.
const INGEST: &str = "rollup_fixture_ingest";
const CREATE_INGEST: &str = "CREATE USER rollup_fixture_ingest
IDENTIFIED WITH sha256_password BY {password:String} DEFAULT ROLE trustiot_ingest";

/// The cells of a tenant view as this test compares them.
macro_rules! aggregates {
    ($view:literal) => {
        concat!(
            "SELECT toString(device_id), point, toUnixTimestamp(bucket), readings,
  numeric_readings, value_sum, value_min, value_max,
  toUnixTimestamp64Milli(first.ts), first.value_type, first.value_float, first.value_int,
  first.value_bool, first.value_string,
  toUnixTimestamp64Milli(last.ts), last.value_type, last.value_float, last.value_int,
  last.value_bool, last.value_string
FROM trustiot.",
            $view,
            " FORMAT TSV"
        )
    };
}

/// The two aggregate views, their query and their bucket length.
const VIEWS: [(&str, &str, i64); 2] = [
    (
        "trustiot.tenant_telemetry_1m",
        aggregates!("tenant_telemetry_1m"),
        MINUTE_MS,
    ),
    (
        "trustiot.tenant_telemetry_1h",
        aggregates!("tenant_telemetry_1h"),
        HOUR_MS,
    ),
];

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
    (view, query): (&str, &str),
    expected: &BTreeMap<(String, String, i64), Expected>,
) -> (Vec<String>, usize) {
    let answer = server
        .query(tenant, query)
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

/// What the energy queries compute for one series of readings in time order: the formula of
/// services/ingest/src/rollup.rs on the buckets of `bucket_ms`, and whether every bucket meets
/// its condition (at most one reset or wrap, after which the bucket's last reading stays below
/// the peak before it), under which the answer is exact. `None` when a bucket's first or last reading
/// is a tie between different values (any of them may be the view's), so the formula's answer
/// is not determined.
fn formula(series: &[(i64, f64)], bucket_ms: i64, wraps_at: f64) -> Option<(f64, bool)> {
    let mut buckets: BTreeMap<i64, Vec<(i64, f64)>> = BTreeMap::new();
    for (ms, value) in series {
        buckets
            .entry(ms.div_euclid(bucket_ms))
            .or_default()
            .push((*ms, *value));
    }
    let mut total = 0.0;
    let mut exact = true;
    let mut previous_last: Option<f64> = None;
    for readings in buckets.values() {
        let (first_ms, first) = readings[0];
        let (last_ms, last) = readings[readings.len() - 1];
        let ties = |ms: i64, value: f64| {
            readings
                .iter()
                .any(|(other_ms, other)| *other_ms == ms && *other != value)
        };
        if ties(first_ms, first) || ties(last_ms, last) {
            return None;
        }
        let min = readings
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::INFINITY, f64::min);
        let max = readings
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::NEG_INFINITY, f64::max);
        total += if last < max || min < first {
            if wraps_at > 0.0 {
                wraps_at - first + last
            } else {
                max - first + last
            }
        } else {
            last - first
        };
        total += match previous_last {
            None => 0.0,
            Some(previous) if first >= previous => first - previous,
            Some(previous) if wraps_at > 0.0 => first + wraps_at - previous,
            Some(_) => first,
        };
        previous_last = Some(last);
        let drops: Vec<usize> = (1..readings.len())
            .filter(|&i| readings[i].1 < readings[i - 1].1)
            .collect();
        exact &= match drops.as_slice() {
            [] => true,
            [i] => last < readings[*i - 1].1,
            _ => false,
        };
    }
    Some((total, exact))
}

fn floor(at: DateTime<Utc>, ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(at.timestamp_millis().div_euclid(ms) * ms)
        .single()
        .unwrap()
}

fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// Creates the account `user` with `create` (a static `CREATE USER` with a password parameter).
/// Creates `user` with `create`, which grants it `role` as its default role.
async fn account(server: &ClickHouse, user: &str, create: &str, role: &str) -> Account {
    let account = Account {
        user: user.to_owned(),
        password: Uuid::new_v4().simple().to_string(),
    };
    server
        .query_with(
            &server.admin,
            create,
            &[("param_password", account.password.as_str())],
        )
        .await
        .unwrap_or_else(|error| panic!("create {user}: {}", error.body));
    server.assert_default_role(user, role).await;
    account
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_aggregates_of_the_dataset_equal_its_readings() {
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

    let index = index_for_tests();
    let dataset = Dataset::open(&index).unwrap();
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
    let tenant = Uuid::parse_str(TENANT).unwrap();
    let loaded = dataset
        .load(&writer, &Target::new(tenant, Uuid::new_v4()), shift)
        .await
        .unwrap();
    let readings = dataset.readings().unwrap();
    assert_eq!(loaded.rows, u64::try_from(readings.len()).unwrap());
    println!(
        "dataset {} version {} ({}): {} readings in {} batches, {} of them committed",
        loaded.dataset,
        loaded.version,
        if std::env::var_os(lab_dataset::INDEX_ENV).is_some() {
            "from TRUSTIOT_LAB_DATASET_INDEX"
        } else {
            "the synthetic fixture"
        },
        loaded.rows,
        loaded.batches,
        loaded.committed_batches
    );

    let reader = account(&server, READER, CREATE_READER, "trustiot_tenant_reader").await;
    let ingest = account(&server, INGEST, CREATE_INGEST, "trustiot_ingest").await;

    // Every cell of both views equals the dataset's.
    for (view, query, bucket_ms) in VIEWS {
        let expected = expected_cells(&loaded, &readings, bucket_ms);
        let (problems, cells) = compare(&server, &reader, (view, query), &expected).await;
        assert!(
            problems.is_empty(),
            "{view} differs from the dataset:\n{}",
            problems.join("\n")
        );
        let counted: u64 = expected.values().map(|cell| cell.readings).sum();
        println!(
            "{view}: {cells} cells equal the dataset's, counting {counted} of its {} readings \
             (the uncommitted batches and the backlog included)",
            readings.len()
        );
        assert_eq!(counted, loaded.rows);
    }

    // The energy of every cumulative point of the index, from both views.
    let cumulative: Vec<(&str, &str, f64)> = dataset
        .index()
        .devices
        .iter()
        .flat_map(|device| {
            device
                .points
                .iter()
                .filter(|point| point.kind == PointKind::Cumulative)
                .map(|point| {
                    (
                        device.key.as_str(),
                        point.key.as_str(),
                        point.wraps_at.map_or(0.0, |wraps_at| wraps_at as f64),
                    )
                })
        })
        .collect();
    assert!(
        !cumulative.is_empty(),
        "the dataset has no cumulative point to compute energy from"
    );
    for (device, point, wraps_at) in cumulative {
        let mut series: Vec<(DateTime<Utc>, f64)> = readings
            .iter()
            .filter(|reading| reading.device == device && reading.point == point)
            .filter_map(|reading| Some((at(&loaded, reading), number(&reading.value)?)))
            .collect();
        if series.is_empty() {
            println!("energy {device}/{point}: no readings");
            continue;
        }
        series.sort_by_key(|(ts, _)| *ts);
        let values: Vec<f64> = series.iter().map(|(_, value)| *value).collect();
        let by_ms: Vec<(i64, f64)> = series
            .iter()
            .map(|(ts, value)| (ts.timestamp_millis(), *value))
            .collect();
        let truth = increase(&values, wraps_at);
        let from = floor(series[0].0, HOUR_MS);
        let to = floor(series[series.len() - 1].0, HOUR_MS) + TimeDelta::hours(1);
        let device_id = loaded.device(device).unwrap().to_string();
        let from_text = from.format("%Y-%m-%d %H:%M:%S").to_string();
        let to_text = to.format("%Y-%m-%d %H:%M:%S").to_string();
        let wraps_text = wraps_at.to_string();
        for (name, statement, bucket_ms) in [
            ("one-minute", ENERGY_1M, MINUTE_MS),
            ("one-hour", ENERGY_1H, HOUR_MS),
        ] {
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
            let Some((expected, exact)) = formula(&by_ms, bucket_ms, wraps_at) else {
                println!(
                    "energy {device}/{point}, {name} view: {answer}; a bucket's first or last \
                     reading is a tie between different values, so it is not compared \
                     (consecutive differences {truth})"
                );
                continue;
            };
            assert!(
                close(answer, expected),
                "{device}/{point}, {name} view: {answer}, the formula gives {expected}"
            );
            if exact {
                assert!(
                    close(answer, truth),
                    "{device}/{point}, {name} view: {answer}, consecutive differences {truth}"
                );
            }
            println!(
                "energy {device}/{point} over {} readings (wraps at {wraps_at}), {name} view: \
                 {answer}, consecutive differences {truth}{}",
                values.len(),
                if exact {
                    String::new()
                } else {
                    format!(
                        "; a bucket is outside the formula's condition, difference {}",
                        answer - truth
                    )
                }
            );
        }
    }

    // The reconcile of every loaded batch, as the ingest service would run it after a rewrite
    // (with the stream's settings), finds nothing to correct.
    let gateway = loaded.gateway.to_string();
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
        let stream_id = loaded.stream(stream).unwrap().to_string();
        let params = ReconcileParams {
            tenant: tenant.to_string(),
            gateway: gateway.clone(),
            stream: stream_id.clone(),
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
        let settings = stream_settings(&gateway, &stream_id);
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .chain(settings.iter())
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
