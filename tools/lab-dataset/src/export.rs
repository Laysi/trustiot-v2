//! The export tool (`lab-dataset export`, Laysi/trustiot-v2#816): reads one tenant's gateway out
//! of ClickHouse's telemetry table and batch completion table and writes it as a dataset in the
//! index format (`datasets/lab/README.md`), which [`crate::Dataset::open`] checks at the end.
//! It is how the first lab dataset is made from the demonstration environment
//! (Laysi/trustiot-v2#680), the reverse of [`crate::Dataset::load`].
//!
//! ClickHouse holds neither the devices' descriptions (model, converter, unit, polling interval,
//! point kinds and units) nor their names, so a configuration file supplies them and maps each
//! device to its UUID. It is the recording configuration's format (`record::Config`): the
//! `[[models]]` section is the same, each `[[devices]]` entry adds the `id` of the device, and
//! the optional `[[streams]]` section names the gateway's streams. Whatever the file says about
//! addresses is ignored: the index only gets logical keys.
//!
//! What the export does with real data that the format forbids (README, 「匯出」):
//!
//! - rows of a device or point the configuration does not list, or stored with another value
//!   type, are refused, not skipped: a dataset must hold what the gateway sent;
//! - rows of one batch must share one `received_at`, which ingest stamps once per message;
//! - a reading stored twice (same device, point and time) with the same value is kept once, the
//!   earliest arrival, and counted; with different values the export is refused;
//! - a committed batch must hold as many distinct rows as its completion record says;
//! - a batch with no completion record is listed as `uncommitted`;
//! - a stream's batches must be consecutive in arrival order, because the format requires it.
//!
//! How it reads (Laysi/trustiot-v2#837; numbers in
//! docs/test-reports/2026-10-31-837-lab-dataset-export-limits.md):
//!
//! 1. It counts the rows of each device and point in the range (refusing a device or point the
//!    configuration does not list before it reads a row) and asks, per device and point, which
//!    reading times more than one row holds: only those readings are remembered across windows
//!    to drop a repeat.
//! 2. For every 30 days of the range it plans windows from per-minute buckets (a second, then a
//!    millisecond, for a minute that alone is too large): consecutive buckets go into one window
//!    while an upper bound of the answer's length stays within [`ReadLimits::window_bytes`], far
//!    below the HTTP client's 16 MiB limit. Windows are cut at bucket edges, which are receive
//!    times, so a window never cuts a batch: all rows of a batch share one `received_at`.
//! 3. It reads each window with the smallest and largest reading time the plan found in it, so
//!    the table's primary key `(tenant_id, device_id, point, ts)` can skip granules, and refuses a
//!    window that returns a number of rows other than the plan's. It groups and checks the
//!    window's batches, reads their completion records, and writes them before it reads the next
//!    window, so its memory holds one window and the state carried between windows (each
//!    stream's sequence numbers and key, the repeated readings), not the range.
//! 4. At the end the rows read per device and point must equal the counts of step 1, so rows
//!    written into or deleted from a window before that window is read make it fail instead of
//!    leaving a mix of two states. A change to a window that was already read is not seen: the
//!    dataset holds each window as it was when it was read (`tests/export_limits.rs` pins this).
//!
//! The files are written to `<out>/<name>.partial/` and renamed to `<out>/<name>/` only when every
//! rule passed, so a refused export leaves no files (a process killed midway leaves the
//! `.partial` directory, which the next run refuses to reuse).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use protocol::StreamId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::clickhouse::{ReadWindow, StoredRow};
use crate::index::{self, MAX_SEGMENT_BYTES, MAX_TEXT_BYTES, ValueType, check_key};
use crate::record::{MIN_SEGMENT_BYTES, Model, plan_point};
use crate::segment::Value;
use crate::writer::{Header, Row, SegmentWriter};
use crate::{ClickHouse, Dataset, Error, INDEX_FILE, time};

/// The longest span one planning query covers: its answer has a line for each minute that holds
/// rows, at most 43,200 lines of about 110 bytes, far below the HTTP client's 16 MiB limit, and
/// the plan of one span is all the export holds of the plan at a time.
const PLAN_SPAN: TimeDelta = TimeDelta::days(30);

/// The bucket widths of the plan, coarse to fine: a minute, then a second and a millisecond for a
/// bucket whose answer alone would pass the window budget. A millisecond is one receive time,
/// which the export never splits.
const BUCKET_WIDTHS_MS: [i64; 3] = [60_000, 1_000, 1];

/// An upper bound of one line of a telemetry answer without its text columns, which the plan
/// measures: the JSON keys, quotes and separators (184 bytes) and the longest value ClickHouse
/// writes for each other column (a UUID of 36 bytes, five 64-bit integers of at most 20, a float
/// of at most 32, `float` and `false`), rounded up.
const LINE_BYTES: u64 = 384;

/// How large one read of the export may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// The longest answer one window may have, by the plan's upper bound of its length: the
    /// HTTP client refuses an answer over 16 MiB (`migrate::http`). The plan's bound is about 1.5
    /// times the real length for numeric readings, so the default of 12 MiB reads windows of
    /// about 8 MiB. A millisecond whose rows alone pass the budget is still read as one window.
    pub window_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            window_bytes: 12 * 1024 * 1024,
        }
    }
}

/// An export configuration (TOML). It may be the recording configuration of the same devices
/// with an `id` added to each device: the converters' `host` and `port`, and `batch_s`, are
/// accepted and ignored. Keep it outside the repository like the recording one.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The dataset's name: the directory the export is written to, and the index's `name`.
    pub name: String,
    /// The index's one-sentence description.
    pub description: String,
    /// Accepted so a recording configuration can be reused; not used.
    #[serde(default)]
    pub batch_s: Option<u32>,
    /// The largest segment file, in bytes ([`MIN_SEGMENT_BYTES`] to the index's 64 MiB).
    #[serde(default = "default_segment_bytes")]
    pub segment_bytes: u64,
    #[serde(default)]
    pub converters: Vec<Converter>,
    /// Names for the gateway's streams; a stream not listed is named `stream-1`, `stream-2`, ...
    /// in the order its first batch arrived.
    #[serde(default)]
    pub streams: Vec<StreamConfig>,
    /// The device models, as in the recording configuration.
    pub models: Vec<Model>,
    pub devices: Vec<DeviceConfig>,
}

const fn default_segment_bytes() -> u64 {
    MAX_SEGMENT_BYTES
}

/// An RS485 converter: its logical key. `host` and `port` are accepted and ignored.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Converter {
    pub key: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

/// A gateway stream and the key the index calls it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    /// The key in the index (lowercase letters, digits and `-`, at most 64).
    pub key: String,
    /// The stream ID the gateway chose (protocol `StreamId`); never written to the index.
    pub id: String,
    /// The stream's description in the index.
    #[serde(default)]
    pub description: Option<String>,
}

/// A device: the recording configuration's entry plus the ID its rows carry in ClickHouse.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub key: String,
    /// The `device_id` of its rows in `trustiot.telemetry`.
    pub id: Uuid,
    pub model: String,
    #[serde(default)]
    pub converter: Option<String>,
    /// Accepted and ignored, like the converters' `host` and `port`.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    pub unit: u8,
    /// The polling interval in seconds.
    pub interval_s: u32,
}

impl Config {
    /// Reads a configuration file.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or is not a configuration.
    pub fn read(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(|error| Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        Self::parse(&text).map_err(Error::Export)
    }

    /// Parses a configuration's text.
    ///
    /// # Errors
    ///
    /// When the text is not TOML of this form.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|error| error.to_string())
    }
}

/// A stream with the key it gets in the index.
#[derive(Clone, Debug)]
struct NamedStream {
    key: String,
    id: String,
    description: Option<String>,
}

