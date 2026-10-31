//! Measures `lab-dataset export` on a large range (Laysi/trustiot-v2#821, the advanced
//! verification deferred from Laysi/trustiot-v2#816, and Laysi/trustiot-v2#837, which changed
//! how the export reads): 30 days of one reading per minute for ten data points (two devices with
//! five points each) are generated inside a throwaway ClickHouse with the product's schema, then
//! the `lab-dataset` command exports them, and the test records
//!
//! - the peak memory of the export process (`VmHWM` of `/proc/<pid>/status`, sampled every 10 ms)
//!   and its wall time, for 7, 15 and 30 days, with the default 64 MiB segments and with 4 MiB
//!   segments, so growth with the range shows; with 4 MiB segments the 30-day peak must stay
//!   within a bound of the 7-day peak that does not depend on the range's rows read in windows
//!   (the final check, `Dataset::open`, keeps a set of every reading of 16 bytes an entry, which
//!   the bound allows);
//! - the export's statements from `system.query_log`, by kind (the rows of a window, the census
//!   of devices and points, the repeated readings, the plan, the completion records): their
//!   number, rows returned and `read_rows`; the exact length of each window's answer body (the
//!   logged statement, with its parameters' values written in, is sent again), against the HTTP
//!   client's 16 MiB limit on one answer (`migrate::http`); and `EXPLAIN indexes = 1` of a
//!   window's statement in the middle of the range;
//! - the 30-day export over `https://` with `CLICKHOUSE_CA_FILE`, whose dataset files must be
//!   those of the http export byte for byte.
//!
//! With `LAB_DATASET_BASELINE` set to a `lab-dataset` executable built from an earlier commit,
//! every range is exported by that executable too, its statements and memory are recorded the
//! same way, and its files must be those of this export byte for byte (each file's SHA-256 is
//! printed).
//!
//! It is a measurement, not a regression test: it takes minutes and several hundred MB of disk,
//! so it is ignored by default. Run it, on Linux (it reads `/proc`), with
//!
//! ```text
//! cargo test --release --locked -p lab-dataset --test export_scale -- --ignored --nocapture
//! ```
//!
//! and read the report between `SCALE-REPORT-BEGIN` and `SCALE-REPORT-END`. The data is
//! generated at run time and never committed. Always a throwaway container, never
//! `TEST_CLICKHOUSE_URL`: the test creates an account that reads every row.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "test helpers unwrap and print like the tests themselves (clippy.toml allows both in \
              tests); the printed report is the evidence"
)]

/// The ClickHouse container and its HTTP client, shared with the ingest service's schema tests.
#[path = "../../../services/ingest/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use lab_dataset::{cli, time};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use testkit::postgres::TestCa;
use uuid::Uuid;

use self::common::{Account, ClickHouse};

const TENANT: Uuid = Uuid::from_u128(0x5ca1e);
/// Not the nil UUID (`binding_id_set`).
const BINDING: Uuid = Uuid::from_u128(0xb1d);
const GATEWAY: &str = "scale-gateway";
const STREAM: &str = "stream-1";
const SENSOR_1: Uuid = Uuid::from_u128(0x5e01);
const SENSOR_2: Uuid = Uuid::from_u128(0x5e02);

/// Days of data, one batch per minute, ten readings per batch.
const DAYS: i64 = 30;
const BATCHES_PER_DAY: u64 = 24 * 60;
const ROWS_PER_BATCH: u64 = 10;

/// The HTTP client's limit on one answer (`tools/migrate/src/http.rs`, `MAX_BODY_BYTES`).
const ANSWER_LIMIT_BYTES: u64 = 16 * 1024 * 1024;

/// The small segments of the memory bound: the final check holds one segment's text and
/// readings at a time, so with the default 64 MiB segments its peak grows with the range until
/// the first segment is full.
const SMALL_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;

/// What the 30-day peak may add to the 7-day peak with small segments: the final check's set of
/// every reading (a 16-byte entry, a hash table of at most twice the entries while it grows) per
/// reading more, and a fixed allowance for the allocator.
const BYTES_PER_MORE_READING: u64 = 64;
const MEMORY_ALLOWANCE_KIB: u64 = 16 * 1024;

