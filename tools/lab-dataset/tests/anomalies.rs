//! Artificial anomaly segments on the synthetic fixture (Laysi/trustiot-v2#679): inserted by test
//! code ([`Dataset::with_anomalies`]) and written in the index (`[[anomalies]]`). Each anomaly
//! changes exactly the readings whose reading time lies in the window its parameters give, as
//! its kind defines (`datasets/lab/README.md`, 「人工異常片段」), keeps cumulative points
//! counting forwards, and carries its expected-trigger labels. Loading the result into
//! ClickHouse is checked by `clickhouse_load.rs`.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "test helpers unwrap and print like the tests themselves (clippy.toml allows both in \
              tests); the printed counts are what `-- --nocapture` shows as evidence"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use lab_dataset::{
    Anomaly, AnomalyKind, Dataset, Expectation, Reading, Value, synthetic_index, time,
};

use self::support::{anomalies, at};

/// The anomalies both this file and `clickhouse_load.rs` insert.
mod support;

/// Each point's (time, value) pairs in reading-time order, by (device, point).
type Series = BTreeMap<(String, String), Vec<(DateTime<Utc>, Value)>>;

/// Each point's (time, value) pairs in reading-time order.
fn series(readings: &[Reading]) -> Series {
    let mut series = Series::new();
    for reading in readings {
        series
            .entry((reading.device.clone(), reading.point.clone()))
            .or_default()
            .push((reading.ts, reading.value.clone()));
    }
    for values in series.values_mut() {
        values.sort_by_key(|(ts, _)| *ts);
    }
    series
}

/// The increase of a cumulative point from `a` to `b`: modulo `wraps_at` for a wrapping
/// counter; `None` for a reset (a decrease without `wraps_at`).
fn increase(a: &Value, b: &Value, wraps_at: Option<u64>) -> Option<f64> {
    match (a, b, wraps_at) {
        (Value::Int(a), Value::Int(b), Some(wraps_at)) => {
            let step = (i128::from(*b) - i128::from(*a)).rem_euclid(i128::from(wraps_at));
            Some(f64::from(u32::try_from(step).unwrap()))
        }
        (Value::Int(a), Value::Int(b), None) => {
            (b >= a).then(|| f64::from(u32::try_from(b - a).unwrap()))
        }
        (Value::Float(a), Value::Float(b), None) => (b >= a).then_some(b - a),
        other => panic!("not a cumulative pair: {other:?}"),
    }
}

/// Checks what every anomaly did to `changed` compared with `recorded`.
fn check(dataset: &Dataset, recorded: &[Reading], changed: &[Reading], anomalies: &[Anomaly]) {
    assert_eq!(
        recorded.len(),
        changed.len(),
        "no reading appears or disappears"
    );
    // Same rows in the same arrival order; only values may differ.
    for (a, b) in recorded.iter().zip(changed) {
        assert_eq!(
            (
                &a.stream,
                a.batch_seq,
                a.received_at,
                &a.device,
                &a.point,
                a.ts
            ),
            (
                &b.stream,
                b.batch_seq,
                b.received_at,
                &b.device,
                &b.point,
                b.ts
            )
        );
    }
    let recorded = series(recorded);
    let changed = series(changed);
    for ((device, point), before) in &recorded {
        let after = &changed[&(device.clone(), point.clone())];
        let declared = dataset
            .index()
            .device(device)
            .unwrap()
            .points
            .iter()
            .find(|p| &p.key == point)
            .unwrap();
        let windows: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|a| &a.device == device && a.points.contains(point))
            .collect();
        let inside = |ts: DateTime<Utc>| windows.iter().find(|a| a.covers(ts));
        let cumulative = declared.kind == lab_dataset::index::PointKind::Cumulative;
        let mut touched = 0;
        for (position, ((ts, old), (_, new))) in before.iter().zip(after).enumerate() {
            match (inside(*ts), cumulative) {
                (Some(anomaly), false) => {
                    touched += 1;
                    let expected = match (anomaly.kind, old) {
                        (AnomalyKind::Spike, _) => Value::Float(480.5),
                        (_, Value::Float(_)) => Value::Float(0.0),
                        (_, Value::Int(_)) => Value::Int(0),
                        (_, Value::Bool(_)) => Value::Bool(false),
                        (_, Value::String(_)) => Value::String(anomaly.state.clone().unwrap()),
                    };
                    assert_eq!(new, &expected, "{device}/{point} at {}", time::format(*ts));
                }
                (None, false) => assert_eq!(new, old, "{device}/{point} outside the windows"),
                (window, true) => {
                    if window.is_some() {
                        touched += 1;
                    }
                    // A stopped counter counts nothing in the window, and every other step is
                    // the recorded one: it never runs backwards, and a reset stays a reset.
                    let Some(position) = position.checked_sub(1) else {
                        continue;
                    };
                    let (_, old_previous) = &before[position];
                    let (_, new_previous) = &after[position];
                    let recorded_step = increase(old_previous, old, declared.wraps_at);
                    let step = increase(new_previous, new, declared.wraps_at);
                    if window.is_some() {
                        assert_eq!(step, Some(0.0), "{device}/{point} stopped at {ts}");
                    } else {
                        match (recorded_step, step) {
                            (None, _) => assert_eq!(new, old, "a reset keeps its value"),
                            (Some(recorded), Some(step)) => assert!(
                                (recorded - step).abs() < 1e-6,
                                "{device}/{point} at {ts}: step {step}, recorded {recorded}"
                            ),
                            (Some(_), None) => panic!("{device}/{point} ran backwards at {ts}"),
                        }
                    }
                }
            }
        }
        // Every point an anomaly names has readings in its window.
        assert_eq!(touched > 0, !windows.is_empty(), "{device}/{point}");
        if !windows.is_empty() {
            println!("{device}/{point}: {touched} readings inside the windows");
        }
    }
}