/// A checked configuration.
#[derive(Debug)]
struct Plan {
    name: String,
    description: String,
    segment_bytes: u64,
    devices: Vec<index::Device>,
    /// A device's position in `devices`, by the ID its rows carry.
    by_id: BTreeMap<Uuid, usize>,
    streams: Vec<NamedStream>,
}

impl Plan {
    fn new(config: &Config) -> Result<Self, String> {
        check_key("dataset name", &config.name)?;
        if config.description.trim().is_empty() {
            return Err("the description is empty".to_owned());
        }
        if !(MIN_SEGMENT_BYTES..=MAX_SEGMENT_BYTES).contains(&config.segment_bytes) {
            return Err(format!(
                "segment_bytes is {MIN_SEGMENT_BYTES} to {MAX_SEGMENT_BYTES}"
            ));
        }
        let mut converters = BTreeSet::new();
        for converter in &config.converters {
            check_key("converter", &converter.key)?;
            if !converters.insert(converter.key.as_str()) {
                return Err(format!("converter `{}` appears twice", converter.key));
            }
        }
        let mut models: BTreeMap<&str, Vec<index::Point>> = BTreeMap::new();
        for model in &config.models {
            if model.name.trim().is_empty() || model.name.len() > 128 {
                return Err(format!(
                    "model `{}`: the name is 1 to 128 bytes",
                    model.name
                ));
            }
            if model.points.is_empty() {
                return Err(format!("model `{}` has no points", model.name));
            }
            let mut keys = BTreeSet::new();
            let mut points = Vec::with_capacity(model.points.len());
            for point in &model.points {
                if !keys.insert(point.key.as_str()) {
                    return Err(format!(
                        "model `{}`: point `{}` appears twice",
                        model.name, point.key
                    ));
                }
                points.push(plan_point(&model.name, point)?.1);
            }
            if models.insert(model.name.as_str(), points).is_some() {
                return Err(format!("model `{}` appears twice", model.name));
            }
        }
        let mut stations = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut by_id = BTreeMap::new();
        let mut devices = Vec::with_capacity(config.devices.len());
        for device in &config.devices {
            let key = device.key.as_str();
            check_key("device", key)?;
            if !keys.insert(key) {
                return Err(format!("device `{key}` appears twice"));
            }
            if device.id.is_nil() {
                return Err(format!("device `{key}`: the id is the nil UUID"));
            }
            if by_id.insert(device.id, devices.len()).is_some() {
                return Err(format!(
                    "device `{key}`: another device has the same id {}",
                    device.id
                ));
            }
            let points = models
                .get(device.model.as_str())
                .ok_or_else(|| format!("device `{key}`: model `{}` is not listed", device.model))?;
            match (&device.converter, &device.host, device.port) {
                (Some(converter), None, None) => {
                    if !converters.contains(converter.as_str()) {
                        return Err(format!(
                            "device `{key}`: converter `{converter}` is not listed"
                        ));
                    }
                    if !(1..=247).contains(&device.unit) {
                        return Err(format!(
                            "device `{key}`: a unit behind a converter is 1 to 247"
                        ));
                    }
                    if !stations.insert((converter.as_str(), device.unit)) {
                        return Err(format!(
                            "device `{key}`: converter `{converter}` already has a device with \
                             unit {}",
                            device.unit
                        ));
                    }
                }
                (None, _, _) => {}
                _ => {
                    return Err(format!(
                        "device `{key}`: give either `converter`, or `host` and `port` (a \
                         Modbus TCP device)"
                    ));
                }
            }
            if device.interval_s == 0 {
                return Err(format!("device `{key}`: the interval is at least 1 second"));
            }
            devices.push(index::Device {
                key: key.to_owned(),
                model: device.model.clone(),
                converter: device.converter.clone(),
                unit: device.unit,
                interval_s: device.interval_s,
                points: points.clone(),
            });
        }
        if devices.is_empty() {
            return Err("the configuration lists no devices".to_owned());
        }
        let mut stream_keys = BTreeSet::new();
        let mut stream_ids = BTreeSet::new();
        let mut streams = Vec::with_capacity(config.streams.len());
        for stream in &config.streams {
            check_key("stream", &stream.key)?;
            StreamId::new(stream.id.as_str())
                .map_err(|error| format!("stream `{}`: {error}", stream.key))?;
            if !stream_keys.insert(stream.key.as_str()) {
                return Err(format!("stream `{}` appears twice", stream.key));
            }
            if !stream_ids.insert(stream.id.as_str()) {
                return Err(format!(
                    "stream `{}`: another stream has the same id",
                    stream.key
                ));
            }
            streams.push(NamedStream {
                key: stream.key.clone(),
                id: stream.id.clone(),
                description: stream.description.clone(),
            });
        }
        Ok(Self {
            name: config.name.clone(),
            description: config.description.clone(),
            segment_bytes: config.segment_bytes,
            devices,
            by_id,
            streams,
        })
    }
}

/// One reading of a batch, with the device and point as positions in the plan.
#[derive(Clone, Debug, PartialEq)]
struct Reading {
    device: usize,
    point: usize,
    ts_ms: i64,
    value: Value,
}

/// The rows of one batch (`stream` is the gateway's stream ID), the readings sorted by device,
/// point and time with the batch's repeated rows collapsed.
#[derive(Debug)]
struct Batch {
    stream: String,
    seq: u64,
    received_ms: i64,
    /// The readings, each device and point and time once.
    readings: Vec<Reading>,
    /// The distinct readings stored for the batch, before any is dropped as a repeat of an
    /// earlier batch's: what the completion record counts.
    found: u64,
}

/// The device and point of a stored row as positions in the plan.
fn series_position(plan: &Plan, device_id: Uuid, point: &str) -> Result<(usize, usize), String> {
    let device = *plan.by_id.get(&device_id).ok_or_else(|| {
        format!(
            "the gateway sent rows of device {device_id}, which the configuration does not list \
             (a dataset holds everything the gateway sent: add the device)"
        )
    })?;
    let listed = &plan.devices[device];
    let position = listed
        .points
        .iter()
        .position(|declared| declared.key == point)
        .ok_or_else(|| {
            format!(
                "device `{}` sent point `{point}`, which its model does not list",
                listed.key
            )
        })?;
    Ok((device, position))
}

/// The value of a stored row as the index's point declares it.
fn reading(plan: &Plan, row: &StoredRow) -> Result<Reading, String> {
    let (device, point) = series_position(plan, row.device_id, &row.point)?;
    let listed = &plan.devices[device];
    let declared = &listed.points[point];
    let at = || format!("{}/{}", listed.key, declared.key);
    if row.value_type != declared.value_type {
        return Err(format!(
            "{} is stored as {}, but the configuration declares {} (check the point's \
             data_type, scale and offset)",
            at(),
            row.value_type.as_str(),
            declared.value_type.as_str()
        ));
    }
    let value = match row.value_type {
        ValueType::Float => match row.value_float {
            Some(float) if float.is_finite() => Value::Float(float),
            _ => {
                return Err(format!(
                    "{} holds a float that is not a finite number",
                    at()
                ));
            }
        },
        ValueType::Int => {
            if let Some(wraps_at) = declared.wraps_at
                && !u64::try_from(row.value_int).is_ok_and(|int| int < wraps_at)
            {
                return Err(format!(
                    "{} holds {}, outside 0 to {} (wraps_at)",
                    at(),
                    row.value_int,
                    wraps_at - 1
                ));
            }
            Value::Int(row.value_int)
        }
        ValueType::Bool => Value::Bool(row.value_bool),
        ValueType::String => {
            if row.value_string.len() > MAX_TEXT_BYTES {
                return Err(format!(
                    "{} holds a text longer than {MAX_TEXT_BYTES} bytes",
                    at()
                ));
            }
            Value::String(row.value_string.clone())
        }
    };
    Ok(Reading {
        device,
        point,
        ts_ms: row.ts_ms,
        value,
    })
}

