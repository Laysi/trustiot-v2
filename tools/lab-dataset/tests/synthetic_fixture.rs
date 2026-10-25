//! The synthetic fixture (`datasets/lab/synthetic`) holds what real recordings hold
//! (Laysi/trustiot-v2#679): a cumulative meter reset to zero, a counter that overflows, gaps,
//! out-of-order arrival with backfill, and two devices on one RS485 converter. Each property is
//! checked in the segment files' content, not only in the index's descriptions.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use chrono::TimeDelta;
use lab_dataset::index::{PointKind, ValueType};
use lab_dataset::{Dataset, Error, Reading, Value, synthetic_index, time};

fn fixture() -> (Dataset, Vec<Reading>) {
    let dataset =
        Dataset::open(synthetic_index()).expect("the synthetic fixture passes its checks");
    let readings = dataset.readings().expect("its segments read");
    (dataset, readings)
}

/// Each point's readings ordered by reading time.
fn series(readings: &[Reading]) -> BTreeMap<(String, String), Vec<&Reading>> {
    let mut series: BTreeMap<(String, String), Vec<&Reading>> = BTreeMap::new();
    for reading in readings {
        series
            .entry((reading.device.clone(), reading.point.clone()))
            .or_default()
            .push(reading);
    }
    for values in series.values_mut() {
        values.sort_by_key(|reading| reading.ts);
    }
    series
}

fn interval(dataset: &Dataset, device: &str) -> TimeDelta {
    let seconds = dataset
        .index()
        .device(device)
        .expect("a listed device")
        .interval_s;
    TimeDelta::seconds(i64::from(seconds))
}

#[test]
fn the_index_matches_its_files() {
    let (dataset, readings) = fixture();
    let rows: u64 = dataset.index().segments.iter().map(|s| s.rows).sum();
    assert_eq!(u64::try_from(readings.len()).unwrap(), rows);
    assert_eq!(rows, 316);
    // Every value type of the telemetry table appears.
    let types: BTreeSet<&str> = readings
        .iter()
        .map(|reading| reading.value.value_type().as_str())
        .collect();
    assert_eq!(types, BTreeSet::from(["bool", "float", "int", "string"]));
}

#[test]
fn a_cumulative_meter_is_reset_to_zero() {
    let (dataset, readings) = fixture();
    let mut resets = Vec::new();
    for ((device, point), values) in series(&readings) {
        let declared = dataset.index().device(&device).unwrap();
        let declared = declared.points.iter().find(|p| p.key == point).unwrap();
        if declared.kind != PointKind::Cumulative || declared.value_type != ValueType::Float {
            continue;
        }
        for pair in values.windows(2) {
            if let (Value::Float(before), Value::Float(after)) = (&pair[0].value, &pair[1].value)
                && after < before
            {
                assert!(
                    after.abs() < f64::EPSILON,
                    "{device}/{point} drops from {before} to {after} at {}: a cumulative meter \
                     only drops to zero",
                    time::format(pair[1].ts)
                );
                resets.push((device.clone(), time::format(pair[1].ts), *before));
            }
        }
    }
    assert_eq!(
        resets,
        vec![(
            "meter-1".to_owned(),
            "2026-01-05T23:30:00.120Z".to_owned(),
            15_293.813
        )],
        "the fixture has exactly one reset to zero"
    );
}

#[test]
fn a_counter_overflows_at_its_register_size() {
    let (dataset, readings) = fixture();
    let mut wraps = Vec::new();
    for ((device, point), values) in series(&readings) {
        let declared = dataset.index().device(&device).unwrap();
        let declared = declared.points.iter().find(|p| p.key == point).unwrap();
        let Some(wraps_at) = declared.wraps_at else {
            continue;
        };
        let wraps_at = i128::from(wraps_at);
        for pair in values.windows(2) {
            let (Value::Int(before), Value::Int(after)) = (&pair[0].value, &pair[1].value) else {
                panic!("{device}/{point} holds non-integers");
            };
            let step = i128::from(*after) - i128::from(*before);
            if step < 0 {
                // Over the top: the true increase is small and positive, like the others.
                let increase = step + wraps_at;
                assert!(
                    (1..1000).contains(&increase),
                    "{device}/{point} wraps from {before} to {after}: not an overflow"
                );
                wraps.push((device.clone(), *before, *after));
            }
        }
    }
    assert_eq!(wraps.len(), 1, "one overflow: {wraps:?}");
    let (device, before, after) = &wraps[0];
    assert_eq!(device, "meter-2");
    assert!(*before > 4_294_967_296 - 1000 && *after < 1000, "{wraps:?}");
}

#[test]
fn readings_are_missing() {
    let (dataset, readings) = fixture();
    // Every distance between neighbouring readings larger than 1.5 polling intervals, in whole
    // intervals.
    let mut gaps: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for ((device, point), values) in series(&readings) {
        let step = interval(&dataset, &device);
        for pair in values.windows(2) {
            let distance = pair[1].ts - pair[0].ts;
            if distance * 2 > step * 3 {
                gaps.entry(format!("{device}/{point}"))
                    .or_default()
                    .insert(distance.num_seconds() / step.num_seconds());
            }
        }
    }
    let expected: BTreeMap<String, BTreeSet<i64>> = [
        // One missed poll of a meter.
        ("meter-2/power_kw", 2),
        ("meter-2/pulses", 2),
        // The second converter unreachable for half an hour: six polls missing.
        ("press-1/cycle_count", 7),
        ("press-1/mode", 7),
        ("press-1/running", 7),
    ]
    .into_iter()
    .map(|(series, intervals)| (series.to_owned(), BTreeSet::from([intervals])))
    .collect();
    assert_eq!(gaps, expected);
}

