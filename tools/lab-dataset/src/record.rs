//! The recording tool (`lab-dataset record`, Laysi/trustiot-v2#679): reads devices with the
//! gateway agent's Modbus module and writes what it reads as a dataset in the index format
//! (`datasets/lab/README.md`), which [`crate::Dataset::open`] checks at the end.
//!
//! The devices and their register maps are `crates/protocol`'s desired configuration
//! (`DeviceModel`, `Device`), validated as the gateway validates it. Each device is polled by
//! `agent::polling::run` (fixed schedule, timeouts, backoff, watchdog) through
//! `agent::modbus::endpoint::Endpoints`, so devices behind one RS485 converter share one
//! connection, one request at a time, and the RTU framing drops other units' answers
//! (Laysi/trustiot-v2#87, Laysi/trustiot-v2#88).
//!
//! The recording configuration names converter addresses and host names, so it stays outside
//! the repository; the index the tool writes names only the converters' logical keys and the
//! unit IDs (`datasets/lab/README.md`, 「錄製」).
//!
//! Batches: every `batch_s` seconds the readings that arrived since the previous batch form one
//! batch (more when they exceed the protocol's batch size, `protocol::telemetry::BatchBuilder`),
//! numbered consecutively in the single stream `live`, and `received_at` is the recorder's clock
//! when the batch closed: what ingest would have stamped with no network delay and no outage.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent::modbus::endpoint::Endpoints;
use agent::modbus::{Clock, DeviceReader, Timestamp};
use agent::polling::{self, Event, Settings};
use chrono::{DateTime, Utc};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device as ModbusDevice, DeviceModel,
    FunctionCode, Host, MAX_POLL_INTERVAL_MS, Transport,
};
use protocol::telemetry::{BatchBuilder, Reading as AgentReading, TelemetryBatch};
use protocol::{
    ConfigVersion, DeviceId, Message, ModelName, PointKey, Seq, StreamId, UnixMillis, V1,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};

use crate::index::{self, MAX_SEGMENT_BYTES, PointKind, ValueType, check_key};
use crate::writer::{Header, Row, SegmentWriter};
use crate::{Dataset, Error, time};

/// The stream every recorded batch belongs to.
pub const STREAM: &str = "live";

/// The default batch interval, in seconds: the gateway agent's prototype closed its open batch
/// every 10 seconds (docs/feasibility/2026-10-13-gateway-memory.md).
pub const DEFAULT_BATCH_S: u32 = 10;

/// The smallest segment size a configuration may ask for, in bytes.
pub const MIN_SEGMENT_BYTES: u64 = 1024;

/// A recording configuration (TOML). It names converter addresses and host names: keep it
/// outside the repository.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The dataset's name: the directory the recording is written to, and the index's `name`.
    pub name: String,
    /// The index's one-sentence description.
    pub description: String,
    /// How often a batch closes, in seconds (1 to 3600).
    #[serde(default = "default_batch_s")]
    pub batch_s: u32,
    /// The largest segment file, in bytes ([`MIN_SEGMENT_BYTES`] to the index's 64 MiB).
    #[serde(default = "default_segment_bytes")]
    pub segment_bytes: u64,
    /// The RS485 converters: a logical key and where to reach it.
    #[serde(default)]
    pub converters: Vec<Converter>,
    /// The device models: register maps, as in the desired configuration.
    pub models: Vec<Model>,
    pub devices: Vec<DeviceConfig>,
}

const fn default_batch_s() -> u32 {
    DEFAULT_BATCH_S
}

const fn default_segment_bytes() -> u64 {
    MAX_SEGMENT_BYTES
}

/// An RS485 converter, read with RTU over TCP.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Converter {
    /// The logical key the index uses, for example `rs485-a`.
    pub key: String,
    /// Its address or host name; never written to the index.
    pub host: String,
    pub port: u16,
}

/// A device model: a generic name (it becomes the index's `model`) and its points.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub name: String,
    pub points: Vec<PointConfig>,
}