/// Groups the rows into batches, checks each row against the configuration, collapses a batch's
/// repeated rows, and returns the batches in arrival order with the number of rows collapsed.
fn group(plan: &Plan, rows: &[StoredRow]) -> Result<(Vec<Batch>, u64), String> {
    let mut batches: BTreeMap<(&str, u64), Batch> = BTreeMap::new();
    for row in rows {
        let batch = batches
            .entry((row.stream_id.as_str(), row.batch_seq))
            .or_insert_with(|| Batch {
                stream: row.stream_id.clone(),
                seq: row.batch_seq,
                received_ms: row.received_ms,
                readings: Vec::new(),
                found: 0,
            });
        if batch.received_ms != row.received_ms {
            return Err(received_twice(&row.stream_id, row.batch_seq));
        }
        batch.readings.push(reading(plan, row)?);
    }
    let mut collapsed = 0;
    let mut batches: Vec<Batch> = batches.into_values().collect();
    for batch in &mut batches {
        batch
            .readings
            .sort_by_key(|reading| (reading.device, reading.point, reading.ts_ms));
        let before = batch.readings.len();
        let mut kept: Vec<Reading> = Vec::with_capacity(before);
        for reading in std::mem::take(&mut batch.readings) {
            match kept.last() {
                Some(previous)
                    if (previous.device, previous.point, previous.ts_ms)
                        == (reading.device, reading.point, reading.ts_ms) =>
                {
                    if previous.value != reading.value {
                        let device = &plan.devices[reading.device];
                        return Err(format!(
                            "{}/{} at {} is stored twice in batch {}/{} with different values",
                            device.key,
                            device.points[reading.point].key,
                            at_text(reading.ts_ms),
                            batch.stream,
                            batch.seq
                        ));
                    }
                    collapsed += 1;
                }
                _ => kept.push(reading),
            }
        }
        batch.found = u64::try_from(kept.len()).unwrap_or(u64::MAX);
        batch.readings = kept;
    }
    batches
        .sort_by(|a, b| (a.received_ms, &a.stream, a.seq).cmp(&(b.received_ms, &b.stream, b.seq)));
    Ok((batches, collapsed))
}

/// The refusal of a batch whose rows were received at two times.
fn received_twice(stream: &str, seq: u64) -> String {
    format!(
        "batch {stream}/{seq} has rows received at different times (a stream's batch numbers \
         started again, or the batch was written twice): narrow --from and --to to one of the \
         two"
    )
}

fn at_text(millis: i64) -> String {
    DateTime::from_timestamp_millis(millis).map_or_else(|| millis.to_string(), time::format)
}

/// A batch ready to write.
#[derive(Debug)]
struct Prepared {
    /// The stream's key in the index.
    stream: String,
    seq: u64,
    received_at: DateTime<Utc>,
    committed: bool,
    readings: Vec<Reading>,
}

/// A reading (device, point, time as positions in the plan) that more than one row of the range
/// holds, with the value of its first arrival once that arrived.
type Repeats = HashMap<(usize, usize, i64), Option<Value>>;

/// What the export carries from one batch to the next, in arrival order: names the streams,
/// checks the arrival order and the completion records, and drops the readings an earlier batch
/// already holds. It never holds a batch it has returned, so its size depends on the number of
/// streams and of repeated readings, not on the range.
#[derive(Debug)]
struct Assembler<'a> {
    plan: &'a Plan,
    /// Stream ID to key, for every stream seen or configured.
    keys: HashMap<String, String>,
    taken: BTreeSet<String>,
    next_auto: u32,
    /// The keys of the streams whose first batch has arrived.
    used: BTreeSet<String>,
    /// Stream ID to its first and last sequence number so far (consecutive, or refused).
    seqs: HashMap<String, (u64, u64)>,
    /// The readings that more than one row holds ([`ClickHouse::repeated_times`]): every other
    /// reading arrives once, so it is never looked up again.
    repeats: Repeats,
    /// Readings dropped as repeats of an earlier arrival's, beyond the batches' own repeats.
    repeated: u64,
    batches: u64,
    any_commit: bool,
}

impl<'a> Assembler<'a> {
    fn new(plan: &'a Plan, repeats: Repeats) -> Self {
        Self {
            plan,
            keys: plan
                .streams
                .iter()
                .map(|stream| (stream.id.clone(), stream.key.clone()))
                .collect(),
            taken: plan.streams.iter().map(|s| s.key.clone()).collect(),
            next_auto: 1,
            used: BTreeSet::new(),
            seqs: HashMap::new(),
            repeats,
            repeated: 0,
            batches: 0,
            any_commit: false,
        }
    }

    /// Takes the next batch in arrival order, with the completion records of its stream and
    /// number (`commits`: the row counts of each (stream ID, sequence number) record). Returns
    /// it ready to write, and the stream's key and description when it is the stream's first
    /// batch, for the index's `[[streams]]`.
    fn add(
        &mut self,
        batch: Batch,
        commits: &BTreeMap<(String, u64), Vec<u64>>,
    ) -> Result<(Prepared, Option<(String, String)>), String> {
        // A batch of an earlier window again: its rows were received at two times. The
        // numbers seen so far are consecutive, so they are exactly first to last.
        if self
            .seqs
            .get(&batch.stream)
            .is_some_and(|(first, last)| (*first..=*last).contains(&batch.seq))
        {
            return Err(received_twice(&batch.stream, batch.seq));
        }
        // Stream keys: the configured ones, then `stream-<n>` in order of first arrival.
        if !self.keys.contains_key(&batch.stream) {
            let key = loop {
                let key = format!("stream-{}", self.next_auto);
                self.next_auto += 1;
                if !self.taken.contains(&key) {
                    break key;
                }
            };
            self.taken.insert(key.clone());
            self.keys.insert(batch.stream.clone(), key);
        }
        let key = self.keys[&batch.stream].clone();
        let first_batch = if self.used.insert(key.clone()) {
            let description = self
                .plan
                .streams
                .iter()
                .find(|stream| stream.key == key)
                .and_then(|stream| stream.description.clone())
                .unwrap_or_else(|| "Batches of a gateway stream, as exported".to_owned());
            Some((key.clone(), description))
        } else {
            None
        };
        match self.seqs.get_mut(&batch.stream) {
            Some((_, last)) => {
                if last.checked_add(1) != Some(batch.seq) {
                    return Err(format!(
                        "stream `{key}`: batch {} arrives after batch {last}: the format needs a \
                         stream's batches consecutive in arrival order, so a batch is missing, \
                         repeated or out of order (start the export after it, or fix the data)",
                        batch.seq
                    ));
                }
                *last = batch.seq;
            }
            None => {
                self.seqs
                    .insert(batch.stream.clone(), (batch.seq, batch.seq));
            }
        }
        let counts = commits.get(&(batch.stream.clone(), batch.seq));
        let committed = counts.is_some_and(|counts| !counts.is_empty());
        if let Some(counts) = counts {
            self.any_commit = true;
            if counts.iter().any(|count| *count != counts[0]) {
                return Err(format!(
                    "stream `{key}`: batch {} has completion records with different row \
                     counts",
                    batch.seq
                ));
            }
            if counts[0] != batch.found {
                return Err(format!(
                    "stream `{key}`: batch {} has {} distinct rows, its completion record \
                     says {}",
                    batch.seq, batch.found, counts[0]
                ));
            }
        }
        let mut readings = Vec::with_capacity(batch.readings.len());
        for reading in batch.readings {
            match self
                .repeats
                .get_mut(&(reading.device, reading.point, reading.ts_ms))
            {
                // One row in the range holds it: nothing can repeat it.
                None => readings.push(reading),
                Some(Some(first)) if *first == reading.value => self.repeated += 1,
                Some(Some(_)) => {
                    let device = &self.plan.devices[reading.device];
                    return Err(format!(
                        "{}/{} at {} is stored twice with different values (batch {key}/{} \
                         and an earlier one)",
                        device.key,
                        device.points[reading.point].key,
                        at_text(reading.ts_ms),
                        batch.seq
                    ));
                }
                Some(first @ None) => {
                    *first = Some(reading.value.clone());
                    readings.push(reading);
                }
            }
        }
        if readings.is_empty() {
            return Err(format!(
                "stream `{key}`: every reading of batch {} was already in an earlier batch, \
                 and a dataset has no empty batch (the stream would skip a number)",
                batch.seq
            ));
        }
        let received_at = DateTime::from_timestamp_millis(batch.received_ms)
            .ok_or_else(|| format!("{} ms is not a time", batch.received_ms))?;
        self.batches += 1;
        Ok((
            Prepared {
                stream: key,
                seq: batch.seq,
                received_at,
                committed,
                readings,
            },
            first_batch,
        ))
    }

