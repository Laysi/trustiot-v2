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
//! A second measurement, `received_at_lookup_candidates` (Laysi/trustiot-v2#852), asks whether
//! the telemetry table should find rows by receive time. On the same 30 days, in one gateway that
//! reads in order, a gateway like it in a tenant with four gateways, and a gateway whose clock was
//! 30 days behind for a day, it adds each candidate to the product table in turn (none, a minmax
//! skip index on `received_at`, a projection ordered by `(tenant_id, gateway_id, received_at)`),
//! materializes it, and records the export's statements and `read_rows` under a row policy that
//! names one tenant, whether a projection served them, `EXPLAIN indexes = 1, projections = 1` of
//! the window that read the most, a read by receive time only, the table's size; the export's
//! files must equal those without a candidate byte for byte. It also runs, for each candidate, a
//! tenant's read through its view, by gateway and receive time, whose condition throws on another
//! tenant's row (the isolation of 0005_tenant_view.sql), and the ingest service's lightweight
//! delete of a batch.
//!
//! Laysi/trustiot-v2#836 adds five more runs of the 30-day export to the first measurement (the
//! distribution of its time; every run must write the same files) and a third measurement,
//! `export_after_one_insert_per_batch`: the same data written with one INSERT per batch and table
//! as the ingest service writes it (the server's background merges run between them), exported at
//! 7, 15 and 30 days with both segment sizes, and five more times at 30 days. The limits of the
//! export on large answers and on rows that change during it are regression tests in
//! `tests/export_limits.rs`.
//!
//! They are measurements, not regression tests: each takes minutes and several hundred MB of
//! disk, so they are ignored by default. Run them, on Linux (the first reads `/proc`), with
//!
//! ```text
//! cargo test --release --locked -p lab-dataset --test export_scale -- --ignored --nocapture
//! ```
//!
//! (add the test's name to run one) and read the reports between `SCALE-REPORT-BEGIN` and
//! `SCALE-REPORT-END`, and `LOOKUP-REPORT-BEGIN` and `LOOKUP-REPORT-END`. The data is
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
/// times five points (three floats, a counter and a flag), read 400 ms before; the batches from
/// `skew_first` up to, not including, `skew_end` are read `skew_ms` earlier still (a gateway
/// clock that was wrong for a while, Laysi/trustiot-v2#852).
const GENERATE_TELEMETRY: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, \
     stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, \
     value_int, value_bool, value_string)
SELECT {tenant:UUID}, {gateway:String}, {stream:String}, n + 1, device.1, point.1,
  fromUnixTimestamp64Milli({start_ms:Int64} + toInt64(n) * 60000 - 400
    - if(n >= {skew_first:UInt64} AND n < {skew_end:UInt64}, {skew_ms:Int64}, 0), 'UTC'),
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
ARRAY JOIN [({device_1:UUID}, 1), ({device_2:UUID}, 2)] AS device";

/// The completion records of the same batches (ten rows each).
const GENERATE_COMMITS: &str = "INSERT INTO trustiot.stream_commits (tenant_id, binding_id, \
     gateway_id, stream_id, seq, row_count, committed_at)
SELECT {tenant:UUID}, {binding:UUID}, {gateway:String}, {stream:String},
  {first:UInt64} + number + 1, 10,
  fromUnixTimestamp64Milli({start_ms:Int64} + toInt64({first:UInt64} + number) * 60000 + 100, 'UTC')
FROM numbers({count:UInt64})";

/// The reader's statements in a time window, from the log, by kind: number, rows returned,
/// rows read. The log keeps a statement with its parameters' values written in; its first words
/// tell its kind. `rows` is the statement that reads a window's (or, before
/// Laysi/trustiot-v2#837, an hour's) rows.
const STATEMENT_KINDS: &str = "SELECT multiIf(
    startsWith(query, 'SELECT stream_id, batch_seq'), 'rows',
    startsWith(query, 'SELECT toString(device_id) AS device, point, count()'), 'series',
    startsWith(query, 'SELECT toUnixTimestamp64Milli(ts) AS ts_ms'), 'repeats',
    startsWith(query, 'SELECT intDiv('), 'plan',
    startsWith(query, 'SELECT seq, row_count'), 'commits', 'other') AS kind,
  count() AS queries, sum(result_rows) AS rows_returned, sum(read_rows) AS rows_read,
  min(read_rows) AS rows_read_min, quantileExact(0.5)(read_rows) AS rows_read_median,
  max(read_rows) AS rows_read_max, max(memory_usage) AS server_memory_max
FROM system.query_log
WHERE type = 'QueryFinish' AND user = {user:String}
  AND event_time_microseconds >= fromUnixTimestamp64Micro({from_us:Int64}, 'UTC')
  AND event_time_microseconds <= fromUnixTimestamp64Micro({to_us:Int64}, 'UTC')
GROUP BY kind ORDER BY kind FORMAT JSONEachRow";

/// The reader's rows statements in a time window, in order, as the log keeps them.
const ROWS_STATEMENTS: &str = "SELECT query FROM system.query_log
WHERE type = 'QueryFinish' AND user = {user:String}
  AND event_time_microseconds >= fromUnixTimestamp64Micro({from_us:Int64}, 'UTC')
  AND event_time_microseconds <= fromUnixTimestamp64Micro({to_us:Int64}, 'UTC')
  AND startsWith(query, 'SELECT stream_id, batch_seq')
ORDER BY event_time_microseconds FORMAT JSONEachRow";

/// The beginning of a statement in the URL's `query` parameter: ClickHouse's HTTP interface
/// joins it with the request body, so a logged statement is explained without building SQL text
/// here (`tools/sql-check`).
const EXPLAIN_PARAMETER: &str = "query=EXPLAIN%20indexes%20%3D%201";

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
    config_text_for(segment_bytes, [SENSOR_1, SENSOR_2])
}

