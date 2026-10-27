//! The recorder against testkit's fake RS485 converter (Laysi/trustiot-v2#679): two devices with
//! units 1 and 2 behind one RTU-over-TCP endpoint, read through the gateway agent's Modbus
//! module. The recording passes `Dataset::open`, holds exactly the values the converter served
//! at the times they were read, names the converter only by its logical key, and the loader
//! writes it into a throwaway ClickHouse with the product's schema with the index's counts.
//!
//! The recording runs on a paused tokio clock that moves only when the test advances it
//! (`testkit::clock::manual`): every poll ends before the clock moves on, so the reading times
//! are exact and nothing depends on how loaded the machine is. The ClickHouse part runs on a
//! normal runtime afterwards (it waits for a container in wall time).

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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::modbus::endpoint::Endpoints;
use chrono::{DateTime, TimeDelta, Utc};
use lab_dataset::record::{Config, Recorded, Recorder};
use lab_dataset::{Dataset, Target, Value, time};
use testkit::clock::{self, bounded};
use testkit::modbus::{FakeModbus, Transport};
use tokio::sync::oneshot;
use tokio::time::advance;
use uuid::Uuid;

use self::common::{Account, ClickHouse};

/// When the recording starts, on the recorder's clock.
const WALL_START: &str = "2026-03-02T08:00:00.000Z";
/// The last tick (seconds after the start) the test lets the devices poll at.
const TICKS: u32 = 10;
/// The meter's energy register starts close to the 32-bit wrap, to cross it.
const ENERGY_START: u64 = 4_294_967_000;

/// The configuration, as the operator would keep it outside the repository: the converter's
/// address is the fake endpoint's.
fn config(port: u16) -> String {
    format!(
        r#"
name = "bench"
description = "Two devices behind one RS485 converter, recorded from testkit's fake converter"
batch_s = 4
segment_bytes = 1024

[[converters]]
key = "rs485-a"
host = "127.0.0.1"
port = {port}

[[models]]
name = "three-phase-meter"
points = [
  {{ key = "energy_wh", function_code = 3, address = 0, data_type = "u32", kind = "cumulative", unit = "Wh", wraps_at = 4294967296 }},
  {{ key = "power_kw", function_code = 3, address = 2, data_type = "f32", kind = "gauge", unit = "kW" }},
]

[[models]]
name = "press-controller"
points = [
  {{ key = "strokes", function_code = 4, address = 0, data_type = "u16", kind = "cumulative", wraps_at = 65536 }},
  {{ key = "running", function_code = 1, address = 0, data_type = "bool", kind = "state" }},
  {{ key = "mode", function_code = 3, address = 10, data_type = "string", length = 2, kind = "state" }},
]

[[devices]]
key = "meter-1"
model = "three-phase-meter"
converter = "rs485-a"
unit = 1
interval_s = 1

[[devices]]
key = "press-1"
model = "press-controller"
converter = "rs485-a"
unit = 2
interval_s = 2
"#
    )
}

/// What the fake converter serves at `tick` seconds, and what the recording must hold for it.
struct Served {
    energy: u64,
    power: f32,
    strokes: u16,
    running: bool,
    mode: &'static str,
}

fn served(tick: u32) -> Served {
    Served {
        energy: (ENERGY_START + 50 * u64::from(tick)) % (1 << 32),
        power: 40.5 + 0.25 * f32::from(u16::try_from(tick).unwrap()),
        strokes: u16::try_from((65_530 + 3 * tick) % 65_536).unwrap(),
        running: tick % 4 != 2,
        mode: if tick % 4 == 2 { "STOP" } else { "AUTO" },
    }
}