#[test]
fn inserted_anomalies_follow_their_parameters() {
    let dataset = Dataset::open(synthetic_index()).unwrap();
    let recorded = dataset.readings().unwrap();
    let inserted = anomalies();
    let with = dataset.clone().with_anomalies(inserted.clone()).unwrap();

    // The labels and times are the parameters, unchanged.
    assert_eq!(with.anomalies(), inserted.as_slice());
    assert_eq!(
        with.anomalies()[3].expect,
        [
            Expectation {
                trigger: "changeover-detected".to_owned(),
                fires: true
            },
            Expectation {
                trigger: "machine-stopped".to_owned(),
                fires: false
            }
        ]
    );
    assert_eq!(with.recorded_readings().unwrap(), recorded);
    let changed = with.readings().unwrap();
    check(&with, &recorded, &changed, &inserted);

    // Spot checks of what the README states.
    let series = series(&changed);
    let power = &series[&("meter-1".to_owned(), "power_kw".to_owned())];
    let spiked: Vec<String> = power
        .iter()
        .filter(|(_, value)| *value == Value::Float(480.5))
        .map(|(ts, _)| time::format(*ts))
        .collect();
    assert_eq!(
        spiked,
        [
            "2026-01-05T22:20:00.160Z",
            "2026-01-05T22:25:00.160Z",
            "2026-01-05T22:30:00.160Z"
        ]
    );
    // The energy meter holds its 00:45 level (54.121 kWh) through 01:00 and counts on from it.
    let energy = &series[&("meter-1".to_owned(), "energy_kwh".to_owned())];
    let level = |text: &str| {
        energy
            .iter()
            .find(|(ts, _)| time::format(*ts) == text)
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    assert_eq!(level("2026-01-06T00:45:00.120Z"), Value::Float(54.121));
    assert_eq!(level("2026-01-06T01:00:00.120Z"), Value::Float(54.121));
    let Value::Float(next) = level("2026-01-06T01:05:00.120Z") else {
        panic!("a float");
    };
    assert!((next - (54.121 + 69.502 - 65.855)).abs() < 1e-9, "{next}");
}

#[test]
fn anomalies_in_the_index_are_applied() {
    let copy = Copy::new();
    let index = copy.dir().join("index.toml");
    let mut text = fs::read_to_string(&index).unwrap();
    text.push('\n');
    text.push_str(&as_toml(&anomalies()));
    fs::write(&index, &text).unwrap();
    let annotated = Dataset::open(&index).unwrap();
    assert_eq!(annotated.anomalies(), anomalies().as_slice());
    let inserted = Dataset::open(synthetic_index())
        .unwrap()
        .with_anomalies(anomalies())
        .unwrap();
    assert_eq!(annotated.readings().unwrap(), inserted.readings().unwrap());

    // The command's summary counts them.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lab-dataset"))
        .arg("check")
        .arg(&index)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["anomalies"], 4);

    // An anomaly whose window holds no reading of a point is refused when the index is opened:
    // press-1 is unreachable from 00:00 to 00:25.
    let gap = Anomaly::outage(
        "press-stop",
        "press-1",
        &["running"],
        at("2026-01-06T00:05:00.000Z"),
        at("2026-01-06T00:20:00.000Z"),
    )
    .expecting("machine-stopped", true);
    text.push('\n');
    text.push_str(&as_toml(&[gap]));
    fs::write(&index, &text).unwrap();
    let error = Dataset::open(&index).unwrap_err().to_string();
    assert!(error.contains("press-1/running has no reading"), "{error}");
}

