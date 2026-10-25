//! The dataset index (`datasets/lab/<name>/index.toml`): what a dataset holds and where its
//! segment files are. The format is described in `datasets/lab/README.md`; this module is its
//! parser and the rules every index must satisfy.
//!
//! The index names devices, data points and a logical converter key only: never a converter's
//! address or a host name (the repository is public, docs/architecture/secrets.md).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The index format this crate reads. An index with another value is refused: the loader and
/// the indexes live in the same repository and change together.
pub const FORMAT: u32 = 1;

/// The largest segment file, in bytes. The loader reads a whole segment into memory to check its
/// hash before it writes a row; recordings are cut into segments below this size.
pub const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// The longest text value, in bytes: the gateway protocol's limit (crates/protocol, `Value`).
pub const MAX_TEXT_BYTES: usize = 1024;

/// The longest data point key: the telemetry table's `point_length` constraint.
pub const MAX_POINT_KEY_CHARS: usize = 128;

/// A whole index file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// [`FORMAT`].
    pub format: u32,
    /// The dataset's name: the name of the directory that holds the index.
    pub name: String,
    /// The dataset version, from 1; see `datasets/lab/README.md`, 「版本化規則」.
    pub version: u32,
    pub description: String,
    /// The gateway's batch streams that appear in the segments.
    pub streams: Vec<Stream>,
    pub devices: Vec<Device>,
    /// The segment files in arrival order.
    pub segments: Vec<Segment>,
    /// Artificial anomaly segments and the triggers they are expected to cause. Reserved: the
    /// insertion API arrives with the second part of Laysi/trustiot-v2#679, and until then the
    /// loader refuses an index that lists any.
    #[serde(default)]
    pub anomalies: Vec<Anomaly>,
}

/// A gateway batch stream (crates/protocol, `TelemetryBatch.stream`). Batch sequence numbers are
/// consecutive within a stream and arrive in order; separate streams interleave.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stream {
    pub key: String,
    pub description: String,
}

/// A device the gateway reads.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub key: String,
    /// A generic model name, for example `three-phase-meter`; never a customer's asset name.
    pub model: String,
    /// The RS485 converter the device hangs on, as a logical key of this dataset (never an
    /// address); absent for a Modbus TCP device.
    #[serde(default)]
    pub converter: Option<String>,
    /// The Modbus unit (station) ID: 1 to 247 behind a converter.
    pub unit: u8,
    /// The polling interval in seconds: a gap is a distance between two readings of one point
    /// larger than this.
    pub interval_s: u32,
    pub points: Vec<Point>,
}

/// A data point of a device.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub key: String,
    #[serde(rename = "type")]
    pub value_type: ValueType,
    pub kind: PointKind,
    /// The engineering unit, for example `kWh`.
    #[serde(default)]
    pub unit: Option<String>,
    /// For a cumulative integer counter that overflows: the value it wraps at (for example
    /// 4294967296 for a 32-bit register); every reading is below it.
    #[serde(default)]
    pub wraps_at: Option<u64>,
}

/// Which value column of the telemetry table holds a point's readings.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    Float,
    Int,
    Bool,
    String,
}

impl ValueType {
    /// The telemetry table's `value_type` enum value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Int => "int",
            Self::Bool => "bool",
            Self::String => "string",
        }
    }
}

/// How a point's readings relate over time.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PointKind {
    /// A momentary value, for example power.
    Gauge,
    /// A counter that only grows, except when it is reset to zero or wraps (`wraps_at`), for
    /// example an energy register.
    Cumulative,
    /// A state or mode, for example running or a mode name.
    State,
}

/// One segment file: a contiguous part of the arrival log.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub key: String,
    /// Where the file is: a path relative to the index's directory, or `s3://<bucket>/<key>`.
    pub location: String,
    /// The file's size in bytes, as stored.
    pub bytes: u64,
    /// SHA-256 of the file as stored, lowercase hex.
    pub sha256: String,
    /// The number of readings (lines).
    pub rows: u64,
    /// The earliest and latest reading time (`ts`).
    pub reading_time: TimeRange,
    /// The earliest and latest receive time (`received_at`).
    pub receive_time: TimeRange,
    /// The batches of each stream in this segment.
    pub batches: Vec<BatchRange>,
    /// Batches whose rows were written but that have no completion record.
    #[serde(default)]
    pub uncommitted: Vec<BatchRef>,
    /// The number of readings of each data point.
    pub series: Vec<SeriesCount>,
}