/// A data point: how to read it (the desired configuration's `DataPoint`) and what it is (the
/// index's `kind`, `unit` and `wraps_at`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointConfig {
    pub key: String,
    /// 1 to 4.
    pub function_code: u8,
    pub address: u16,
    pub data_type: DataType,
    #[serde(default)]
    pub byte_order: ByteOrder,
    #[serde(default)]
    pub length: Option<u16>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub offset: Option<f64>,
    pub kind: PointKind,
    #[serde(default)]
    pub unit: Option<String>,
    /// A cumulative unsigned counter's wrap value: 2 to 2^bits of its data type.
    #[serde(default)]
    pub wraps_at: Option<u64>,
}

/// A device: behind a converter (`converter`), or a Modbus TCP device (`host` and `port`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub key: String,
    pub model: String,
    #[serde(default)]
    pub converter: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    pub unit: u8,
    /// The polling interval in seconds (1 to 86400).
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
        Self::parse(&text).map_err(Error::Record)
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

/// The number type a data point's readings have: what `agent::modbus::decode` sends.
fn value_type(point: &PointConfig) -> ValueType {
    if point.data_type.is_numeric() && (point.scale.is_some() || point.offset.is_some()) {
        return ValueType::Float;
    }
    match point.data_type {
        DataType::Bool => ValueType::Bool,
        DataType::U16
        | DataType::I16
        | DataType::U32
        | DataType::I32
        | DataType::U64
        | DataType::I64 => ValueType::Int,
        DataType::F32 | DataType::F64 => ValueType::Float,
        DataType::String => ValueType::String,
    }
}

/// The largest `wraps_at` of an unsigned counter of `data_type`: 2^bits (2^63 for `u64`, whose
/// values above `i64::MAX` are read failures).
const fn wrap_limit(data_type: DataType) -> Option<u64> {
    match data_type {
        DataType::U16 => Some(1 << 16),
        DataType::U32 => Some(1 << 32),
        DataType::U64 => Some(1 << 63),
        _ => None,
    }
}

/// One device ready to poll.
#[derive(Clone, Debug)]
struct Planned {
    modbus: ModbusDevice,
    reader: DeviceReader,
}

/// A checked configuration: the devices with their readers, and the index's device entries.
#[derive(Debug)]
pub struct Recorder {
    config: Config,
    devices: Vec<Planned>,
    index_devices: Vec<index::Device>,
    polls: Arc<watch::Sender<u64>>,
}

/// What a recording wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Recorded {
    pub dataset: String,
    /// The index file written.
    pub index: PathBuf,
    pub segments: usize,
    pub rows: u64,
    pub batches: u64,
    /// Readings left out because they break the index's rules (a counter reading at or above
    /// its `wraps_at`, or a value the protocol cannot carry).
    pub dropped_readings: u64,
    /// The earliest reading time and the latest receive time, when anything was read.
    pub start: String,
    pub end: String,
}