fn serve(endpoint: &FakeModbus, tick: u32) {
    let values = served(tick);
    let energy = u32::try_from(values.energy).unwrap();
    let power = values.power.to_bits();
    let halves = |word: u32| {
        [
            u16::try_from(word >> 16).unwrap(),
            u16::try_from(word & 0xffff).unwrap(),
        ]
    };
    let ([e1, e2], [p1, p2]) = (halves(energy), halves(power));
    endpoint.set_holding_registers(1, 0, &[e1, e2, p1, p2]);
    endpoint.set_input_registers(2, 0, &[values.strokes]);
    endpoint.set_coils(2, 0, &[values.running]);
    let mode = values.mode.as_bytes();
    endpoint.set_holding_registers(
        2,
        10,
        &[
            u16::from_be_bytes([mode[0], mode[1]]),
            u16::from_be_bytes([mode[2], mode[3]]),
        ],
    );
}

/// Each (device, point) and its (time, value) pairs in reading-time order.
type Series = BTreeMap<(String, String), Vec<(DateTime<Utc>, Value)>>;

/// Every (device, point) and its (time, value) pairs the recording must hold.
fn expected() -> Series {
    let start = time::parse(WALL_START).unwrap();
    let mut expected = Series::new();
    let mut add = |device: &str, point: &str, tick: u32, value: Value| {
        expected
            .entry((device.to_owned(), point.to_owned()))
            .or_default()
            .push((start + TimeDelta::seconds(i64::from(tick)), value));
    };
    for tick in 0..=TICKS {
        let values = served(tick);
        add(
            "meter-1",
            "energy_wh",
            tick,
            Value::Int(i64::try_from(values.energy).unwrap()),
        );
        add(
            "meter-1",
            "power_kw",
            tick,
            Value::Float(f64::from(values.power)),
        );
        if tick % 2 == 0 {
            add(
                "press-1",
                "strokes",
                tick,
                Value::Int(i64::from(values.strokes)),
            );
            add("press-1", "running", tick, Value::Bool(values.running));
            add(
                "press-1",
                "mode",
                tick,
                Value::String(values.mode.to_owned()),
            );
        }
    }
    expected
}

/// The recorder's endpoints with testkit's fake converter on 127.0.0.1 let through: the agent's
/// address guard refuses loopback (Laysi/trustiot-v2#653). `exempt_for_tests` exists here only
/// because this crate's dev-dependency on the agent enables `test-exempt-addresses`.
fn endpoints() -> Endpoints {
    Recorder::endpoints().exempt_for_tests([Ipv4Addr::LOCALHOST.into()])
}

/// Polls that have ended once every device polled at `tick`: the meter every second, the press
/// every other second, both first at 0.
fn polls_by(tick: u32) -> u64 {
    let tick = u64::from(tick);
    (tick + 1) + (tick / 2 + 1)
}

/// Records on a paused clock. Returns the summary, the request log's function codes and units,
/// and the number of connections the converter accepted.
async fn record(config_path: &Path, out: &Path) -> (Recorded, BTreeSet<(u8, u8)>, usize) {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    serve(&endpoint, 0);
    fs::write(config_path, config(endpoint.address().port())).unwrap();
    let recorder = Recorder::new(Config::read(config_path).unwrap()).unwrap();
    let mut polls = recorder.polls();
    let (stop, stopped) = oneshot::channel::<()>();
    let start = time::parse(WALL_START).unwrap();
    let run = recorder.run(endpoints(), out, start, async {
        let _ = stopped.await;
    });
    let drive = async {
        for tick in 0..=TICKS {
            if tick > 0 {
                serve(&endpoint, tick);
                advance(Duration::from_secs(1)).await;
            }
            let due = polls_by(tick);
            bounded(
                format!("{due} polls by second {tick}"),
                polls.wait_for(|count| *count >= due),
            )
            .await
            .unwrap();
        }
        stop.send(()).unwrap();
    };
    let (recorded, ()) = tokio::join!(run, drive);
    let recorded = recorded.unwrap();
    let requests: BTreeSet<(u8, u8)> = endpoint
        .requests()
        .iter()
        .map(|request| (request.function, request.unit))
        .collect();
    (recorded, requests, endpoint.connections().len())
}