/// [`config_text`] for the two devices `devices`.
fn config_text_for(segment_bytes: Option<u64>, [sensor_1, sensor_2]: [Uuid; 2]) -> String {
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
id = "{sensor_1}"
model = "environment-sensor"
unit = 1
interval_s = 60

[[devices]]
key = "sensor-2"
id = "{sensor_2}"
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

/// Runs `binary export` of [`TENANT`]'s [`GATEWAY`] for `[from, to)` into `out` and measures it.
async fn run_export(
    binary: &Path,
    config: &Path,
    range: (DateTime<Utc>, DateTime<Utc>),
    out: &Path,
    server: (&str, &Account),
    ca_file: Option<&Path>,
) -> Run {
    run_export_of(
        binary,
        config,
        (TENANT, GATEWAY),
        range,
        out,
        server,
        ca_file,
    )
    .await
}

/// [`run_export`] of `tenant`'s `gateway`.
async fn run_export_of(
    binary: &Path,
    config: &Path,
    (tenant, gateway): (Uuid, &str),
    range: (DateTime<Utc>, DateTime<Utc>),
    out: &Path,
    (url, account): (&str, &Account),
    ca_file: Option<&Path>,
) -> Run {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("export")
        .arg(config)
        .args(["--tenant", &tenant.to_string()])
        .args(["--gateway", gateway])
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

/// One gateway's generated data: whose it is, its two devices and the batches its clock read
/// too early (`first`, `end` and how many milliseconds; `(0, 0, 0)` for none).
#[derive(Clone, Copy, Debug)]
struct Fleet {
    tenant: Uuid,
    gateway: &'static str,
    devices: [Uuid; 2],
    skew: (u64, u64, i64),
}

/// The gateway the 30-day measurement exports.
const IN_ORDER: Fleet = Fleet {
    tenant: TENANT,
    gateway: GATEWAY,
    devices: [SENSOR_1, SENSOR_2],
    skew: (0, 0, 0),
};

/// Writes `DAYS` days of batches ending at `end`, one INSERT per day for each table.
async fn generate(server: &ClickHouse, start_ms: i64) {
    generate_batches(
        server,
        start_ms,
        &IN_ORDER,
        0,
        DAYS.unsigned_abs() * BATCHES_PER_DAY,
    )
    .await;
}

/// Writes `fleet`'s batches `first` to `first + count`, one INSERT per day (at most) for each
/// table; batch `n` is received at `start_ms + n` minutes.
async fn generate_batches(
    server: &ClickHouse,
    start_ms: i64,
    fleet: &Fleet,
    first: u64,
    count: u64,
) {
    generate_batches_in(server, start_ms, fleet, (first, count), BATCHES_PER_DAY).await;
}

/// [`generate_batches`] with at most `per_insert` batches in each INSERT: `1` writes a part per
/// batch and table, as the ingest service does (Laysi/trustiot-v2#836).
async fn generate_batches_in(
    server: &ClickHouse,
    start_ms: i64,
    fleet: &Fleet,
    (first, count): (u64, u64),
    per_insert: u64,
) {
    let (tenant, binding) = (fleet.tenant.to_string(), BINDING.to_string());
    let [device_1, device_2] = fleet.devices.map(|device| device.to_string());
    let (skew_first, skew_end, skew_ms) = (
        fleet.skew.0.to_string(),
        fleet.skew.1.to_string(),
        fleet.skew.2.to_string(),
    );
    let start_ms = start_ms.to_string();
    let mut next = first;
    while next < first + count {
        let batches = per_insert.min(first + count - next);
        let (from, batches_text) = (next.to_string(), batches.to_string());
        let parameters = [
            ("param_tenant", tenant.as_str()),
            ("param_binding", binding.as_str()),
            ("param_gateway", fleet.gateway),
            ("param_stream", STREAM),
            ("param_device_1", device_1.as_str()),
            ("param_device_2", device_2.as_str()),
            ("param_skew_first", skew_first.as_str()),
            ("param_skew_end", skew_end.as_str()),
            ("param_skew_ms", skew_ms.as_str()),
            ("param_start_ms", start_ms.as_str()),
            ("param_first", from.as_str()),
            ("param_count", batches_text.as_str()),
        ];
        for statement in [GENERATE_TELEMETRY, GENERATE_COMMITS] {
            server
                .query_with(&server.admin, statement, &parameters)
                .await
                .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
        }
        next += batches;
    }
}

/// Runs a log statement for the reader's statements in `window`; returns its JSON lines.
async fn log_lines(
    server: &ClickHouse,
    statement: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Vec<Value> {
    log_lines_of(server, statement, READER, window).await
}

/// [`log_lines`] for the statements of `user`.
async fn log_lines_of(
    server: &ClickHouse,
    statement: &str,
    user: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Vec<Value> {
    let micros = |at: DateTime<Utc>| at.timestamp_micros().to_string();
    let (from, to) = (micros(window.0), micros(window.1));
    let parameters = [
        ("param_user", user),
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
    statement_kinds_of(server, READER, window).await
}

/// [`statement_kinds`] for the statements of `user`.
async fn statement_kinds_of(
    server: &ClickHouse,
    user: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Value {
    let mut kinds = serde_json::Map::new();
    let mut read_total = 0;
    for mut line in log_lines_of(server, STATEMENT_KINDS, user, window).await {
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
    log_lines(server, ROWS_STATEMENTS, window)
        .await
        .into_iter()
        .map(|line| line["query"].as_str().unwrap().to_owned())
        .collect()
}

/// Sends `statement` as the reader to `url`; returns the answer's body.
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
    let explain_url = format!("{}/?{EXPLAIN_PARAMETER}", url.trim_end_matches('/'));
    let explain = String::from_utf8(
        send(
            http,
            &explain_url,
            reader,
            middle.trim_end().trim_end_matches(';'),
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

    // The same 30 days again, for the distribution of the time (Laysi/trustiot-v2#836).
    let repeats = repeat_runs(
        &current,
        &config,
        range,
        &scratch.0,
        (&server.url, &reader),
        expected_rows,
        REPEATS,
    )
    .await;
    println!("repeats of the 30-day export: {repeats}");

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
        "repeats_30_days": repeats,
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

// Laysi/trustiot-v2#836: the repeated runs of the 30-day export, and the same data written the way
// the ingest service writes it.

/// How many times the 30-day export is repeated for the distribution of its time.
const REPEATS: usize = 5;

/// Mean and (population) standard deviation of `values`.
fn mean_and_deviation(values: &[f64]) -> (f64, f64) {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    (mean, variance.sqrt())
}

/// Exports `range` with `binary` `runs` times, each into its own directory; every run must write
/// the files of the first, byte for byte. Returns each run's wall time and peak memory and their
/// distribution.
async fn repeat_runs(
    binary: &Path,
    config: &Path,
    range: (DateTime<Utc>, DateTime<Utc>),
    scratch: &Path,
    server: (&str, &Account),
    rows: u64,
    runs: usize,
) -> Value {
    let mut walls = Vec::new();
    let mut peaks = Vec::new();
    let mut first: Option<PathBuf> = None;
    for index in 0..runs {
        let out = scratch.join(format!("repeat-{index}"));
        let run = run_export(binary, config, range, &out, server, None).await;
        assert_eq!(run.printed["rows"], rows, "repeat {index}");
        println!(
            "repeat {index}: peak memory {} MiB, {:.2} s, {} windows",
            run.peak_kib / 1024,
            run.wall.as_secs_f64(),
            run.printed["windows"]
        );
        walls.push(run.wall.as_secs_f64());
        peaks.push(run.peak_kib);
        match &first {
            None => first = Some(out),
            Some(first) => {
                compare_files(
                    &format!("repeat {index}"),
                    &first.join("scale"),
                    &out.join("scale"),
                );
                fs::remove_dir_all(&out).unwrap();
            }
        }
    }
    fs::remove_dir_all(first.unwrap()).unwrap();
    let mut sorted = walls.clone();
    sorted.sort_by(f64::total_cmp);
    let (mean, deviation) = mean_and_deviation(&walls);
    json!({
        "runs": runs,
        "wall_seconds": walls,
        "peak_memory_kib": peaks,
        "wall_seconds_distribution": {
            "min": sorted[0],
            "median": sorted[sorted.len() / 2],
            "max": sorted[sorted.len() - 1],
            "mean": mean,
            "standard_deviation": deviation,
        },
        "files_identical_across_runs": true,
    })
}

/// How many active parts the telemetry table has, and the number of merges that ran since the
/// server started.
async fn part_counts(server: &ClickHouse) -> Value {
    let parts = server
        .admin_ok(
            "SELECT count() AS active_parts, sum(rows) AS rows, \
             uniqExact(partition) AS partitions FROM system.parts \
             WHERE database = 'trustiot' AND table = 'telemetry' AND active FORMAT JSONEachRow",
        )
        .await;
    server.admin_ok("SYSTEM FLUSH LOGS").await;
    let merges = server
        .admin_ok(
            "SELECT countIf(event_type = 'NewPart') AS new_parts, \
             countIf(event_type = 'MergeParts') AS merges FROM system.part_log \
             WHERE database = 'trustiot' AND table = 'telemetry' FORMAT JSONEachRow",
        )
        .await;
    json!({
        "now": serde_json::from_str::<Value>(parts.trim()).unwrap(),
        "history": serde_json::from_str::<Value>(merges.trim()).unwrap(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement of many minutes and several hundred MB of disk (see the module's docs)"]
#[allow(
    clippy::too_many_lines,
    reason = "one measurement in order: data, runs, report"
)]
async fn export_after_one_insert_per_batch() {
    let Some((server, _https_url, _ca_pem)) = start().await else {
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
    let current = PathBuf::from(env!("CARGO_BIN_EXE_lab-dataset"));
    let end = {
        let hour = TimeDelta::hours(1).num_milliseconds();
        let ms = (Utc::now() - TimeDelta::days(2)).timestamp_millis();
        DateTime::from_timestamp_millis(ms - ms.rem_euclid(hour)).unwrap()
    };
    let start = end - TimeDelta::days(DAYS);
    let expected_rows = DAYS.unsigned_abs() * BATCHES_PER_DAY * ROWS_PER_BATCH;

    // One INSERT per batch and table, in order, with the server's background merges running
    // between them (ingest writes a batch when it arrives); nothing is merged by hand.
    let generated = Instant::now();
    generate_batches_in(
        &server,
        start.timestamp_millis(),
        &IN_ORDER,
        (0, DAYS.unsigned_abs() * BATCHES_PER_DAY),
        1,
    )
    .await;
    let generated = generated.elapsed();
    let held = server
        .query(&reader, "SELECT count() FROM trustiot.telemetry FORMAT TSV")
        .await
        .unwrap_or_else(|error| panic!("count the rows: {}", error.body));
    assert_eq!(held.trim(), expected_rows.to_string());
    let parts_after_writing = part_counts(&server).await;
    println!(
        "generated {expected_rows} rows in {:.0} s, one INSERT per batch: {parts_after_writing}",
        generated.as_secs_f64()
    );

    let scratch = Scratch::new();
    let config = scratch.0.join("export-0.toml");
    fs::write(&config, config_text(None)).unwrap();
    let small_config = scratch.0.join("export-small.toml");
    fs::write(&small_config, config_text(Some(SMALL_SEGMENT_BYTES))).unwrap();
    let mut exports = Vec::new();
    for (segments, config) in [("64 MiB", &config), ("4 MiB", &small_config)] {
        for days in [7, 15, DAYS] {
            let range = (end - TimeDelta::days(days), end);
            let rows = days.unsigned_abs() * BATCHES_PER_DAY * ROWS_PER_BATCH;
            let out = scratch.0.join(format!("once-{days}-{segments}"));
            let run = run_export(&current, config, range, &out, (&server.url, &reader), None).await;
            assert_eq!(run.printed["rows"], rows, "{days} days");
            assert_eq!(run.printed["uncommitted_batches"], 0);
            assert_eq!(run.printed["repeated_rows_dropped"], 0);
            server.admin_ok("SYSTEM FLUSH LOGS").await;
            let statements = statement_kinds(&server, run.window).await;
            assert_eq!(number(&statements["rows"]["rows_returned"]), rows);
            assert_eq!(statements["rows"]["queries"], run.printed["windows"]);
            println!(
                "{segments} segments, {days} days: peak memory {} MiB, {:.1} s, {} windows; \
                 statements {statements}",
                run.peak_kib / 1024,
                run.wall.as_secs_f64(),
                run.printed["windows"]
            );
            exports.push(json!({
                "segment_bytes": segments,
                "days": days,
                "rows": rows,
                "segments": run.printed["segments"],
                "windows": run.printed["windows"],
                "peak_memory_kib": run.peak_kib,
                "wall_seconds": run.wall.as_secs_f64(),
                "statements": statements,
            }));
            fs::remove_dir_all(&out).unwrap();
        }
    }
    let range = (end - TimeDelta::days(DAYS), end);
    let repeats = repeat_runs(
        &current,
        &config,
        range,
        &scratch.0,
        (&server.url, &reader),
        expected_rows,
        REPEATS,
    )
    .await;
    let parts_after_export = part_counts(&server).await;
    let report = json!({
        "clickhouse": version,
        "data": {
            "days": DAYS,
            "batches": DAYS * 24 * 60,
            "rows": expected_rows,
            "inserts_per_table": DAYS * 24 * 60,
            "generation_seconds": generated.as_secs_f64(),
            "parts_after_writing": parts_after_writing,
            "parts_after_export": parts_after_export,
        },
        "exports": exports,
        "repeats_30_days": repeats,
    });
    println!(
        "SCALE-REPORT-BEGIN\n{}\nSCALE-REPORT-END",
        serde_json::to_string_pretty(&report).unwrap()
    );
}

// Laysi/trustiot-v2#852: can the table find a gateway's rows by receive time? The candidates are
// added to the product table one at a time, on the same data, and the export, a receive-time-only
// read, a tenant's read through its view and the ingest service's batch delete run against each.

/// The tenants of the lookup measurement, next to [`TENANT`] in UUID order, so that their rows
/// share granules with its rows at the boundaries (in the table's order and in a projection's).
const NEIGHBOUR: Uuid = Uuid::from_u128(0x5ca1f);
const FOUR_GATEWAYS: Uuid = Uuid::from_u128(0x5ca20);
const WRONG_CLOCK: Uuid = Uuid::from_u128(0x5ca21);
const DELETED: Uuid = Uuid::from_u128(0x5ca22);

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A gateway's two devices, as [`Fleet::devices`] wants them.
const fn devices(first: u128) -> [Uuid; 2] {
    [Uuid::from_u128(first), Uuid::from_u128(first + 1)]
}

/// The exported gateway of `tenant`, whose devices start at `first_device`. Every gateway has its
/// own ID: a gateway ID is unique across tenants, and the completion table keys its records by
/// gateway, stream and number only (0012_gateway_stream_ids.sql), so two tenants' gateways of
/// one name would replace each other's records.
const fn one_gateway(
    tenant: Uuid,
    gateway: &'static str,
    first_device: u128,
    skew: (u64, u64, i64),
) -> Fleet {
    Fleet {
        tenant,
        gateway,
        devices: devices(first_device),
        skew,
    }
}

/// Another gateway of `tenant`, never exported.
const fn other_gateway(tenant: Uuid, gateway: &'static str, first_device: u128) -> Fleet {
    Fleet {
        tenant,
        gateway,
        devices: devices(first_device),
        skew: (0, 0, 0),
    }
}

/// A scenario: the gateways of one tenant, the exported one first.
struct Scenario {
    name: &'static str,
    fleets: &'static [Fleet],
}

/// The scenarios, in the order of [`LOOKUP_READERS`].
const SCENARIOS: [Scenario; 3] = [
    // The 30 days of the measurement above.
    Scenario {
        name: "in-order",
        fleets: &[IN_ORDER],
    },
    // A gateway like the one above in a tenant with three more gateways of the same size.
    Scenario {
        name: "four-gateways",
        fleets: &[
            one_gateway(FOUR_GATEWAYS, "four-gateways-0", 0x5e31, (0, 0, 0)),
            other_gateway(FOUR_GATEWAYS, "other-gateway-1", 0x5e33),
            other_gateway(FOUR_GATEWAYS, "other-gateway-2", 0x5e35),
            other_gateway(FOUR_GATEWAYS, "other-gateway-3", 0x5e37),
        ],
    },
    // One gateway whose clock read 30 days behind for the 15th day: receive time and reading time
    // part ways (the reading times are wrong, not late, so they repeat no reading).
    Scenario {
        name: "wrong-clock",
        fleets: &[one_gateway(
            WRONG_CLOCK,
            "wrong-clock-gateway",
            0x5e41,
            (14 * BATCHES_PER_DAY, 15 * BATCHES_PER_DAY, 30 * DAY_MS),
        )],
    },
];

/// One batch of another tenant, received an hour before the end: its rows sit next to the last
/// rows of [`TENANT`] in either order.
const NEIGHBOUR_FLEET: Fleet = other_gateway(NEIGHBOUR, "neighbour-gateway", 0x5e51);

/// The batch each candidate's delete check writes and deletes, as the ingest service does (I1).
const DELETED_FLEET: Fleet = other_gateway(DELETED, "deleted-gateway", 0x5e61);

/// The export accounts of the scenarios: each reads only its tenant's rows, by a row policy
/// written as datasets/lab/README.md writes it (ClickHouse takes no query parameter in a policy,
/// so the tenant IDs are literals, checked against the constants by the test).
const LOOKUP_READERS: [(&str, [&str; 5]); 3] = [
    (
        "lookup_in_order",
        [
            "CREATE USER lookup_in_order IDENTIFIED WITH sha256_password BY {password:String} \
             HOST ANY",
            "GRANT SELECT ON trustiot.telemetry TO lookup_in_order",
            "GRANT SELECT ON trustiot.stream_commits TO lookup_in_order",
            "CREATE ROW POLICY lookup_in_order ON trustiot.telemetry AS PERMISSIVE FOR SELECT \
             USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca1e') TO lookup_in_order",
            "CREATE ROW POLICY lookup_in_order ON trustiot.stream_commits AS PERMISSIVE FOR \
             SELECT USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca1e') TO \
             lookup_in_order",
        ],
    ),
    (
        "lookup_four_gateways",
        [
            "CREATE USER lookup_four_gateways IDENTIFIED WITH sha256_password BY \
             {password:String} HOST ANY",
            "GRANT SELECT ON trustiot.telemetry TO lookup_four_gateways",
            "GRANT SELECT ON trustiot.stream_commits TO lookup_four_gateways",
            "CREATE ROW POLICY lookup_four_gateways ON trustiot.telemetry AS PERMISSIVE FOR \
             SELECT USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca20') TO \
             lookup_four_gateways",
            "CREATE ROW POLICY lookup_four_gateways ON trustiot.stream_commits AS PERMISSIVE FOR \
             SELECT USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca20') TO \
             lookup_four_gateways",
        ],
    ),
    (
        "lookup_wrong_clock",
        [
            "CREATE USER lookup_wrong_clock IDENTIFIED WITH sha256_password BY {password:String} \
             HOST ANY",
            "GRANT SELECT ON trustiot.telemetry TO lookup_wrong_clock",
            "GRANT SELECT ON trustiot.stream_commits TO lookup_wrong_clock",
            "CREATE ROW POLICY lookup_wrong_clock ON trustiot.telemetry AS PERMISSIVE FOR SELECT \
             USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca21') TO \
             lookup_wrong_clock",
            "CREATE ROW POLICY lookup_wrong_clock ON trustiot.stream_commits AS PERMISSIVE FOR \
             SELECT USING tenant_id = toUUID('00000000-0000-0000-0000-00000005ca21') TO \
             lookup_wrong_clock",
        ],
    ),
];

/// A tenant account of [`TENANT`] as telemetry-query creates one (0004_access.sql), with the
/// longest queryable period (0011_queryable_period.sql): it reads through the tenant view.
const LOOKUP_TENANT: &str = "lookup_tenant";
const CREATE_LOOKUP_TENANT: &str = "CREATE USER lookup_tenant
IDENTIFIED WITH sha256_password BY {password:String}
DEFAULT ROLE trustiot_tenant_reader
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '00000000-0000-0000-0000-00000005ca1e' CONST,
  SQL_queryable_raw_days = 36500 CONST, SQL_queryable_1m_months = 1200 CONST,
  SQL_queryable_1h_months = 1200 CONST";

/// A candidate: its name, how it is added to the table and materialized for the rows already
/// there, and how it is removed before the next one (each statement with `mutations_sync = 2`).
type Candidate = (&'static str, &'static [&'static str], Option<&'static str>);

const CANDIDATES: [Candidate; 3] = [
    ("none", &[], None),
    (
        "minmax skip index",
        &[
            "ALTER TABLE trustiot.telemetry ADD INDEX lookup_received_at received_at TYPE minmax \
             GRANULARITY 1",
            "ALTER TABLE trustiot.telemetry MATERIALIZE INDEX lookup_received_at",
        ],
        Some("ALTER TABLE trustiot.telemetry DROP INDEX lookup_received_at"),
    ),
    (
        "projection",
        &[
            "ALTER TABLE trustiot.telemetry ADD PROJECTION lookup_received_at (SELECT * ORDER BY \
             tenant_id, gateway_id, received_at)",
            "ALTER TABLE trustiot.telemetry MATERIALIZE PROJECTION lookup_received_at",
        ],
        Some("ALTER TABLE trustiot.telemetry DROP PROJECTION lookup_received_at"),
    ),
];

/// The rows statements of a user in a time window: what each read, and whether a projection
/// served it.
const LOOKUP_ROWS_LOG: &str = "SELECT query, read_rows, result_rows,
  toUInt8(notEmpty(projections)) AS projection
FROM system.query_log
WHERE type = 'QueryFinish' AND user = {user:String}
  AND event_time_microseconds >= fromUnixTimestamp64Micro({from_us:Int64}, 'UTC')
  AND event_time_microseconds <= fromUnixTimestamp64Micro({to_us:Int64}, 'UTC')
  AND startsWith(query, 'SELECT stream_id, batch_seq')
ORDER BY event_time_microseconds FORMAT JSONEachRow";

/// How many statements of a user in a time window ended (finished or failed), how many of them a
/// projection served, and which projections.
const LOOKUP_PROJECTIONS_LOG: &str = "SELECT count() AS statements,
  countIf(notEmpty(projections)) AS with_projection,
  groupUniqArrayArray(projections) AS used_projections
FROM system.query_log
WHERE type != 'QueryStart' AND user = {user:String}
  AND event_time_microseconds >= fromUnixTimestamp64Micro({from_us:Int64}, 'UTC')
  AND event_time_microseconds <= fromUnixTimestamp64Micro({to_us:Int64}, 'UTC')
FORMAT JSONEachRow";

/// The rows of one gateway received in an hour, by receive time only: a read without the export's
/// reading time bound.
const LOOKUP_HOUR: &str = "SELECT count() FROM trustiot.telemetry \
     WHERE tenant_id = {tenant:UUID} AND gateway_id = {gateway:String} \
     AND received_at >= fromUnixTimestamp64Milli({from_ms:Int64}, 'UTC') \
     AND received_at < fromUnixTimestamp64Milli({to_ms:Int64}, 'UTC') FORMAT TSV";

/// A tenant's read through its view whose own condition throws on any row of another tenant
/// (services/ingest/tests/clickhouse.rs, "direct: throwIf on any row of another tenant"): the
/// definer's row policy has to remove every such row before the condition runs
/// (0005_tenant_view.sql). The gateway condition names the neighbouring tenant's gateway too, so
/// its rows pass every condition but the policy, and lets a projection that starts with
/// `(tenant_id, gateway_id, received_at)` serve the read.
const LOOKUP_ISOLATION: &str = "SELECT count() FROM trustiot.tenant_telemetry \
     WHERE gateway_id IN {gateways:Array(String)} \
     AND received_at >= fromUnixTimestamp64Milli({from_ms:Int64}, 'UTC') \
     AND throwIf(tenant_id != {tenant:UUID}, 'a row of another tenant reached the condition') = 0 \
     FORMAT TSV";

/// The gateways [`LOOKUP_ISOLATION`] names: [`GATEWAY`] and [`NEIGHBOUR_FLEET`]'s.
const ISOLATION_GATEWAYS: &str = "['scale-gateway','neighbour-gateway']";

/// Deletes one batch's rows as the ingest service's I1 rewrite does (`DELETE_ROWS` of
/// services/ingest/src/telemetry/store.rs, with fewer conditions).
const DELETE_BATCH: &str = "DELETE FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} \
     AND gateway_id = {gateway:String} AND stream_id = {stream:String} AND batch_seq = {seq:UInt64}";

/// The table's size: rows, parts, bytes of the parts (their projections included), of the
/// projections alone, and of the skip indexes.
const LOOKUP_STORAGE: &str = "SELECT
  (SELECT sum(rows) FROM system.parts WHERE database = 'trustiot' AND table = 'telemetry' AND active) AS rows,
  (SELECT count() FROM system.parts WHERE database = 'trustiot' AND table = 'telemetry' AND active) AS parts,
  (SELECT sum(bytes_on_disk) FROM system.parts WHERE database = 'trustiot' AND table = 'telemetry' AND active) AS part_bytes,
  (SELECT sum(bytes_on_disk) FROM system.projection_parts WHERE database = 'trustiot' AND table = 'telemetry' AND active) AS projection_bytes,
  (SELECT sum(data_compressed_bytes + marks_bytes) FROM system.data_skipping_indices WHERE database = 'trustiot' AND table = 'telemetry') AS skip_index_bytes
FORMAT JSONEachRow";

/// `EXPLAIN indexes = 1, projections = 1` in the URL's `query` parameter (see
/// [`EXPLAIN_PARAMETER`]).
const EXPLAIN_LOOKUP_PARAMETER: &str =
    "query=EXPLAIN%20indexes%20%3D%201%2C%20projections%20%3D%201";

/// `EXPLAIN indexes = 1, projections = 1` of a logged statement, as `reader`.
async fn explain_lookup(
    http: &reqwest::Client,
    url: &str,
    reader: &Account,
    statement: &str,
) -> String {
    let explain_url = format!("{}/?{EXPLAIN_LOOKUP_PARAMETER}", url.trim_end_matches('/'));
    String::from_utf8(
        send(
            http,
            &explain_url,
            reader,
            statement.trim_end().trim_end_matches(';'),
        )
        .await,
    )
    .unwrap()
}

/// What ClickHouse answered: the text, or the error's name.
fn outcome(answer: &Result<String, common::QueryError>) -> Value {
    match answer {
        Ok(text) => json!({ "ok": text.trim() }),
        Err(error) => json!({ "error": error.code() }),
    }
}

/// The first JSON line of a log statement's answer.
async fn log_line(
    server: &ClickHouse,
    statement: &str,
    user: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Value {
    log_lines_of(server, statement, user, window)
        .await
        .into_iter()
        .next()
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement of minutes and several hundred MB of disk (see the module's docs)"]
#[allow(
    clippy::too_many_lines,
    reason = "one measurement in order: data, candidates, checks, report"
)]
async fn received_at_lookup_candidates() {
    // The policies above name the tenants by literal.
    for (tenant, text) in [
        (TENANT, "00000000-0000-0000-0000-00000005ca1e"),
        (FOUR_GATEWAYS, "00000000-0000-0000-0000-00000005ca20"),
        (WRONG_CLOCK, "00000000-0000-0000-0000-00000005ca21"),
    ] {
        assert_eq!(tenant.to_string(), text);
    }
    let Some((server, _, _)) = start().await else {
        return;
    };
    let version = server.admin_ok("SELECT version()").await.trim().to_owned();
    let mut readers = Vec::new();
    for (user, statements) in LOOKUP_READERS {
        let reader = Account {
            user: user.to_owned(),
            password: Uuid::new_v4().simple().to_string(),
        };
        for statement in statements {
            server
                .query_with(
                    &server.admin,
                    statement,
                    &[("param_password", reader.password.as_str())],
                )
                .await
                .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
        }
        readers.push(reader);
    }
    let tenant_account = Account {
        user: LOOKUP_TENANT.to_owned(),
        password: Uuid::new_v4().simple().to_string(),
    };
    server
        .query_with(
            &server.admin,
            CREATE_LOOKUP_TENANT,
            &[("param_password", tenant_account.password.as_str())],
        )
        .await
        .unwrap_or_else(|error| panic!("{CREATE_LOOKUP_TENANT}\n{}", error.body));
    server
        .assert_default_role(LOOKUP_TENANT, "trustiot_tenant_reader")
        .await;

    // The same 30 days as the measurement above, for every gateway of every scenario.
    let end = {
        let hour = TimeDelta::hours(1).num_milliseconds();
        let ms = (Utc::now() - TimeDelta::days(2)).timestamp_millis();
        DateTime::from_timestamp_millis(ms - ms.rem_euclid(hour)).unwrap()
    };
    let range = (end - TimeDelta::days(DAYS), end);
    let start_ms = range.0.timestamp_millis();
    let batches = DAYS.unsigned_abs() * BATCHES_PER_DAY;
    let expected_rows = batches * ROWS_PER_BATCH;
    let generated = Instant::now();
    for scenario in &SCENARIOS {
        for fleet in scenario.fleets {
            generate_batches(&server, start_ms, fleet, 0, batches).await;
        }
    }
    generate_batches(&server, start_ms, &NEIGHBOUR_FLEET, batches - 60, 1).await;
    println!(
        "generated {} gateways of {expected_rows} rows in {} s",
        SCENARIOS.iter().map(|s| s.fleets.len()).sum::<usize>(),
        generated.elapsed().as_secs()
    );
    // Each export account sees its tenant's rows and completion records and nothing else.
    for (scenario, reader) in SCENARIOS.iter().zip(&readers) {
        let fleets = u64::try_from(scenario.fleets.len()).unwrap();
        for (statement, expected) in [
            (
                "SELECT count() FROM trustiot.telemetry",
                fleets * expected_rows,
            ),
            (
                "SELECT count() FROM trustiot.stream_commits",
                fleets * batches,
            ),
        ] {
            let seen = server
                .query(reader, statement)
                .await
                .unwrap_or_else(|error| panic!("{} {statement}: {}", reader.user, error.body));
            assert_eq!(
                seen.trim(),
                expected.to_string(),
                "{} {statement}",
                reader.user
            );
        }
    }

    let scratch = Scratch::new();
    let mut configs = Vec::new();
    for scenario in &SCENARIOS {
        let config = scratch.0.join(format!("{}.toml", scenario.name));
        fs::write(&config, config_text_for(None, scenario.fleets[0].devices)).unwrap();
        configs.push(config);
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let current = PathBuf::from(env!("CARGO_BIN_EXE_lab-dataset"));
    let mutations_sync = [("mutations_sync", "2")];
    let tenant_rows_in_two_days = (2 * BATCHES_PER_DAY * ROWS_PER_BATCH).to_string();

    let mut references = Vec::new();
    let mut results = Vec::new();
    for (index, (candidate, apply, remove)) in CANDIDATES.into_iter().enumerate() {
        // The same layout for every candidate: one part per monthly partition.
        server
            .query_with(
                &server.admin,
                "OPTIMIZE TABLE trustiot.telemetry FINAL",
                &mutations_sync,
            )
            .await
            .unwrap_or_else(|error| panic!("OPTIMIZE: {}", error.body));
        let applying = Instant::now();
        for statement in apply {
            server
                .query_with(&server.admin, statement, &mutations_sync)
                .await
                .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
        }
        let apply_seconds = applying.elapsed().as_secs_f64();
        let storage: Value =
            serde_json::from_str(server.admin_ok(LOOKUP_STORAGE).await.trim()).unwrap();
        println!("{candidate}: applied in {apply_seconds:.1} s, storage {storage}");

        let mut scenarios = Vec::new();
        for ((scenario, reader), config) in SCENARIOS.iter().zip(&readers).zip(&configs) {
            let fleet = scenario.fleets[0];
            let label = format!("{candidate}, {}", scenario.name);
            let out = scratch.0.join(format!("{index}-{}", scenario.name));
            println!("{label}: exporting");
            let run = run_export_of(
                &current,
                config,
                (fleet.tenant, fleet.gateway),
                range,
                &out,
                (&server.url, reader),
                None,
            )
            .await;
            assert_eq!(run.printed["rows"], expected_rows, "{label}");
            server.admin_ok("SYSTEM FLUSH LOGS").await;
            let statements = statement_kinds_of(&server, &reader.user, run.window).await;
            let projections =
                log_line(&server, LOOKUP_PROJECTIONS_LOG, &reader.user, run.window).await;
            let rows_log = log_lines_of(&server, LOOKUP_ROWS_LOG, &reader.user, run.window).await;
            let heaviest = rows_log
                .iter()
                .max_by_key(|line| number(&line["read_rows"]))
                .unwrap();
            let explain = explain_lookup(
                &http,
                &server.url,
                reader,
                heaviest["query"].as_str().unwrap(),
            )
            .await;
            println!(
                "EXPLAIN-BEGIN {label}: the window that read the most rows ({} read, {} \
                 returned)\n{explain}EXPLAIN-END {label}",
                heaviest["read_rows"], heaviest["result_rows"]
            );

            // A read by receive time only: the hour in the middle of the range.
            let hour_from = range.0 + TimeDelta::days(DAYS / 2) + TimeDelta::hours(12);
            let (from_ms, to_ms) = (
                hour_from.timestamp_millis().to_string(),
                (hour_from + TimeDelta::hours(1))
                    .timestamp_millis()
                    .to_string(),
            );
            let tenant = fleet.tenant.to_string();
            let hour = server
                .query_with_summary(
                    reader,
                    LOOKUP_HOUR,
                    &[
                        ("param_tenant", tenant.as_str()),
                        ("param_gateway", fleet.gateway),
                        ("param_from_ms", from_ms.as_str()),
                        ("param_to_ms", to_ms.as_str()),
                    ],
                )
                .await;
            let hour_rows = hour
                .result
                .unwrap_or_else(|error| panic!("{LOOKUP_HOUR}\n{}", error.body));
            assert_eq!(hour_rows.trim(), "600", "{label}");

            // The files are those of the table without a candidate, byte for byte.
            let files = if index == 0 {
                references.push(out.clone());
                dataset_paths(&out.join("scale")).len()
            } else {
                let reference = references[scenarios.len()].join("scale");
                let files = compare_files(&label, &out.join("scale"), &reference);
                fs::remove_dir_all(&out).unwrap();
                files.len()
            };
            let summary = json!({
                "scenario": scenario.name,
                "gateways_of_the_tenant": scenario.fleets.len(),
                "rows": expected_rows,
                "windows": run.printed["windows"],
                "statements": statements,
                "statements_with_projection": projections,
                "heaviest_window": {
                    "read_rows": number(&heaviest["read_rows"]),
                    "result_rows": number(&heaviest["result_rows"]),
                    "projection": number(&heaviest["projection"]) == 1,
                },
                "hour_by_receive_time": { "rows": 600, "read_rows": hour.read_rows },
                "files_identical_to_none": files,
            });
            println!("{label}: {summary}");
            scenarios.push(summary);
        }

        // A tenant's read through its view, over the last two days of TENANT's rows; the
        // neighbouring tenant's batch arrived within them.
        let from_ms = (end - TimeDelta::days(2)).timestamp_millis().to_string();
        let tenant = TENANT.to_string();
        let began = Utc::now();
        let isolation = server
            .query_with(
                &tenant_account,
                LOOKUP_ISOLATION,
                &[
                    ("param_from_ms", from_ms.as_str()),
                    ("param_tenant", tenant.as_str()),
                    ("param_gateways", ISOLATION_GATEWAYS),
                ],
            )
            .await;
        let ended = Utc::now();
        if candidate != "projection" {
            assert_eq!(
                isolation
                    .as_ref()
                    .map(|text| text.trim())
                    .map_err(|error| error.body.as_str()),
                Ok(tenant_rows_in_two_days.as_str()),
                "{candidate}: the tenant view must keep the condition off other tenants' rows"
            );
        }
        let isolation = outcome(&isolation);
        server.admin_ok("SYSTEM FLUSH LOGS").await;
        let isolation_log = log_line(
            &server,
            LOOKUP_PROJECTIONS_LOG,
            LOOKUP_TENANT,
            (began, ended),
        )
        .await;
        println!("{candidate}: tenant view with throwIf {isolation}, {isolation_log}");

        // The ingest service's batch delete.
        let first = batches - 30 + u64::try_from(index).unwrap();
        generate_batches(&server, start_ms, &DELETED_FLEET, first, 1).await;
        let (deleted_tenant, seq) = (DELETED.to_string(), (first + 1).to_string());
        let delete = outcome(
            &server
                .query_with(
                    &server.admin,
                    DELETE_BATCH,
                    &[
                        ("param_tenant", deleted_tenant.as_str()),
                        ("param_gateway", DELETED_FLEET.gateway),
                        ("param_stream", STREAM),
                        ("param_seq", seq.as_str()),
                        ("lightweight_deletes_sync", "2"),
                    ],
                )
                .await,
        );
        println!("{candidate}: lightweight delete of a batch {delete}");

        if let Some(statement) = remove {
            server
                .query_with(&server.admin, statement, &mutations_sync)
                .await
                .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
        }
        results.push(json!({
            "candidate": candidate,
            "apply_seconds": apply_seconds,
            "storage": storage,
            "scenarios": scenarios,
            "tenant_view_throw_if": { "answer": isolation, "log": isolation_log },
            "lightweight_delete": delete,
        }));
    }

    let report = json!({
        "clickhouse": version,
        "data": {
            "days": DAYS,
            "rows_per_gateway": expected_rows,
            "scenarios": SCENARIOS.iter().map(|scenario| json!({
                "name": scenario.name,
                "gateways": scenario.fleets.iter().map(|fleet| fleet.gateway).collect::<Vec<_>>(),
                "skewed_batches": [scenario.fleets[0].skew.0, scenario.fleets[0].skew.1],
                "skew_ms": scenario.fleets[0].skew.2,
            })).collect::<Vec<_>>(),
        },
        "candidates": results,
    });
    println!(
        "LOOKUP-REPORT-BEGIN\n{}\nLOOKUP-REPORT-END",
        serde_json::to_string_pretty(&report).unwrap()
    );
}