impl Recorder {
    /// Checks `config` the way the gateway checks a desired configuration, plus the index's
    /// rules, and plans every device's reads.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn new(config: Config) -> Result<Self, Error> {
        let (devices, index_devices) = plan(&config).map_err(Error::Record)?;
        Ok(Self {
            config,
            devices,
            index_devices,
            polls: Arc::new(watch::channel(0).0),
        })
    }

    /// The number of polls that have ended so far, across all devices. A poll's readings are
    /// handed to the batch former before the count moves; tests wait on it before they move a
    /// paused clock on.
    #[must_use]
    pub fn polls(&self) -> watch::Receiver<u64> {
        self.polls.subscribe()
    }

    /// The endpoints a recording reads through: the gateway agent's, with the polling
    /// settings' connect timeout. They connect only to addresses the agent's address guard
    /// allows (Laysi/trustiot-v2#653), so a converter on loopback or on one of this machine's
    /// own addresses, such as one reached through a local port forward, is refused.
    #[must_use]
    pub fn endpoints() -> Endpoints {
        Endpoints::new(Settings::default().connect_timeout)
    }

    /// Polls every device through `endpoints` (normally [`Recorder::endpoints`]) until `stop`
    /// completes, then writes the last batch, the index, and checks the result with
    /// [`Dataset::open`]. The dataset goes to `out/<name>/`, which must not exist yet (a
    /// dataset's files never change once written). `wall_start` is the time of the
    /// recording's start; later times add the elapsed time of tokio's monotonic clock, so a
    /// clock step during the recording cannot reorder readings.
    ///
    /// # Errors
    ///
    /// When the directory exists or cannot be written, nothing was read, or the result fails
    /// its own check (files already written stay for inspection).
    pub async fn run(
        self,
        mut endpoints: Endpoints,
        out: &Path,
        wall_start: DateTime<Utc>,
        stop: impl Future<Output = ()>,
    ) -> Result<Recorded, Error> {
        let dir = out.join(&self.config.name);
        if dir.exists() {
            return Err(Error::Record(format!(
                "{} exists: a recording never writes into an existing dataset",
                dir.display()
            )));
        }
        let io = |path: &Path| {
            let path = path.display().to_string();
            move |error: std::io::Error| Error::Io {
                path: path.clone(),
                message: error.to_string(),
            }
        };
        fs::create_dir_all(dir.join("segments")).map_err(io(&dir))?;
        let wall_start_ms = u64::try_from(wall_start.timestamp_millis())
            .map_err(|_| Error::Record("the start lies before 1970".to_owned()))?;
        let clock = Arc::new(RecorderClock {
            wall_start_ms,
            start: Instant::now(),
        });
        let settings = Settings::default();
        let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<AgentReading>>();
        let mut tasks = JoinSet::new();
        for planned in &self.devices {
            let mut connection = endpoints.connection(&planned.modbus);
            let reader = planned.reader.clone();
            let period = polling::poll_period(&planned.modbus);
            let clock = Arc::clone(&clock);
            let sender = sender.clone();
            let polls = Arc::clone(&self.polls);
            tasks.spawn(async move {
                polling::run(
                    &reader,
                    period,
                    &mut connection,
                    &*clock,
                    &settings,
                    |event| {
                        if let Event::Polled(polled) = event {
                            let _ = sender.send(polled.poll.readings);
                            polls.send_modify(|count| *count += 1);
                        }
                        ControlFlow::Continue(())
                    },
                )
                .await;
            });
        }
        drop(sender);

        let stream = StreamId::new(STREAM).map_err(|error| Error::Record(error.to_string()))?;
        let mut former = Former {
            builder: BatchBuilder::new(stream, Seq::FIRST),
            writer: Writer::new(&dir, &self.config, self.index_devices.clone()),
        };
        let period = Duration::from_secs(u64::from(self.config.batch_s));
        let mut ticks = tokio::time::interval_at(Instant::now() + period, period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                biased;
                () = &mut stop => break,
                _ = ticks.tick() => former.close(&mut receiver, &clock)?,
            }
        }
        // No poll ends after this: the last batch holds everything read.
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        former.close(&mut receiver, &clock)?;
        let written = former.writer.finish()?;
        Dataset::open(&written.index).map_err(|error| {
            Error::Record(format!("the recording fails its own check: {error}"))
        })?;
        Ok(written)
    }
}