/// A scratch directory outside the repository, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("lab-record-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Checks the recording's files; returns the dataset.
fn check_files(recorded: &Recorded, out: &Path) -> Dataset {
    let index_path = out.join("bench").join("index.toml");
    assert_eq!(recorded.index, index_path);
    let dataset = Dataset::open(&index_path).expect("the recording passes Dataset::open");
    let index = dataset.index();

    // Only logical converter keys and unit IDs: no address, no port.
    let text = fs::read_to_string(&index_path).unwrap();
    println!("index.toml as recorded:\n{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(!text.contains("host") && !text.contains("port"), "{text}");
    let devices: Vec<(&str, Option<&str>, u8)> = index
        .devices
        .iter()
        .map(|device| {
            (
                device.key.as_str(),
                device.converter.as_deref(),
                device.unit,
            )
        })
        .collect();
    assert_eq!(
        devices,
        [
            ("meter-1", Some("rs485-a"), 1),
            ("press-1", Some("rs485-a"), 2)
        ]
    );

    // The readings are what the converter served, at the second each poll ran.
    let readings = dataset.readings().unwrap();
    let mut found = Series::new();
    for reading in &readings {
        assert!(reading.ts <= reading.received_at, "{reading:?}");
        assert_eq!(reading.stream, "live");
        found
            .entry((reading.device.clone(), reading.point.clone()))
            .or_default()
            .push((reading.ts, reading.value.clone()));
    }
    for values in found.values_mut() {
        values.sort_by_key(|(ts, _)| *ts);
    }
    assert_eq!(found, expected());

    // Several segments (a segment closes before a batch that would take it past 1 KiB; each
    // batch here is larger, so each gets its own), batches numbered from 1 without a gap
    // (checked by Dataset::open), closed every 4 s and at the stop.
    let rows: u64 = index.segments.iter().map(|segment| segment.rows).sum();
    assert_eq!(rows, recorded.rows);
    assert_eq!(rows, 40);
    assert_eq!(index.segments.len(), 3);
    assert!(index.segments.iter().all(|segment| segment.bytes > 1024));
    let receive_times: BTreeSet<String> = readings
        .iter()
        .map(|reading| time::format(reading.received_at))
        .collect();
    assert_eq!(
        receive_times.into_iter().collect::<Vec<_>>(),
        [
            "2026-03-02T08:00:04.000Z",
            "2026-03-02T08:00:08.000Z",
            "2026-03-02T08:00:10.000Z"
        ]
    );
    assert_eq!(recorded.batches, 3);
    assert_eq!(recorded.dropped_readings, 0);
    dataset
}

const READER: &str = "lab_record_test_reader";
const SETUP: [&str; 5] = [
    "CREATE USER lab_record_test_reader IDENTIFIED WITH sha256_password BY {password:String} HOST ANY",
    "GRANT SELECT ON trustiot.telemetry TO lab_record_test_reader",
    "GRANT SELECT ON trustiot.stream_commits TO lab_record_test_reader",
    "CREATE ROW POLICY lab_record_test_all_rows ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 TO lab_record_test_reader",
    "CREATE ROW POLICY lab_record_test_all_rows ON trustiot.stream_commits AS PERMISSIVE FOR SELECT USING 1 TO lab_record_test_reader",
];
const SERIES: &str = "SELECT toString(device_id), point, count()
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} GROUP BY device_id, point FORMAT TSV";
const COMMITS: &str = "SELECT count(), sum(row_count)
FROM trustiot.stream_commits FINAL WHERE tenant_id = {tenant:UUID} FORMAT TSV";

