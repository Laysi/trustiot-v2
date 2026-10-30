//! Exports the synthetic fixture back out of a throwaway ClickHouse with the product's schema
//! (Laysi/trustiot-v2#816): the fixture is loaded (shifted to the present) as the ingest service
//! would write it, `lab-dataset export` reads it back as a dataset, and the result must equal
//! the fixture minus the load's time shift: the readings, the batches (stream, number, receive
//! time), the uncommitted batch, the devices and the series. The exported dataset passes
//! `Dataset::open`, and neither its index nor its segment files name an address, a tenant, a
//! gateway, a device ID or a stream ID.
//!
//! The readers are accounts of the throwaway server with only `SELECT` on the two tables and a
//! row policy that names them, the account `datasets/lab/README.md` (「匯出」) asks for; the
//! administrator, which no policy names, reads no rows and the export refuses it.
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`: the test creates accounts and row
//! policies that read every row, which a shared server must not keep.

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
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use lab_dataset::export::{Config, Exported, Exporter};
use lab_dataset::{Dataset, Loaded, Reading, Target, TimeShift, Value, cli, synthetic_index, time};
use uuid::Uuid;

use self::common::{Account, ClickHouse};

const TENANT_A: Uuid = Uuid::from_u128(0xa);
const TENANT_B: Uuid = Uuid::from_u128(0xb);
const TENANT_C: Uuid = Uuid::from_u128(0xc);
const GATEWAY_A: Uuid = Uuid::from_u128(0x1a);
const GATEWAY_B: Uuid = Uuid::from_u128(0x1b);
const GATEWAY_C: Uuid = Uuid::from_u128(0x1c);

/// A reader whose policy names every tenant.
const ALL: &str = "lab_export_all";
/// A reader whose policy names only tenant A.
const ONLY_A: &str = "lab_export_a";
/// A reader that may read the telemetry rows but is not named on the completion table.
const NO_COMMITS: &str = "lab_export_nocommits";

const CREATE_ALL: &str = "CREATE USER lab_export_all IDENTIFIED WITH sha256_password BY \
     {password:String} HOST ANY";
const CREATE_ONLY_A: &str = "CREATE USER lab_export_a IDENTIFIED WITH sha256_password BY \
     {password:String} HOST ANY";
const CREATE_NO_COMMITS: &str = "CREATE USER lab_export_nocommits IDENTIFIED WITH \
     sha256_password BY {password:String} HOST ANY";

/// Only `SELECT`: nothing the export sends could write.
const GRANTS: [&str; 6] = [
    "GRANT SELECT ON trustiot.telemetry TO lab_export_all",
    "GRANT SELECT ON trustiot.stream_commits TO lab_export_all",
    "GRANT SELECT ON trustiot.telemetry TO lab_export_a",
    "GRANT SELECT ON trustiot.stream_commits TO lab_export_a",
    "GRANT SELECT ON trustiot.telemetry TO lab_export_nocommits",
    "GRANT SELECT ON trustiot.stream_commits TO lab_export_nocommits",
];

const POLICIES: [&str; 5] = [
    "CREATE ROW POLICY lab_export_all ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 \
     TO lab_export_all",
    "CREATE ROW POLICY lab_export_all ON trustiot.stream_commits AS PERMISSIVE FOR SELECT \
     USING 1 TO lab_export_all",
    "CREATE ROW POLICY lab_export_a ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING \
     tenant_id = toUUID('00000000-0000-0000-0000-00000000000a') TO lab_export_a",
    "CREATE ROW POLICY lab_export_a ON trustiot.stream_commits AS PERMISSIVE FOR SELECT USING \
     tenant_id = toUUID('00000000-0000-0000-0000-00000000000a') TO lab_export_a",
    "CREATE ROW POLICY lab_export_nocommits ON trustiot.telemetry AS PERMISSIVE FOR SELECT \
     USING 1 TO lab_export_nocommits",
];

/// What the ingest service inserts, to put a row the loader would not write.
const INSERT_TELEMETRY: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, \
     stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, \
     value_int, value_bool, value_string) FORMAT JSONEachRow";

fn random_hex() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// A scratch directory outside the repository, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("lab-export-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The export configuration of the fixture: its devices, models and streams as the operator
/// would write them, with the IDs the load gave them. The converter's address is one that must
/// not reach the index.
fn config_text(loaded: &Loaded) -> String {
    let device = |key: &str| loaded.device(key).unwrap();
    let stream = |key: &str| loaded.stream(key).unwrap();
    format!(
        r#"
name = "synthetic"
description = "Round trip of the synthetic fixture"

[[converters]]
key = "rs485-a"
host = "192.0.2.10"
port = 5020

[[converters]]
key = "rs485-b"
host = "192.0.2.11"
port = 5020

[[streams]]
key = "live"
id = "{live}"

[[streams]]
key = "backlog"
id = "{backlog}"
description = "Buffered during the uplink outage"

[[models]]
name = "three-phase-meter"
points = [
  {{ key = "energy_kwh", function_code = 3, address = 0, data_type = "f32", kind = "cumulative", unit = "kWh" }},
  {{ key = "power_kw", function_code = 4, address = 0, data_type = "f32", kind = "gauge", unit = "kW" }},
]

[[models]]
name = "pulse-meter"
points = [
  {{ key = "pulses", function_code = 3, address = 0, data_type = "u32", kind = "cumulative", wraps_at = 4294967296 }},
  {{ key = "power_kw", function_code = 4, address = 0, data_type = "f32", kind = "gauge", unit = "kW" }},
]

[[models]]
name = "press-controller"
points = [
  {{ key = "running", function_code = 1, address = 0, data_type = "bool", kind = "state" }},
  {{ key = "mode", function_code = 3, address = 10, data_type = "string", length = 2, kind = "state" }},
  {{ key = "cycle_count", function_code = 3, address = 20, data_type = "u32", kind = "cumulative" }},
]

[[devices]]
key = "meter-1"
id = "{meter_1}"
model = "three-phase-meter"
converter = "rs485-a"
unit = 1
interval_s = 300

[[devices]]
key = "meter-2"
id = "{meter_2}"
model = "pulse-meter"
converter = "rs485-a"
unit = 2
interval_s = 300

[[devices]]
key = "press-1"
id = "{press_1}"
model = "press-controller"
converter = "rs485-b"
unit = 1
interval_s = 300
"#,
        live = stream("live"),
        backlog = stream("backlog"),
        meter_1 = device("meter-1"),
        meter_2 = device("meter-2"),
        press_1 = device("press-1"),
    )
}

/// The fixture's readings with the load's shift applied, in a canonical order: arrival, then
/// device, point and reading time (a batch's order within itself is not kept by ClickHouse).
fn shifted(readings: Vec<Reading>, shift: TimeShift) -> Vec<Reading> {
    let mut readings: Vec<Reading> = readings
        .into_iter()
        .map(|reading| Reading {
            received_at: shift.apply(reading.received_at).unwrap(),
            ts: shift.apply(reading.ts).unwrap(),
            ..reading
        })
        .collect();
    readings.sort_by(|a, b| {
        (
            a.received_at,
            &a.stream,
            a.batch_seq,
            &a.device,
            &a.point,
            a.ts,
        )
            .cmp(&(
                b.received_at,
                &b.stream,
                b.batch_seq,
                &b.device,
                &b.point,
                b.ts,
            ))
    });
    readings
}

fn uncommitted(dataset: &Dataset) -> BTreeSet<(String, u64)> {
    dataset
        .index()
        .segments
        .iter()
        .flat_map(|segment| &segment.uncommitted)
        .map(|batch| (batch.stream.clone(), batch.batch_seq))
        .collect()
}

/// The first and last batch of each stream over the dataset's segments.
fn batch_ranges(dataset: &Dataset) -> BTreeMap<String, (u64, u64)> {
    let mut ranges: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for range in dataset
        .index()
        .segments
        .iter()
        .flat_map(|segment| &segment.batches)
    {
        ranges
            .entry(range.stream.clone())
            .and_modify(|(first, last)| {
                *first = (*first).min(range.first);
                *last = (*last).max(range.last);
            })
            .or_insert((range.first, range.last));
    }
    ranges
}

fn series(dataset: &Dataset) -> BTreeMap<(String, String), u64> {
    let mut counts = BTreeMap::new();
    for series in dataset
        .index()
        .segments
        .iter()
        .flat_map(|segment| &segment.series)
    {
        *counts
            .entry((series.device.clone(), series.point.clone()))
            .or_default() += series.rows;
    }
    counts
}

/// Everything the dataset's files say, to look for what must not be there.
fn files_text(dataset_dir: &Path) -> String {
    let mut text = fs::read_to_string(dataset_dir.join("index.toml")).unwrap();
    let mut segments: Vec<PathBuf> = fs::read_dir(dataset_dir.join("segments"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    segments.sort();
    for segment in segments {
        text.push_str(&fs::read_to_string(segment).unwrap());
    }
    text
}

async fn start() -> Option<ClickHouse> {
    let server = ClickHouse::start_throwaway().await?;
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
    Some(server)
}

/// Creates the three readers; returns their accounts by name.
async fn create_readers(server: &ClickHouse) -> BTreeMap<&'static str, Account> {
    let mut accounts = BTreeMap::new();
    for (name, create) in [
        (ALL, CREATE_ALL),
        (ONLY_A, CREATE_ONLY_A),
        (NO_COMMITS, CREATE_NO_COMMITS),
    ] {
        let account = Account {
            user: name.to_owned(),
            password: random_hex(),
        };
        server
            .query_with(
                &server.admin,
                create,
                &[("param_password", account.password.as_str())],
            )
            .await
            .unwrap_or_else(|error| panic!("{create}\n{}", error.body));
        accounts.insert(name, account);
    }
    for statement in GRANTS.into_iter().chain(POLICIES) {
        server.admin_ok(statement).await;
    }
    accounts
}

fn clickhouse(server: &ClickHouse, account: &Account) -> lab_dataset::ClickHouse {
    lab_dataset::ClickHouse::new(&server.url, &account.user, &account.password, None).unwrap()
}

async fn export(
    server: &ClickHouse,
    account: &Account,
    config: &str,
    tenant: Uuid,
    gateway: Uuid,
    range: (DateTime<Utc>, DateTime<Utc>),
    out: &Path,
) -> Result<Exported, lab_dataset::Error> {
    Exporter::new(&Config::parse(config).unwrap())
        .unwrap()
        .run(
            &clickhouse(server, account),
            tenant,
            &gateway.to_string(),
            range.0,
            range.1,
            out,
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_survives_a_load_and_an_export() {
    let Some(server) = start().await else {
        return;
    };
    let readers = create_readers(&server).await;
    let writer = lab_dataset::ClickHouse::new(
        &server.url,
        &server.admin.user,
        &server.admin.password,
        None,
    )
    .unwrap();
    let dataset = Dataset::open(synthetic_index()).unwrap();
    let shift = dataset
        .shift_ending_before(Utc::now(), TimeDelta::days(1))
        .unwrap();
    let a = dataset
        .load(&writer, &Target::new(TENANT_A, GATEWAY_A), shift)
        .await
        .unwrap();
    let b = dataset
        .load(&writer, &Target::new(TENANT_B, GATEWAY_B), shift)
        .await
        .unwrap();
    let all = &readers[ALL];
    let expected = shifted(dataset.readings().unwrap(), shift);
    let (start, end) = (
        shift.apply(dataset.span().0).unwrap(),
        shift.apply(dataset.end()).unwrap(),
    );
    let whole = (start - TimeDelta::hours(2), end + TimeDelta::hours(2));
    let scratch = Scratch::new();

    // The whole dataset of tenant A, as the library exports it.
    let out = scratch.0.join("whole");
    let exported = export(
        &server,
        all,
        &config_text(&a),
        TENANT_A,
        GATEWAY_A,
        whole,
        &out,
    )
    .await
    .unwrap();
    let index_path = out.join("synthetic").join("index.toml");
    assert_eq!(exported.index, index_path);
    let result = Dataset::open(&index_path).expect("the export passes Dataset::open");
    assert_eq!(
        (
            exported.rows,
            exported.batches,
            exported.uncommitted_batches
        ),
        (316, 16, 1)
    );
    assert_eq!(exported.repeated_rows_dropped, 0);
    assert_eq!(
        shifted(result.readings().unwrap(), TimeShift::NONE),
        expected
    );
    assert_eq!(uncommitted(&result), uncommitted(&dataset));
    assert_eq!(
        uncommitted(&result),
        BTreeSet::from([("live".to_owned(), 13)])
    );
    assert_eq!(batch_ranges(&result), batch_ranges(&dataset));
    assert_eq!(series(&result), series(&dataset));
    assert_eq!(
        format!("{:?}", result.index().devices),
        format!("{:?}", dataset.index().devices)
    );
    let streams: Vec<(&str, &str)> = result
        .index()
        .streams
        .iter()
        .map(|stream| (stream.key.as_str(), stream.description.as_str()))
        .collect();
    assert_eq!(
        streams,
        [
            ("live", "Batches of a gateway stream, as exported"),
            ("backlog", "Buffered during the uplink outage")
        ]
    );
    assert_eq!(
        (result.span().0, result.end()),
        (start, end),
        "reading and receive times are the stored ones"
    );
    println!(
        "round trip: {} rows, {} batches ({} uncommitted) read back equal to the fixture \
         shifted by {} ms; streams {:?}; Dataset::open passes",
        exported.rows,
        exported.batches,
        exported.uncommitted_batches,
        a.shift_ms,
        batch_ranges(&result)
    );

    // Nothing that identifies where the data came from is in the files.
    let text = files_text(&out.join("synthetic"));
    let mut forbidden = vec![
        "192.0.2.".to_owned(),
        "host =".to_owned(),
        "port =".to_owned(),
        TENANT_A.to_string(),
        GATEWAY_A.to_string(),
    ];
    forbidden.extend(a.devices.values().map(ToString::to_string));
    forbidden.extend(a.streams.values().map(ToString::to_string));
    for word in &forbidden {
        assert!(
            !text.contains(word.as_str()),
            "the export contains `{word}`"
        );
    }
    println!(
        "no address, port, tenant, gateway, device ID or stream ID among the {} bytes of the \
         index and segment files",
        text.len()
    );

    // A narrower range, through the command: batches 3 to 6 of `live` only (a range starts
    // anywhere in a stream), with the account's credentials in the environment.
    let received = |stream: &str, seq: u64| {
        expected
            .iter()
            .find(|reading| reading.stream == stream && reading.batch_seq == seq)
            .unwrap()
            .received_at
    };
    let (from, to) = (received("live", 3), received("live", 7));
    let config_path = scratch.0.join("export.toml");
    fs::write(&config_path, config_text(&a)).unwrap();
    let narrow_out = scratch.0.join("narrow");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_lab-dataset"))
        .arg("export")
        .arg(&config_path)
        .args(["--tenant", &TENANT_A.to_string()])
        .args(["--gateway", &GATEWAY_A.to_string()])
        .args(["--from", &time::format(from)])
        .args(["--to", &time::format(to)])
        .arg("--out")
        .arg(&narrow_out)
        .env(cli::URL_ENV, &server.url)
        .env(cli::USER_ENV, &all.user)
        .env(cli::PASSWORD_ENV, &all.password)
        .env_remove(cli::CA_FILE_ENV)
        .output()
        .await
        .expect("run lab-dataset");
    assert!(
        output.status.success(),
        "lab-dataset export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    println!("lab-dataset export printed:\n{printed:#}");
    let narrow_index = narrow_out.join("synthetic").join("index.toml");
    let narrow = Dataset::open(&narrow_index).unwrap();
    let slice: Vec<Reading> = expected
        .iter()
        .filter(|reading| reading.stream == "live" && (3..7).contains(&reading.batch_seq))
        .cloned()
        .collect();
    assert_eq!(shifted(narrow.readings().unwrap(), TimeShift::NONE), slice);
    assert_eq!(
        batch_ranges(&narrow),
        BTreeMap::from([("live".to_owned(), (3, 6))])
    );
    assert!(uncommitted(&narrow).is_empty());
    assert_eq!(printed["batches"], 4);
    assert_eq!(printed["rows"], u64::try_from(slice.len()).unwrap());
    // The command's output is a dataset `check` accepts too.
    let checked = tokio::process::Command::new(env!("CARGO_BIN_EXE_lab-dataset"))
        .arg("check")
        .arg(&narrow_index)
        .output()
        .await
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    // An account no row policy names reads no rows: the administrator, and a reader whose
    // policy names another tenant. The export fails and writes nothing.
    for (who, account, tenant, gateway, loaded) in [
        ("the administrator", &server.admin, TENANT_A, GATEWAY_A, &a),
        (
            "a reader of tenant A",
            &readers[ONLY_A],
            TENANT_B,
            GATEWAY_B,
            &b,
        ),
    ] {
        let out = scratch.0.join("refused");
        let error = export(
            &server,
            account,
            &config_text(loaded),
            tenant,
            gateway,
            whole,
            &out,
        )
        .await
        .expect_err(who)
        .to_string();
        assert!(error.contains("returned no rows"), "{who}: {error}");
        assert!(
            !out.exists(),
            "{who}: a refused export wrote {}",
            out.display()
        );
        println!("{who} reads 0 rows: {error}");
    }
    // The reader named only for tenant A exports tenant A.
    let out = scratch.0.join("only-a");
    let only_a = export(
        &server,
        &readers[ONLY_A],
        &config_text(&a),
        TENANT_A,
        GATEWAY_A,
        whole,
        &out,
    )
    .await
    .unwrap();
    assert_eq!(only_a.rows, 316);
    // A reader that sees the rows but no completion records would list every batch as
    // uncommitted: refused.
    let out = scratch.0.join("no-commits");
    let error = export(
        &server,
        &readers[NO_COMMITS],
        &config_text(&a),
        TENANT_A,
        GATEWAY_A,
        whole,
        &out,
    )
    .await
    .expect_err("no completion records")
    .to_string();
    assert!(
        error.contains("no batch has a completion record"),
        "{error}"
    );
    assert!(!out.exists());
    // The destination of an earlier export is never written into.
    let error = export(
        &server,
        all,
        &config_text(&a),
        TENANT_A,
        GATEWAY_A,
        whole,
        &scratch.0.join("whole"),
    )
    .await
    .expect_err("existing dataset")
    .to_string();
    assert!(error.contains("exists"), "{error}");

    // A configuration that lacks a device is refused, not exported without it.
    let missing = config_text(&a).replace(
        &format!("id = \"{}\"", a.device("press-1").unwrap()),
        "id = \"00000000-0000-0000-0000-0000000000ff\"",
    );
    let out = scratch.0.join("missing");
    let error = export(&server, all, &missing, TENANT_A, GATEWAY_A, whole, &out)
        .await
        .expect_err("a device without a description")
        .to_string();
    assert!(error.contains("does not list"), "{error}");
    assert!(!out.exists());

    // Real data that breaks the format. Tenant C holds the fixture plus rows the ingest
    // service could have left: a row stored twice with the same value, then again with another.
    let c = dataset
        .load(&writer, &Target::new(TENANT_C, GATEWAY_C), shift)
        .await
        .unwrap();
    let first = &expected[0];
    assert_eq!((first.stream.as_str(), first.batch_seq), ("live", 1));
    let Value::Float(value) = first.value else {
        panic!("the fixture's first reading is a float");
    };
    let duplicate = |value: f64| {
        let clickhouse_time =
            |time: DateTime<Utc>| time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        serde_json::json!({
            "tenant_id": TENANT_C,
            "gateway_id": GATEWAY_C.to_string(),
            "stream_id": c.stream("live").unwrap().to_string(),
            "batch_seq": 1,
            "device_id": c.device("meter-1").unwrap(),
            "point": "energy_kwh",
            "ts": clickhouse_time(first.ts),
            "received_at": clickhouse_time(first.received_at),
            "value_type": "float",
            "value_float": value,
            "value_int": 0,
            "value_bool": false,
            "value_string": "",
        })
        .to_string()
            + "\n"
    };
    let insert = server
        .insert_data(&server.admin, INSERT_TELEMETRY, duplicate(value), &[])
        .await;
    insert.result.unwrap();
    let out = scratch.0.join("repeated");
    // The completion record counts the batch's rows once, so the repeat is a row more than the
    // record says until the export collapses it.
    let repeated = export(
        &server,
        all,
        &config_text(&c),
        TENANT_C,
        GATEWAY_C,
        whole,
        &out,
    )
    .await
    .unwrap();
    assert_eq!(repeated.rows, 316);
    assert_eq!(repeated.repeated_rows_dropped, 1);
    let result = Dataset::open(out.join("synthetic").join("index.toml")).unwrap();
    assert_eq!(
        shifted(result.readings().unwrap(), TimeShift::NONE),
        expected
    );
    println!(
        "a row stored twice in batch live/1: exported once ({} rows, {} repeated row dropped)",
        repeated.rows, repeated.repeated_rows_dropped
    );
    let insert = server
        .insert_data(&server.admin, INSERT_TELEMETRY, duplicate(value + 1.0), &[])
        .await;
    insert.result.unwrap();
    let out = scratch.0.join("conflicting");
    let error = export(
        &server,
        all,
        &config_text(&c),
        TENANT_C,
        GATEWAY_C,
        whole,
        &out,
    )
    .await
    .expect_err("a reading stored with two values")
    .to_string();
    assert!(error.contains("different values"), "{error}");
    assert!(!out.exists());
    println!("a reading stored with two values is refused: {error}");
}