#[test]
fn backfilled_readings_arrive_after_newer_ones() {
    let (dataset, readings) = fixture();
    // In arrival (file) order: a reading older than one of its point already received.
    let mut newest: BTreeMap<(&str, &str), chrono::DateTime<chrono::Utc>> = BTreeMap::new();
    let mut late = Vec::new();
    for reading in &readings {
        let key = (reading.device.as_str(), reading.point.as_str());
        match newest.get(&key) {
            Some(newest) if reading.ts < *newest => late.push(reading),
            _ => {
                newest.insert(key, reading.ts);
            }
        }
    }
    assert!(!late.is_empty(), "no reading arrives out of order");
    // Every point of every device has backfilled readings, all on the backlog stream.
    let points: BTreeSet<(&str, &str)> = late
        .iter()
        .map(|r| (r.device.as_str(), r.point.as_str()))
        .collect();
    assert_eq!(points.len(), 7);
    assert!(late.iter().all(|r| r.stream == "backlog"));
    assert_eq!(late.len(), 63);
    // Backfill: received later than any live reading is, up to an hour after it was read.
    let delay = |r: &&Reading| r.received_at - r.ts;
    let live_delay = readings
        .iter()
        .filter(|r| r.stream == "live")
        .map(|r| r.received_at - r.ts)
        .max()
        .unwrap();
    let shortest = late.iter().map(delay).min().unwrap();
    let longest = late.iter().map(delay).max().unwrap();
    assert!(
        live_delay <= interval(&dataset, "meter-1") * 3,
        "live data waits at most one batch (three polls): {live_delay}"
    );
    assert!(shortest > live_delay, "{shortest} vs live {live_delay}");
    assert!(longest >= TimeDelta::minutes(50), "{longest}");
}

#[test]
fn two_devices_share_a_converter() {
    let (dataset, readings) = fixture();
    let with_readings: BTreeSet<&str> = readings.iter().map(|r| r.device.as_str()).collect();
    let mut by_converter: BTreeMap<&str, Vec<(u8, &str)>> = BTreeMap::new();
    for device in &dataset.index().devices {
        if let Some(converter) = &device.converter
            && with_readings.contains(device.key.as_str())
        {
            by_converter
                .entry(converter)
                .or_default()
                .push((device.unit, device.key.as_str()));
        }
    }
    let shared = &by_converter["rs485-a"];
    assert_eq!(shared, &vec![(1, "meter-1"), (2, "meter-2")]);
    // The other converter reuses unit 1: units are unique per converter, not per gateway.
    assert_eq!(by_converter["rs485-b"], vec![(1, "press-1")]);
}

#[test]
fn one_batch_is_uncommitted() {
    let (dataset, readings) = fixture();
    let uncommitted: Vec<_> = dataset
        .index()
        .segments
        .iter()
        .flat_map(|segment| segment.uncommitted.iter())
        .collect();
    assert_eq!(uncommitted.len(), 1);
    let batch = uncommitted[0];
    let rows = readings
        .iter()
        .filter(|r| r.stream == batch.stream && r.batch_seq == batch.batch_seq)
        .count();
    assert_eq!(rows, 21, "the uncommitted batch has rows");
}

/// A scratch copy of the fixture, removed when dropped.
struct Copy {
    root: std::path::PathBuf,
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

    fn dir(&self) -> std::path::PathBuf {
        self.root.join("synthetic")
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn open(dir: &Path) -> Result<Dataset, Error> {
    Dataset::open(dir.join("index.toml"))
}

#[test]
fn a_changed_copy_is_refused() {
    let copy = Copy::new();
    open(&copy.dir()).expect("an unchanged copy opens");

    // One digit of one value.
    let part = copy.dir().join("segments/part-2.jsonl");
    let text = fs::read_to_string(&part).unwrap();
    fs::write(
        &part,
        text.replacen("\"value\":85.668", "\"value\":85.669", 1),
    )
    .unwrap();
    let error = open(&copy.dir()).unwrap_err().to_string();
    assert!(
        error.contains("segment `part-2`") && error.contains("SHA-256"),
        "{error}"
    );
    fs::write(&part, &text).unwrap();

    // A different directory name than the dataset's name.
    let renamed = copy.root.join("other");
    fs::rename(copy.dir(), &renamed).unwrap();
    let error = open(&renamed).unwrap_err().to_string();
    assert!(error.contains("directory of that name"), "{error}");
    fs::rename(&renamed, copy.dir()).unwrap();

    // A segment file that is a link out of the dataset's directory.
    #[cfg(unix)]
    {
        let outside = copy.root.join("part-2.jsonl");
        fs::rename(&part, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &part).unwrap();
        let error = open(&copy.dir()).unwrap_err().to_string();
        assert!(error.contains("outside the dataset's directory"), "{error}");
    }
}