    /// After the last batch: the number of readings dropped as repeats of an earlier batch's.
    fn finish(self) -> Result<u64, String> {
        if self.batches > 0 && !self.any_commit {
            return Err(
                "no batch has a completion record: the account probably has no row policy that \
                 names it on trustiot.stream_commits (the export would list every batch as \
                 uncommitted)"
                    .to_owned(),
            );
        }
        Ok(self.repeated)
    }
}

/// Rows received from `from_ms` up to, not including, `to_ms`, as the plan counted them: their
/// number, an upper bound of their answer's length, and their smallest and largest reading time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    from_ms: i64,
    to_ms: i64,
    rows: u64,
    bytes: u64,
    ts_min: i64,
    ts_max: i64,
}

impl Span {
    /// This span and the later `next`, with the time between them (which holds no rows).
    fn joined(self, next: Self) -> Self {
        Self {
            from_ms: self.from_ms,
            to_ms: next.to_ms,
            rows: self.rows.saturating_add(next.rows),
            bytes: self.bytes.saturating_add(next.bytes),
            ts_min: self.ts_min.min(next.ts_min),
            ts_max: self.ts_max.max(next.ts_max),
        }
    }

    fn read_window(&self) -> ReadWindow {
        ReadWindow {
            from_ms: self.from_ms,
            to_ms: self.to_ms,
            ts_min: self.ts_min,
            ts_max: self.ts_max,
        }
    }
}

/// Joins consecutive spans into windows while their answer's bound stays within the budget; a
/// span over the budget on its own is a window of its own.
#[derive(Debug)]
struct Windows {
    budget: u64,
    open: Option<Span>,
    done: Vec<Span>,
}

impl Windows {
    fn new(budget: u64) -> Self {
        Self {
            budget,
            open: None,
            done: Vec::new(),
        }
    }

    fn push(&mut self, span: Span) {
        self.open = Some(match self.open.take() {
            Some(open) if open.bytes.saturating_add(span.bytes) <= self.budget => open.joined(span),
            Some(open) => {
                self.done.push(open);
                span
            }
            None => span,
        });
    }

    fn finish(mut self) -> Vec<Span> {
        self.done.extend(self.open.take());
        self.done
    }
}

/// The spans of the buckets of width `BUCKET_WIDTHS_MS[level]` that hold rows from `from_ms` up
/// to, not including, `to_ms`.
async fn spans(
    clickhouse: &ClickHouse,
    (tenant, gateway): (Uuid, &str),
    from_ms: i64,
    to_ms: i64,
    level: usize,
) -> Result<Vec<Span>, Error> {
    let width = BUCKET_WIDTHS_MS[level];
    clickhouse
        .plan(tenant, gateway, from_ms, to_ms, width)
        .await?
        .into_iter()
        .map(|bucket| {
            let start = bucket
                .bucket
                .checked_mul(width)
                .and_then(|offset| from_ms.checked_add(offset))
                .filter(|start| (from_ms..to_ms).contains(start))
                .ok_or_else(|| {
                    Error::Export(format!(
                        "the plan has bucket {} of {width} ms outside the window it planned",
                        bucket.bucket
                    ))
                })?;
            Ok(Span {
                from_ms: start,
                to_ms: start.saturating_add(width).min(to_ms),
                rows: bucket.rows,
                bytes: bucket
                    .rows
                    .saturating_mul(LINE_BYTES)
                    .saturating_add(bucket.text_bytes),
                ts_min: bucket.ts_min,
                ts_max: bucket.ts_max,
            })
        })
        .collect()
}

/// The windows from `from_ms` up to, not including, `to_ms`, in order: per-minute buckets joined
/// within `limits`, and a minute over the budget split into seconds, a second into milliseconds.
async fn plan_windows(
    clickhouse: &ClickHouse,
    source: (Uuid, &str),
    from_ms: i64,
    to_ms: i64,
    limits: ReadLimits,
) -> Result<Vec<Span>, Error> {
    let mut pending: VecDeque<(Span, usize)> = spans(clickhouse, source, from_ms, to_ms, 0)
        .await?
        .into_iter()
        .map(|span| (span, 0))
        .collect();
    let mut windows = Windows::new(limits.window_bytes);
    while let Some((span, level)) = pending.pop_front() {
        if span.bytes > limits.window_bytes && level + 1 < BUCKET_WIDTHS_MS.len() {
            let finer = spans(clickhouse, source, span.from_ms, span.to_ms, level + 1).await?;
            for part in finer.into_iter().rev() {
                pending.push_front((part, level + 1));
            }
        } else {
            windows.push(span);
        }
    }
    Ok(windows.finish())
}

/// The directories `create_dir_all(path)` would create, deepest first.
fn missing_dirs(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .map(Path::to_path_buf)
        .collect()
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |error| Error::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

/// What an export wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Exported {
    pub dataset: String,
    /// The index file written.
    pub index: PathBuf,
    pub segments: usize,
    pub rows: u64,
    pub batches: u64,
    /// Batches without a completion record, listed as `uncommitted` in the index.
    pub uncommitted_batches: u64,
    /// Readings stored more than once with the same value, kept once (README, 「匯出」).
    pub repeated_rows_dropped: u64,
    /// The earliest reading time and the latest receive time.
    pub start: String,
    pub end: String,
    /// The windows the rows were read in: one query of the telemetry table each.
    pub windows: u64,
}

/// A checked export configuration.
#[derive(Debug)]
pub struct Exporter {
    plan: Plan,
    limits: ReadLimits,
}

/// What [`Exporter::write`] wrote into the `.partial` directory.
struct Written {
    segments: usize,
    rows: u64,
    batches: u64,
    uncommitted: u64,
    repeated: u64,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    windows: u64,
}