fn plan(config: &Config) -> Result<(Vec<Planned>, Vec<index::Device>), String> {
    check_key("dataset name", &config.name)?;
    if config.description.trim().is_empty() {
        return Err("the description is empty".to_owned());
    }
    if !(1..=3600).contains(&config.batch_s) {
        return Err("batch_s is 1 to 3600".to_owned());
    }
    if !(MIN_SEGMENT_BYTES..=MAX_SEGMENT_BYTES).contains(&config.segment_bytes) {
        return Err(format!(
            "segment_bytes is {MIN_SEGMENT_BYTES} to {MAX_SEGMENT_BYTES}"
        ));
    }
    let mut converters = BTreeMap::new();
    for converter in &config.converters {
        check_key("converter", &converter.key)?;
        let host = Host::new(converter.host.as_str())
            .map_err(|error| format!("converter `{}`: {error}", converter.key))?;
        if converter.port == 0 {
            return Err(format!("converter `{}`: port 0", converter.key));
        }
        if converters
            .insert(converter.key.as_str(), (host, converter.port))
            .is_some()
        {
            return Err(format!("converter `{}` appears twice", converter.key));
        }
    }
    let mut models = BTreeMap::new();
    for model in &config.models {
        let name = ModelName::new(model.name.as_str()).map_err(|error| error.to_string())?;
        let mut points = Vec::with_capacity(model.points.len());
        let mut index_points = Vec::with_capacity(model.points.len());
        for point in &model.points {
            let (data_point, index_point) = plan_point(&model.name, point)?;
            points.push(data_point);
            index_points.push(index_point);
        }
        let entry = (DeviceModel { name, points }, index_points);
        if models.insert(model.name.as_str(), entry).is_some() {
            return Err(format!("model `{}` appears twice", model.name));
        }
    }
    let mut stations = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut modbus_devices = Vec::with_capacity(config.devices.len());
    let mut index_devices = Vec::with_capacity(config.devices.len());
    for device in &config.devices {
        let key = device.key.as_str();
        check_key("device", key)?;
        if !keys.insert(key) {
            return Err(format!("device `{key}` appears twice"));
        }
        let (model, index_points) = models
            .get(device.model.as_str())
            .ok_or_else(|| format!("device `{key}`: model `{}` is not listed", device.model))?;
        let (transport, host, port) = match (&device.converter, &device.host, device.port) {
            (Some(converter), None, None) => {
                let (host, port) = converters.get(converter.as_str()).ok_or_else(|| {
                    format!("device `{key}`: converter `{converter}` is not listed")
                })?;
                if !stations.insert((converter.as_str(), device.unit)) {
                    return Err(format!(
                        "device `{key}`: converter `{converter}` already has a device with unit {}",
                        device.unit
                    ));
                }
                (Transport::RtuOverTcp, host.clone(), *port)
            }
            (None, Some(host), Some(port)) if port != 0 => (
                Transport::ModbusTcp,
                Host::new(host.as_str()).map_err(|error| format!("device `{key}`: {error}"))?,
                port,
            ),
            _ => {
                return Err(format!(
                    "device `{key}`: give either `converter`, or `host` and a nonzero `port`"
                ));
            }
        };
        if !(1..=MAX_POLL_INTERVAL_MS / 1000).contains(&device.interval_s) {
            return Err(format!(
                "device `{key}`: interval_s is 1 to {}",
                MAX_POLL_INTERVAL_MS / 1000
            ));
        }
        modbus_devices.push(ModbusDevice {
            id: DeviceId::new(key).map_err(|error| error.to_string())?,
            model: model.name.clone(),
            transport,
            host,
            port,
            unit_id: device.unit,
            poll_interval_ms: device.interval_s * 1000,
        });
        index_devices.push(index::Device {
            key: key.to_owned(),
            model: device.model.clone(),
            converter: device.converter.clone(),
            unit: device.unit,
            interval_s: device.interval_s,
            points: index_points.clone(),
        });
    }
    if modbus_devices.is_empty() {
        return Err("the configuration lists no devices".to_owned());
    }
    let used: BTreeSet<&str> = config.devices.iter().map(|d| d.model.as_str()).collect();
    let desired = DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(1).ok_or("internal: version")?,
        models: models
            .iter()
            .filter(|(name, _)| used.contains(*name))
            .map(|(_, (model, _))| model.clone())
            .collect(),
        devices: modbus_devices.clone(),
    };
    desired
        .validate()
        .map_err(|invalid| format!("the devices and models: {invalid}"))?;
    let planned = modbus_devices
        .into_iter()
        .map(|modbus| {
            let model = desired
                .model(&modbus.model)
                .ok_or_else(|| "internal: model".to_owned())?;
            let reader = DeviceReader::new(&modbus, model).map_err(|error| format!("{error:?}"))?;
            Ok(Planned { modbus, reader })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((planned, index_devices))
}

pub(crate) fn plan_point(
    model: &str,
    point: &PointConfig,
) -> Result<(DataPoint, index::Point), String> {
    let at = || format!("model `{model}`, point `{}`", point.key);
    if !index::is_point_key(&point.key) {
        return Err(format!("{}: not a point key", at()));
    }
    let function_code = FunctionCode::try_from(point.function_code)
        .map_err(|error| format!("{}: {error}", at()))?;
    if point
        .unit
        .as_ref()
        .is_some_and(|unit| unit.trim().is_empty() || unit.len() > 32)
    {
        return Err(format!("{}: the unit is 1 to 32 bytes", at()));
    }
    let value_type = value_type(point);
    if let Some(wraps_at) = point.wraps_at {
        let limit = wrap_limit(point.data_type).filter(|_| value_type == ValueType::Int);
        let fits = limit.is_some_and(|limit| (2..=limit).contains(&wraps_at));
        if point.kind != PointKind::Cumulative || !fits {
            return Err(format!(
                "{}: wraps_at is for a cumulative u16, u32 or u64 point without scale or offset, \
                 2 to 2^bits",
                at()
            ));
        }
    }
    let data_point = DataPoint {
        key: PointKey::new(point.key.as_str()).map_err(|error| format!("{}: {error}", at()))?,
        function_code,
        address: point.address,
        data_type: point.data_type,
        byte_order: point.byte_order,
        length: point.length,
        scale: point.scale,
        offset: point.offset,
    };
    let index_point = index::Point {
        key: point.key.clone(),
        value_type,
        kind: point.kind,
        unit: point.unit.clone(),
        wraps_at: point.wraps_at,
    };
    Ok((data_point, index_point))
}

/// The recorder's clock: the wall time at the start plus tokio's monotonic elapsed time.
#[derive(Debug)]
struct RecorderClock {
    wall_start_ms: u64,
    start: Instant,
}

impl RecorderClock {
    fn millis(&self) -> u64 {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.wall_start_ms.saturating_add(elapsed)
    }
}

impl Clock for RecorderClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(self.millis()),
            synced: true,
        }
    }
}