#[test]
fn the_readme_example_is_a_valid_index_table() {
    let readme =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../datasets/lab/README.md");
    let readme = fs::read_to_string(readme).unwrap();
    let example = readme
        .split("```toml\n")
        .filter_map(|block| block.split("```").next())
        .find(|block| block.starts_with("[[anomalies]]"))
        .expect("datasets/lab/README.md has an [[anomalies]] example");
    let copy = Copy::new();
    let index = copy.dir().join("index.toml");
    let mut text = fs::read_to_string(&index).unwrap();
    text.push('\n');
    text.push_str(example);
    fs::write(&index, &text).unwrap();
    let dataset = Dataset::open(&index).unwrap();
    assert_eq!(dataset.anomalies(), &anomalies()[3..]);
}

#[test]
fn bad_insertions_are_refused() {
    let dataset = Dataset::open(synthetic_index()).unwrap();
    // The energy meter resets at 23:30: a stop across the reset is ambiguous.
    let across_reset = Anomaly::outage(
        "meter-stop",
        "meter-1",
        &["energy_kwh"],
        at("2026-01-05T23:20:00.000Z"),
        at("2026-01-05T23:40:00.000Z"),
    )
    .expecting("machine-stopped", true);
    let error = dataset
        .clone()
        .with_anomalies([across_reset])
        .unwrap_err()
        .to_string();
    assert!(error.contains("resets inside the window"), "{error}");

    // A key the index's anomalies or an earlier insertion already uses.
    let twice = dataset
        .clone()
        .with_anomalies(anomalies())
        .unwrap()
        .with_anomalies([anomalies().remove(0)])
        .unwrap_err()
        .to_string();
    assert!(twice.contains("appears twice"), "{twice}");

    // No labels.
    let unlabelled = Anomaly::spike(
        "power-spike",
        "meter-1",
        &["power_kw"],
        at("2026-01-05T22:20:00.000Z"),
        at("2026-01-05T22:30:00.000Z"),
        480.5,
    );
    let error = dataset
        .with_anomalies([unlabelled])
        .unwrap_err()
        .to_string();
    assert!(error.contains("no expected triggers"), "{error}");
}

/// `anomalies` as the index's `[[anomalies]]` tables.
fn as_toml(anomalies: &[Anomaly]) -> String {
    #[derive(serde::Serialize)]
    struct Table<'a> {
        anomalies: &'a [Anomaly],
    }
    toml::to_string(&Table { anomalies }).unwrap()
}

/// A scratch copy of the fixture, removed when dropped.
struct Copy {
    root: PathBuf,
}

impl Copy {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("lab-dataset-{}", uuid::Uuid::new_v4().simple()));
        let dir = root.join("synthetic");
        fs::create_dir_all(dir.join("segments")).unwrap();
        let source = synthetic_index();
        let source = source.parent().unwrap();
        for file in [
            "index.toml",
            "segments/part-1.jsonl",
            "segments/part-2.jsonl",
        ] {
            fs::copy(source.join(file), dir.join(file)).unwrap();
        }
        Self { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("synthetic")
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