/// An inclusive time range.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimeRange {
    #[serde(deserialize_with = "crate::time::deserialize")]
    pub start: DateTime<Utc>,
    #[serde(deserialize_with = "crate::time::deserialize")]
    pub end: DateTime<Utc>,
}

/// The batches of one stream in a segment: `first` to `last`, consecutive.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRange {
    pub stream: String,
    pub first: u64,
    pub last: u64,
}

/// One batch.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BatchRef {
    pub stream: String,
    pub batch_seq: u64,
}

/// The number of readings of one data point in a segment.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesCount {
    pub device: String,
    pub point: String,
    pub rows: u64,
}

/// An artificial anomaly segment (reserved, see [`Index::anomalies`]).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anomaly {
    pub key: String,
    pub kind: AnomalyKind,
    pub device: String,
    pub points: Vec<String>,
    /// When the anomaly starts and ends, on the dataset's time base (shifted with the data).
    #[serde(deserialize_with = "crate::time::deserialize")]
    pub start: DateTime<Utc>,
    #[serde(deserialize_with = "crate::time::deserialize")]
    pub end: DateTime<Utc>,
    /// What the anomaly is expected to cause.
    #[serde(default)]
    pub expect: Vec<Expectation>,
}

/// The kinds of artificial anomaly (checklist M7).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AnomalyKind {
    /// A short excursion of a value.
    Spike,
    /// The device stops.
    Outage,
    /// A product changeover.
    Changeover,
}

/// One expected trigger of an anomaly.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// The rule or detector, by key.
    pub trigger: String,
    /// Whether it is expected to fire.
    pub fires: bool,
}

/// Where a segment file is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A path relative to the index's directory, without `.` or `..` components.
    Relative(String),
    /// An object in an S3-compatible store (the provider is chosen in Laysi/trustiot-v2#133).
    S3 { bucket: String, key: String },
}

/// A key of a dataset, stream, device, converter, segment or anomaly: lowercase letters, digits
/// and `-`, starting with a letter or digit, at most 64 characters.
#[must_use]
pub fn is_key(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && text.len() <= 64
}

/// A data point key: ASCII letters, digits, `_`, `-` and `.`, 1 to [`MAX_POINT_KEY_CHARS`].
#[must_use]
pub fn is_point_key(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_POINT_KEY_CHARS
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A path component of a location: ASCII letters, digits, `_`, `-` and `.`, not `.` or `..`.
fn is_path_component(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// An S3 bucket name: 3 to 63 lowercase letters, digits, `.` and `-`.
fn is_bucket(text: &str) -> bool {
    (3..=63).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
}

impl Location {
    /// Parses a segment's `location`. Both forms end in `.jsonl`, the only segment file format
    /// of index format 1.
    ///
    /// # Errors
    ///
    /// When the text is neither form, or a relative path could leave the index's directory.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (location, path) = if let Some(rest) = text.strip_prefix("s3://") {
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| format!("`{text}` has no object key after the bucket"))?;
            if !is_bucket(bucket) {
                return Err(format!("`{bucket}` is not an S3 bucket name"));
            }
            (
                Self::S3 {
                    bucket: bucket.to_owned(),
                    key: key.to_owned(),
                },
                key,
            )
        } else if text.contains(':') {
            return Err(format!(
                "`{text}`: a location is a relative path or an s3:// URI"
            ));
        } else {
            (Self::Relative(text.to_owned()), text)
        };
        if !path.split('/').all(is_path_component) {
            return Err(format!(
                "`{text}`: every path component must be letters, digits, `_`, `-` or `.`, and \
                 not `.` or `..` (no absolute paths)"
            ));
        }
        if !path.ends_with(".jsonl") {
            return Err(format!("`{text}`: segment files are JSON lines (`.jsonl`)"));
        }
        Ok(location)
    }
}

fn unique<'a>(what: &str, keys: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(format!("{what} `{key}` appears twice"));
        }
    }
    Ok(())
}