/// The environment variable that names a `lab-dataset` executable built from an earlier commit.
const BASELINE_ENV: &str = "LAB_DATASET_BASELINE";

const READER: &str = "lab_export_scale";
const CREATE_READER: &str = "CREATE USER lab_export_scale IDENTIFIED WITH sha256_password BY \
     {password:String} HOST ANY";
const READER_STATEMENTS: [&str; 4] = [
    "GRANT SELECT ON trustiot.telemetry TO lab_export_scale",
    "GRANT SELECT ON trustiot.stream_commits TO lab_export_scale",
    "CREATE ROW POLICY lab_export_scale ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 \
     TO lab_export_scale",
    "CREATE ROW POLICY lab_export_scale ON trustiot.stream_commits AS PERMISSIVE FOR SELECT \
     USING 1 TO lab_export_scale",
];

/// One day of batches `first` to `first + count`, as the ingest service would have written them
/// but generated by the server: batch `n` is received at `start_ms + n` minutes with two devices
/// times five points (three floats, a counter and a flag), read 400 ms before.
const GENERATE_TELEMETRY: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, \
     stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, \
     value_int, value_bool, value_string)
SELECT {tenant:UUID}, {gateway:String}, {stream:String}, n + 1, device.1, point.1,
  fromUnixTimestamp64Milli({start_ms:Int64} + toInt64(n) * 60000 - 400, 'UTC'),
  fromUnixTimestamp64Milli({start_ms:Int64} + toInt64(n) * 60000, 'UTC'),
  CAST(point.2 AS Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4)),
  if(point.2 = 'float', round(20 + 5 * sin(n / 229.0 + device.2)
    + (cityHash64(n, point.1, device.1) % 1000) / 1000.0, 3), 0),
  if(point.2 = 'int', toInt64(n * 7 + device.2 * 1000), 0),
  toBool(point.2 = 'bool' AND n % 1440 < 1000),
  ''
FROM (SELECT {first:UInt64} + number AS n FROM numbers({count:UInt64}))
ARRAY JOIN [('temperature', 'float'), ('humidity', 'float'), ('voltage', 'float'),
  ('pulses', 'int'), ('running', 'bool')] AS point
ARRAY JOIN [(toUUID('00000000-0000-0000-0000-000000005e01'), 1),
  (toUUID('00000000-0000-0000-0000-000000005e02'), 2)] AS device";

/// The completion records of the same batches (ten rows each).
const GENERATE_COMMITS: &str = "INSERT INTO trustiot.stream_commits (tenant_id, binding_id, \
     gateway_id, stream_id, seq, row_count, committed_at)
SELECT {tenant:UUID}, {binding:UUID}, {gateway:String}, {stream:String},
  {first:UInt64} + number + 1, 10,
  fromUnixTimestamp64Milli({start_ms:Int64} + toInt64({first:UInt64} + number) * 60000 + 100, 'UTC')
FROM numbers({count:UInt64})";

/// The reader's statements in a time window, from the log. The log keeps a statement with its
/// parameters' values written in; its first words tell its kind. `rows` is the statement that
/// reads a window's (or, before Laysi/trustiot-v2#837, an hour's) rows.
const LOGGED: &str = "WHERE type = 'QueryFinish' AND user = {user:String}
  AND event_time_microseconds >= fromUnixTimestamp64Micro({from_us:Int64}, 'UTC')
  AND event_time_microseconds <= fromUnixTimestamp64Micro({to_us:Int64}, 'UTC')";
const KIND: &str = "multiIf(startsWith(query, 'SELECT stream_id, batch_seq'), 'rows',
  startsWith(query, 'SELECT toString(device_id) AS device, point, count()'), 'series',
  startsWith(query, 'SELECT toUnixTimestamp64Milli(ts) AS ts_ms'), 'repeats',
  startsWith(query, 'SELECT intDiv('), 'plan',
  startsWith(query, 'SELECT seq, row_count'), 'commits', 'other')";

/// What one run of the command did.
struct Run {
    wall: Duration,
    /// `VmHWM` of the process, in KiB, the last sample before it exited.
    peak_kib: u64,
    printed: Value,
    /// When it started and ended, as ClickHouse's clock reads them (the container shares the
    /// host's clock).
    window: (DateTime<Utc>, DateTime<Utc>),
}