fn utc(millis: u64) -> Result<DateTime<Utc>, Error> {
    i64::try_from(millis)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .ok_or_else(|| Error::Record(format!("{millis} ms is not a time")))
}

/// Forms batches from the readings that arrived and hands them to the writer.
struct Former {
    builder: BatchBuilder,
    writer: Writer,
}

impl Former {
    /// Closes the batch of everything that arrived: one batch, or several when it exceeds the
    /// protocol's batch size. `received_at` is taken after the readings are collected, so it is
    /// never earlier than a reading in the batch.
    fn close(
        &mut self,
        receiver: &mut mpsc::UnboundedReceiver<Vec<AgentReading>>,
        clock: &RecorderClock,
    ) -> Result<(), Error> {
        let mut arrived = Vec::new();
        while let Ok(readings) = receiver.try_recv() {
            arrived.extend(readings);
        }
        let received_at = utc(clock.millis())?;
        for reading in arrived {
            match self.builder.push(reading) {
                Ok(Some(batch)) => self.writer.write(&batch, received_at)?,
                Ok(None) => {}
                Err(_) => self.writer.dropped += 1,
            }
        }
        if let Some(batch) = self.builder.finish() {
            self.writer.write(&batch, received_at)?;
        }
        Ok(())
    }
}

/// Numbers the recorder's batches and hands them to the shared segment writer. Batches are
/// numbered here, consecutively from 1, so that a batch whose every reading was dropped leaves
/// no gap in the stream.
struct Writer {
    inner: SegmentWriter,
    name: String,
    /// Each point's type and wrap value.
    declared: BTreeMap<(String, String), (ValueType, Option<u64>)>,
    next_batch: u64,
    dropped: u64,
}

impl Writer {
    fn new(dir: &Path, config: &Config, devices: Vec<index::Device>) -> Self {
        let declared = devices
            .iter()
            .flat_map(|device| {
                device.points.iter().map(|point| {
                    (
                        (device.key.clone(), point.key.clone()),
                        (point.value_type, point.wraps_at),
                    )
                })
            })
            .collect();
        let header = Header {
            name: config.name.clone(),
            description: config.description.clone(),
            origin: "Recorded by `lab-dataset record` (Laysi/trustiot-v2#679)".to_owned(),
            streams: vec![(
                STREAM.to_owned(),
                format!(
                    "Batches as the recorder formed them, every {} s",
                    config.batch_s
                ),
            )],
            segment_bytes: config.segment_bytes,
        };
        Self {
            inner: SegmentWriter::new(dir, header, devices),
            name: config.name.clone(),
            declared,
            next_batch: 1,
            dropped: 0,
        }
    }