fn check_key(what: &str, key: &str) -> Result<(), String> {
    if is_key(key) {
        Ok(())
    } else {
        Err(format!(
            "{what} `{key}` is not a key (lowercase letters, digits and `-`, at most 64)"
        ))
    }
}

impl Index {
    /// Parses and validates an index file's text.
    ///
    /// # Errors
    ///
    /// When the text is not TOML of this format or breaks a rule of [`Index::validate`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let index: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        index.validate()?;
        Ok(index)
    }

    /// The device with `key`.
    #[must_use]
    pub fn device(&self, key: &str) -> Option<&Device> {
        self.devices.iter().find(|device| device.key == key)
    }

    /// The rules every index satisfies, besides the ones the segment files are checked against
    /// (`crate::segment`).
    ///
    /// # Errors
    ///
    /// The first rule broken.
    pub fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(format!(
                "index format {} is not supported (this loader reads format {FORMAT})",
                self.format
            ));
        }
        check_key("dataset name", &self.name)?;
        if self.version == 0 {
            return Err("the version starts at 1".to_owned());
        }
        self.validate_streams()?;
        self.validate_devices()?;
        self.validate_segments()?;
        self.validate_anomalies()
    }

    fn validate_streams(&self) -> Result<(), String> {
        if self.streams.is_empty() {
            return Err("an index lists at least one stream".to_owned());
        }
        for stream in &self.streams {
            check_key("stream", &stream.key)?;
        }
        unique("stream", self.streams.iter().map(|s| s.key.as_str()))
    }

    fn validate_devices(&self) -> Result<(), String> {
        if self.devices.is_empty() {
            return Err("an index lists at least one device".to_owned());
        }
        let mut stations = BTreeSet::new();
        for device in &self.devices {
            check_key("device", &device.key)?;
            if device.model.trim().is_empty() || device.model.len() > 128 {
                return Err(format!(
                    "device `{}`: the model is 1 to 128 bytes",
                    device.key
                ));
            }
            if let Some(converter) = &device.converter {
                check_key("converter", converter)?;
                if !(1..=247).contains(&device.unit) {
                    return Err(format!(
                        "device `{}`: a unit behind a converter is 1 to 247",
                        device.key
                    ));
                }
                if !stations.insert((converter.as_str(), device.unit)) {
                    return Err(format!(
                        "device `{}`: converter `{converter}` already has a device with unit {}",
                        device.key, device.unit
                    ));
                }
            }
            if device.interval_s == 0 {
                return Err(format!(
                    "device `{}`: the interval is at least 1 second",
                    device.key
                ));
            }
            if device.points.is_empty() {
                return Err(format!("device `{}` has no points", device.key));
            }
            for point in &device.points {
                validate_point(&device.key, point)?;
            }
            unique(
                &format!("device `{}`: point", device.key),
                device.points.iter().map(|p| p.key.as_str()),
            )?;
        }
        unique("device", self.devices.iter().map(|d| d.key.as_str()))
    }

    fn validate_segments(&self) -> Result<(), String> {
        if self.segments.is_empty() {
            return Err("an index lists at least one segment".to_owned());
        }
        unique("segment", self.segments.iter().map(|s| s.key.as_str()))?;
        // The next batch each stream must start with, once it has appeared.
        let mut next: BTreeMap<&str, u64> = BTreeMap::new();
        let mut previous_end: Option<DateTime<Utc>> = None;
        for segment in &self.segments {
            let key = segment.key.as_str();
            check_key("segment", key)?;
            Location::parse(&segment.location)
                .map_err(|error| format!("segment `{key}`: {error}"))?;
            if segment.bytes == 0 || segment.bytes > MAX_SEGMENT_BYTES {
                return Err(format!(
                    "segment `{key}`: a segment file is 1 to {MAX_SEGMENT_BYTES} bytes"
                ));
            }
            if segment.sha256.len() != 64
                || !segment
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
            {
                return Err(format!(
                    "segment `{key}`: sha256 is 64 lowercase hex digits"
                ));
            }
            if segment.rows == 0 {
                return Err(format!("segment `{key}` has no rows"));
            }
            for (what, range) in [
                ("reading_time", segment.reading_time),
                ("receive_time", segment.receive_time),
            ] {
                if range.start > range.end {
                    return Err(format!("segment `{key}`: {what} starts after it ends"));
                }
            }
            // Arrival order: a segment starts no earlier than the previous one ended.
            if previous_end.is_some_and(|end| segment.receive_time.start < end) {
                return Err(format!(
                    "segment `{key}`: receive_time starts before the previous segment's ends"
                ));
            }
            previous_end = Some(segment.receive_time.end);
            if segment.batches.is_empty() {
                return Err(format!("segment `{key}` lists no batches"));
            }
            unique(
                &format!("segment `{key}`: stream"),
                segment.batches.iter().map(|b| b.stream.as_str()),
            )?;
            for range in &segment.batches {
                if !self.streams.iter().any(|s| s.key == range.stream) {
                    return Err(format!(
                        "segment `{key}`: stream `{}` is not listed in streams",
                        range.stream
                    ));
                }
                if range.first == 0 || range.first > range.last {
                    return Err(format!(
                        "segment `{key}`: stream `{}`: batches run from first >= 1 to last >= first",
                        range.stream
                    ));
                }
                if let Some(expected) = next.get(range.stream.as_str())
                    && range.first != *expected
                {
                    return Err(format!(
                        "segment `{key}`: stream `{}` continues at batch {}, not {} \
                         (sequence numbers are consecutive)",
                        range.stream, expected, range.first
                    ));
                }
                let following = range
                    .last
                    .checked_add(1)
                    .ok_or_else(|| format!("segment `{key}`: batch numbers overflow"))?;
                next.insert(range.stream.as_str(), following);
            }
            let mut uncommitted = BTreeSet::new();
            for batch in &segment.uncommitted {
                let inside = segment.batches.iter().any(|range| {
                    range.stream == batch.stream
                        && (range.first..=range.last).contains(&batch.batch_seq)
                });
                if !inside {
                    return Err(format!(
                        "segment `{key}`: uncommitted batch {}/{} is not one of its batches",
                        batch.stream, batch.batch_seq
                    ));
                }
                if !uncommitted.insert(batch) {
                    return Err(format!(
                        "segment `{key}`: uncommitted batch {}/{} appears twice",
                        batch.stream, batch.batch_seq
                    ));
                }
            }
            let mut total: u64 = 0;
            let mut seen = BTreeSet::new();
            for series in &segment.series {
                let known = self
                    .device(&series.device)
                    .is_some_and(|d| d.points.iter().any(|p| p.key == series.point));
                if !known {
                    return Err(format!(
                        "segment `{key}`: series {}/{} is not a declared point",
                        series.device, series.point
                    ));
                }
                if !seen.insert((series.device.as_str(), series.point.as_str())) {
                    return Err(format!(
                        "segment `{key}`: series {}/{} appears twice",
                        series.device, series.point
                    ));
                }
                if series.rows == 0 {
                    return Err(format!(
                        "segment `{key}`: series {}/{} lists 0 rows; leave it out instead",
                        series.device, series.point
                    ));
                }
                total = total.saturating_add(series.rows);
            }
            if total != segment.rows {
                return Err(format!(
                    "segment `{key}`: the series add up to {total} rows, not {}",
                    segment.rows
                ));
            }
        }
        Ok(())
    }

    fn validate_anomalies(&self) -> Result<(), String> {
        for anomaly in &self.anomalies {
            check_key("anomaly", &anomaly.key)?;
            let device = self.device(&anomaly.device).ok_or_else(|| {
                format!(
                    "anomaly `{}`: device `{}` is not listed",
                    anomaly.key, anomaly.device
                )
            })?;
            if anomaly.points.is_empty() {
                return Err(format!("anomaly `{}` names no points", anomaly.key));
            }
            for point in &anomaly.points {
                if !device.points.iter().any(|p| &p.key == point) {
                    return Err(format!(
                        "anomaly `{}`: device `{}` has no point `{point}`",
                        anomaly.key, anomaly.device
                    ));
                }
            }
            if anomaly.start > anomaly.end {
                return Err(format!("anomaly `{}` starts after it ends", anomaly.key));
            }
            for expectation in &anomaly.expect {
                check_key(
                    &format!("anomaly `{}`: trigger", anomaly.key),
                    &expectation.trigger,
                )?;
            }
        }
        unique("anomaly", self.anomalies.iter().map(|a| a.key.as_str()))
    }
}