impl Exporter {
    /// Checks `config`.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn new(config: &Config) -> Result<Self, Error> {
        Plan::new(config)
            .map(|plan| Self {
                plan,
                limits: ReadLimits::default(),
            })
            .map_err(Error::Export)
    }

    /// The same export with other read limits: a test makes the windows small to make the
    /// export split its reads on little data. The files written do not depend on the limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: ReadLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Reads the gateway `gateway` of `tenant` for the batches received from `from` up to, not
    /// including, `to`, and writes the dataset to `out/<name>/`, which must not exist yet. Times
    /// are written as stored. Then checks the result with [`Dataset::open`].
    ///
    /// Window by window (the module's documentation): the files go to `out/<name>.partial/`,
    /// which becomes `out/<name>/` only when every row passed the rules, so a refused export
    /// leaves no files, and directories the export created for `out` are removed again.
    ///
    /// # Errors
    ///
    /// When the directory exists, ClickHouse refuses a read, no row is found (the account is
    /// probably not named by a row policy), a rule of the module's documentation is broken, the
    /// rows in the range change while the export runs, or the result fails its own check. A
    /// failed export removes what it wrote.
    pub async fn run(
        self,
        clickhouse: &ClickHouse,
        tenant: Uuid,
        gateway: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        out: &Path,
    ) -> Result<Exported, Error> {
        let plan = &self.plan;
        if from >= to {
            return Err(Error::Export("--from must be before --to".to_owned()));
        }
        let dir = out.join(&plan.name);
        if dir.exists() {
            return Err(Error::Export(format!(
                "{} exists: an export never writes into an existing dataset",
                dir.display()
            )));
        }
        let (from_ms, to_ms) = (from.timestamp_millis(), to.timestamp_millis());
        // Every device and point in the range, before any row is read.
        let mut census: BTreeMap<(usize, usize), u64> = BTreeMap::new();
        let mut listed = Vec::new();
        for series in clickhouse.series(tenant, gateway, from_ms, to_ms).await? {
            let position =
                series_position(plan, series.device, &series.point).map_err(Error::Export)?;
            census.insert(position, series.rows);
            if series.rows > 1 {
                listed.push((position, series.device, series.point));
            }
        }
        if census.is_empty() {
            return Err(Error::Export(
                "ClickHouse returned no rows for this tenant, gateway and time range. Either \
                 there is no data, or the account is not named by a row policy on \
                 trustiot.telemetry: a table's row policies hide every row from an account \
                 they do not name, the administrator account included (datasets/lab/README.md, \
                 「匯出」, names the account to read with)"
                    .to_owned(),
            ));
        }
        // The readings more than one row holds: the only ones remembered across windows.
        let mut repeats = Repeats::new();
        for ((device, point), device_id, point_key) in &listed {
            for ts_ms in clickhouse
                .repeated_times(tenant, gateway, (*device_id, point_key), from_ms, to_ms)
                .await?
            {
                repeats.insert((*device, *point, ts_ms), None);
            }
        }

        let created = missing_dirs(out);
        fs::create_dir_all(out).map_err(io_error(out))?;
        let partial = out.join(format!("{}.partial", plan.name));
        if let Err(error) = fs::create_dir(&partial) {
            remove_created(&created);
            return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Export(format!(
                    "{} exists: an export that stopped before it finished left it; remove it",
                    partial.display()
                ))
            } else {
                io_error(&partial)(error)
            });
        }
        let written = match self
            .write(
                clickhouse,
                (tenant, gateway),
                (from_ms, to_ms),
                census,
                repeats,
                &partial,
            )
            .await
        {
            Ok(written) if !dir.exists() => written,
            Ok(_) => {
                let _ = fs::remove_dir_all(&partial);
                return Err(Error::Export(format!(
                    "{} appeared while the export ran: an export never writes into an \
                     existing dataset",
                    dir.display()
                )));
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&partial);
                remove_created(&created);
                return Err(error);
            }
        };
        if let Err(error) = fs::rename(&partial, &dir) {
            let _ = fs::remove_dir_all(&partial);
            remove_created(&created);
            return Err(io_error(&dir)(error));
        }
        let index = dir.join(INDEX_FILE);
        if let Err(error) = Dataset::open(&index) {
            let _ = fs::remove_dir_all(&dir);
            remove_created(&created);
            return Err(Error::Export(format!(
                "the export fails its own check: {error}"
            )));
        }
        Ok(Exported {
            dataset: plan.name.clone(),
            index,
            segments: written.segments,
            rows: written.rows,
            batches: written.batches,
            uncommitted_batches: written.uncommitted,
            repeated_rows_dropped: written.repeated,
            start: written.start.map(time::format).unwrap_or_default(),
            end: written.end.map(time::format).unwrap_or_default(),
            windows: written.windows,
        })
    }

    /// Reads the range window by window and writes the dataset into `partial`. `census` holds
    /// the rows of each device and point in the range when the export started, `repeats` the
    /// readings more than one of them holds.
    async fn write(
        &self,
        clickhouse: &ClickHouse,
        (tenant, gateway): (Uuid, &str),
        (from_ms, to_ms): (i64, i64),
        census: BTreeMap<(usize, usize), u64>,
        repeats: Repeats,
        partial: &Path,
    ) -> Result<Written, Error> {
        let plan = &self.plan;
        let segments = partial.join("segments");
        fs::create_dir(&segments).map_err(io_error(&segments))?;
        let header = Header {
            name: plan.name.clone(),
            description: plan.description.clone(),
            origin: "Exported by `lab-dataset export` (Laysi/trustiot-v2#816)".to_owned(),
            // Declared as each stream's first batch arrives.
            streams: Vec::new(),
            segment_bytes: plan.segment_bytes,
        };
        let mut writer = SegmentWriter::new(partial, header, plan.devices.clone());
        let mut assembler = Assembler::new(plan, repeats);
        let mut read: BTreeMap<(usize, usize), u64> = BTreeMap::new();
        let (mut collapsed, mut uncommitted, mut read_windows) = (0, 0, 0);
        let span_ms = PLAN_SPAN.num_milliseconds();
        let mut start_ms = from_ms;
        while start_ms < to_ms {
            let end_ms = start_ms.saturating_add(span_ms).min(to_ms);
            let windows =
                plan_windows(clickhouse, (tenant, gateway), start_ms, end_ms, self.limits).await?;
            for window in windows {
                read_windows += 1;
                let rows = clickhouse
                    .telemetry_rows(tenant, gateway, &window.read_window())
                    .await
                    .map_err(|error| {
                        Error::Export(format!(
                            "reading the rows received from {} up to {}: {error}",
                            at_text(window.from_ms),
                            at_text(window.to_ms)
                        ))
                    })?;
                let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
                if count != window.rows {
                    return Err(changed(format!(
                        "{} rows were received from {} up to {} when the export planned its \
                         reads, {count} when it read them",
                        window.rows,
                        at_text(window.from_ms),
                        at_text(window.to_ms)
                    )));
                }
                let (batches, window_collapsed) = group(plan, &rows).map_err(Error::Export)?;
                collapsed += window_collapsed;
                for row in &rows {
                    let position =
                        series_position(plan, row.device_id, &row.point).map_err(Error::Export)?;
                    *read.entry(position).or_default() += 1;
                }
                drop(rows);
                let commits = window_commits(clickhouse, (tenant, gateway), &batches).await?;
                for batch in batches {
                    let (prepared, first) =
                        assembler.add(batch, &commits).map_err(Error::Export)?;
                    if let Some((key, description)) = first {
                        writer.add_stream(key, description);
                    }
                    write_batch(&mut writer, plan, &prepared)?;
                    uncommitted += u64::from(!prepared.committed);
                }
            }
            start_ms = end_ms;
        }
        let repeated = assembler.finish().map_err(Error::Export)?;
        if read != census {
            let differs = census
                .keys()
                .chain(read.keys())
                .find(|position| census.get(position) != read.get(position))
                .copied()
                .unwrap_or_default();
            let device = &plan.devices[differs.0];
            return Err(changed(format!(
                "{}/{} had {} rows in the range when the export started, {} when it read them",
                device.key,
                device.points[differs.1].key,
                census.get(&differs).copied().unwrap_or_default(),
                read.get(&differs).copied().unwrap_or_default()
            )));
        }
        let finished = writer.finish()?;
        Ok(Written {
            segments: finished.segments,
            rows: finished.rows,
            batches: finished.batches,
            uncommitted,
            repeated: collapsed + repeated,
            start: finished.start,
            end: finished.end,
            windows: read_windows,
        })
    }
}