    /// The JSON value of a reading, or `None` when it breaks its point's rules.
    fn value(
        &self,
        device: &str,
        point: &str,
        value: &protocol::Value,
    ) -> Option<(&'static str, serde_json::Value)> {
        let (value_type, wraps_at) = self.declared.get(&(device.to_owned(), point.to_owned()))?;
        match (value_type, value) {
            (ValueType::Float, protocol::Value::Float(float)) => {
                serde_json::Number::from_f64(*float)
                    .map(|number| ("float", serde_json::Value::Number(number)))
            }
            (ValueType::Int, protocol::Value::Int(int)) => {
                let inside = wraps_at
                    .is_none_or(|wraps_at| u64::try_from(*int).is_ok_and(|int| int < wraps_at));
                inside.then(|| ("int", serde_json::Value::from(*int)))
            }
            (ValueType::Bool, protocol::Value::Bool(flag)) => {
                Some(("bool", serde_json::Value::Bool(*flag)))
            }
            (ValueType::String, protocol::Value::Text(text))
                if text.len() <= index::MAX_TEXT_BYTES =>
            {
                Some(("string", serde_json::Value::String(text.clone())))
            }
            _ => None,
        }
    }

    fn write(&mut self, batch: &TelemetryBatch, received_at: DateTime<Utc>) -> Result<(), Error> {
        let mut rows = Vec::new();
        for series in &batch.series {
            for sample in &series.samples {
                let (device, point) = (series.device.as_str(), series.point.as_str());
                let Some((value_type, value)) = self.value(device, point, &sample.value) else {
                    self.dropped += 1;
                    continue;
                };
                rows.push(Row {
                    device,
                    point,
                    ts: utc(sample.at.get())?,
                    value_type,
                    value,
                });
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        self.inner
            .write_batch(STREAM, self.next_batch, received_at, &rows, true)?;
        self.next_batch += 1;
        Ok(())
    }

    fn finish(self) -> Result<Recorded, Error> {
        let finished = self.inner.finish()?;
        if finished.segments == 0 {
            return Err(Error::Record(
                "nothing was read: no device answered with a value".to_owned(),
            ));
        }
        Ok(Recorded {
            dataset: self.name,
            index: finished.index,
            segments: finished.segments,
            rows: finished.rows,
            batches: finished.batches,
            dropped_readings: self.dropped,
            start: finished.start.map(time::format).unwrap_or_default(),
            end: finished.end.map(time::format).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
name = "bench"
description = "unit test"

[[converters]]
key = "rs485-a"
host = "192.0.2.10"
port = 502

[[models]]
name = "meter"
points = [
  { key = "energy_wh", function_code = 3, address = 0, data_type = "u32", kind = "cumulative", unit = "Wh", wraps_at = 4294967296 },
  { key = "power_kw", function_code = 3, address = 2, data_type = "f32", kind = "gauge" },
  { key = "voltage", function_code = 4, address = 0, data_type = "u16", scale = 0.1, kind = "gauge" },
  { key = "running", function_code = 1, address = 0, data_type = "bool", kind = "state" },
  { key = "mode", function_code = 3, address = 10, data_type = "string", length = 2, kind = "state" },
]

[[devices]]
key = "meter-1"
model = "meter"
converter = "rs485-a"
unit = 1
interval_s = 60

[[devices]]
key = "meter-2"
model = "meter"
converter = "rs485-a"
unit = 2
interval_s = 60
"#;

    fn with(from: &str, to: &str) -> Result<Recorder, String> {
        assert!(
            CONFIG.contains(from),
            "the base configuration has no `{from}`"
        );
        let config = Config::parse(&CONFIG.replacen(from, to, 1))?;
        Recorder::new(config).map_err(|error| error.to_string())
    }

    #[test]
    fn the_base_configuration_plans_both_devices() {
        let recorder = with("", "").unwrap();
        assert_eq!(recorder.devices.len(), 2);
        let types: Vec<(&str, ValueType)> = recorder.index_devices[0]
            .points
            .iter()
            .map(|point| (point.key.as_str(), point.value_type))
            .collect();
        assert_eq!(
            types,
            [
                ("energy_wh", ValueType::Int),
                ("power_kw", ValueType::Float),
                ("voltage", ValueType::Float),
                ("running", ValueType::Bool),
                ("mode", ValueType::String),
            ]
        );
        // The index's devices name the converter by its key only.
        assert_eq!(
            recorder.index_devices[1].converter.as_deref(),
            Some("rs485-a")
        );
        assert_eq!(recorder.index_devices[1].unit, 2);
    }

    #[test]
    fn every_rule_refuses_its_violation() {
        for (from, to, expected) in [
            ("name = \"bench\"", "name = \"Bench\"", "not a key"),
            (
                "description = \"unit test\"",
                "description = \" \"",
                "empty",
            ),
            (
                "description = \"unit test\"",
                "description = \"x\"\nbatch_s = 0",
                "batch_s",
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
                "host = \"192.0.2.10\"",
                "host = \"224.0.0.1\"",
                "invalid host",
            ),
            ("port = 502", "port = 0", "port 0"),
            ("unit = 2", "unit = 1", "already has a device with unit 1"),
            ("unit = 2", "unit = 0", "unit"),
            (
                "interval_s = 60\n\n[[devices]]",
                "interval_s = 0\n\n[[devices]]",
                "interval_s",
            ),
            (
                "model = \"meter\"\nconverter",
                "model = \"other\"\nconverter",
                "not listed",
            ),
            (
                "converter = \"rs485-a\"\nunit = 1",
                "converter = \"rs485-b\"\nunit = 1",
                "not listed",
            ),
            (
                "converter = \"rs485-a\"\nunit = 1",
                "converter = \"rs485-a\"\nhost = \"192.0.2.11\"\nunit = 1",
                "either",
            ),
            (
                "function_code = 1",
                "function_code = 5",
                "not a read function",
            ),
            (
                "key = \"power_kw\"",
                "key = \"power kw\"",
                "not a point key",
            ),
            ("key = \"power_kw\"", "key = \"energy_wh\"", "energy_wh"),
            ("wraps_at = 4294967296", "wraps_at = 4294967297", "wraps_at"),
            (
                "kind = \"gauge\" },\n  { key = \"voltage\"",
                "kind = \"gauge\", wraps_at = 2 },\n  { key = \"voltage\"",
                "wraps_at",
            ),
            ("address = 2", "address = 1", "overlap"),
            (
                "data_type = \"f32\"",
                "data_type = \"f128\"",
                "unknown variant",
            ),
        ] {
            let error = with(from, to)
                .map(|_| ())
                .expect_err(&format!("`{from}` -> `{to}` was accepted"));
            assert!(
                error.contains(expected),
                "`{from}` -> `{to}`: `{error}` lacks `{expected}`"
            );
        }
    }

    #[test]
    fn values_follow_the_points_rules() {
        let recorder = with("", "").unwrap();
        let writer = Writer::new(
            Path::new("."),
            &recorder.config,
            recorder.index_devices.clone(),
        );
        let value = |point: &str, value: protocol::Value| writer.value("meter-1", point, &value);
        assert_eq!(
            value("energy_wh", protocol::Value::Int(5)),
            Some(("int", serde_json::Value::from(5)))
        );
        assert_eq!(value("energy_wh", protocol::Value::Int(1 << 32)), None);
        assert_eq!(value("energy_wh", protocol::Value::Int(-1)), None);
        assert_eq!(value("energy_wh", protocol::Value::Float(1.0)), None);
        assert_eq!(
            value("mode", protocol::Value::Text("AB".to_owned())),
            Some(("string", serde_json::Value::from("AB")))
        );
        assert_eq!(value("unknown", protocol::Value::Bool(true)), None);
    }

    #[test]
    fn the_readme_example_is_a_valid_configuration() {
        let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../datasets/lab/README.md");
        let readme = fs::read_to_string(readme).unwrap();
        let example = readme
            .split("```toml\n")
            .filter_map(|block| block.split("```").next())
            .find(|block| block.contains("[[converters]]"))
            .expect("datasets/lab/README.md has a recording configuration");
        let config = Config::parse(&example.replace("<轉換器的位址>", "192.0.2.10")).unwrap();
        let recorder = Recorder::new(config).unwrap();
        let points: Vec<(&str, ValueType)> = recorder.index_devices[0]
            .points
            .iter()
            .map(|point| (point.key.as_str(), point.value_type))
            .collect();
        assert_eq!(
            points,
            [
                ("energy_kwh", ValueType::Float),
                ("pulses", ValueType::Int),
                ("power_kw", ValueType::Float),
            ]
        );
    }
}