fn validate_point(device: &str, point: &Point) -> Result<(), String> {
    if !is_point_key(&point.key) {
        return Err(format!(
            "device `{device}`: `{}` is not a point key (letters, digits, `_`, `-`, `.`, 1 to \
             {MAX_POINT_KEY_CHARS})",
            point.key
        ));
    }
    if point
        .unit
        .as_ref()
        .is_some_and(|unit| unit.trim().is_empty() || unit.len() > 32)
    {
        return Err(format!(
            "device `{device}`, point `{}`: the unit is 1 to 32 bytes",
            point.key
        ));
    }
    if let Some(wraps_at) = point.wraps_at {
        if point.kind != PointKind::Cumulative || point.value_type != ValueType::Int {
            return Err(format!(
                "device `{device}`, point `{}`: only a cumulative int point wraps",
                point.key
            ));
        }
        // Readings are i64 and lie in 0..wraps_at.
        if wraps_at < 2 || wraps_at > i64::MAX.unsigned_abs() + 1 {
            return Err(format!(
                "device `{device}`, point `{}`: wraps_at is 2 to 2^63",
                point.key
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small valid index, the base the rule tests change one thing in.
    pub(crate) const VALID: &str = r#"
format = 1
name = "example"
version = 1
description = "unit test"

[[streams]]
key = "live"
description = "live batches"

[[devices]]
key = "meter-1"
model = "three-phase-meter"
converter = "rs485-a"
unit = 1
interval_s = 60
points = [
  { key = "energy_kwh", type = "float", kind = "cumulative", unit = "kWh" },
  { key = "pulses", type = "int", kind = "cumulative", wraps_at = 65536 },
]

[[devices]]
key = "meter-2"
model = "three-phase-meter"
converter = "rs485-a"
unit = 2
interval_s = 60
points = [{ key = "running", type = "bool", kind = "state" }]

[[segments]]
key = "part-1"
location = "segments/part-1.jsonl"
bytes = 10
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
rows = 3
reading_time = { start = "2026-01-05T00:00:00.000Z", end = "2026-01-05T00:01:00.000Z" }
receive_time = { start = "2026-01-05T00:00:01.000Z", end = "2026-01-05T00:01:01.000Z" }
batches = [{ stream = "live", first = 1, last = 2 }]
uncommitted = [{ stream = "live", batch_seq = 2 }]
series = [
  { device = "meter-1", point = "energy_kwh", rows = 2 },
  { device = "meter-2", point = "running", rows = 1 },
]
"#;

    fn with(from: &str, to: &str) -> Result<Index, String> {
        assert!(VALID.contains(from), "the base index has no `{from}`");
        Index::parse(&VALID.replacen(from, to, 1))
    }

    #[test]
    fn the_base_index_is_valid() {
        let index = Index::parse(VALID).unwrap();
        assert_eq!(index.devices[0].points[1].wraps_at, Some(65536));
        assert!(index.anomalies.is_empty());
    }

    #[test]
    fn every_rule_refuses_its_violation() {
        let cases = [
            ("format = 1", "format = 2", "format 2"),
            ("name = \"example\"", "name = \"Example\"", "not a key"),
            ("version = 1", "version = 0", "starts at 1"),
            (
                "description = \"unit test\"",
                "description = \"x\"\nextra = 1",
                "unknown field",
            ),
            ("key = \"live\"", "key = \"live stream\"", "not a key"),
            ("unit = 2", "unit = 1", "already has a device with unit 1"),
            ("unit = 2", "unit = 0", "1 to 247"),
            (
                "interval_s = 60\npoints = [{",
                "interval_s = 0\npoints = [{",
                "at least 1 second",
            ),
            (
                "key = \"energy_kwh\"",
                "key = \"energy kwh\"",
                "not a point key",
            ),
            ("key = \"energy_kwh\"", "key = \"pulses\"", "appears twice"),
            (
                "kind = \"state\"",
                "kind = \"state\", wraps_at = 2",
                "only a cumulative int",
            ),
            ("wraps_at = 65536", "wraps_at = 1", "wraps_at is 2"),
            ("type = \"bool\"", "type = \"double\"", "unknown variant"),
            (
                "segments/part-1.jsonl",
                "../part-1.jsonl",
                "not `.` or `..`",
            ),
            (
                "segments/part-1.jsonl",
                "/tmp/part-1.jsonl",
                "not `.` or `..`",
            ),
            (
                "segments/part-1.jsonl",
                "https://example.com/a.jsonl",
                "relative path or an s3",
            ),
            ("segments/part-1.jsonl", "segments/part-1.csv", "JSON lines"),
            ("bytes = 10", "bytes = 0", "1 to"),
            ("sha256 = \"0000", "sha256 = \"A000", "hex"),
            ("rows = 3", "rows = 4", "add up to 3 rows, not 4"),
            (
                "end = \"2026-01-05T00:01:00.000Z\"",
                "end = \"2026-01-04T00:01:00.000Z\"",
                "starts after it ends",
            ),
            (
                "end = \"2026-01-05T00:01:00.000Z\"",
                "end = \"2026-01-05T00:01:00Z\"",
                "in the form",
            ),
            ("first = 1, last = 2", "first = 0, last = 2", "first >= 1"),
            (
                "{ stream = \"live\", first",
                "{ stream = \"other\", first",
                "not listed in streams",
            ),
            (
                "batch_seq = 2 }",
                "batch_seq = 3 }",
                "not one of its batches",
            ),
            (
                "point = \"running\", rows = 1",
                "point = \"mode\", rows = 1",
                "not a declared point",
            ),
        ];
        for (from, to, expected) in cases {
            let error = with(from, to).expect_err(&format!("`{from}` -> `{to}` was accepted"));
            assert!(
                error.contains(expected),
                "`{from}` -> `{to}`: `{error}` does not mention `{expected}`"
            );
        }
    }

    #[test]
    fn segments_continue_their_streams_and_arrive_in_order() {
        let second = r#"
[[segments]]
key = "part-2"
location = "segments/part-2.jsonl"
bytes = 10
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
rows = 1
reading_time = { start = "2026-01-05T00:02:00.000Z", end = "2026-01-05T00:02:00.000Z" }
receive_time = { start = "RECEIVED", end = "2026-01-05T00:02:01.000Z" }
batches = [{ stream = "live", first = FIRST, last = 9 }]
series = [{ device = "meter-2", point = "running", rows = 1 }]
"#;
        let index = |first: &str, received: &str| {
            Index::parse(&format!(
                "{VALID}{}",
                second.replace("FIRST", first).replace("RECEIVED", received)
            ))
        };
        index("3", "2026-01-05T00:01:01.000Z").unwrap();
        let skipped = index("4", "2026-01-05T00:01:01.000Z").unwrap_err();
        assert!(skipped.contains("continues at batch 3, not 4"), "{skipped}");
        let early = index("3", "2026-01-05T00:01:00.999Z").unwrap_err();
        assert!(early.contains("before the previous segment"), "{early}");
    }

    #[test]
    fn locations() {
        assert_eq!(
            Location::parse("segments/part-1.jsonl").unwrap(),
            Location::Relative("segments/part-1.jsonl".to_owned())
        );
        assert_eq!(
            Location::parse("s3://trustiot-lab-datasets/lab/v3/part-1.jsonl").unwrap(),
            Location::S3 {
                bucket: "trustiot-lab-datasets".to_owned(),
                key: "lab/v3/part-1.jsonl".to_owned()
            }
        );
        for bad in [
            "s3://Bucket/a.jsonl",
            "s3://bucket",
            "s3://bucket/../a.jsonl",
            "s3://bucket//a.jsonl",
            "segments//a.jsonl",
            "./a.jsonl",
            "a\\b.jsonl",
            "C:a.jsonl",
            "a.jsonl?x=1",
        ] {
            assert!(Location::parse(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn anomalies_reference_declared_points() {
        let anomaly = r#"
[[anomalies]]
key = "spike-1"
kind = "spike"
device = "meter-1"
points = ["POINT"]
start = "2026-01-05T00:00:30.000Z"
end = "2026-01-05T00:00:40.000Z"
expect = [{ trigger = "power-spike", fires = true }]
"#;
        let index =
            |point: &str| Index::parse(&format!("{VALID}{}", anomaly.replace("POINT", point)));
        assert_eq!(index("energy_kwh").unwrap().anomalies.len(), 1);
        assert!(index("missing").unwrap_err().contains("has no point"));
    }
}