/// The refusal of a range whose rows changed while the export read it.
fn changed(detail: String) -> Error {
    Error::Export(format!(
        "the rows in the range changed while the export read it ({detail}): export a range that \
         is no longer written to, or run it again"
    ))
}

/// Removes the directories [`missing_dirs`] listed, deepest first, if they are still empty.
fn remove_created(created: &[PathBuf]) {
    for dir in created {
        let _ = fs::remove_dir(dir);
    }
}

/// The completion records of a window's batches: for each stream, its numbers from the
/// window's first to its last batch.
async fn window_commits(
    clickhouse: &ClickHouse,
    (tenant, gateway): (Uuid, &str),
    batches: &[Batch],
) -> Result<BTreeMap<(String, u64), Vec<u64>>, Error> {
    let mut ranges: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for batch in batches {
        ranges
            .entry(batch.stream.as_str())
            .and_modify(|(first, last)| {
                *first = (*first).min(batch.seq);
                *last = (*last).max(batch.seq);
            })
            .or_insert((batch.seq, batch.seq));
    }
    let mut commits: BTreeMap<(String, u64), Vec<u64>> = BTreeMap::new();
    for (stream, (first, last)) in ranges {
        for (seq, row_count) in clickhouse
            .commits(tenant, gateway, stream, first, last)
            .await?
        {
            commits
                .entry((stream.to_owned(), seq))
                .or_default()
                .push(row_count);
        }
    }
    Ok(commits)
}

/// Writes one prepared batch.
fn write_batch(writer: &mut SegmentWriter, plan: &Plan, batch: &Prepared) -> Result<(), Error> {
    let rows: Vec<Row<'_>> = batch
        .readings
        .iter()
        .map(|reading| {
            let device = &plan.devices[reading.device];
            let point = &device.points[reading.point];
            let (value_type, value) = json(&reading.value);
            Ok(Row {
                device: &device.key,
                point: &point.key,
                ts: DateTime::from_timestamp_millis(reading.ts_ms)
                    .ok_or_else(|| Error::Export(format!("{} ms is not a time", reading.ts_ms)))?,
                value_type,
                value,
            })
        })
        .collect::<Result<_, Error>>()?;
    writer.write_batch(
        &batch.stream,
        batch.seq,
        batch.received_at,
        &rows,
        batch.committed,
    )
}