/// A scratch directory outside the repository, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("lab-export-scale-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("{value} is not a number"))
}

/// Smallest, median and largest of `values`.
fn spread(values: &[u64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    json!({
        "min": sorted[0],
        "median": sorted[sorted.len() / 2],
        "max": sorted[sorted.len() - 1],
    })
}

/// The export configuration: two devices of one model, five points, one stream; segments of
/// `segment_bytes` when given, else the default.
fn config_text(segment_bytes: Option<u64>) -> String {
    let segments = segment_bytes
        .map(|bytes| format!("segment_bytes = {bytes}\n"))
        .unwrap_or_default();
    format!(
        r#"
name = "scale"
description = "Scale measurement of the export"
{segments}
[[streams]]
key = "live"
id = "{STREAM}"

[[models]]
name = "environment-sensor"
points = [
  {{ key = "temperature", function_code = 4, address = 0, data_type = "f32", kind = "gauge", unit = "degC" }},
  {{ key = "humidity", function_code = 4, address = 2, data_type = "f32", kind = "gauge", unit = "percent" }},
  {{ key = "voltage", function_code = 4, address = 4, data_type = "f32", kind = "gauge", unit = "V" }},
  {{ key = "pulses", function_code = 3, address = 0, data_type = "u32", kind = "cumulative", wraps_at = 4294967296 }},
  {{ key = "running", function_code = 1, address = 0, data_type = "bool", kind = "state" }},
]

[[devices]]
key = "sensor-1"
id = "{SENSOR_1}"
model = "environment-sensor"
unit = 1
interval_s = 60

[[devices]]
key = "sensor-2"
id = "{SENSOR_2}"
model = "environment-sensor"
unit = 2
interval_s = 60
"#
    )
}

/// `VmHWM` in KiB of process `pid`, when it is `lab-dataset`: a child that has forked but not yet
/// run its program still shows the test's own memory.
fn peak_memory_kib(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut name = "";
    let mut peak = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Name:") {
            name = rest.trim();
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            peak = rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    if name == "lab-dataset" { peak } else { None }
}

/// Runs `binary export` for `[from, to)` into `out` and measures it.
async fn run_export(
    binary: &Path,
    config: &Path,
    range: (DateTime<Utc>, DateTime<Utc>),
    out: &Path,
    (url, account): (&str, &Account),
    ca_file: Option<&Path>,
) -> Run {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("export")
        .arg(config)
        .args(["--tenant", &TENANT.to_string()])
        .args(["--gateway", GATEWAY])
        .args(["--from", &time::format(range.0)])
        .args(["--to", &time::format(range.1)])
        .arg("--out")
        .arg(out)
        .env(cli::URL_ENV, url)
        .env(cli::USER_ENV, &account.user)
        .env(cli::PASSWORD_ENV, &account.password)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match ca_file {
        Some(ca_file) => command.env(cli::CA_FILE_ENV, ca_file),
        None => command.env_remove(cli::CA_FILE_ENV),
    };
    let began = Utc::now();
    let started = Instant::now();
    let child = command.spawn().expect("run lab-dataset");
    let pid = child.id().expect("the child has a process ID");
    let sampler = tokio::spawn(async move {
        let mut peak = 0;
        let mut seen = false;
        loop {
            match peak_memory_kib(pid) {
                Some(kib) => {
                    seen = true;
                    peak = peak.max(kib);
                }
                // Gone (or a zombie that has released its memory) after it was seen.
                None if seen => return peak,
                None => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let output = child.wait_with_output().await.unwrap();
    let wall = started.elapsed();
    let ended = Utc::now();
    assert!(
        output.status.success(),
        "lab-dataset export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let peak_kib = tokio::time::timeout(Duration::from_secs(30), sampler)
        .await
        .expect("the memory sampler ended after the process exited")
        .unwrap();
    assert!(peak_kib > 0, "the memory sampler never saw the process");
    Run {
        wall,
        peak_kib,
        printed: serde_json::from_slice(&output.stdout).unwrap(),
        window: (began, ended),
    }
}

/// The files of a dataset directory, relative to it, in order.
fn dataset_paths(dataset_dir: &Path) -> Vec<String> {
    let mut paths = vec!["index.toml".to_owned()];
    let mut segments: Vec<String> = fs::read_dir(dataset_dir.join("segments"))
        .unwrap()
        .map(|entry| format!("segments/{}", entry.unwrap().file_name().to_string_lossy()))
        .collect();
    segments.sort();
    paths.extend(segments);
    paths
}

/// The SHA-256 of `bytes`, in hex.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Checks that the datasets in `ours` and `theirs` have the same files, byte for byte; returns
/// each file's path, length and SHA-256.
fn compare_files(label: &str, ours: &Path, theirs: &Path) -> Vec<Value> {
    let paths = dataset_paths(ours);
    assert_eq!(paths, dataset_paths(theirs), "{label}: other files");
    paths
        .iter()
        .map(|path| {
            let bytes = fs::read(ours.join(path)).unwrap();
            assert!(
                bytes == fs::read(theirs.join(path)).unwrap(),
                "{label}: {path} differs"
            );
            json!({
                "path": path,
                "bytes": bytes.len(),
                "sha256": sha256_hex(&bytes),
            })
        })
        .collect()
}

async fn start() -> Option<(ClickHouse, String, String)> {
    let ca = TestCa::new("lab export scale CA");
    let (certificate, key) = ca.server_certificate(&["127.0.0.1"]);
    let (server, https_url) = ClickHouse::start_throwaway_with_https(&certificate, &key).await?;
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
    Some((server, https_url, ca.pem))
}

/// Writes `DAYS` days of batches ending at `end`, one INSERT per day for each table.
async fn generate(server: &ClickHouse, start_ms: i64) {
    let (tenant, binding) = (TENANT.to_string(), BINDING.to_string());
    let start_ms = start_ms.to_string();
    let count = BATCHES_PER_DAY.to_string();
    for day in 0..DAYS.unsigned_abs() {
        let first = (day * BATCHES_PER_DAY).to_string();
        let parameters = [
            ("param_tenant", tenant.as_str()),
            ("param_binding", binding.as_str()),
            ("param_gateway", GATEWAY),
            ("param_stream", STREAM),
            ("param_start_ms", start_ms.as_str()),
            ("param_first", first.as_str()),
            ("param_count", count.as_str()),
        ];
        for statement in [GENERATE_TELEMETRY, GENERATE_COMMITS] {
            server
                .query_with(&server.admin, statement, &parameters)
                .await
                .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
        }
    }
}

/// Runs a log statement for the reader's statements in `window`; returns its JSON lines.
async fn log_lines(
    server: &ClickHouse,
    statement: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Vec<Value> {
    let micros = |at: DateTime<Utc>| at.timestamp_micros().to_string();
    let (from, to) = (micros(window.0), micros(window.1));
    let parameters = [
        ("param_user", READER),
        ("param_from_us", from.as_str()),
        ("param_to_us", to.as_str()),
    ];
    server
        .query_with(&server.admin, statement, &parameters)
        .await
        .unwrap_or_else(|error| panic!("{statement}\n{}", error.body))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The reader's statements in `window` by kind: number, rows returned, rows read.
async fn statement_kinds(server: &ClickHouse, window: (DateTime<Utc>, DateTime<Utc>)) -> Value {
    let statement = format!(
        "SELECT {KIND} AS kind, count() AS queries, sum(result_rows) AS rows_returned, \
         sum(read_rows) AS rows_read, min(read_rows) AS rows_read_min, \
         quantileExact(0.5)(read_rows) AS rows_read_median, max(read_rows) AS rows_read_max, \
         max(memory_usage) AS server_memory_max \
         FROM system.query_log {LOGGED} GROUP BY kind ORDER BY kind FORMAT JSONEachRow"
    );
    let mut kinds = serde_json::Map::new();
    let mut read_total = 0;
    for mut line in log_lines(server, &statement, window).await {
        let kind = line["kind"].as_str().unwrap().to_owned();
        assert_ne!(kind, "other", "a statement of an unknown kind: {line}");
        read_total += number(&line["rows_read"]);
        let object = line.as_object_mut().unwrap();
        object.remove("kind");
        for value in object.values_mut() {
            *value = json!(number(value));
        }
        kinds.insert(kind, line);
    }
    kinds.insert("rows_read_total".to_owned(), json!(read_total));
    Value::Object(kinds)
}

/// The rows statements of the reader in `window`, in order, as the log keeps them.
async fn rows_statements(
    server: &ClickHouse,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Vec<String> {
    let statement = format!(
        "SELECT query FROM system.query_log {LOGGED} \
         AND startsWith(query, 'SELECT stream_id, batch_seq') \
         ORDER BY event_time_microseconds FORMAT JSONEachRow"
    );
    log_lines(server, &statement, window)
        .await
        .into_iter()
        .map(|line| line["query"].as_str().unwrap().to_owned())
        .collect()
}

/// Sends `statement` as the reader; returns the answer's body.
async fn send(http: &reqwest::Client, url: &str, reader: &Account, statement: &str) -> Vec<u8> {
    http.post(url)
        .header("X-ClickHouse-User", &reader.user)
        .header("X-ClickHouse-Key", &reader.password)
        .body(statement.to_owned())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

/// The answer body lengths of the rows statements, sent again, and `EXPLAIN indexes = 1` of the
/// one in the middle; checks that the answers hold `expected_rows` lines, each under the limit.
async fn answers(
    http: &reqwest::Client,
    url: &str,
    reader: &Account,
    statements: &[String],
    expected_rows: u64,
) -> (Value, String) {
    let mut body_bytes = Vec::new();
    let mut body_rows = 0_u64;
    for statement in statements {
        let body = send(http, url, reader, statement).await;
        body_bytes.push(u64::try_from(body.len()).unwrap());
        body_rows += u64::try_from(body.iter().filter(|byte| **byte == b'\n').count()).unwrap();
    }
    assert_eq!(body_rows, expected_rows);
    let total: u64 = body_bytes.iter().sum();
    let largest = *body_bytes.iter().max().unwrap();
    assert!(
        largest < ANSWER_LIMIT_BYTES,
        "an answer of {largest} bytes reaches the {ANSWER_LIMIT_BYTES}-byte limit"
    );
    let middle = &statements[statements.len() / 2];
    let explain = String::from_utf8(
        send(
            http,
            url,
            reader,
            &format!(
                "EXPLAIN indexes = 1 {}",
                middle.trim_end().trim_end_matches(';')
            ),
        )
        .await,
    )
    .unwrap();
    (
        json!({
            "count": statements.len(),
            "answer_body_bytes": spread(&body_bytes),
            "answer_bytes_total": total,
            "answer_bytes_per_row": total as f64 / expected_rows as f64,
            "largest_answer_share_of_limit": largest as f64 / ANSWER_LIMIT_BYTES as f64,
        }),
        explain,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement of minutes and several hundred MB of disk (see the module's docs)"]
#[allow(
    clippy::too_many_lines,
    reason = "one measurement in order: data, runs, comparisons, report"
)]
async fn export_thirty_days_and_over_https() {
    let Some((server, https_url, ca_pem)) = start().await else {
        return;
    };
    let reader = Account {
        user: READER.to_owned(),
        password: Uuid::new_v4().simple().to_string(),
    };
    server
        .query_with(
            &server.admin,
            CREATE_READER,
            &[("param_password", reader.password.as_str())],
        )
        .await
        .unwrap_or_else(|error| panic!("{CREATE_READER}\n{}", error.body));
    for statement in READER_STATEMENTS {
        server.admin_ok(statement).await;
    }
    let version = server.admin_ok("SELECT version()").await.trim().to_owned();
    let baseline = std::env::var_os(BASELINE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let current = PathBuf::from(env!("CARGO_BIN_EXE_lab-dataset"));

    // Whole hours, as before Laysi/trustiot-v2#837 (the hourly windows then started at the
    // range's start). The data ends two days before now, well inside the table's retention.
    let end = {
        let hour = TimeDelta::hours(1).num_milliseconds();
        let ms = (Utc::now() - TimeDelta::days(2)).timestamp_millis();
        DateTime::from_timestamp_millis(ms - ms.rem_euclid(hour)).unwrap()
    };
    let start = end - TimeDelta::days(DAYS);
    let generated = Instant::now();
    generate(&server, start.timestamp_millis()).await;
    let generated = generated.elapsed();
    let expected_rows = DAYS.unsigned_abs() * BATCHES_PER_DAY * ROWS_PER_BATCH;
    // The administrator reads no rows (no row policy names it); the reader does.
    let held = server
        .query(&reader, "SELECT count() FROM trustiot.telemetry FORMAT TSV")
        .await
        .unwrap_or_else(|error| panic!("count the rows: {}", error.body));
    assert_eq!(held.trim(), expected_rows.to_string());
    let parts: Vec<Value> = server
        .admin_ok(
            "SELECT name, rows, toString(min_time) AS received_from, \
             toString(max_time) AS received_to FROM system.parts \
             WHERE database = 'trustiot' AND table = 'telemetry' AND active \
             ORDER BY min_time FORMAT JSONEachRow",
        )
        .await
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    println!(
        "generated {expected_rows} rows in {} s: {} active parts {parts:?}",
        generated.as_secs(),
        parts.len()
    );

    let scratch = Scratch::new();
    let ca_file = scratch.0.join("ca.pem");
    fs::write(&ca_file, &ca_pem).unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let mut exports = Vec::new();
    let mut comparisons = Vec::new();
    let mut small_peaks = Vec::new();
    let mut thirty_days = None;
    for (segments, segment_bytes) in [("64 MiB", None), ("4 MiB", Some(SMALL_SEGMENT_BYTES))] {
        let config = scratch
            .0
            .join(format!("export-{}.toml", segment_bytes.unwrap_or(0)));
        fs::write(&config, config_text(segment_bytes)).unwrap();
        for days in [7, 15, DAYS] {
            let range = (end - TimeDelta::days(days), end);
            let rows = days.unsigned_abs() * BATCHES_PER_DAY * ROWS_PER_BATCH;
            let mut runs = vec![("current", current.clone())];
            runs.extend(baseline.clone().map(|path| ("baseline", path)));
            let mut outs = Vec::new();
            for (binary, path) in runs {
                let out = scratch
                    .0
                    .join(format!("{binary}-{days}-{}", segment_bytes.unwrap_or(0)));
                let run =
                    run_export(&path, &config, range, &out, (&server.url, &reader), None).await;
                assert_eq!(run.printed["rows"], rows, "{binary}, {days} days");
                assert_eq!(
                    run.printed["batches"],
                    rows / ROWS_PER_BATCH,
                    "{binary}, {days} days"
                );
                assert_eq!(
                    run.printed["uncommitted_batches"], 0,
                    "{binary}, {days} days"
                );
                assert_eq!(
                    run.printed["repeated_rows_dropped"], 0,
                    "{binary}, {days} days"
                );
                server.admin_ok("SYSTEM FLUSH LOGS").await;
                let statements = statement_kinds(&server, run.window).await;
                assert_eq!(
                    number(&statements["rows"]["rows_returned"]),
                    rows,
                    "{binary}, {days} days: the rows statements return every row once"
                );
                if binary == "current" {
                    assert_eq!(
                        statements["rows"]["queries"], run.printed["windows"],
                        "{days} days: one rows statement per window"
                    );
                }
                println!(
                    "{binary}, {segments} segments, {days} days: {rows} rows, peak memory {} \
                     MiB, {:.1} s, {} segments, {} windows; statements {statements}",
                    run.peak_kib / 1024,
                    run.wall.as_secs_f64(),
                    run.printed["segments"],
                    run.printed["windows"]
                );
                if binary == "current" && segment_bytes.is_some() {
                    small_peaks.push((days, rows, run.peak_kib));
                }
                exports.push(json!({
                    "binary": binary,
                    "segment_bytes": segments,
                    "days": days,
                    "rows": rows,
                    "segments": run.printed["segments"],
                    "windows": run.printed.get("windows"),
                    "peak_memory_kib": run.peak_kib,
                    "wall_seconds": run.wall.as_secs_f64(),
                    "statements": statements,
                }));
                outs.push((binary, out, run.window));
            }
            let current_dir = outs[0].1.join("scale");
            if let Some((_, baseline_out, _)) = outs.get(1) {
                let label = format!("{segments} segments, {days} days");
                let files = compare_files(&label, &current_dir, &baseline_out.join("scale"));
                for file in &files {
                    println!("BASELINE {label} {file}: identical");
                }
                comparisons.push(json!({
                    "segment_bytes": segments,
                    "days": days,
                    "identical": true,
                    "files": files,
                }));
            }
            if days == DAYS && segment_bytes.is_none() {
                thirty_days = Some((outs, range));
            } else {
                for (_, out, _) in outs {
                    fs::remove_dir_all(&out).unwrap();
                }
            }
        }
    }

    // The memory bound with small segments: the 30-day peak against the 7-day one.
    let (_, rows_7, peak_7) = small_peaks[0];
    let (_, rows_30, peak_30) = *small_peaks.last().unwrap();
    let bound_kib =
        peak_7 + MEMORY_ALLOWANCE_KIB + (rows_30 - rows_7) * BYTES_PER_MORE_READING / 1024;
    assert!(
        peak_30 <= bound_kib,
        "with {SMALL_SEGMENT_BYTES}-byte segments the 30-day peak of {peak_30} KiB passes the \
         bound of {bound_kib} KiB from the 7-day peak of {peak_7} KiB"
    );
    println!(
        "memory with 4 MiB segments: 7 days {peak_7} KiB, 30 days {peak_30} KiB, bound \
         {bound_kib} KiB"
    );

    // The 30-day answers, sent again, and EXPLAIN of a statement in the middle of the range.
    let (outs, range) = thirty_days.unwrap();
    let mut rows_answers = serde_json::Map::new();
    for (binary, _, window) in &outs {
        let statements = rows_statements(&server, *window).await;
        let (summary, explain) =
            answers(&http, &server.url, &reader, &statements, expected_rows).await;
        println!("EXPLAIN-BEGIN {binary}\n{explain}EXPLAIN-END {binary}");
        println!("{binary}, 30 days: rows statements {summary}");
        rows_answers.insert((*binary).to_owned(), summary);
    }

    // https with the CA file: the same 30 days, the same files, byte for byte.
    let http_dir = outs[0].1.join("scale");
    let https_out = scratch.0.join("https-30");
    let config = scratch.0.join("export-0.toml");
    let secure = run_export(
        &current,
        &config,
        range,
        &https_out,
        (&https_url, &reader),
        Some(&ca_file),
    )
    .await;
    assert_eq!(secure.printed["rows"], expected_rows);
    let https_files = compare_files("https", &http_dir, &https_out.join("scale"));
    let files_bytes: u64 = https_files.iter().map(|file| number(&file["bytes"])).sum();
    println!(
        "https, {DAYS} days: peak memory {} MiB, {:.1} s; {} files, {files_bytes} bytes, \
         identical to the http export",
        secure.peak_kib / 1024,
        secure.wall.as_secs_f64(),
        https_files.len()
    );

    // The result is a dataset the checker accepts.
    let checked = tokio::process::Command::new(&current)
        .arg("check")
        .arg(http_dir.join("index.toml"))
        .output()
        .await
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let report = json!({
        "clickhouse": version,
        "baseline": baseline.is_some(),
        "data": {
            "days": DAYS,
            "devices": 2,
            "points_per_device": 5,
            "batches": DAYS * 24 * 60,
            "rows": expected_rows,
            "active_parts": parts,
            "generation_seconds": generated.as_secs_f64(),
        },
        "exports": exports,
        "memory_bound_4_mib_segments": {
            "peak_7_days_kib": peak_7,
            "peak_30_days_kib": peak_30,
            "bound_kib": bound_kib,
        },
        "rows_statements_30_days": rows_answers,
        "answer_limit_bytes": ANSWER_LIMIT_BYTES,
        "baseline_comparisons": comparisons,
        "https_30_days": {
            "peak_memory_kib": secure.peak_kib,
            "wall_seconds": secure.wall.as_secs_f64(),
            "files": https_files,
            "identical_to_http": true,
        },
    });
    println!(
        "SCALE-REPORT-BEGIN\n{}\nSCALE-REPORT-END",
        serde_json::to_string_pretty(&report).unwrap()
    );
}