/// Loads the recording into a throwaway ClickHouse and compares the counts with the index.
async fn load(dataset: &Dataset) {
    let Some(server) = ClickHouse::start_throwaway().await else {
        return;
    };
    for migration in migrate::registry::PRODUCT.clickhouse {
        for statement in migrate::clickhouse::split_statements(migration.sql).unwrap() {
            server
                .admin(&statement)
                .await
                .unwrap_or_else(|error| panic!("{}: {statement}\n{}", migration.name, error.body));
        }
    }
    let reader = Account {
        user: READER.to_owned(),
        password: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
    };
    for statement in SETUP {
        server
            .query_with(
                &server.admin,
                statement,
                &[("param_password", reader.password.as_str())],
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
    }
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
    let loaded = dataset
        .load(&writer, &Target::new(Uuid::new_v4(), Uuid::new_v4()), shift)
        .await
        .unwrap();
    let tenant = loaded.tenant.to_string();
    let query = |sql: &'static str| {
        let (server, reader, tenant) = (&server, &reader, tenant.clone());
        async move {
            server
                .query_with(reader, sql, &[("param_tenant", &tenant)])
                .await
                .unwrap_or_else(|error| panic!("{sql}\n{}", error.body))
        }
    };
    let names: BTreeMap<String, &str> = loaded
        .devices
        .iter()
        .map(|(key, id)| (id.to_string(), key.as_str()))
        .collect();
    let counted: BTreeMap<(String, String), u64> = query(SERIES)
        .await
        .lines()
        .map(|line| {
            let row: Vec<&str> = line.split('\t').collect();
            (
                (names[row[0]].to_owned(), row[1].to_owned()),
                row[2].parse().unwrap(),
            )
        })
        .collect();
    let mut stated: BTreeMap<(String, String), u64> = BTreeMap::new();
    for series in dataset.index().segments.iter().flat_map(|s| &s.series) {
        *stated
            .entry((series.device.clone(), series.point.clone()))
            .or_default() += series.rows;
    }
    assert_eq!(counted, stated, "rows per point");
    let rows: u64 = dataset.index().segments.iter().map(|s| s.rows).sum();
    assert_eq!(counted.values().sum::<u64>(), rows);
    assert_eq!(loaded.rows, rows);
    let commits = query(COMMITS).await;
    assert_eq!(commits.trim(), format!("{}\t{rows}", loaded.batches));
    println!(
        "recording `bench` loaded for tenant {tenant}: {} rows in ClickHouse, index states {rows}; \
         {} batches with completion records",
        counted.values().sum::<u64>(),
        loaded.batches
    );
    for ((device, point), count) in &counted {
        println!(
            "  {device}/{point}: {count} rows, index states {}",
            stated[&(device.clone(), point.clone())]
        );
    }
}

#[test]
fn a_recording_of_a_fake_converter_loads_into_clickhouse() {
    let scratch = Scratch::new();
    let config_path = scratch.0.join("recording.toml");
    let out = scratch.0.join("out");
    let paused = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let (recorded, requests, connections) = paused.block_on(record(&config_path, &out));
    drop(paused);

    // One connection for both units, read-only function codes, both units asked.
    assert_eq!(
        connections, 1,
        "devices behind one converter share a connection"
    );
    assert!(
        requests
            .iter()
            .all(|(function, _)| (1..=4).contains(function))
    );
    let units: BTreeSet<u8> = requests.iter().map(|(_, unit)| *unit).collect();
    assert_eq!(units, BTreeSet::from([1, 2]));

    let dataset = check_files(&recorded, &out);
    println!(
        "recorded {} rows in {} segments and {} batches from units {units:?} on {connections} \
         connection; Dataset::open accepts it",
        recorded.rows,
        dataset.index().segments.len(),
        recorded.batches
    );

    // A second recording into the same directory is refused.
    let again = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let recorder = Recorder::new(Config::read(&config_path).unwrap()).unwrap();
            recorder
                .run(
                    Recorder::endpoints(),
                    &out,
                    Utc::now(),
                    std::future::ready(()),
                )
                .await
                .unwrap_err()
                .to_string()
        });
    assert!(again.contains("exists"), "{again}");

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(load(&dataset));
}
