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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use protocol::StreamId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::clickhouse::StoredRow;
use crate::index::{self, MAX_SEGMENT_BYTES, MAX_TEXT_BYTES, ValueType, check_key};
use crate::record::{MIN_SEGMENT_BYTES, Model, plan_point};
use crate::segment::Value;
use crate::writer::{Header, Row, SegmentWriter};
use crate::{ClickHouse, Dataset, Error, time};

/// The time span of one read: a batch lies in one window, because all its rows share one
/// `received_at`, so no window cuts a batch, and one answer stays far below the HTTP client's
/// body limit for the polling rates of the lab.
const WINDOW: TimeDelta = TimeDelta::hours(1);

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

/// The value of a stored row as the index's point declares it.
fn reading(plan: &Plan, row: &StoredRow) -> Result<Reading, String> {
    let device = *plan.by_id.get(&row.device_id).ok_or_else(|| {
        format!(
            "the gateway sent rows of device {}, which the configuration does not list (a \
             dataset holds everything the gateway sent: add the device)",
            row.device_id
        )
    })?;
    let listed = &plan.devices[device];
    let point = listed
        .points
        .iter()
        .position(|point| point.key == row.point)
        .ok_or_else(|| {
            format!(
                "device `{}` sent point `{}`, which its model does not list",
                listed.key, row.point
            )
        })?;
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
            return Err(format!(
                "batch {}/{} has rows received at different times (a stream's batch numbers \
                 started again, or the batch was written twice): narrow --from and --to to \
                 one of the two",
                row.stream_id, row.batch_seq
            ));
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

/// What [`finish`] returns.
#[derive(Debug)]
struct Assembled {
    batches: Vec<Prepared>,
    /// The streams used, in the order their first batch arrived: (key, description).
    streams: Vec<(String, String)>,
    /// Readings dropped as repeats of an earlier arrival's, beyond the batches' own repeats.
    repeated: u64,
}

/// Names the streams, checks the arrival order and the completion records, and drops the
/// readings an earlier batch already holds. `commits` has, for each (stream ID, sequence
/// number), the row counts of its completion records.
fn finish(
    plan: &Plan,
    batches: Vec<Batch>,
    commits: &BTreeMap<(String, u64), Vec<u64>>,
) -> Result<Assembled, String> {
    // Stream keys: the configured ones, then `stream-<n>` in order of first arrival.
    let mut keys: HashMap<&str, String> = plan
        .streams
        .iter()
        .map(|stream| (stream.id.as_str(), stream.key.clone()))
        .collect();
    let mut taken: BTreeSet<String> = plan.streams.iter().map(|s| s.key.clone()).collect();
    let mut used: Vec<(String, String)> = Vec::new();
    let mut next_auto = 1_u32;
    let mut last_seq: HashMap<&str, u64> = HashMap::new();
    let mut seen: HashMap<(usize, usize, i64), &Value> = HashMap::new();
    let mut repeated = 0;
    let mut prepared = Vec::with_capacity(batches.len());
    let mut any_commit = false;
    for batch in &batches {
        if !keys.contains_key(batch.stream.as_str()) {
            let key = loop {
                let key = format!("stream-{next_auto}");
                next_auto += 1;
                if !taken.contains(&key) {
                    break key;
                }
            };
            taken.insert(key.clone());
            keys.insert(batch.stream.as_str(), key);
        }
        let key = keys[batch.stream.as_str()].clone();
        if !used.iter().any(|(used_key, _)| *used_key == key) {
            let description = plan
                .streams
                .iter()
                .find(|stream| stream.key == key)
                .and_then(|stream| stream.description.clone())
                .unwrap_or_else(|| "Batches of a gateway stream, as exported".to_owned());
            used.push((key.clone(), description));
        }
        if let Some(previous) = last_seq.insert(batch.stream.as_str(), batch.seq)
            && previous.checked_add(1) != Some(batch.seq)
        {
            return Err(format!(
                "stream `{key}`: batch {} arrives after batch {previous}: the format needs a \
                 stream's batches consecutive in arrival order, so a batch is missing, \
                 repeated or out of order (start the export after it, or fix the data)",
                batch.seq
            ));
        }
        let counts = commits.get(&(batch.stream.clone(), batch.seq));
        let committed = counts.is_some_and(|counts| !counts.is_empty());
        if let Some(counts) = counts {
            any_commit = true;
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
        for reading in &batch.readings {
            let at = (reading.device, reading.point, reading.ts_ms);
            match seen.get(&at) {
                Some(first) if **first == reading.value => repeated += 1,
                Some(_) => {
                    let device = &plan.devices[reading.device];
                    return Err(format!(
                        "{}/{} at {} is stored twice with different values (batch {key}/{} \
                         and an earlier one)",
                        device.key,
                        device.points[reading.point].key,
                        at_text(reading.ts_ms),
                        batch.seq
                    ));
                }
                None => {
                    seen.insert(at, &reading.value);
                    readings.push(reading.clone());
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
        prepared.push(Prepared {
            stream: key,
            seq: batch.seq,
            received_at,
            committed,
            readings,
        });
    }
    if !batches.is_empty() && !any_commit {
        return Err(
            "no batch has a completion record: the account probably has no row policy that \
             names it on trustiot.stream_commits (the export would list every batch as \
             uncommitted)"
                .to_owned(),
        );
    }
    Ok(Assembled {
        batches: prepared,
        streams: used,
        repeated,
    })
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
}

/// A checked export configuration.
#[derive(Debug)]
pub struct Exporter {
    plan: Plan,
}

impl Exporter {
    /// Checks `config`.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn new(config: &Config) -> Result<Self, Error> {
        Plan::new(config)
            .map(|plan| Self { plan })
            .map_err(Error::Export)
    }

    /// Reads the gateway `gateway` of `tenant` for the batches received from `from` up to, not
    /// including, `to`, and writes the dataset to `out/<name>/`, which must not exist yet. Times
    /// are written as stored. Then checks the result with [`Dataset::open`].
    ///
    /// Nothing is written unless every row passes the rules, so a refused export leaves no
    /// files.
    ///
    /// # Errors
    ///
    /// When the directory exists, ClickHouse refuses a read, no row is found (the account is
    /// probably not named by a row policy), a rule of the module's documentation is broken, or
    /// the result fails its own check (files already written stay for inspection).
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
        let mut rows = Vec::new();
        let mut start = from;
        while start < to {
            let end = (start + WINDOW).min(to);
            rows.extend(
                clickhouse
                    .telemetry_rows(tenant, gateway, start, end)
                    .await?,
            );
            start = end;
        }
        if rows.is_empty() {
            return Err(Error::Export(
                "ClickHouse returned no rows for this tenant, gateway and time range. Either \
                 there is no data, or the account is not named by a row policy on \
                 trustiot.telemetry: a table's row policies hide every row from an account \
                 they do not name, the administrator account included (datasets/lab/README.md, \
                 「匯出」, names the account to read with)"
                    .to_owned(),
            ));
        }
        let (batches, collapsed) = group(plan, &rows).map_err(Error::Export)?;
        drop(rows);
        let mut ranges: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for batch in &batches {
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
        let assembled = finish(plan, batches, &commits).map_err(Error::Export)?;

        fs::create_dir_all(dir.join("segments")).map_err(|error| Error::Io {
            path: dir.display().to_string(),
            message: error.to_string(),
        })?;
        let header = Header {
            name: plan.name.clone(),
            description: plan.description.clone(),
            origin: "Exported by `lab-dataset export` (Laysi/trustiot-v2#816)".to_owned(),
            streams: assembled.streams.clone(),
            segment_bytes: plan.segment_bytes,
        };
        let mut writer = SegmentWriter::new(&dir, header, plan.devices.clone());
        let mut uncommitted = 0;
        for batch in &assembled.batches {
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
                        ts: DateTime::from_timestamp_millis(reading.ts_ms).ok_or_else(|| {
                            Error::Export(format!("{} ms is not a time", reading.ts_ms))
                        })?,
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
            )?;
            uncommitted += u64::from(!batch.committed);
        }
        let finished = writer.finish()?;
        Dataset::open(&finished.index)
            .map_err(|error| Error::Export(format!("the export fails its own check: {error}")))?;
        Ok(Exported {
            dataset: plan.name.clone(),
            index: finished.index,
            segments: finished.segments,
            rows: finished.rows,
            batches: finished.batches,
            uncommitted_batches: uncommitted,
            repeated_rows_dropped: collapsed + assembled.repeated,
            start: finished.start.map(time::format).unwrap_or_default(),
            end: finished.end.map(time::format).unwrap_or_default(),
        })
    }
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

    fn assemble(
        plan: &Plan,
        rows: &[StoredRow],
        commits: &BTreeMap<(String, u64), Vec<u64>>,
    ) -> Result<(Assembled, u64), String> {
        let (batches, collapsed) = group(plan, rows)?;
        finish(plan, batches, commits).map(|assembled| (assembled, collapsed))
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
