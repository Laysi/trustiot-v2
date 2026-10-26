//! The one-minute and one-hour aggregates (migrations/clickhouse/0010_telemetry_rollups.sql,
//! src/rollup.rs; Laysi/trustiot-v2#141) against a real ClickHouse. After every step the
//! aggregates a tenant reads through its views are compared with cells this test computes
//! itself from the raw rows the same tenant reads through its raw view:
//!
//! - the backfill of rows written before the aggregates existed, as the migration Job runs it
//!   (src/rollup.rs `BACKFILL_1M`, `BACKFILL_1H`), and a second backfill that writes nothing;
//!   the backfill views show nothing to the admin until it names itself in their policy;
//! - the bound of the backfill and of the runbook's check and repair (`REPAIR_1M`,
//!   `REPAIR_1H`): after the raw rows of older receive days expired (a shorter raw TTL and a
//!   merge, one day only partly), applying the file again and backfilling as after a lost
//!   `trustiot_migrate.applied` writes nothing, even with too long a retention, the check
//!   reports nothing for those days, the repair leaves their cells as they were and never
//!   erases part of the cut-off day; the same check with too long a retention would erase them.
//!   Within the bound the backfill leaves a cell whose raw rows were deleted alone and the
//!   repair corrects it, and the backfill fills the cells of a day the aggregates lost;
//! - an exact retry of an INSERT, which the raw table and the aggregates both drop;
//! - the I1 rewrite of a batch after a partial attempt, with the reconcile of src/rollup.rs,
//!   interrupted after its first statement and run again, leaving another stream's cells alone;
//! - the reconcile window: a rewrite after the midnight that ended the gateway buffer's 30th day
//!   for a batch first received at 23:59, which a 30-day window misses and the contract's 31
//!   receive days (`rollup::RECONCILE_WINDOW_DAYS`) reach;
//! - the deduplication hazard of the rewrite (a DELETE followed by an INSERT that ClickHouse
//!   drops as a duplicate, `insert_deduplicate = 0` included) and the contract's
//!   `deduplicate_insert = 'disable'` (`rollup::REWRITE_SETTINGS`);
//! - a crash between the raw part and a view's part of an INSERT;
//! - the stream's query ID of the contract (`rollup::stream_settings`): a synchronous INSERT of
//!   the stream that still runs is refused another statement with its ID, and is stopped by the
//!   reconcile, which then runs; the rows it landed before stay, and none land afterwards;
//! - the energy of cumulative points across resets and wraps, from both views;
//! - retention by the receive day in calendar months, checked against chrono.
//!
//! Every statement that writes a stream runs with `rollup::stream_settings`, as the contract
//! asks. All SQL is static: values, passwords and settings travel as query parameters or URL
//! settings, rows as the INSERT's data (tools/sql-check).
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`: the test applies the files in two
//! steps, detaches a materialized view, changes the raw TTL and merges the tables, server-wide
//! changes a shared server must not see. Skipped without podman or docker unless `CI` is set.
//!
//! Run with `--nocapture` to see each step's evidence.

#![allow(
    clippy::print_stdout,
    reason = "the printed steps are the test evidence (`-- --nocapture`)"
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Days, Months, TimeDelta, TimeZone, Utc};
use common::{Account, Answer, ClickHouse, Migration, migrations};
use ingest::rollup::{
    BACKFILL_1H, BACKFILL_1M, ENERGY_1H, ENERGY_1M, RECONCILE_1H, RECONCILE_1M,
    RECONCILE_WINDOW_DAYS, REPAIR_1H, REPAIR_1M, REWRITE_SETTINGS, ReconcileParams,
    stream_query_id, stream_settings,
};
use protocol::telemetry::BUFFER_MAX_AGE_DAYS;
use uuid::Uuid;

/// The file that adds the aggregates; the files before it are the schema they are added to.
const ROLLUP_FILE: &str = "0010_telemetry_rollups.sql";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// The ingest service's account: the ingest role, nothing else (`DEFAULT ROLE` grants it, which
/// [`Test::new`] checks).
const CREATE_INGEST: &str = "CREATE USER rollup_test_ingest
IDENTIFIED WITH sha256_password BY {password:String} DEFAULT ROLE trustiot_ingest";
const INGEST_USER: &str = "rollup_test_ingest";
/// The slow statement of the query ID step reads system.numbers; the product's ingest role
/// does not need it.
const GRANT_NUMBERS: &str = "GRANT SELECT ON system.numbers TO rollup_test_ingest";

/// A tenant account as telemetry-query creates one (0004_access.sql), for one fixed tenant.
struct TenantAccount {
    user: &'static str,
    tenant: &'static str,
    create: &'static str,
}

/// A [`TenantAccount`] whose `CREATE USER` is one static statement: ClickHouse 26.8 takes no
/// query parameter in a `SETTINGS` value, so the tenant ID is a literal of the statement. The
/// queryable period (0011_queryable_period.sql) is the longest the settings accept, so readings
/// at fixed past times stay visible.
macro_rules! tenant_account {
    ($user:literal, $tenant:literal) => {
        TenantAccount {
            user: $user,
            tenant: $tenant,
            create: concat!(
                "CREATE USER ",
                $user,
                " IDENTIFIED WITH sha256_password BY {password:String}
DEFAULT ROLE trustiot_tenant_reader
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '",
                $tenant,
                "' CONST, SQL_queryable_raw_days = 36500 CONST,
  SQL_queryable_1m_months = 1200 CONST, SQL_queryable_1h_months = 1200 CONST"
            ),
        }
    };
}

const BACKFILL_TENANT: TenantAccount = tenant_account!(
    "rollup_test_backfill",
    "00000000-0000-4000-8000-000000014101"
);
const RETRY_TENANT: TenantAccount =
    tenant_account!("rollup_test_retry", "00000000-0000-4000-8000-000000014102");
const REWRITE_TENANT: TenantAccount = tenant_account!(
    "rollup_test_rewrite",
    "00000000-0000-4000-8000-000000014103"
);
const HAZARD_TENANT: TenantAccount =
    tenant_account!("rollup_test_hazard", "00000000-0000-4000-8000-000000014104");
const CRASH_TENANT: TenantAccount =
    tenant_account!("rollup_test_crash", "00000000-0000-4000-8000-000000014105");
const QUERY_ID_TENANT: TenantAccount = tenant_account!(
    "rollup_test_query_id",
    "00000000-0000-4000-8000-000000014106"
);
const ENERGY_TENANT: TenantAccount =
    tenant_account!("rollup_test_energy", "00000000-0000-4000-8000-000000014107");
/// Read through the ingest account only.
const RETENTION_TENANT: &str = "00000000-0000-4000-8000-000000014108";
const WINDOW_TENANT: TenantAccount =
    tenant_account!("rollup_test_window", "00000000-0000-4000-8000-000000014109");

/// Rows as the ingest service sends them: the data of the request, not part of the SQL.
const INSERT_ROWS: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
    batch_seq, device_id, point, ts, received_at, value_type, value_float, value_int, value_bool,
    value_string) FORMAT JSONEachRow";
/// The first step of the rewrite: the batch's rows go, synchronously.
const DELETE_BATCH: &str = "DELETE FROM trustiot.telemetry
WHERE toString(stream_id) = {stream:String} AND batch_seq = {seq:UInt64}";
const BATCH_ROWS: &str = "SELECT count() FROM trustiot.telemetry
WHERE toString(stream_id) = {stream:String} AND batch_seq = {seq:UInt64} FORMAT TSV";

const RAW: &str = "SELECT toString(device_id), point, toUnixTimestamp64Milli(ts), value_type,
  value_float, value_int, value_bool, value_string FROM trustiot.tenant_telemetry FORMAT TSV";

/// The cells of a tenant view as this test compares them.
macro_rules! aggregates {
    ($view:literal) => {
        concat!(
            "SELECT toString(device_id), point, toUnixTimestamp(bucket), readings, numeric_readings,
  value_sum, value_min, value_max,
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

/// One stream's cells in an aggregate table, as the ingest service reads them.
macro_rules! stream_cells {
    ($table:literal) => {
        concat!(
            "SELECT device_id, point, bucket, received_day, sum(readings),
  sum(numeric_readings), sum(value_sum), min(value_min), max(value_max),
  toString(argMinMerge(first)), toString(argMaxMerge(last))
FROM trustiot.",
            $table,
            " WHERE stream_id = {stream:String}
GROUP BY device_id, point, bucket, received_day ORDER BY ALL FORMAT TSV"
        )
    };
}
const STREAM_CELLS: [&str; 2] = [stream_cells!("telemetry_1m"), stream_cells!("telemetry_1h")];

/// A tenant's cells summed per receive day, in an aggregate table.
macro_rules! day_cells {
    ($table:literal) => {
        concat!(
            "SELECT toString(received_day), sum(readings), sum(numeric_readings),
  round(sum(value_sum), 6)
FROM trustiot.",
            $table,
            " WHERE tenant_id = {tenant:UUID} GROUP BY received_day ORDER BY received_day FORMAT TSV"
        )
    };
}
const DAY_CELLS: [(&str, &str); 2] = [
    ("telemetry_1m", day_cells!("telemetry_1m")),
    ("telemetry_1h", day_cells!("telemetry_1h")),
];
/// A tenant's raw rows per receive day.
const RAW_DAYS: &str = "SELECT toString(toDate(received_at)), count()
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} GROUP BY 1 ORDER BY 1 FORMAT TSV";

/// The runbook's steps around the backfill views (docs/runbooks/migrations.md): name the
/// account that reads them, check a range of receive days, and name nobody again.
const SHOW_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO CURRENT_USER";
const HIDE_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO NONE";
macro_rules! check {
    ($view:literal) => {
        concat!(
            "SELECT toString(received_day), count(), sum(readings)
FROM trustiot.",
            $view,
            "(first_day = {first_day:Date}, last_day = {last_day:Date},
    raw_retention_days = {raw_retention_days:UInt32})
GROUP BY received_day ORDER BY received_day FORMAT TSV"
        )
    };
}
const CHECKS: [(&str, &str); 2] = [
    ("telemetry_1m_backfill", check!("telemetry_1m_backfill")),
    ("telemetry_1h_backfill", check!("telemetry_1h_backfill")),
];
/// Every receive day a Date holds; the views leave out what the raw retention does not allow.
const ALL_DAYS: [(&str, &str); 2] = [
    ("param_first_day", "1970-01-01"),
    ("param_last_day", "2149-06-06"),
];
/// The statements the migration Job runs (they add the cells the aggregates lack), and the
/// runbook's repair (every difference).
const BACKFILLS: [&str; 2] = [BACKFILL_1M, BACKFILL_1H];
const REPAIRS: [&str; 2] = [REPAIR_1M, REPAIR_1H];
/// The cells of one receive day go, as a lost part or a failed backfill would leave them.
const DELETE_DAY_CELLS: [&str; 2] = [
    "ALTER TABLE trustiot.telemetry_1m DELETE WHERE received_day = {day:Date}
SETTINGS mutations_sync = 2",
    "ALTER TABLE trustiot.telemetry_1h DELETE WHERE received_day = {day:Date}
SETTINGS mutations_sync = 2",
];
/// The raw TTL the files create (0002_telemetry.sql), and a short one under which rows of the
/// backfill step expire.
const RAW_TTL_DAYS: &str = "400";
const SHORT_RAW_TTL_DAYS: &str = "10";
const SET_RAW_TTL: &str = "ALTER TABLE trustiot.telemetry
MODIFY TTL toDateTime(received_at) + toIntervalDay({days:UInt32})";

const MERGE_TELEMETRY: &str = "OPTIMIZE TABLE trustiot.telemetry FINAL";
const MERGES: [&str; 2] = [
    "OPTIMIZE TABLE trustiot.telemetry_1m FINAL",
    "OPTIMIZE TABLE trustiot.telemetry_1h FINAL",
];
const KEPT_DAYS: [(&str, &str); 2] = [
    (
        "telemetry_1m",
        "SELECT DISTINCT toString(received_day) FROM trustiot.telemetry_1m
WHERE tenant_id = {tenant:UUID} ORDER BY 1 FORMAT TSV",
    ),
    (
        "telemetry_1h",
        "SELECT DISTINCT toString(received_day) FROM trustiot.telemetry_1h
WHERE tenant_id = {tenant:UUID} ORDER BY 1 FORMAT TSV",
    ),
];

const DETACH_HOUR_VIEW: &str = "DETACH TABLE trustiot.telemetry_1h_mv";
const ATTACH_HOUR_VIEW: &str = "ATTACH TABLE trustiot.telemetry_1h_mv";

/// An attempt's INSERT that runs until it is stopped (ten minutes at most): one row a second
/// from system.numbers, each written as a block of its own with [`SLOW_INSERT_BLOCKS`], so its
/// rows and their view parts land while it runs, as the blocks of a large batch would.
const SLOW_INSERT: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
    batch_seq, device_id, point, ts, received_at, value_type, value_float)
SELECT {tenant:UUID}, {gateway:UUID}, {stream:UUID}, {seq:UInt64}, {device:UUID}, 'slow',
    now64(3, 'UTC') - toIntervalSecond(number), now64(3, 'UTC'), 'float', toFloat64(number)
FROM system.numbers WHERE sleepEachRow(1) = 0 LIMIT 600";
const SLOW_INSERT_BLOCKS: [(&str, &str); 3] = [
    ("max_block_size", "1"),
    ("min_insert_block_size_rows", "1"),
    ("min_insert_block_size_bytes", "1"),
];
/// The point the slow INSERT writes, and the reading times its rows can have.
const SLOW_POINT: &str = "slow";
/// How many rows the slow INSERT lands before the test replaces it.
const SLOW_ROWS_FIRST: u64 = 3;
const SLOW_SPAN: TimeDelta = TimeDelta::minutes(11);
const RUNNING: &str =
    "SELECT count() FROM system.processes WHERE query_id = {id:String} FORMAT TSV";
const PROBE: &str = "SELECT 1 FORMAT TSV";
/// Makes every buffered asynchronous insert land now: rows of the stopped INSERT, had it been
/// asynchronous, would show after this.
const FLUSH_ASYNC_INSERTS: &str = "SYSTEM FLUSH ASYNC INSERT QUEUE";
/// How long the query ID step waits for the slow INSERT to show in system.processes and land
/// its first rows.
const START_WAIT: Duration = Duration::from_secs(120);
/// How close to midnight (UTC) the backfill step may start: it names receive days by their
/// distance from today, on the server and here, so it waits for the next day instead.
const MIDNIGHT_MARGIN: TimeDelta = TimeDelta::minutes(15);

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

    /// The contract's settings for every statement that writes this stream.
    fn settings(self) -> [(&'static str, String); 5] {
        stream_settings(&self.gateway.to_string(), &self.stream.to_string())
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

/// Noon (UTC) `days` days before today.
fn noon(days: u64) -> DateTime<Utc> {
    (Utc::now().date_naive() - Days::new(days))
        .and_hms_opt(12, 0, 0)
        .expect("noon")
        .and_utc()
}

/// The start of the UTC day `days` days before today, plus `ms` milliseconds.
fn day_start(days: u64, ms: i64) -> DateTime<Utc> {
    (Utc::now().date_naive() - Days::new(days))
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        + TimeDelta::milliseconds(ms)
}

fn tenant_id(account: &TenantAccount) -> Uuid {
    Uuid::parse_str(account.tenant).expect("a tenant UUID")
}

impl Row {
    /// The row as one line of `JSONEachRow`.
    fn json(&self) -> String {
        let (kind, float, int, flag, text) = match &self.value {
            Value::Float(value) => ("float", *value, 0, false, ""),
            Value::Int(value) => ("int", 0.0, *value, false, ""),
            Value::Bool(value) => ("bool", 0.0, 0, *value, ""),
            Value::Text(value) => ("string", 0.0, 0, false, *value),
        };
        serde_json::json!({
            "tenant_id": self.on.tenant.to_string(),
            "gateway_id": self.on.gateway.to_string(),
            "stream_id": self.on.stream.to_string(),
            "batch_seq": self.seq,
            "device_id": self.on.device.to_string(),
            "point": self.point,
            "ts": time(self.ts),
            "received_at": time(self.received_at),
            "value_type": kind,
            "value_float": float,
            "value_int": int,
            "value_bool": flag,
            "value_string": text,
        })
        .to_string()
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

/// The servers and the ingest service's account.
struct Test {
    ch: ClickHouse,
    ingest: Account,
}

fn password() -> String {
    Uuid::new_v4().simple().to_string()
}

/// TSV lines as rows of fields.
fn table(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(|line| line.split('\t').map(str::to_owned).collect())
        .collect()
}

impl Test {
    async fn new(ch: ClickHouse) -> Self {
        let ingest = Account {
            user: INGEST_USER.to_owned(),
            password: password(),
        };
        ch.query_with(
            &ch.admin,
            CREATE_INGEST,
            &[("param_password", ingest.password.as_str())],
        )
        .await
        .unwrap_or_else(|error| panic!("create the ingest account: {}", error.body));
        ch.assert_default_role(INGEST_USER, "trustiot_ingest").await;
        ch.admin_ok(GRANT_NUMBERS).await;
        Self { ch, ingest }
    }

    /// The account of one of the test's tenants.
    async fn tenant(&self, account: &TenantAccount) -> Account {
        let tenant = Account {
            user: account.user.to_owned(),
            password: password(),
        };
        self.ch
            .query_with(
                &self.ch.admin,
                account.create,
                &[("param_password", tenant.password.as_str())],
            )
            .await
            .unwrap_or_else(|error| panic!("create {}: {}", account.user, error.body));
        self.ch
            .assert_default_role(account.user, "trustiot_tenant_reader")
            .await;
        tenant
    }

    /// Inserts `rows`, all of one stream, as one block, as the ingest service: with the
    /// contract's settings for the stream and `settings`.
    async fn insert(&self, rows: &[Row], settings: &[(&str, &str)]) -> Answer {
        let on = rows.first().expect("a batch").on;
        assert!(
            rows.iter().all(|row| row.on.stream == on.stream),
            "one INSERT writes one stream"
        );
        let stream = on.settings();
        let mut params: Vec<(&str, &str)> = stream
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        params.extend_from_slice(settings);
        let data: Vec<String> = rows.iter().map(Row::json).collect();
        self.ch
            .insert_data(&self.ingest, INSERT_ROWS, data.join("\n"), &params)
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
        let stream = on.stream.to_string();
        let seq = seq.to_string();
        let settings = on.settings();
        let mut params = vec![
            ("param_stream", stream.as_str()),
            ("param_seq", seq.as_str()),
            ("lightweight_deletes_sync", "2"),
        ];
        params.extend(settings.iter().map(|(name, value)| (*name, value.as_str())));
        self.ch
            .query_with(&self.ingest, DELETE_BATCH, &params)
            .await
            .unwrap_or_else(|error| panic!("delete: {}", error.body));
    }

    /// The raw rows of a batch, as the ingest service counts them.
    async fn raw_rows(&self, on: Stream, seq: u64) -> u64 {
        let stream = on.stream.to_string();
        let seq = seq.to_string();
        self.ch
            .query_with(
                &self.ingest,
                BATCH_ROWS,
                &[
                    ("param_stream", stream.as_str()),
                    ("param_seq", seq.as_str()),
                ],
            )
            .await
            .unwrap_or_else(|error| panic!("count: {}", error.body))
            .trim()
            .parse()
            .expect("a count")
    }

    /// Runs one reconcile statement for the batch `rows`, as the ingest service, with the
    /// contract's settings for the stream; the rows it wrote.
    async fn reconcile(&self, statement: &str, rows: &[Row]) -> u64 {
        self.reconcile_within(statement, rows, RECONCILE_WINDOW_DAYS)
            .await
    }

    /// [`Test::reconcile`] reaching back `window_days` receive days instead of the contract's
    /// window.
    async fn reconcile_within(&self, statement: &str, rows: &[Row], window_days: u32) -> u64 {
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
        let mut pairs = params.to_query_pairs();
        for (name, value) in &mut pairs {
            if *name == "param_window_days" {
                *value = window_days.to_string();
            }
        }
        let settings = first.on.settings();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .chain(settings.iter())
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
        for (index, (view, query, bucket_ms)) in VIEWS.into_iter().enumerate() {
            let expected = expected_cells(&rows, bucket_ms);
            let answer = self
                .ch
                .query(tenant, query)
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
        let stream = on.stream.to_string();
        let mut text = String::new();
        for query in STREAM_CELLS {
            text.push_str(
                &self
                    .ch
                    .query_with(&self.ingest, query, &[("param_stream", stream.as_str())])
                    .await
                    .unwrap_or_else(|error| panic!("stream cells: {}", error.body)),
            );
        }
        text
    }

    /// A tenant's cells per receive day in each aggregate table, as the ingest service reads
    /// them: table, day, readings, numeric readings, sum.
    async fn day_cells(&self, tenant: Uuid) -> Vec<Vec<String>> {
        let tenant = tenant.to_string();
        let mut rows = Vec::new();
        for (table, query) in DAY_CELLS {
            let answer = self
                .ch
                .query_with(&self.ingest, query, &[("param_tenant", tenant.as_str())])
                .await
                .unwrap_or_else(|error| panic!("{table}: {}", error.body));
            rows.extend(table_rows(table, &answer));
        }
        rows
    }

    /// A tenant's raw rows per receive day.
    async fn raw_days(&self, tenant: Uuid) -> BTreeMap<String, u64> {
        let tenant = tenant.to_string();
        let answer = self
            .ch
            .query_with(&self.ingest, RAW_DAYS, &[("param_tenant", tenant.as_str())])
            .await
            .unwrap_or_else(|error| panic!("raw days: {}", error.body));
        table(&answer)
            .into_iter()
            .map(|row| (row[0].clone(), row[1].parse().expect("a count")))
            .collect()
    }

    /// The backfill views' differences per receive day, as the admin reads them with the
    /// runbook's check: view, day, cells, readings.
    async fn check(&self, raw_retention_days: &str) -> Vec<Vec<String>> {
        let mut params = ALL_DAYS.to_vec();
        params.push(("param_raw_retention_days", raw_retention_days));
        let mut rows = Vec::new();
        for (view, query) in CHECKS {
            let answer = self
                .ch
                .query_with(&self.ch.admin, query, &params)
                .await
                .unwrap_or_else(|error| panic!("check {view}: {}", error.body));
            rows.extend(table_rows(view, &answer));
        }
        rows
    }

    /// The one-minute and the one-hour statement of [`BACKFILLS`] or [`REPAIRS`] as the admin,
    /// over every receive day the views allow with `raw_retention_days`: the rows each wrote.
    async fn insert_differences(
        &self,
        statements: [&str; 2],
        raw_retention_days: &str,
    ) -> (u64, u64) {
        let mut params = ALL_DAYS.to_vec();
        params.push(("param_raw_retention_days", raw_retention_days));
        let mut written = [0; 2];
        for (count, statement) in written.iter_mut().zip(statements) {
            let answer = self
                .ch
                .query_with_summary(&self.ch.admin, statement, &params)
                .await;
            if let Err(error) = &answer.result {
                panic!("{statement}\n{}", error.body);
            }
            *count = answer
                .written_rows
                .expect("the summary names the rows written");
        }
        (written[0], written[1])
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

/// TSV rows prefixed with `name`.
fn table_rows(name: &str, answer: &str) -> Vec<Vec<String>> {
    table(answer)
        .into_iter()
        .map(|mut row| {
            row.insert(0, name.to_owned());
            row
        })
        .collect()
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

/// The readings per receive day of the aggregate rows `cells` (from [`Test::day_cells`]) for
/// one table.
fn readings_by_day(cells: &[Vec<String>], table: &str) -> BTreeMap<String, u64> {
    cells
        .iter()
        .filter(|row| row[0] == table)
        .map(|row| (row[1].clone(), row[2].parse().expect("readings")))
        .filter(|(_, readings)| *readings > 0)
        .collect()
}

/// Rows written before the aggregates existed are backfilled by the statements the migration
/// Job runs; a second backfill writes nothing; rows written after reach the aggregates through
/// the views. Then the bound of the backfill views: after raw rows of older receive days
/// expired, nothing the backfill, the check or the repair does reaches those days, the cut-off
/// day included, while within the bound a cell whose raw rows were deleted is corrected.
async fn backfill_bounded_by_raw_retention(test: &Test, rollup: &Migration) {
    let midnight = (Utc::now().date_naive() + Days::new(1))
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();
    if midnight - Utc::now() < MIDNIGHT_MARGIN {
        let wait = (midnight - Utc::now() + TimeDelta::seconds(5))
            .to_std()
            .unwrap_or_default();
        println!("backfill: waiting {wait:?} for the UTC date to change");
        tokio::time::sleep(wait).await;
    }
    let today = Utc::now().date_naive();
    let now = Utc::now();
    let tenant = tenant_id(&BACKFILL_TENANT);
    let device = Uuid::new_v4();
    let a = Stream::new(tenant, device);
    let b = Stream::new(tenant, device);
    let hour = floor(now, HOUR_MS);
    // Within the bound of a 10-day raw retention (no day before today - 8): 8, 3 and 1 days
    // ago, an hour ago, and a backlog of three-day-old readings received now. Outside it: 9
    // days ago (the margin), the cut-off day 10 days ago (rows at its first and its last
    // millisecond) and 12 days ago.
    let batches = [
        readings(a, 1, noon(8), noon(8), 8),
        readings(a, 2, noon(3), noon(3), 8),
        readings(b, 1, noon(1), noon(1), 8),
        readings(
            b,
            2,
            hour - TimeDelta::hours(2) + TimeDelta::minutes(55),
            now - TimeDelta::hours(1),
            24,
        ),
        readings(a, 3, noon(3) + TimeDelta::minutes(30), now, 12),
        readings(b, 3, noon(9), noon(9), 8),
        readings(a, 4, day_start(10, 0), day_start(10, 0), 4),
        readings(a, 5, noon(10), day_start(10, 86_399_999), 4),
        readings(b, 4, noon(12), noon(12), 8),
    ];
    for batch in &batches {
        test.insert_ok(batch, &[]).await;
    }

    // The file writes nothing; the backfill views show nothing to the admin, who applied it.
    assert_eq!(
        apply(&test.ch, rollup).await,
        0,
        "{ROLLUP_FILE} inserted rows"
    );
    assert!(
        test.check(RAW_TTL_DAYS).await.is_empty(),
        "the backfill views must show nothing to an account their policy does not name"
    );
    test.ch.admin_ok(SHOW_BACKFILL_VIEWS).await;
    let pending = test.check(RAW_TTL_DAYS).await;
    assert!(!pending.is_empty(), "the rows before 0010 are not counted");
    let (minute, hour_rows) = test.insert_differences(BACKFILLS, RAW_TTL_DAYS).await;
    assert!(minute > 0 && hour_rows > 0, "the backfill wrote nothing");
    let account = test.tenant(&BACKFILL_TENANT).await;
    test.assert_equal(&account, "backfill of rows written before 0010")
        .await;
    assert_eq!(
        test.insert_differences(BACKFILLS, RAW_TTL_DAYS).await,
        (0, 0),
        "a second backfill wrote aggregate rows"
    );
    test.ch.admin_ok(HIDE_BACKFILL_VIEWS).await;
    println!(
        "backfill: the backfill views showed the admin nothing until it named itself, then \
         {} receive days to fill; the backfill wrote {minute} one-minute and {hour_rows} \
         one-hour rows, a second one 0 and 0",
        pending.len() / 2
    );

    test.insert_ok(&readings(b, 5, hour - TimeDelta::minutes(5), now, 16), &[])
        .await;
    test.assert_equal(&account, "rows written after 0010").await;

    // The raw rows of the older days expire: a 10-day raw TTL, applied by a merge. Day 12 goes,
    // day 10 keeps only the rows of its last millisecond; day 9 stays.
    test.ch
        .query_with(
            &test.ch.admin,
            SET_RAW_TTL,
            &[("param_days", SHORT_RAW_TTL_DAYS), ("mutations_sync", "2")],
        )
        .await
        .unwrap_or_else(|error| panic!("shorten the raw TTL: {}", error.body));
    test.ch.admin_ok(MERGE_TELEMETRY).await;
    let day = |days: u64| (Utc::now().date_naive() - Days::new(days)).to_string();
    let raw = test.raw_days(tenant).await;
    assert_eq!(raw.get(&day(12)), None, "day 12's raw rows expired");
    assert_eq!(
        raw.get(&day(10)),
        Some(&4),
        "the cut-off day keeps its last rows"
    );
    assert_eq!(raw.get(&day(9)), Some(&8), "day 9's raw rows are kept");
    let before = test.day_cells(tenant).await;
    let minute_before = readings_by_day(&before, "telemetry_1m");
    assert_eq!(minute_before.get(&day(10)), Some(&8));
    assert_eq!(minute_before.get(&day(12)), Some(&8));

    // The file is applied again and the backfill runs again, as after a lost
    // trustiot_migrate.applied, with the raw retention the deployment has: nothing changes. It
    // only adds cells the aggregates lack, so even too long a retention takes nothing away.
    assert_eq!(
        apply(&test.ch, rollup).await,
        0,
        "{ROLLUP_FILE} inserted rows"
    );
    test.ch.admin_ok(SHOW_BACKFILL_VIEWS).await;
    for raw_retention_days in [SHORT_RAW_TTL_DAYS, RAW_TTL_DAYS] {
        assert_eq!(
            test.insert_differences(BACKFILLS, raw_retention_days).await,
            (0, 0),
            "the backfill after a lost record with {raw_retention_days} days changed the \
             aggregates"
        );
    }
    assert_eq!(test.day_cells(tenant).await, before);

    // The runbook's check reports nothing for the expired days; with too long a retention it
    // would offer to erase them (negative readings), which is why the views need the bound.
    assert_eq!(
        test.check(SHORT_RAW_TTL_DAYS).await,
        Vec::<Vec<String>>::new()
    );
    let unbounded = test.check(RAW_TTL_DAYS).await;
    let erased: BTreeSet<&str> = unbounded.iter().map(|row| row[1].as_str()).collect();
    assert_eq!(
        erased,
        BTreeSet::from([day(10).as_str(), day(12).as_str()]),
        "{unbounded:?}"
    );
    assert!(
        unbounded
            .iter()
            .all(|row| row[3].parse::<i64>().expect("readings") < 0),
        "{unbounded:?}"
    );
    // The repair with the bound leaves them as they are; the cut-off day keeps both its rows.
    assert_eq!(
        test.insert_differences(REPAIRS, SHORT_RAW_TTL_DAYS).await,
        (0, 0)
    );
    let after = test.day_cells(tenant).await;
    assert_eq!(
        after, before,
        "the repair changed the cells of expired days"
    );
    println!(
        "bound: after the raw rows of days 12 and 10 (part of it) expired, a second apply and a \
         backfill with a 10-day or a 400-day raw retention wrote nothing, the check with 10 days \
         reported nothing and the repair with 10 days wrote nothing; the same check with 400 \
         days would have erased {:?}; the cells per receive day stayed {:?}",
        unbounded,
        readings_by_day(&after, "telemetry_1m")
    );

    // Within the bound the views still correct a cell whose raw rows are gone: a batch deleted
    // without its reconcile (I1) is taken out by the repair.
    test.delete_batch(a, 2).await;
    let deleted = test.check(SHORT_RAW_TTL_DAYS).await;
    assert_eq!(
        deleted
            .iter()
            .map(|row| (row[1].as_str(), row[3].as_str()))
            .collect::<Vec<_>>(),
        [(day(3).as_str(), "-8"), (day(3).as_str(), "-8")],
        "{deleted:?}"
    );
    assert_eq!(
        test.insert_differences(BACKFILLS, SHORT_RAW_TTL_DAYS).await,
        (0, 0),
        "the backfill changed cells the aggregates hold"
    );
    let (minute, hour_rows) = test.insert_differences(REPAIRS, SHORT_RAW_TTL_DAYS).await;
    assert!(minute > 0 && hour_rows > 0, "the repair wrote nothing");
    assert_eq!(
        test.check(SHORT_RAW_TTL_DAYS).await,
        Vec::<Vec<String>>::new()
    );

    // Cells the aggregates lack within the bound, as a failed backfill leaves them: the
    // backfill fills them.
    let day_8 = day(8);
    for statement in DELETE_DAY_CELLS {
        test.ch
            .query_with(&test.ch.admin, statement, &[("param_day", day_8.as_str())])
            .await
            .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
    }
    let lost = test.check(SHORT_RAW_TTL_DAYS).await;
    assert_eq!(
        lost.iter()
            .map(|row| (row[1].as_str(), row[3].as_str()))
            .collect::<Vec<_>>(),
        [(day_8.as_str(), "8"), (day_8.as_str(), "8")],
        "{lost:?}"
    );
    let (minute, hour_rows) = test.insert_differences(BACKFILLS, SHORT_RAW_TTL_DAYS).await;
    assert!(minute > 0 && hour_rows > 0, "the backfill filled nothing");
    assert_eq!(
        test.check(SHORT_RAW_TTL_DAYS).await,
        Vec::<Vec<String>>::new()
    );
    test.ch.admin_ok(HIDE_BACKFILL_VIEWS).await;
    let raw = test.raw_days(tenant).await;
    let repaired = test.day_cells(tenant).await;
    for table in ["telemetry_1m", "telemetry_1h"] {
        let cells = readings_by_day(&repaired, table);
        let previous = readings_by_day(&before, table);
        for (received, readings) in &cells {
            if *received >= day(8) {
                assert_eq!(raw.get(received), Some(readings), "{table} {received}");
            } else {
                assert_eq!(previous.get(received), Some(readings), "{table} {received}");
            }
        }
        assert_eq!(cells.get(&day(3)), raw.get(&day(3)), "{table}");
    }
    println!(
        "bound: within it, the cells of a batch deleted without its reconcile were offered as \
         {deleted:?}, left alone by the backfill and corrected by the repair; the lost cells of \
         {day_8} were offered as {lost:?} and filled by the backfill; the expired days kept their \
         cells"
    );

    test.ch
        .query_with(&test.ch.admin, SET_RAW_TTL, &[("param_days", RAW_TTL_DAYS)])
        .await
        .unwrap_or_else(|error| panic!("restore the raw TTL: {}", error.body));
    assert_eq!(
        Utc::now().date_naive(),
        today,
        "the UTC date changed during the backfill step, which names receive days by their \
         distance from today: run the test again"
    );
}

/// An INSERT retried unchanged (its acknowledgement was lost) is dropped by the raw table and by
/// both aggregate tables.
async fn exact_retry_counts_once(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(tenant_id(&RETRY_TENANT), Uuid::new_v4());
    let batch = readings(on, 1, floor(now, HOUR_MS) - TimeDelta::minutes(5), now, 20);
    test.insert_ok(&batch, &[]).await;
    test.insert_ok(&batch, &[]).await;
    assert_eq!(test.raw_rows(on, 1).await, 20, "the retry was not dropped");
    let account = test.tenant(&RETRY_TENANT).await;
    test.assert_equal(&account, "exact retry").await;
}

/// The I1 rewrite after a partial attempt: the DELETE does not reach the aggregates, the
/// reconcile corrects them, even when it stopped after its first statement, and it leaves
/// another stream's cells as they were.
async fn rewrite_after_a_partial_attempt(test: &Test) {
    let now = Utc::now();
    let tenant = tenant_id(&REWRITE_TENANT);
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
    let account = test.tenant(&REWRITE_TENANT).await;

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
    let tenant = tenant_id(&HAZARD_TENANT);
    let device = Uuid::new_v4();
    let start = floor(now, HOUR_MS) - TimeDelta::minutes(3);
    let account = test.tenant(&HAZARD_TENANT).await;
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

/// The reconcile window (owner decision of 2026-10-26): a batch whose first, partial attempt was
/// received at 23:59 on the day [`RECONCILE_WINDOW_DAYS`] days ago is rewritten today, after the
/// midnight that ended the 30th day after it. A window of the gateway buffer's 30 days does not
/// reach the first attempt's receive day, and its cells stay counted twice; the contract's
/// window of 31 days does.
async fn a_rewrite_after_the_buffer_age_reaches_the_first_attempt(test: &Test) {
    let today = Utc::now().date_naive();
    // 23:59 on the receive day of the first attempt.
    let first_received = day_start(u64::from(RECONCILE_WINDOW_DAYS), 86_340_000);
    let on = Stream::new(tenant_id(&WINDOW_TENANT), Uuid::new_v4());
    let account = test.tenant(&WINDOW_TENANT).await;
    let batch = readings(
        on,
        9,
        first_received - TimeDelta::minutes(10),
        first_received,
        12,
    );

    // Attempt 1: the first half of the batch reached ClickHouse, then the connection failed.
    test.insert_ok(&batch[..6], &[]).await;
    // The rewrite today: the batch's rows go, and the whole batch comes again, received now.
    test.delete_batch(on, 9).await;
    let now = Utc::now();
    let resent: Vec<Row> = batch
        .iter()
        .cloned()
        .map(|row| Row {
            received_at: now,
            ..row
        })
        .collect();
    test.insert_ok(&resent, &REWRITE_SETTINGS).await;
    let (problems, _) = test.differences(&account).await;
    assert!(
        !problems.is_empty(),
        "the aggregates should still count the first attempt before the reconcile"
    );

    // A window of the buffer's age leaves the first attempt's cells as they are.
    let short = (
        test.reconcile_within(RECONCILE_1M, &resent, BUFFER_MAX_AGE_DAYS)
            .await,
        test.reconcile_within(RECONCILE_1H, &resent, BUFFER_MAX_AGE_DAYS)
            .await,
    );
    let (still, _) = test.differences(&account).await;
    // The contract's window reaches them.
    let written = test.reconcile_both(&resent).await;
    assert_eq!(
        Utc::now().date_naive(),
        today,
        "the UTC date changed during the step, which names the first attempt's receive day by \
         its distance from today: run the test again"
    );
    assert_eq!(
        short,
        (0, 0),
        "a 30-day window should not reach the first attempt"
    );
    assert_eq!(
        still, problems,
        "a 30-day window should leave the first attempt's cells counted twice"
    );
    assert!(
        written.0 > 0 && written.1 > 0,
        "the contract's window wrote {written:?}"
    );
    test.assert_equal(&account, "rewrite a day after the buffer's age")
        .await;
    println!(
        "reconcile window: a batch first received at {} and rewritten at {}: with a window of \
         {BUFFER_MAX_AGE_DAYS} receive days the reconcile wrote {short:?} rows and {} cells \
         still differed; with the contract's {RECONCILE_WINDOW_DAYS} days it wrote {} \
         one-minute and {} one-hour rows and the aggregates equal the raw rows",
        time(first_received),
        time(now),
        still.len(),
        written.0,
        written.1
    );
}

/// A crash between the raw part of an INSERT and a view's part (here the one-hour view is
/// detached during attempt 1): after the rewrite the one-minute cells count attempt 1 twice
/// and the one-hour cells once; the reconcile corrects the first and writes nothing for the
/// second.
async fn crash_between_the_raw_and_a_view_part(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(tenant_id(&CRASH_TENANT), Uuid::new_v4());
    let batch = readings(on, 3, floor(now, HOUR_MS) - TimeDelta::minutes(2), now, 16);
    let account = test.tenant(&CRASH_TENANT).await;
    test.ch.admin_ok(DETACH_HOUR_VIEW).await;
    test.insert_ok(&batch, &[]).await;
    test.ch.admin_ok(ATTACH_HOUR_VIEW).await;
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

/// Step 1 of the contract on ClickHouse 26.8: while an INSERT of an earlier attempt still runs
/// with the stream's settings (its client gave up on it), another statement with the stream's
/// query ID is refused, unless it has `replace_running_query = 1` as `stream_settings` gives it:
/// then the running INSERT is cancelled and the new statement runs. The rows the INSERT landed
/// before it stopped stay, and none land after the replacing statement has run: it is
/// synchronous (`async_insert = 0`), so nothing of it waits in the asynchronous insert queue.
async fn an_earlier_statement_of_the_stream_is_stopped(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(tenant_id(&QUERY_ID_TENANT), Uuid::new_v4());
    let account = test.tenant(&QUERY_ID_TENANT).await;
    let batch = readings(on, 1, floor(now, HOUR_MS) - TimeDelta::minutes(4), now, 12);
    test.insert_ok(&batch, &[]).await;
    let id = stream_query_id(&on.gateway.to_string(), &on.stream.to_string());

    // Attempt 2's INSERT, with the stream's settings. Its time limit is lengthened so that the
    // replacement ends it, not the limit, however long a loaded machine takes to get there.
    let stream = on.settings();
    let (tenant, gateway, stream_id, device) = (
        on.tenant.to_string(),
        on.gateway.to_string(),
        on.stream.to_string(),
        on.device.to_string(),
    );
    let mut slow_settings: Vec<(&str, &str)> = stream
        .iter()
        .map(|(name, value)| {
            if *name == "max_execution_time" {
                (*name, "600")
            } else {
                (*name, value.as_str())
            }
        })
        .collect();
    assert!(slow_settings.contains(&("async_insert", "0")));
    slow_settings.extend(SLOW_INSERT_BLOCKS);
    slow_settings.extend([
        ("param_tenant", tenant.as_str()),
        ("param_gateway", gateway.as_str()),
        ("param_stream", stream_id.as_str()),
        ("param_seq", "2"),
        ("param_device", device.as_str()),
        // The answer waits for the end, so the cancellation comes back as an error status.
        ("wait_end_of_query", "1"),
    ]);
    let slow = test
        .ch
        .query_with(&test.ingest, SLOW_INSERT, &slow_settings);
    let next = async {
        let started = Instant::now();
        loop {
            let running = test
                .ch
                .query_with(&test.ch.admin, RUNNING, &[("param_id", id.as_str())])
                .await
                .unwrap_or_else(|error| panic!("system.processes: {}", error.body));
            if running.trim() == "1" && test.raw_rows(on, 2).await >= SLOW_ROWS_FIRST {
                break;
            }
            assert!(
                started.elapsed() < START_WAIT,
                "the slow INSERT with the stream's query ID did not show in system.processes \
                 and land {SLOW_ROWS_FIRST} rows within {START_WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let refused = test
            .ch
            .query_with(&test.ingest, PROBE, &[("query_id", id.as_str())])
            .await
            .expect_err("a second statement with a running query ID must be refused");
        let written = test.reconcile_both(&batch).await;
        // The replacing statement has run: what the INSERT landed until now.
        let landed = (test.raw_rows(on, 2).await, test.stream_cells(on).await);
        (refused, written, landed)
    };
    let (slow, (refused, written, landed)) = tokio::join!(slow, next);
    assert_eq!(
        refused.code(),
        "QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING",
        "{}",
        refused.body
    );
    let slow = slow.expect_err("the slow INSERT must have been stopped");
    assert_eq!(slow.code(), "QUERY_WAS_CANCELLED", "{}", slow.body);
    assert_eq!(written, (0, 0), "the views already counted the batch");

    // Nothing of the stopped INSERT lands later: it no longer runs, and nothing of it waits in
    // the asynchronous insert queue.
    let running = test
        .ch
        .query_with(&test.ch.admin, RUNNING, &[("param_id", id.as_str())])
        .await
        .unwrap_or_else(|error| panic!("system.processes: {}", error.body));
    assert_eq!(running.trim(), "0", "the stopped INSERT still runs");
    test.ch.admin_ok(FLUSH_ASYNC_INSERTS).await;
    let later = (test.raw_rows(on, 2).await, test.stream_cells(on).await);
    assert_eq!(
        later, landed,
        "rows of the stopped INSERT landed after the replacing statement had run"
    );

    // The next attempt's reconcile of the rows it landed, then the aggregates equal the raw
    // rows.
    let span = [
        slow_row(on, Utc::now() - SLOW_SPAN),
        slow_row(on, Utc::now() + TimeDelta::minutes(1)),
    ];
    let repaired = test.reconcile_both(&span).await;
    test.assert_equal(&account, "reconcile after stopping an earlier INSERT")
        .await;
    println!(
        "query ID: while an INSERT of the stream ran with the stream's settings, another \
         statement with its ID was refused ({}); the reconcile with the stream's settings \
         stopped it ({}) after it had landed {} rows, and ran; once it had run no row landed \
         (still {} raw rows and the same cells after the asynchronous insert queue was \
         flushed); the reconcile of those rows wrote {} one-minute and {} one-hour rows",
        refused.code(),
        slow.code(),
        landed.0,
        later.0,
        repaired.0,
        repaired.1
    );
}

/// A row of the slow INSERT's point at `ts`, to name its cells to the reconcile.
fn slow_row(on: Stream, ts: DateTime<Utc>) -> Row {
    Row {
        on,
        seq: 2,
        point: SLOW_POINT,
        ts,
        received_at: ts,
        value: Value::Float(0.0),
    }
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
/// queries are exact under (at most one reset or wrap per bucket, and after it the last
/// reading stays below the peak before it); one case outside it shows the one-hour answer
/// drifting while the one-minute one, with one reading per bucket, stays exact.
async fn energy_of_cumulative_points(test: &Test) {
    let now = Utc::now();
    let on = Stream::new(tenant_id(&ENERGY_TENANT), Uuid::new_v4());
    let account = test.tenant(&ENERGY_TENANT).await;
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
    let tenant = Uuid::parse_str(RETENTION_TENANT).expect("a tenant UUID");
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
    for merge in MERGES {
        // A merge applies the TTL; FINAL merges every part.
        test.ch.admin_ok(merge).await;
    }
    let after = Utc::now();
    let tenant_text = tenant.to_string();
    let mut kept = Vec::new();
    for (table, query) in KEPT_DAYS {
        kept.push(
            test.ch
                .query_with(
                    &test.ingest,
                    query,
                    &[("param_tenant", tenant_text.as_str())],
                )
                .await
                .unwrap_or_else(|error| panic!("{table}: {}", error.body))
                .lines()
                .map(str::to_owned)
                .collect::<BTreeSet<String>>(),
        );
    }
    let (minute, hour) = (&kept[0], &kept[1]);
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
    backfill_bounded_by_raw_retention(&test, rollup).await;
    for migration in rest
        .iter()
        .filter(|migration| migration.name != ROLLUP_FILE)
    {
        apply(&test.ch, migration).await;
    }
    exact_retry_counts_once(&test).await;
    rewrite_after_a_partial_attempt(&test).await;
    a_rewrite_after_the_buffer_age_reaches_the_first_attempt(&test).await;
    rewrite_must_not_deduplicate(&test).await;
    crash_between_the_raw_and_a_view_part(&test).await;
    an_earlier_statement_of_the_stream_is_stopped(&test).await;
    energy_of_cumulative_points(&test).await;
    retention_by_receive_day(&test).await;
    println!(
        "server: {}",
        test.ch.admin_ok("SELECT version()").await.trim()
    );
}
