//! The one-minute and one-hour aggregates (migrations/clickhouse/0010_telemetry_rollups.sql,
//! src/rollup.rs; Laysi/trustiot-v2#141) against a real ClickHouse. After every step the
//! aggregates a tenant reads through its views are compared with cells this test computes
//! itself from the raw rows the same tenant reads through its raw view:
//!
//! - the backfill of rows written before the aggregates existed, and a second apply of the file
//!   that writes nothing;
//! - an exact retry of an INSERT, which the raw table and the aggregates both drop;
//! - the I1 rewrite of a batch after a partial attempt, with the reconcile of src/rollup.rs,
//!   interrupted after its first statement and run again, leaving another stream's cells alone;
//! - the deduplication hazard of the rewrite (a DELETE followed by an INSERT that ClickHouse
//!   drops as a duplicate, `insert_deduplicate = 0` included) and the contract's
//!   `deduplicate_insert = 'disable'` (`rollup::REWRITE_SETTINGS`);
//! - a crash between the raw part and a view's part of an INSERT;
//! - the energy of cumulative points across resets and wraps, from both views;
//! - retention by the receive day in calendar months, checked against chrono.
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`: the test applies the files in two
//! steps, detaches a materialized view and merges the aggregate tables, server-wide changes a
//! shared server must not see. Skipped without podman or docker unless `CI` is set.
//!
//! Run with `--nocapture` to see each step's evidence.

#![allow(
    clippy::print_stdout,
    reason = "the printed steps are the test evidence (`-- --nocapture`)"
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Days, Months, TimeDelta, TimeZone, Utc};
use common::{Account, Answer, ClickHouse, Migration, migrations};
use ingest::rollup::{
    ENERGY_1H, ENERGY_1M, RECONCILE_1H, RECONCILE_1M, REWRITE_SETTINGS, ReconcileParams,
};
use uuid::Uuid;

/// The file that adds the aggregates; the files before it are the schema they are added to.
const ROLLUP_FILE: &str = "0010_telemetry_rollups.sql";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// The two aggregate views and their bucket length.
const VIEWS: [(&str, i64); 2] = [
    ("trustiot.tenant_telemetry_1m", MINUTE_MS),
    ("trustiot.tenant_telemetry_1h", HOUR_MS),
];

/// A reading's value, in the telemetry table's column for its type.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(&'static str),
}

/// The batch stream a reading arrives in, and its device.
#[derive(Clone, Copy, Debug)]
struct Stream {
    tenant: Uuid,
    gateway: Uuid,
    stream: Uuid,
    device: Uuid,
}

impl Stream {
    fn new(tenant: Uuid, device: Uuid) -> Self {
        Self {
            tenant,
            gateway: Uuid::new_v4(),
            stream: Uuid::new_v4(),
            device,
        }
    }
}

/// One telemetry row as the ingest service writes it.
#[derive(Clone, Debug)]
struct Row {
    on: Stream,
    seq: u64,
    point: &'static str,
    ts: DateTime<Utc>,
    received_at: DateTime<Utc>,
    value: Value,
}

fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// `at` truncated to a multiple of `ms` since the epoch.
fn floor(at: DateTime<Utc>, ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(at.timestamp_millis().div_euclid(ms) * ms)
        .single()
        .expect("a valid time")
}

impl Row {
    fn literal(&self) -> String {
        let (kind, float, int, flag, text) = match &self.value {
            Value::Float(value) => ("float", *value, 0, false, ""),
            Value::Int(value) => ("int", 0.0, *value, false, ""),
            Value::Bool(value) => ("bool", 0.0, 0, *value, ""),
            Value::Text(value) => ("string", 0.0, 0, false, *value),
        };
        format!(
            "('{}', '{}', '{}', {}, '{}', '{}', '{}', '{}', '{kind}', {float:?}, {int}, {flag}, '{text}')",
            self.on.tenant,
            self.on.gateway,
            self.on.stream,
            self.seq,
            self.on.device,
            self.point,
            time(self.ts),
            time(self.received_at),
        )
    }
}

/// `count` readings of one batch, 20 seconds apart from `start`, cycling through a float, an
/// int (negative values too), a bool and a string point, so the cells cover every value type
/// and several minutes.
fn readings(
    on: Stream,
    seq: u64,
    start: DateTime<Utc>,
    received_at: DateTime<Utc>,
    count: i64,
) -> Vec<Row> {
    (0..count)
        .map(|n| {
            let (point, value) = match n % 4 {
                0 => (
                    "energy",
                    Value::Float(100.0 + 0.37 * f64::from(i32::try_from(n).expect("small"))),
                ),
                1 => ("count", Value::Int(3 * n - 7)),
                2 => ("running", Value::Bool(n % 3 == 0)),
                _ => (
                    "mode",
                    Value::Text(if n % 2 == 0 { "auto" } else { "manual" }),
                ),
            };
            Row {
                on,
                seq,
                point,
                ts: start + TimeDelta::seconds(20 * n),
                received_at,
                value,
            }
        })
        .collect()
}

/// A reading as a cell's first or last: its time and typed value.
#[derive(Clone, Debug, PartialEq)]
struct Reading {
    ts_ms: i64,
    kind: String,
    float: f64,
    int: i64,
    flag: bool,
    text: String,
}

impl Reading {
    /// Six TSV fields: time in ms, type, float, int, bool, string.
    fn parse(fields: &[&str]) -> Self {
        Self {
            ts_ms: fields[0].parse().expect("ts"),
            kind: fields[1].to_owned(),
            float: fields[2].parse().expect("float"),
            int: fields[3].parse().expect("int"),
            flag: fields[4] == "true",
            text: fields[5].to_owned(),
        }
    }

    /// The value the aggregates count as a number: float, int, and bool as 0 or 1.
    fn number(&self) -> Option<f64> {
        match self.kind.as_str() {
            "float" => Some(self.float),
            // As toFloat64 converts it in the aggregates.
            "int" => Some(self.int as f64),
            "bool" => Some(if self.flag { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

/// A cell as this test computes it from the raw rows: the readings with the earliest and the
/// latest time are kept as candidates, since a tie may give either.
#[derive(Debug, Default)]
struct Expected {
    readings: u64,
    numeric: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    firsts: Vec<Reading>,
    lasts: Vec<Reading>,
}

/// A cell as a tenant view returns it.
#[derive(Debug)]
struct Actual {
    readings: u64,
    numeric: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    first: Reading,
    last: Reading,
}

type Key = (String, String, i64);

/// The cells of `rows` (device, point, reading) with buckets of `bucket_ms`, keyed by device,
/// point and bucket start in seconds.
fn expected_cells(rows: &[(String, String, Reading)], bucket_ms: i64) -> BTreeMap<Key, Expected> {
    let mut cells: BTreeMap<Key, Expected> = BTreeMap::new();
    for (device, point, reading) in rows {
        let bucket = reading.ts_ms.div_euclid(bucket_ms) * bucket_ms / 1000;
        let cell = cells
            .entry((device.clone(), point.clone(), bucket))
            .or_default();
        cell.readings += 1;
        if let Some(number) = reading.number() {
            cell.numeric += 1;
            cell.sum += number;
            cell.min = Some(cell.min.map_or(number, |min| min.min(number)));
            cell.max = Some(cell.max.map_or(number, |max| max.max(number)));
        }
        match cell.firsts.first().map(|first| first.ts_ms) {
            Some(ts) if ts < reading.ts_ms => {}
            Some(ts) if ts == reading.ts_ms => cell.firsts.push(reading.clone()),
            _ => cell.firsts = vec![reading.clone()],
        }
        match cell.lasts.first().map(|last| last.ts_ms) {
            Some(ts) if ts > reading.ts_ms => {}
            Some(ts) if ts == reading.ts_ms => cell.lasts.push(reading.clone()),
            _ => cell.lasts = vec![reading.clone()],
        }
    }
    cells
}

fn optional(field: &str) -> Option<f64> {
    (field != "\\N").then(|| field.parse().expect("a number"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

const RAW: &str = "SELECT toString(device_id), point, toUnixTimestamp64Milli(ts), value_type,
  value_float, value_int, value_bool, value_string FROM trustiot.tenant_telemetry FORMAT TSV";

fn aggregate_query(view: &str) -> String {
    format!(
        "SELECT toString(device_id), point, toUnixTimestamp(bucket), readings, numeric_readings,
  value_sum, value_min, value_max,
  toUnixTimestamp64Milli(first.ts), first.value_type, first.value_float, first.value_int,
  first.value_bool, first.value_string,
  toUnixTimestamp64Milli(last.ts), last.value_type, last.value_float, last.value_int,
  last.value_bool, last.value_string
FROM {view} FORMAT TSV"
    )
}

/// The servers, the ingest service's account and a tenant account per tenant.
struct Test {
    ch: ClickHouse,
    ingest: Account,
}

fn password() -> String {
    Uuid::new_v4().simple().to_string()
}

impl Test {
    async fn new(ch: ClickHouse) -> Self {
        let ingest = Account {
            user: "rollup_test_ingest".to_owned(),
            password: password(),
        };
        ch.admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}'",
            ingest.user, ingest.password
        ))
        .await;
        ch.admin_ok(&format!("GRANT trustiot_ingest TO {}", ingest.user))
            .await;
        Self { ch, ingest }
    }

    /// A tenant account as telemetry-query creates one (0004_access.sql).
    async fn tenant(&self, tenant: Uuid) -> Account {
        let account = Account {
            user: format!("rollup_test_tenant_{}", tenant.simple()),
            password: password(),
        };
        self.ch
            .admin_ok(&format!(
                "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}' \
                 SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '{tenant}' CONST",
                account.user, account.password
            ))
            .await;
        self.ch
            .admin_ok(&format!("GRANT trustiot_tenant_reader TO {}", account.user))
            .await;
        account
    }

    /// Inserts `rows` as one block, as the ingest service, with `settings` in the URL.
    async fn insert(&self, rows: &[Row], settings: &[(&str, &str)]) -> Answer {
        let values: Vec<String> = rows.iter().map(Row::literal).collect();
        self.ch
            .query_with_summary(
                &self.ingest,
                &format!(
                    "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, \
                     device_id, point, ts, received_at, value_type, value_float, value_int, \
                     value_bool, value_string) VALUES {}",
                    values.join(", ")
                ),
                settings,
            )
            .await
    }

    async fn insert_ok(&self, rows: &[Row], settings: &[(&str, &str)]) {
        let answer = self.insert(rows, settings).await;
        if let Err(error) = answer.result {
            panic!("insert failed: {}", error.body);
        }
    }

    /// The first step of the rewrite: the batch's rows go, synchronously.
    async fn delete_batch(&self, on: Stream, seq: u64) {
        self.ch
            .query_with(
                &self.ingest,
                &format!(
                    "DELETE FROM trustiot.telemetry WHERE stream_id = '{}' AND batch_seq = {seq}",
                    on.stream
                ),
                &[("lightweight_deletes_sync", "2")],
            )
            .await
            .unwrap_or_else(|error| panic!("delete: {}", error.body));
    }

    /// The raw rows of a batch, as the ingest service counts them.
    async fn raw_rows(&self, on: Stream, seq: u64) -> u64 {
        self.ch
            .query(
                &self.ingest,
                &format!(
                    "SELECT count() FROM trustiot.telemetry WHERE stream_id = '{}' AND batch_seq = {seq}",
                    on.stream
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("count: {}", error.body))
            .trim()
            .parse()
            .expect("a count")
    }

    /// Runs one reconcile statement for the batch `rows`, as the ingest service; the rows it
    /// wrote.
    async fn reconcile(&self, statement: &str, rows: &[Row]) -> u64 {
        let first = rows.first().expect("a batch");
        let series: BTreeSet<(String, String)> = rows
            .iter()
            .map(|row| (row.on.device.to_string(), row.point.to_owned()))
            .collect();
        let params = ReconcileParams {
            tenant: first.on.tenant.to_string(),
            gateway: first.on.gateway.to_string(),
            stream: first.on.stream.to_string(),
            series: series.into_iter().collect(),
            first_ts: time(rows.iter().map(|row| row.ts).min().expect("a batch")),
            last_ts: time(rows.iter().map(|row| row.ts).max().expect("a batch")),
        };
        let pairs = params.to_query_pairs();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let answer = self
            .ch
            .query_with_summary(&self.ingest, statement, &pairs)
            .await;
        if let Err(error) = answer.result {
            panic!("reconcile failed: {}", error.body);
        }
        answer
            .written_rows
            .expect("the summary names the rows written")
    }

    /// Both reconcile statements, one-minute first: the rows each wrote.
    async fn reconcile_both(&self, rows: &[Row]) -> (u64, u64) {
        (
            self.reconcile(RECONCILE_1M, rows).await,
            self.reconcile(RECONCILE_1H, rows).await,
        )
    }

    /// Where the tenant's aggregates differ from the cells of its raw rows, both read through
    /// the tenant's views; empty when they agree. Also the number of cells of each view.
    async fn differences(&self, tenant: &Account) -> (Vec<String>, [usize; 2]) {
        let raw = self
            .ch
            .query(tenant, RAW)
            .await
            .unwrap_or_else(|error| panic!("raw view: {}", error.body));
        let rows: Vec<(String, String, Reading)> = raw
            .lines()
            .map(|line| {
                let fields: Vec<&str> = line.split('\t').collect();
                (
                    fields[0].to_owned(),
                    fields[1].to_owned(),
                    Reading::parse(&fields[2..]),
                )
            })
            .collect();
        let mut problems = Vec::new();
        let mut cells = [0; 2];
        for (index, (view, bucket_ms)) in VIEWS.into_iter().enumerate() {
            let expected = expected_cells(&rows, bucket_ms);
            let answer = self
                .ch
                .query(tenant, &aggregate_query(view))
                .await
                .unwrap_or_else(|error| panic!("{view}: {}", error.body));
            let mut actual: BTreeMap<Key, Actual> = BTreeMap::new();
            for line in answer.lines() {
                let fields: Vec<&str> = line.split('\t').collect();
                actual.insert(
                    (
                        fields[0].to_owned(),
                        fields[1].to_owned(),
                        fields[2].parse().expect("bucket"),
                    ),
                    Actual {
                        readings: fields[3].parse().expect("readings"),
                        numeric: fields[4].parse().expect("numeric readings"),
                        sum: fields[5].parse().expect("sum"),
                        min: optional(fields[6]),
                        max: optional(fields[7]),
                        first: Reading::parse(&fields[8..14]),
                        last: Reading::parse(&fields[14..20]),
                    },
                );
            }
            cells[index] = actual.len();
            for key in expected
                .keys()
                .chain(actual.keys())
                .collect::<BTreeSet<_>>()
            {
                match (expected.get(key), actual.get(key)) {
                    (Some(want), Some(got)) => {
                        let same = want.readings == got.readings
                            && want.numeric == got.numeric
                            && close(want.sum, got.sum)
                            && want.min == got.min
                            && want.max == got.max
                            && want.firsts.contains(&got.first)
                            && want.lasts.contains(&got.last);
                        if !same {
                            problems
                                .push(format!("{view} {key:?}: expected {want:?}, got {got:?}"));
                        }
                    }
                    (Some(want), None) => {
                        problems.push(format!("{view} {key:?}: missing, expected {want:?}"));
                    }
                    (None, Some(got)) => {
                        problems.push(format!("{view} {key:?}: no raw rows, got {got:?}"));
                    }
                    (None, None) => {}
                }
            }
        }
        (problems, cells)
    }

    /// Fails unless the tenant's aggregates equal the cells of its raw rows.
    async fn assert_equal(&self, tenant: &Account, step: &str) -> [usize; 2] {
        let (problems, cells) = self.differences(tenant).await;
        assert!(
            problems.is_empty(),
            "{step}: the aggregates differ from the raw rows:\n{}",
            problems.join("\n")
        );
        println!(
            "{step}: the aggregates equal the raw rows ({} one-minute and {} one-hour cells)",
            cells[0], cells[1]
        );
        cells
    }

    /// The cells of one stream in both tables, as the ingest service reads them.
    async fn stream_cells(&self, on: Stream) -> String {
        let mut text = String::new();
        for table in ["telemetry_1m", "telemetry_1h"] {
            text.push_str(
                &self
                    .ch
                    .query(
                        &self.ingest,
                        &format!(
                            "SELECT device_id, point, bucket, received_day, sum(readings),
  sum(numeric_readings), sum(value_sum), min(value_min), max(value_max),
  toString(argMinMerge(first)), toString(argMaxMerge(last))
FROM trustiot.{table} WHERE stream_id = '{}'
GROUP BY device_id, point, bucket, received_day ORDER BY ALL FORMAT TSV",
                            on.stream
                        ),
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{table}: {}", error.body)),
            );
        }
        text
    }

    /// The increase of a cumulative point from one of the energy queries, as the tenant.
    async fn energy(
        &self,
        tenant: &Account,
        statement: &str,
        on: Stream,
        point: &str,
        (from, to): (DateTime<Utc>, DateTime<Utc>),
        wraps_at: f64,
    ) -> f64 {
        let device = on.device.to_string();
        let from = from.format("%Y-%m-%d %H:%M:%S").to_string();
        let to = to.format("%Y-%m-%d %H:%M:%S").to_string();
        let wraps_at = wraps_at.to_string();
        self.ch
            .query_with(
                tenant,
                statement,
                &[
                    ("param_device", device.as_str()),
                    ("param_point", point),
                    ("param_from", from.as_str()),
                    ("param_to", to.as_str()),
                    ("param_wraps_at", wraps_at.as_str()),
                ],
            )
            .await
            .unwrap_or_else(|error| panic!("energy: {}", error.body))
            .trim()
            .parse()
            .expect("a number")
    }
}

/// Applies one migration file as the admin, as the migration Job does; the rows its INSERT
/// statements wrote.
async fn apply(ch: &ClickHouse, migration: &Migration) -> u64 {
    let mut written = 0;
    for statement in &migration.statements {
        let answer = ch.query_with_summary(&ch.admin, statement, &[]).await;
        if let Err(error) = &answer.result {
            panic!(
                "{}: statement failed:\n{statement}\n{}",
                migration.name, error.body
            );
        }
        if statement.starts_with("INSERT") {
            written += answer
                .written_rows
                .expect("the summary names the rows written");
        }
    }
    written
}

/// Rows written before the aggregates existed are backfilled when the file is applied; applying
/// it again writes nothing; rows written after reach the aggregates through the views.
async fn backfill_then_views(test: &Test, rollup: &Migration) {
    let now = Utc::now();
    let tenant = Uuid::new_v4();
    let device = Uuid::new_v4();
    let a = Stream::new(tenant, device);
    let b = Stream::new(tenant, device);
    let hour = floor(now, HOUR_MS);
    // Three days ago, an hour ago, and a backlog of three-day-old readings received now.
    let batches = [
        readings(
            a,
            1,
            hour - TimeDelta::days(3) + TimeDelta::minutes(55),
            now - TimeDelta::days(3),
            24,
        ),
        readings(
            b,
            1,
            hour - TimeDelta::hours(2) + TimeDelta::minutes(55),
            now - TimeDelta::hours(1),
            24,
        ),
        readings(
            a,
            2,
            hour - TimeDelta::days(3) + TimeDelta::minutes(30),
            now,
            12,
        ),
    ];
    for batch in &batches {
        test.insert_ok(batch, &[]).await;
    }

    let written = apply(&test.ch, rollup).await;
    assert!(written > 0, "the backfill wrote nothing");
    let account = test.tenant(tenant).await;
    test.assert_equal(&account, "backfill of rows written before 0010")
        .await;
    let again = apply(&test.ch, rollup).await;
    assert_eq!(
        again, 0,
        "a second apply of {ROLLUP_FILE} wrote aggregate rows"
    );
    test.assert_equal(&account, "second apply").await;
    println!("backfill: the first apply wrote {written} aggregate rows, the second {again}");

    test.insert_ok(&readings(b, 2, hour - TimeDelta::minutes(5), now, 16), &[])
        .await;
    test.assert_equal(&account, "rows written after 0010").await;
}

/// An INSERT retried unchanged (its acknowledgement was lost) is dropped by the raw table and by
/// both aggregate tables.
async fn exact_retry_counts_once(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(Uuid::new_v4(), Uuid::new_v4());
    let batch = readings(on, 1, floor(now, HOUR_MS) - TimeDelta::minutes(5), now, 20);
    test.insert_ok(&batch, &[]).await;
    test.insert_ok(&batch, &[]).await;
    assert_eq!(test.raw_rows(on, 1).await, 20, "the retry was not dropped");
    let account = test.tenant(on.tenant).await;
    test.assert_equal(&account, "exact retry").await;
}

/// The I1 rewrite after a partial attempt: the DELETE does not reach the aggregates, the
/// reconcile corrects them, even when it stopped after its first statement, and it leaves
/// another stream's cells as they were.
async fn rewrite_after_a_partial_attempt(test: &Test) {
    let now = Utc::now();
    let tenant = Uuid::new_v4();
    let device = Uuid::new_v4();
    let on = Stream::new(tenant, device);
    let other = Stream::new(tenant, device);
    let start = floor(now, HOUR_MS) - TimeDelta::minutes(5);
    // The other stream's readings fall in the same cells of the tenant views.
    test.insert_ok(&readings(other, 1, start, now, 24), &[])
        .await;
    let other_before = test.stream_cells(other).await;
    let mut batch = readings(on, 7, start, now, 24);
    // Readings three days old in the same batch: a gateway's backlog.
    batch.extend(readings(on, 7, start - TimeDelta::days(3), now, 8));
    let account = test.tenant(tenant).await;

    // Attempt 1: the first of the batch's two INSERTs reached ClickHouse, then the connection
    // failed. Without a completion record its rows still count, in the raw view and in the
    // aggregates alike.
    test.insert_ok(&batch[..10], &[]).await;
    test.assert_equal(&account, "partial attempt, no completion record")
        .await;

    // The rewrite: delete the batch's rows, insert the whole batch without deduplication.
    test.delete_batch(on, 7).await;
    test.insert_ok(&batch, &REWRITE_SETTINGS).await;
    assert_eq!(test.raw_rows(on, 7).await, 32);
    let (problems, _) = test.differences(&account).await;
    assert!(
        !problems.is_empty(),
        "the aggregates should still count the first attempt before the reconcile"
    );
    println!(
        "rewrite: before the reconcile, {} cells differ from the raw rows",
        problems.len()
    );

    // The reconcile stops after its one-minute statement (a crash), then runs again in full.
    let minute = test.reconcile(RECONCILE_1M, &batch).await;
    assert!(minute > 0, "the one-minute reconcile wrote nothing");
    let (minute_again, hour) = test.reconcile_both(&batch).await;
    assert_eq!(
        minute_again, 0,
        "the one-minute cells were already corrected"
    );
    assert!(hour > 0, "the one-hour reconcile wrote nothing");
    test.assert_equal(&account, "rewrite and reconcile").await;
    assert_eq!(
        test.reconcile_both(&batch).await,
        (0, 0),
        "a reconcile of agreeing cells must write nothing"
    );
    assert_eq!(
        test.stream_cells(other).await,
        other_before,
        "the reconcile changed another stream's cells"
    );
    println!(
        "rewrite: the reconcile wrote {minute} one-minute rows, then 0 and {hour} one-hour rows \
         after the interruption, then 0 and 0; the other stream's cells are unchanged"
    );
}

/// How a deduplicating rewrite repeats the first attempt, the stream it writes, and the settings
/// of both INSERTs.
type HazardCase<'a> = (&'static str, Stream, Vec<(&'a str, &'a str)>);

/// The deduplication hazard of the rewrite (Laysi/trustiot-v2#141, finding E7a): after the
/// synchronous DELETE, an INSERT of the same batch that deduplicates, by the same
/// insert_deduplication_token or by identical content, is dropped without an error, and the
/// batch's rows are lost; on ClickHouse 26.8 `insert_deduplicate = 0` does not prevent it. With
/// the contract's `deduplicate_insert = 'disable'` every row lands and the reconcile makes the
/// aggregates agree.
async fn rewrite_must_not_deduplicate(test: &Test) {
    let now = Utc::now();
    let tenant = Uuid::new_v4();
    let device = Uuid::new_v4();
    let start = floor(now, HOUR_MS) - TimeDelta::minutes(3);
    let account = test.tenant(tenant).await;
    let streams = [
        Stream::new(tenant, device),
        Stream::new(tenant, device),
        Stream::new(tenant, device),
    ];
    let token = format!("{}:1", streams[0].stream);
    let cases: [HazardCase<'_>; 3] = [
        (
            "the same insert_deduplication_token",
            streams[0],
            vec![("insert_deduplication_token", token.as_str())],
        ),
        ("identical content", streams[1], Vec::new()),
        (
            "identical content and insert_deduplicate = 0",
            streams[2],
            vec![("insert_deduplicate", "0")],
        ),
    ];
    let mut batches = Vec::new();
    for (how, on, settings) in &cases {
        let batch = readings(*on, 1, start, now, 12);
        test.insert_ok(&batch, settings).await;
        test.delete_batch(*on, 1).await;
        test.insert_ok(&batch, settings).await;
        assert_eq!(
            test.raw_rows(*on, 1).await,
            0,
            "the rewrite with {how} was expected to be dropped"
        );
        println!(
            "deduplication hazard: a rewrite with {how} answered OK and wrote 0 of {} rows",
            batch.len()
        );
        batches.push((*on, batch));
    }
    let (problems, _) = test.differences(&account).await;
    assert!(
        !problems.is_empty(),
        "the aggregates should still count the deleted rows"
    );
    for (on, batch) in &batches {
        test.insert_ok(batch, &REWRITE_SETTINGS).await;
        assert_eq!(test.raw_rows(*on, 1).await, 12);
        let (minute, hour) = test.reconcile_both(batch).await;
        assert!(
            minute > 0 && hour > 0,
            "the aggregates counted the batch twice"
        );
    }
    test.assert_equal(
        &account,
        "rewrite with deduplicate_insert = 'disable' and reconcile",
    )
    .await;
}

/// A crash between the raw part of an INSERT and a view's part (here the one-hour view is
/// detached during attempt 1): after the rewrite the one-minute cells count attempt 1 twice
/// and the one-hour cells once; the reconcile corrects the first and writes nothing for the
/// second.
async fn crash_between_the_raw_and_a_view_part(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(Uuid::new_v4(), Uuid::new_v4());
    let batch = readings(on, 3, floor(now, HOUR_MS) - TimeDelta::minutes(2), now, 16);
    let account = test.tenant(on.tenant).await;
    test.ch
        .admin_ok("DETACH TABLE trustiot.telemetry_1h_mv")
        .await;
    test.insert_ok(&batch, &[]).await;
    test.ch
        .admin_ok("ATTACH TABLE trustiot.telemetry_1h_mv")
        .await;
    let (problems, _) = test.differences(&account).await;
    assert!(
        problems
            .iter()
            .all(|problem| problem.starts_with("trustiot.tenant_telemetry_1h ")),
        "only the one-hour cells should lack attempt 1: {problems:?}"
    );
    assert!(
        !problems.is_empty(),
        "the one-hour cells should lack attempt 1"
    );
    test.delete_batch(on, 3).await;
    test.insert_ok(&batch, &REWRITE_SETTINGS).await;
    let (minute, hour) = test.reconcile_both(&batch).await;
    assert!(minute > 0, "the one-minute cells counted attempt 1 twice");
    assert_eq!(hour, 0, "the one-hour cells hold the rewrite only");
    test.assert_equal(&account, "crash between the raw and a view part")
        .await;
}

/// A whole-number reading of the test's counters as an int.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the test's counter values are small whole numbers"
)]
fn whole(value: f64) -> i64 {
    value as i64
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

/// A cumulative point of the energy test: its key, its readings as (minutes after the first
/// hour, value), its wrap value (0 for none), and the one-hour answer when the case lies outside
/// the condition the queries are exact under.
type EnergyCase = (&'static str, &'static [(i64, f64)], f64, Option<f64>);

/// The energy queries of src/rollup.rs on both views, for cumulative points this test writes:
/// a reset within an hour (first 5, peak 7, reset to 0, last 6: 8, not 1), a first reading of 0,
/// a wrap, a reset between hours, and a steady rise. Every case meets the condition the
/// queries are exact under (at most one reset or wrap per bucket, and after a reset the last
/// reading stays below the peak before it); one case outside it shows the one-hour answer
/// drifting while the one-minute one, with one reading per bucket, stays exact.
async fn energy_of_cumulative_points(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(Uuid::new_v4(), Uuid::new_v4());
    let account = test.tenant(on.tenant).await;
    let h0 = floor(now, HOUR_MS) - TimeDelta::hours(3);
    let minutes = |m: i64| h0 + TimeDelta::minutes(m);
    let cases: [EnergyCase; 6] = [
        (
            "reset_in_hour",
            &[(0, 5.0), (10, 7.0), (20, 0.0), (30, 6.0)],
            0.0,
            None,
        ),
        (
            "first_zero",
            &[(0, 0.0), (10, 5.0), (20, 0.0), (30, 3.0)],
            0.0,
            None,
        ),
        (
            "wrap_in_hour",
            &[(0, 90.0), (10, 95.0), (20, 3.0), (30, 8.0)],
            100.0,
            None,
        ),
        (
            "reset_between_hours",
            &[(0, 10.0), (10, 12.0), (60, 4.0), (70, 9.0)],
            0.0,
            None,
        ),
        (
            "steady",
            &[(0, 1.0), (10, 2.0), (20, 3.0), (60, 5.0), (70, 6.0)],
            0.0,
            None,
        ),
        // Last reading above the peak before the reset: outside the condition.
        (
            "outside_condition",
            &[(0, 5.0), (10, 7.0), (20, 0.0), (30, 9.0)],
            0.0,
            Some(13.0),
        ),
    ];
    let mut batch = Vec::new();
    for (point, values, wraps_at, _) in &cases {
        for (minute, value) in *values {
            batch.push(Row {
                on,
                seq: 1,
                point,
                ts: minutes(*minute),
                received_at: now,
                value: if *wraps_at > 0.0 {
                    Value::Int(whole(*value))
                } else {
                    Value::Float(*value)
                },
            });
        }
    }
    test.insert_ok(&batch, &[]).await;
    test.assert_equal(&account, "cumulative points").await;
    let range = (h0, h0 + TimeDelta::hours(2));
    for (point, values, wraps_at, hour_outside) in &cases {
        let readings: Vec<f64> = values.iter().map(|(_, value)| *value).collect();
        let truth = increase(&readings, *wraps_at);
        let minute = test
            .energy(&account, ENERGY_1M, on, point, range, *wraps_at)
            .await;
        let hour = test
            .energy(&account, ENERGY_1H, on, point, range, *wraps_at)
            .await;
        println!(
            "energy {point} {readings:?} (wraps at {wraps_at}): consecutive differences {truth}, \
             one-minute view {minute}, one-hour view {hour}"
        );
        assert!(
            close(minute, truth),
            "{point}: one-minute {minute}, expected {truth}"
        );
        match hour_outside {
            None => assert!(
                close(hour, truth),
                "{point}: one-hour {hour}, expected {truth}"
            ),
            Some(drift) => assert!(
                close(hour, *drift) && !close(hour, truth),
                "{point}: outside the condition the one-hour answer is (peak - first) + last = \
                 {drift}, got {hour}"
            ),
        }
    }
}

/// The aggregates expire by the receive day, in calendar months: a row is kept at least 13
/// months after it was received (never early) and goes within a day after that, by chrono's
/// month arithmetic as an independent reference. The one-hour table keeps them all (48 months).
async fn retention_by_receive_day(test: &Test) {
    let tenant = Uuid::new_v4();
    let on = Stream::new(tenant, Uuid::new_v4());
    let today = Utc::now().date_naive();
    let rows: Vec<Row> = (380..=410)
        .map(|days_ago| {
            let received_at = (today - Days::new(days_ago))
                .and_hms_opt(12, 0, 0)
                .expect("noon")
                .and_utc();
            Row {
                on,
                seq: days_ago,
                point: "kept",
                ts: received_at,
                received_at,
                value: Value::Float(1.0),
            }
        })
        .collect();
    test.insert_ok(&rows, &[]).await;
    let before = Utc::now();
    for table in ["telemetry_1m", "telemetry_1h"] {
        // A merge applies the TTL; FINAL merges every part.
        test.ch
            .admin_ok(&format!("OPTIMIZE TABLE trustiot.{table} FINAL"))
            .await;
    }
    let after = Utc::now();
    let kept = |table: &'static str| async move {
        test.ch
            .query(
                &test.ingest,
                &format!(
                    "SELECT DISTINCT toString(received_day) FROM trustiot.{table} \
                     WHERE tenant_id = '{tenant}' ORDER BY 1 FORMAT TSV"
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{table}: {}", error.body))
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<String>>()
    };
    let minute = kept("telemetry_1m").await;
    let hour = kept("telemetry_1h").await;
    let (mut stayed, mut went) = (0, 0);
    for row in &rows {
        let day = row.received_at.date_naive();
        let thirteen = row
            .received_at
            .checked_add_months(Months::new(13))
            .expect("a date");
        let expires = (day.checked_add_months(Months::new(13)).expect("a date") + Days::new(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc();
        assert!(
            expires >= thirteen && expires - thirteen <= TimeDelta::days(1),
            "{day}: the TTL {expires} is not within a day after 13 months ({thirteen})"
        );
        let key = day.to_string();
        if expires > after {
            assert!(minute.contains(&key), "{day} expired early (TTL {expires})");
            stayed += 1;
        } else if expires <= before {
            assert!(!minute.contains(&key), "{day} outlived its TTL {expires}");
            went += 1;
        }
        assert!(hour.contains(&key), "{day} left the one-hour table");
    }
    assert!(stayed > 0 && went > 0, "the range must straddle 13 months");
    println!(
        "retention: of {} receive days 380 to 410 days ago, the one-minute table kept {stayed} \
         and dropped {went} by chrono's 13 months plus a day; the one-hour table kept all {}",
        rows.len(),
        hour.len()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_aggregates_follow_the_raw_rows() {
    let Some(ch) = ClickHouse::start_throwaway().await else {
        return;
    };
    let files = migrations();
    let (earlier, rest): (Vec<_>, Vec<_>) = files
        .iter()
        .partition(|migration| migration.name.as_str() < ROLLUP_FILE);
    for migration in &earlier {
        apply(&ch, migration).await;
    }
    let test = Test::new(ch).await;
    let rollup = rest
        .iter()
        .find(|migration| migration.name == ROLLUP_FILE)
        .expect("the rollup file");
    backfill_then_views(&test, rollup).await;
    for migration in rest
        .iter()
        .filter(|migration| migration.name != ROLLUP_FILE)
    {
        apply(&test.ch, migration).await;
    }
    exact_retry_counts_once(&test).await;
    rewrite_after_a_partial_attempt(&test).await;
    rewrite_must_not_deduplicate(&test).await;
    crash_between_the_raw_and_a_view_part(&test).await;
    energy_of_cumulative_points(&test).await;
    retention_by_receive_day(&test).await;
    println!(
        "server: {}",
        test.ch.admin_ok("SELECT version()").await.trim()
    );
}