/// A value as a segment file line writes it.
fn json(value: &Value) -> (&'static str, serde_json::Value) {
    match value {
        Value::Float(float) => (
            "float",
            // Finite: `reading` refused anything else.
            serde_json::Number::from_f64(*float).map_or(serde_json::Value::Null, Into::into),
        ),
        Value::Int(int) => ("int", serde_json::Value::from(*int)),
        Value::Bool(flag) => ("bool", serde_json::Value::Bool(*flag)),
        Value::String(text) => ("string", serde_json::Value::String(text.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
name = "demo"
description = "unit test"

[[converters]]
key = "rs485-a"
host = "192.0.2.10"
port = 502

[[models]]
name = "transmitter"
points = [
  { key = "temperature", function_code = 4, address = 0, data_type = "i16", scale = 0.1, kind = "gauge", unit = "degC" },
  { key = "cycles", function_code = 3, address = 2, data_type = "u32", kind = "cumulative", wraps_at = 4294967296 },
]

[[devices]]
key = "sensor-1"
id = "00000000-0000-0000-0000-0000000000a1"
model = "transmitter"
converter = "rs485-a"
unit = 1
interval_s = 60
"#;

    const SENSOR: Uuid = Uuid::from_u128(0xa1);

    fn plan(from: &str, to: &str) -> Result<Plan, String> {
        assert!(
            CONFIG.contains(from),
            "the base configuration has no `{from}`"
        );
        Plan::new(&Config::parse(&CONFIG.replacen(from, to, 1))?)
    }

    /// A stored float row of the `temperature` point (or of `point`).
    fn row(stream: &str, seq: u64, ts_ms: i64, point: &str, value: f64) -> StoredRow {
        StoredRow {
            stream_id: stream.to_owned(),
            batch_seq: seq,
            device_id: SENSOR,
            point: point.to_owned(),
            ts_ms,
            received_ms: 1_000_000 + i64::try_from(seq).unwrap() * 1000,
            value_type: ValueType::Float,
            value_float: Some(value),
            value_int: 0,
            value_bool: false,
            value_string: String::new(),
        }
    }

    /// A stored integer row of the `cycles` point.
    fn cycles(stream: &str, seq: u64, ts_ms: i64, value: i64) -> StoredRow {
        StoredRow {
            point: "cycles".to_owned(),
            value_type: ValueType::Int,
            value_float: None,
            value_int: value,
            ..row(stream, seq, ts_ms, "cycles", 0.0)
        }
    }

    fn commits(entries: &[(&str, u64, u64)]) -> BTreeMap<(String, u64), Vec<u64>> {
        let mut map: BTreeMap<(String, u64), Vec<u64>> = BTreeMap::new();
        for (stream, seq, count) in entries {
            map.entry(((*stream).to_owned(), *seq))
                .or_default()
                .push(*count);
        }
        map
    }

    /// What the assembler made of the batches of one or more windows.
    #[derive(Debug)]
    struct Assembled {
        batches: Vec<Prepared>,
        /// The streams in the order their first batch arrived: (key, description).
        streams: Vec<(String, String)>,
        repeated: u64,
    }

    /// The readings more than one of `rows` holds, as the export asks ClickHouse for them
    /// (`ClickHouse::repeated_times`).
    fn repeats_of<'r>(plan: &Plan, rows: impl Iterator<Item = &'r StoredRow>) -> Repeats {
        let mut counts: HashMap<(usize, usize, i64), u64> = HashMap::new();
        for row in rows {
            if let Ok((device, point)) = series_position(plan, row.device_id, &row.point) {
                *counts.entry((device, point, row.ts_ms)).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(at, _)| (at, None))
            .collect()
    }

    /// Groups and assembles `windows`, each a window's rows in arrival order, as the export
    /// does.
    fn assemble_windows(
        plan: &Plan,
        windows: &[&[StoredRow]],
        commits: &BTreeMap<(String, u64), Vec<u64>>,
    ) -> Result<(Assembled, u64), String> {
        let repeats = repeats_of(plan, windows.iter().flat_map(|rows| rows.iter()));
        let mut assembler = Assembler::new(plan, repeats);
        let mut assembled = Assembled {
            batches: Vec::new(),
            streams: Vec::new(),
            repeated: 0,
        };
        let mut collapsed = 0;
        for rows in windows {
            let (batches, window_collapsed) = group(plan, rows)?;
            collapsed += window_collapsed;
            for batch in batches {
                let (prepared, first) = assembler.add(batch, commits)?;
                assembled.streams.extend(first);
                assembled.batches.push(prepared);
            }
        }
        assembled.repeated = assembler.finish()?;
        Ok((assembled, collapsed))
    }

    fn assemble(
        plan: &Plan,
        rows: &[StoredRow],
        commits: &BTreeMap<(String, u64), Vec<u64>>,
    ) -> Result<(Assembled, u64), String> {
        assemble_windows(plan, &[rows], commits)
    }

    #[test]
    fn the_base_configuration_plans_the_device() {
        let plan = plan("", "").unwrap();
        assert_eq!(plan.devices.len(), 1);
        assert_eq!(plan.devices[0].converter.as_deref(), Some("rs485-a"));
        let types: Vec<(&str, ValueType)> = plan.devices[0]
            .points
            .iter()
            .map(|point| (point.key.as_str(), point.value_type))
            .collect();
        assert_eq!(
            types,
            [
                ("temperature", ValueType::Float),
                ("cycles", ValueType::Int)
            ]
        );
    }

    #[test]
    fn every_configuration_rule_refuses_its_violation() {
        for (from, to, expected) in [
            ("name = \"demo\"", "name = \"Demo\"", "not a key"),
            (
                "description = \"unit test\"",
                "description = \" \"",
                "empty",
            ),
            (
                "description = \"unit test\"",
                "description = \"x\"\nsegment_bytes = 10",
                "segment_bytes",
            ),
            (
                "description = \"unit test\"",
                "description = \"x\"\nextra = 1",
                "unknown field",
            ),
            (
                "id = \"00000000-0000-0000-0000-0000000000a1\"",
                "id = \"00000000-0000-0000-0000-000000000000\"",
                "nil UUID",
            ),
            (
                "id = \"00000000-0000-0000-0000-0000000000a1\"\n",
                "",
                "missing field `id`",
            ),
            ("unit = 1", "unit = 0", "1 to 247"),
            ("interval_s = 60", "interval_s = 0", "interval"),
            (
                "model = \"transmitter\"\nconverter",
                "model = \"other\"\nconverter",
                "not listed",
            ),
            (
                "converter = \"rs485-a\"\nunit",
                "converter = \"rs485-b\"\nunit",
                "not listed",
            ),
            (
                "converter = \"rs485-a\"\nunit",
                "converter = \"rs485-a\"\nhost = \"192.0.2.11\"\nunit",
                "either",
            ),
            ("key = \"cycles\"", "key = \"temperature\"", "appears twice"),
            ("wraps_at = 4294967296", "wraps_at = 4294967297", "wraps_at"),
            (
                "[[devices]]",
                "[[streams]]\nkey = \"live\"\nid = \"bad id\"\n\n[[devices]]",
                "stream `live`",
            ),
        ] {
            let error = plan(from, to)
                .map(|_| ())
                .expect_err(&format!("`{from}` -> `{to}` was accepted"));
            assert!(
                error.contains(expected),
                "`{from}` -> `{to}`: `{error}` lacks `{expected}`"
            );
        }
        // A recording configuration's addresses and batch interval are accepted and ignored.
        assert!(plan("[[converters]]", "batch_s = 4\n\n[[converters]]").is_ok());
    }

    #[test]
    fn a_batch_is_written_with_its_commit_and_streams_are_named() {
        let plan = plan("", "").unwrap();
        let rows = [
            row("gw-stream", 1, 1_000, "temperature", 21.5),
            cycles("gw-stream", 1, 1_000, 7),
            row("gw-stream", 2, 61_000, "temperature", 21.6),
            row("other", 9, 62_000, "temperature", 20.0),
        ];
        let (assembled, collapsed) = assemble(
            &plan,
            &rows,
            &commits(&[("gw-stream", 1, 2), ("gw-stream", 2, 1)]),
        )
        .unwrap();
        assert_eq!(collapsed, 0);
        let described: Vec<(&str, u64, bool, usize)> = assembled
            .batches
            .iter()
            .map(|b| (b.stream.as_str(), b.seq, b.committed, b.readings.len()))
            .collect();
        assert_eq!(
            described,
            [
                ("stream-1", 1, true, 2),
                ("stream-1", 2, true, 1),
                // Another stream whose first batch is 9: no earlier batch is required.
                ("stream-2", 9, false, 1),
            ]
        );
        assert_eq!(
            assembled
                .streams
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>(),
            ["stream-1", "stream-2"]
        );
    }

    #[test]
    fn a_configured_stream_keeps_its_key_and_an_automatic_one_skips_taken_keys() {
        let plan = plan(
            "[[devices]]",
            "[[streams]]\nkey = \"stream-1\"\nid = \"other\"\ndescription = \"the second\"\n\n[[devices]]",
        )
        .unwrap();
        let rows = [
            row("gw-stream", 1, 1_000, "temperature", 21.5),
            row("other", 1, 2_000, "temperature", 21.6),
        ];
        let (assembled, _) = assemble(&plan, &rows, &commits(&[("gw-stream", 1, 1)])).unwrap();
        let keys: Vec<&str> = assembled
            .batches
            .iter()
            .map(|b| b.stream.as_str())
            .collect();
        // `gw-stream` is automatic and must not take the configured `stream-1`.
        assert_eq!(keys, ["stream-2", "stream-1"]);
        assert_eq!(assembled.streams[1].1, "the second");
    }

    #[test]
    fn a_repeated_row_with_the_same_value_is_kept_once() {
        let plan = plan("", "").unwrap();
        // The same reading twice in one batch (a rewrite), and again in the next batch (a resend).
        let rows = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 1, 1_000, "temperature", 21.5),
            cycles("s", 1, 1_000, 7),
            row("s", 2, 1_000, "temperature", 21.5),
            row("s", 2, 61_000, "temperature", 21.6),
        ];
        let (assembled, collapsed) =
            assemble(&plan, &rows, &commits(&[("s", 1, 2), ("s", 2, 2)])).unwrap();
        assert_eq!(collapsed, 1);
        assert_eq!(assembled.repeated, 1);
        let counts: Vec<usize> = assembled.batches.iter().map(|b| b.readings.len()).collect();
        // Batch 2 keeps only the reading that batch 1 does not hold.
        assert_eq!(counts, [2, 1]);
    }

    #[test]
    fn a_repeated_row_with_another_value_is_refused() {
        let plan = plan("", "").unwrap();
        let in_batch = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 1, 1_000, "temperature", 22.5),
        ];
        let error = assemble(&plan, &in_batch, &commits(&[("s", 1, 1)])).unwrap_err();
        assert!(error.contains("different values"), "{error}");
        let across = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 2, 1_000, "temperature", 22.5),
        ];
        let error = assemble(&plan, &across, &commits(&[("s", 1, 1), ("s", 2, 1)])).unwrap_err();
        assert!(error.contains("different values"), "{error}");
    }

    #[test]
    fn a_batch_made_only_of_repeats_is_refused() {
        let plan = plan("", "").unwrap();
        let rows = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 2, 1_000, "temperature", 21.5),
        ];
        let error = assemble(&plan, &rows, &commits(&[("s", 1, 1), ("s", 2, 1)])).unwrap_err();
        assert!(error.contains("empty batch"), "{error}");
    }

    #[test]
    fn a_batch_must_agree_with_its_completion_record() {
        let plan = plan("", "").unwrap();
        let rows = [row("s", 1, 1_000, "temperature", 21.5)];
        let error = assemble(&plan, &rows, &commits(&[("s", 1, 2)])).unwrap_err();
        assert!(error.contains("completion record says 2"), "{error}");
        // The same record twice (a replacing table before its merge) is one record.
        let (assembled, _) = assemble(&plan, &rows, &commits(&[("s", 1, 1), ("s", 1, 1)])).unwrap();
        assert!(assembled.batches[0].committed);
        // Two records that disagree are refused.
        let error = assemble(&plan, &rows, &commits(&[("s", 1, 1), ("s", 1, 2)])).unwrap_err();
        assert!(error.contains("different row counts"), "{error}");
    }

    #[test]
    fn no_completion_record_at_all_is_refused() {
        let plan = plan("", "").unwrap();
        let rows = [row("s", 1, 1_000, "temperature", 21.5)];
        let error = assemble(&plan, &rows, &commits(&[])).unwrap_err();
        assert!(
            error.contains("no batch has a completion record"),
            "{error}"
        );
        // One committed batch among uncommitted ones is a normal export.
        let rows = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 2, 61_000, "temperature", 21.6),
        ];
        let (assembled, _) = assemble(&plan, &rows, &commits(&[("s", 1, 1)])).unwrap();
        assert_eq!(
            assembled
                .batches
                .iter()
                .map(|b| b.committed)
                .collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn a_gap_in_a_stream_is_refused() {
        let plan = plan("", "").unwrap();
        let rows = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 3, 121_000, "temperature", 21.7),
        ];
        let error = assemble(&plan, &rows, &commits(&[("s", 1, 1), ("s", 3, 1)])).unwrap_err();
        assert!(error.contains("batch 3 arrives after batch 1"), "{error}");
    }

    #[test]
    fn rows_the_configuration_does_not_describe_are_refused() {
        let plan = plan("", "").unwrap();
        let ok = commits(&[("s", 1, 1)]);
        let mut unknown_device = row("s", 1, 1_000, "temperature", 21.5);
        unknown_device.device_id = Uuid::from_u128(0xb2);
        let mut unknown_point = row("s", 1, 1_000, "temperature", 21.5);
        unknown_point.point = "humidity".to_owned();
        let mut wrong_type = row("s", 1, 1_000, "temperature", 21.5);
        wrong_type.value_type = ValueType::Int;
        let wrapped = cycles("s", 1, 1_000, 4_294_967_296);
        let mut not_finite = row("s", 1, 1_000, "temperature", 21.5);
        not_finite.value_float = None;
        let mut text = row("s", 1, 1_000, "temperature", 21.5);
        text.value_type = ValueType::String;
        for (bad, expected) in [
            (unknown_device, "does not list"),
            (unknown_point, "does not list"),
            (wrong_type, "declares float"),
            (wrapped, "wraps_at"),
            (not_finite, "finite"),
            (text, "declares float"),
        ] {
            let error = assemble(&plan, &[bad], &ok).unwrap_err();
            assert!(error.contains(expected), "`{error}` lacks `{expected}`");
        }
    }

    #[test]
    fn a_batch_has_one_receive_time() {
        let plan = plan("", "").unwrap();
        let mut late = row("s", 1, 61_000, "temperature", 21.6);
        late.received_ms += 1;
        let rows = [row("s", 1, 1_000, "temperature", 21.5), late];
        let error = assemble(&plan, &rows, &commits(&[("s", 1, 2)])).unwrap_err();
        assert!(error.contains("different times"), "{error}");
    }

    #[test]
    fn windows_give_the_same_batches_as_one_read() {
        let plan = plan("", "").unwrap();
        // In arrival order (`row` receives batch n at 1,000,000 + n * 1000 ms).
        let rows = [
            row("s", 1, 1_000, "temperature", 21.5),
            cycles("s", 1, 1_000, 7),
            // A resend of batch 1's temperature, and its own reading.
            row("s", 2, 1_000, "temperature", 21.5),
            row("s", 2, 61_000, "temperature", 21.6),
            row("s", 3, 121_000, "temperature", 21.7),
            row("other", 9, 62_000, "temperature", 20.0),
        ];
        let commits = commits(&[("s", 1, 2), ("s", 2, 2), ("s", 3, 1)]);
        let describe = |assembled: &Assembled| {
            assembled
                .batches
                .iter()
                .map(|b| {
                    (
                        b.stream.clone(),
                        b.seq,
                        b.committed,
                        b.readings.clone(),
                        b.received_at,
                    )
                })
                .collect::<Vec<_>>()
        };
        let (whole, collapsed) = assemble(&plan, &rows, &commits).unwrap();
        // A window ends where the receive time changes: it never cuts a batch.
        let cuts: Vec<usize> = (1..rows.len())
            .filter(|cut| rows[cut - 1].received_ms != rows[*cut].received_ms)
            .collect();
        assert_eq!(cuts, [2, 4, 5]);
        for cut in cuts {
            let (windowed, windowed_collapsed) =
                assemble_windows(&plan, &[&rows[..cut], &rows[cut..]], &commits).unwrap();
            assert_eq!(describe(&windowed), describe(&whole), "cut at {cut}");
            assert_eq!(windowed.streams, whole.streams, "cut at {cut}");
            assert_eq!(
                (windowed.repeated, windowed_collapsed),
                (whole.repeated, collapsed),
                "cut at {cut}"
            );
        }
        assert_eq!(whole.repeated, 1);
    }

    #[test]
    fn a_batch_in_two_windows_is_refused() {
        let plan = plan("", "").unwrap();
        // Batch 1 again in a later window: its rows were received at two times.
        let first = [
            row("s", 1, 1_000, "temperature", 21.5),
            row("s", 2, 61_000, "temperature", 21.6),
        ];
        let mut again = row("s", 1, 1_000, "temperature", 21.5);
        again.received_ms += 500_000;
        let error = assemble_windows(
            &plan,
            &[&first, &[again]],
            &commits(&[("s", 1, 1), ("s", 2, 1)]),
        )
        .unwrap_err();
        assert!(
            error.contains("batch s/1 has rows received at different times"),
            "{error}"
        );
        // The same within one window is `group`'s refusal, with the same words.
        let mut late = row("s", 1, 61_000, "temperature", 21.6);
        late.received_ms += 1;
        let error = assemble_windows(
            &plan,
            &[&[row("s", 1, 1_000, "temperature", 21.5), late]],
            &commits(&[("s", 1, 2)]),
        )
        .unwrap_err();
        assert!(
            error.contains("batch s/1 has rows received at different times"),
            "{error}"
        );
    }

    #[test]
    fn windows_join_spans_within_the_budget() {
        let span = |from_ms: i64, bytes: u64, ts: i64| Span {
            from_ms,
            to_ms: from_ms + 60_000,
            rows: 1,
            bytes,
            ts_min: ts,
            ts_max: ts,
        };
        let mut windows = Windows::new(100);
        windows.push(span(0, 40, 5));
        // A gap of empty minutes, then a reading whose clock is behind.
        windows.push(span(180_000, 60, -7));
        windows.push(span(240_000, 1, 9));
        // Over the budget on its own: a window of its own.
        windows.push(span(300_000, 250, 3));
        windows.push(span(360_000, 10, 4));
        let windows = windows.finish();
        let described: Vec<(i64, i64, u64, u64, i64, i64)> = windows
            .iter()
            .map(|w| (w.from_ms, w.to_ms, w.rows, w.bytes, w.ts_min, w.ts_max))
            .collect();
        assert_eq!(
            described,
            [
                (0, 240_000, 2, 100, -7, 5),
                (240_000, 300_000, 1, 1, 9, 9),
                (300_000, 360_000, 1, 250, 3, 3),
                (360_000, 420_000, 1, 10, 4, 4),
            ]
        );
        assert!(Windows::new(100).finish().is_empty());
    }

    #[test]
    fn missing_dirs_lists_what_an_export_creates() {
        let base = std::env::temp_dir().join(format!(
            "lab-dataset-missing-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&base).unwrap();
        let out = base.join("a").join("b");
        assert_eq!(missing_dirs(&out), [out.clone(), base.join("a")]);
        assert!(missing_dirs(&base).is_empty());
        fs::create_dir_all(&out).unwrap();
        remove_created(&[out.clone(), base.join("a")]);
        assert!(!base.join("a").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_readme_example_is_a_valid_configuration() {
        let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../datasets/lab/README.md");
        let readme = fs::read_to_string(readme).unwrap();
        let example = readme
            .split("```toml\n")
            .filter_map(|block| block.split("```").next())
            .find(|block| block.contains("[[devices]]") && block.contains("\nid = \""))
            .expect("datasets/lab/README.md has an export configuration");
        let config = Config::parse(&example.replace("<閘道器的串流 ID>", "stream-1")).unwrap();
        let plan = Plan::new(&config).unwrap();
        assert_eq!(plan.devices.len(), 2);
        assert_eq!(plan.streams.len(), 1);
        assert_eq!(plan.devices[1].converter.as_deref(), Some("rs485-a"));
    }
}
