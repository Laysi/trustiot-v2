//! Lab datasets (checklist M7, Laysi/trustiot-v2#679): the index format of `datasets/lab/`, its
//! validation, the loader that writes a dataset into ClickHouse's telemetry table the way the
//! ingest service writes it, the recorder, and the exporter that reads a gateway's rows back
//! out of ClickHouse as a dataset. The format, the versioning rules and how a test loads a
//! dataset are in `datasets/lab/README.md`; the crate's own notes are in its README.
//!
//! ```no_run
//! # async fn example(url: &str, user: &str, password: &str) -> Result<(), lab_dataset::Error> {
//! use lab_dataset::{ClickHouse, Dataset, Target, TimeShift};
//!
//! let dataset = Dataset::open(lab_dataset::synthetic_index())?;
//! let shift = dataset.shift_ending_before(chrono::Utc::now(), chrono::TimeDelta::days(1))?;
//! let clickhouse = ClickHouse::new(url, user, password, None)?;
//! let tenant = uuid::Uuid::from_u128(1);
//! let gateway = uuid::Uuid::from_u128(2);
//! let loaded = dataset.load(&clickhouse, &Target::new(tenant, gateway), shift).await?;
//! let meter = loaded.device("meter-1");
//! # let _ = (meter, TimeShift::NONE);
//! # Ok(())
//! # }
//! ```

pub mod anomaly;
pub mod cli;
pub mod clickhouse;
pub mod export;
pub mod index;
pub mod record;
pub mod segment;
pub mod time;
mod writer;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

pub use crate::anomaly::{Anomaly, AnomalyKind, Expectation, Number};
pub use crate::clickhouse::{BatchCommit, ClickHouse};
pub use crate::index::{Index, Location};
pub use crate::segment::{Reading, Value};
pub use crate::time::TimeShift;

use crate::clickhouse::TelemetryRow;

/// The index file of every dataset directory.
pub const INDEX_FILE: &str = "index.toml";

/// The environment variable that points tests at another dataset's index
/// ([`index_for_tests`]), for example the lab recording (Laysi/trustiot-v2#681).
pub const INDEX_ENV: &str = "TRUSTIOT_LAB_DATASET_INDEX";

/// `datasets/lab/` of this repository.
#[must_use]
pub fn datasets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../datasets/lab")
}

/// The synthetic fixture's index (`datasets/lab/synthetic/index.toml`).
#[must_use]
pub fn synthetic_index() -> PathBuf {
    datasets_dir().join("synthetic").join(INDEX_FILE)
}

/// The index a downstream test loads: [`INDEX_ENV`] when it is set, otherwise the synthetic
/// fixture. A test that takes its index from here and its expected values from the dataset
/// ([`Dataset::readings`]) runs unchanged on another dataset.
#[must_use]
pub fn index_for_tests() -> PathBuf {
    std::env::var_os(INDEX_ENV)
        .filter(|path| !path.is_empty())
        .map_or_else(synthetic_index, PathBuf::from)
}

/// Why a dataset could not be read or loaded. Never contains a password.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {message}")]
    Io { path: String, message: String },
    #[error("index {path}: {message}")]
    Index { path: String, message: String },
    #[error("segment `{segment}`{}: {message}", line.map_or_else(String::new, |line| format!(", line {line}")))]
    Segment {
        segment: String,
        line: Option<usize>,
        message: String,
    },
    #[error("anomaly: {0}")]
    Anomaly(String),
    #[error("record: {0}")]
    Record(String),
    #[error("export: {0}")]
    Export(String),
    #[error("target: {0}")]
    Target(String),
    #[error("time: {0}")]
    Time(String),
    #[error("ClickHouse: {0}")]
    ClickHouse(String),
    #[error("not supported yet: {0}")]
    Unsupported(String),
}

/// The batches of a segment that get no completion record: (stream, sequence number).
type Uncommitted<'a> = BTreeSet<(&'a str, u64)>;

/// A dataset whose index and segment files passed every check, with its anomaly segments: the
/// index's, and any a test inserted ([`Dataset::with_anomalies`]).
#[derive(Clone, Debug)]
pub struct Dataset {
    dir: PathBuf,
    index: Index,
    anomalies: Vec<Anomaly>,
}

/// Where a dataset is written: the tenant and gateway the caller chose, and optionally the
/// device IDs to use. A device without one gets an ID derived from the dataset, the tenant, the
/// gateway and its key ([`derived_id`]), so loading the same dataset for two tenants (the
/// cross-tenant tests) gives each tenant its own devices.
#[derive(Clone, Debug)]
pub struct Target {
    pub tenant: Uuid,
    pub gateway: Uuid,
    devices: BTreeMap<String, Uuid>,
}

impl Target {
    #[must_use]
    pub fn new(tenant: Uuid, gateway: Uuid) -> Self {
        Self {
            tenant,
            gateway,
            devices: BTreeMap::new(),
        }
    }

    /// Writes the dataset's device `key` as `id`, for example a device the test created in the
    /// assets service.
    #[must_use]
    pub fn with_device(mut self, key: impl Into<String>, id: Uuid) -> Self {
        self.devices.insert(key.into(), id);
        self
    }
}

/// The IDs a load writes ([`Dataset::ids`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ids {
    /// Each device key and its device ID.
    pub devices: BTreeMap<String, Uuid>,
    /// Each stream key and its stream ID.
    pub streams: BTreeMap<String, Uuid>,
}

/// What [`Dataset::load`] wrote.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Loaded {
    pub dataset: String,
    pub version: u32,
    pub tenant: Uuid,
    pub gateway: Uuid,
    /// The gateway binding the completion records carry (`binding_id`). The loader has no binding
    /// table, so the ID is derived like the device and stream IDs ([`derived_id`] of kind
    /// `binding` with an empty key): the same for a dataset, tenant and gateway, and no real
    /// binding's.
    pub binding: Uuid,
    /// The shift added to every time ([`TimeShift`]).
    pub shift_ms: i64,
    /// Each device key and the device ID its rows carry.
    pub devices: BTreeMap<String, Uuid>,
    /// Each stream key and the stream ID its rows and completion records carry.
    pub streams: BTreeMap<String, Uuid>,
    /// Telemetry rows written.
    pub rows: u64,
    /// Batches written (one INSERT each).
    pub batches: u64,
    /// Completion records written: `batches` minus the uncommitted ones.
    pub committed_batches: u64,
    /// The anomaly segments applied, with their windows moved by the shift: where they lie in
    /// ClickHouse, with the triggers each is expected to cause.
    pub anomalies: Vec<Anomaly>,
}

impl Loaded {
    /// The device ID of the dataset's device `key`.
    #[must_use]
    pub fn device(&self, key: &str) -> Option<Uuid> {
        self.devices.get(key).copied()
    }

    /// The stream ID of the dataset's stream `key`.
    #[must_use]
    pub fn stream(&self, key: &str) -> Option<Uuid> {
        self.streams.get(key).copied()
    }

    /// The shift the load applied: a dataset time plus this shift is the time in ClickHouse.
    #[must_use]
    pub const fn shift(&self) -> TimeShift {
        TimeShift::from_millis(self.shift_ms)
    }
}

/// The ID of a dataset's device or stream, or of its stand-in gateway binding, for one tenant and
/// gateway: a UUID version 8 from the first 16 bytes of SHA-256 over the kind (`device`,
/// `stream` or `binding`), the dataset name, the tenant, the gateway and the key. The same
/// inputs always give the same ID.
#[must_use]
pub fn derived_id(kind: &str, dataset: &str, tenant: Uuid, gateway: Uuid, key: &str) -> Uuid {
    let mut hash = Sha256::new();
    for part in [
        b"trustiot-v2 lab-dataset id 1".as_slice(),
        kind.as_bytes(),
        dataset.as_bytes(),
        tenant.as_bytes(),
        gateway.as_bytes(),
        key.as_bytes(),
    ] {
        // Length-prefixed, so no two different inputs hash the same bytes.
        hash.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::new_v8(bytes)
}

impl Dataset {
    /// Reads an index and checks it and every segment file it lists. The index is
    /// `datasets/lab/<name>/index.toml`: its directory's name is the dataset's name.
    ///
    /// # Errors
    ///
    /// When a file cannot be read or breaks a rule (`datasets/lab/README.md`).
    pub fn open(index_path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = index_path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|error| Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        let index = Index::parse(&text).map_err(|message| Error::Index {
            path: path.display().to_string(),
            message,
        })?;
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let directory_name = std::fs::canonicalize(&dir).ok().and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        if directory_name.as_deref() != Some(index.name.as_str()) {
            return Err(Error::Index {
                path: path.display().to_string(),
                message: format!(
                    "the dataset `{}` must be in a directory of that name",
                    index.name
                ),
            });
        }
        let anomalies = index.anomalies.clone();
        let dataset = Self {
            dir,
            index,
            anomalies,
        };
        // Every segment, and no reading twice across segments. The readings are remembered by
        // the position of their device and point in the index, not by the two texts: the set
        // holds every reading of the dataset, so its entry size sets the check's memory
        // (Laysi/trustiot-v2#837).
        let series: HashMap<(&str, &str), u32> = dataset
            .index
            .devices
            .iter()
            .flat_map(|device| {
                device
                    .points
                    .iter()
                    .map(move |point| (device.key.as_str(), point.key.as_str()))
            })
            .zip(0..)
            .collect();
        let mut seen: HashSet<(u32, DateTime<Utc>)> = HashSet::new();
        let mut readings = Vec::new();
        for segment in &dataset.index.segments {
            for reading in dataset.read_segment(segment)? {
                // `read_segment` refused a device or point the index does not declare.
                let position = series
                    .get(&(reading.device.as_str(), reading.point.as_str()))
                    .copied()
                    .ok_or_else(|| Error::Segment {
                        segment: segment.key.clone(),
                        line: None,
                        message: format!(
                            "{}/{} is not in the index",
                            reading.device, reading.point
                        ),
                    })?;
                if !seen.insert((position, reading.ts)) {
                    return Err(Error::Segment {
                        segment: segment.key.clone(),
                        line: None,
                        message: format!(
                            "{}/{} at {} is also in an earlier segment",
                            reading.device,
                            reading.point,
                            time::format(reading.ts)
                        ),
                    });
                }
                if !dataset.anomalies.is_empty() {
                    readings.push(reading);
                }
            }
        }
        // The anomalies' rules that need the readings (a reading in each window, no reset
        // inside an outage).
        anomaly::apply(&dataset.index, &dataset.anomalies, &mut readings)
            .map_err(Error::Anomaly)?;
        Ok(dataset)
    }

    /// The dataset with `anomalies` inserted besides the index's own: the readings
    /// ([`Dataset::readings`]) and a load ([`Dataset::load`]) carry them. Their times are on the
    /// dataset's time base, like the index's (`datasets/lab/README.md`, 「人工異常片段」).
    ///
    /// # Errors
    ///
    /// When an anomaly breaks a rule: an unknown device or point, a kind that does not apply to
    /// a point, a missing parameter or label, a key used twice, a window that overlaps another
    /// on the same point or holds no reading of a point, or a reset inside a stop.
    pub fn with_anomalies(
        mut self,
        anomalies: impl IntoIterator<Item = Anomaly>,
    ) -> Result<Self, Error> {
        self.anomalies.extend(anomalies);
        anomaly::validate_all(&self.index, &self.anomalies).map_err(Error::Anomaly)?;
        let mut readings = self.recorded_readings()?;
        anomaly::apply(&self.index, &self.anomalies, &mut readings).map_err(Error::Anomaly)?;
        Ok(self)
    }

    /// The anomaly segments the readings carry: the index's first, then the inserted ones.
    #[must_use]
    pub fn anomalies(&self) -> &[Anomaly] {
        &self.anomalies
    }

    #[must_use]
    pub const fn index(&self) -> &Index {
        &self.index
    }

    /// The earliest reading time and the latest receive time of the dataset.
    #[must_use]
    pub fn span(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        let start = self
            .index
            .segments
            .iter()
            .map(|segment| segment.reading_time.start.min(segment.receive_time.start))
            .min();
        let end = self
            .index
            .segments
            .iter()
            .map(|segment| segment.reading_time.end.max(segment.receive_time.end))
            .max();
        // An index has at least one segment (Index::validate).
        let fallback = DateTime::<Utc>::UNIX_EPOCH;
        (start.unwrap_or(fallback), end.unwrap_or(fallback))
    }

    /// The dataset's last time, reading or receive: what [`Dataset::shift_ending_before`] moves.
    #[must_use]
    pub fn end(&self) -> DateTime<Utc> {
        self.span().1
    }

    /// The shift by whole multiples of `align` that moves the dataset's end to `at` or just
    /// before it ([`TimeShift::ending_before`]). With `at` the current time and `align` one day,
    /// the newest data lies within the last day, nothing lies in the future, and every reading
    /// keeps its position in its UTC minute, hour and day.
    ///
    /// # Errors
    ///
    /// When `align` is not a positive whole number of milliseconds or the shift overflows.
    pub fn shift_ending_before(
        &self,
        at: DateTime<Utc>,
        align: TimeDelta,
    ) -> Result<TimeShift, Error> {
        TimeShift::ending_before(self.end(), at, align).map_err(Error::Time)
    }

    /// Every reading of the dataset in arrival order (the order the loader inserts them), read
    /// and checked again from the segment files, with the anomalies applied: what a load writes.
    /// Times are the dataset's own: add [`Loaded::shift`] to compare them with what was loaded.
    ///
    /// # Errors
    ///
    /// When a segment file can no longer be read or no longer matches its index entry.
    pub fn readings(&self) -> Result<Vec<Reading>, Error> {
        let mut readings = self.recorded_readings()?;
        anomaly::apply(&self.index, &self.anomalies, &mut readings).map_err(Error::Anomaly)?;
        Ok(readings)
    }

    /// The readings as the segment files hold them, without the anomalies.
    ///
    /// # Errors
    ///
    /// When a segment file can no longer be read or no longer matches its index entry.
    pub fn recorded_readings(&self) -> Result<Vec<Reading>, Error> {
        let mut readings = Vec::new();
        for segment in &self.index.segments {
            readings.extend(self.read_segment(segment)?);
        }
        Ok(readings)
    }

    fn read_segment(&self, segment: &index::Segment) -> Result<Vec<Reading>, Error> {
        let path = match Location::parse(&segment.location) {
            Ok(Location::Relative(relative)) => self.dir.join(relative),
            Ok(Location::S3 { .. }) => {
                return Err(Error::Unsupported(format!(
                    "segment `{}` is in object storage: download it next to the index and use a \
                     relative location (the s3:// URL form depends on the provider chosen in \
                     Laysi/trustiot-v2#133)",
                    segment.key
                )));
            }
            Err(message) => {
                return Err(Error::Segment {
                    segment: segment.key.clone(),
                    line: None,
                    message,
                });
            }
        };
        let display = path.display().to_string();
        let io = |error: std::io::Error| Error::Io {
            path: display.clone(),
            message: error.to_string(),
        };
        // A symbolic link must not lead out of the dataset's directory.
        let canonical = std::fs::canonicalize(&path).map_err(io)?;
        let root = std::fs::canonicalize(&self.dir).map_err(io)?;
        if !canonical.starts_with(&root) {
            return Err(Error::Segment {
                segment: segment.key.clone(),
                line: None,
                message: "the file lies outside the dataset's directory".to_owned(),
            });
        }
        let length = std::fs::metadata(&canonical).map_err(io)?.len();
        if length > index::MAX_SEGMENT_BYTES {
            return Err(Error::Segment {
                segment: segment.key.clone(),
                line: None,
                message: format!(
                    "the file has {length} bytes, more than {}",
                    index::MAX_SEGMENT_BYTES
                ),
            });
        }
        let bytes = std::fs::read(&canonical).map_err(io)?;
        segment::parse(&self.index, segment, &bytes).map_err(|error| Error::Segment {
            segment: segment.key.clone(),
            line: error.line,
            message: error.message,
        })
    }

    /// The device and stream IDs a load for `target` writes.
    ///
    /// # Errors
    ///
    /// When the tenant or gateway is the nil UUID, or a device override names an unknown device,
    /// is the nil UUID, or repeats another device's ID.
    pub fn ids(&self, target: &Target) -> Result<Ids, Error> {
        if target.tenant.is_nil() || target.gateway.is_nil() {
            return Err(Error::Target(
                "the tenant and the gateway must not be the nil UUID".to_owned(),
            ));
        }
        for key in target.devices.keys() {
            if self.index.device(key).is_none() {
                return Err(Error::Target(format!(
                    "device `{key}` is not in dataset `{}`",
                    self.index.name
                )));
            }
        }
        let name = self.index.name.as_str();
        let devices: BTreeMap<String, Uuid> = self
            .index
            .devices
            .iter()
            .map(|device| {
                let id = target.devices.get(&device.key).copied().unwrap_or_else(|| {
                    derived_id("device", name, target.tenant, target.gateway, &device.key)
                });
                (device.key.clone(), id)
            })
            .collect();
        let distinct: BTreeSet<Uuid> = devices.values().copied().collect();
        if distinct.len() != devices.len() || distinct.contains(&Uuid::nil()) {
            return Err(Error::Target(
                "every device needs its own, non-nil ID".to_owned(),
            ));
        }
        let streams = self
            .index
            .streams
            .iter()
            .map(|stream| {
                let id = derived_id("stream", name, target.tenant, target.gateway, &stream.key);
                (stream.key.clone(), id)
            })
            .collect();
        Ok(Ids { devices, streams })
    }

    /// Writes the dataset into `trustiot.telemetry` and `trustiot.stream_commits` for `target`,
    /// with every time moved by `shift`, as the ingest service would have written it: one INSERT
    /// per batch in arrival order, the batch's rows with their `ts`, `received_at`, stream and
    /// sequence number, then its completion record (committed at the batch's receive time),
    /// except for the batches the index lists as uncommitted. The rows carry the anomalies'
    /// values ([`Dataset::readings`]), and [`Loaded::anomalies`] their shifted windows.
    ///
    /// Everything is checked before the first write. A failure part-way through (a ClickHouse
    /// error) leaves the batches written so far; load into a new tenant or gateway after one.
    /// Loading the same dataset twice for the same tenant and gateway writes the same IDs twice
    /// and is not supported.
    ///
    /// # Errors
    ///
    /// When the target, the shift or a segment file is refused, or ClickHouse refuses a write.
    pub async fn load(
        &self,
        clickhouse: &ClickHouse,
        target: &Target,
        shift: TimeShift,
    ) -> Result<Loaded, Error> {
        let Ids { devices, streams } = self.ids(target)?;
        let (start, end) = self.span();
        shift.apply(start).map_err(Error::Time)?;
        shift.apply(end).map_err(Error::Time)?;
        let anomalies = self
            .anomalies
            .iter()
            .map(|anomaly| anomaly.shifted(shift))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Time)?;
        // Read and check every segment, and apply the anomalies across them, before the first
        // write.
        let mut all = Vec::new();
        let mut segments = Vec::with_capacity(self.index.segments.len());
        for segment in &self.index.segments {
            let uncommitted: Uncommitted<'_> = segment
                .uncommitted
                .iter()
                .map(|batch| (batch.stream.as_str(), batch.batch_seq))
                .collect();
            let readings = self.read_segment(segment)?;
            segments.push((readings.len(), uncommitted));
            all.extend(readings);
        }
        anomaly::apply(&self.index, &self.anomalies, &mut all).map_err(Error::Anomaly)?;
        let mut rest = all.as_slice();
        let segments: Vec<(&[Reading], Uncommitted<'_>)> = segments
            .into_iter()
            .map(|(length, uncommitted)| {
                let (readings, after) = rest.split_at(length);
                rest = after;
                (readings, uncommitted)
            })
            .collect();
        let mut loaded = Loaded {
            dataset: self.index.name.clone(),
            version: self.index.version,
            tenant: target.tenant,
            gateway: target.gateway,
            binding: derived_id(
                "binding",
                &self.index.name,
                target.tenant,
                target.gateway,
                "",
            ),
            shift_ms: shift.millis(),
            devices,
            streams,
            rows: 0,
            batches: 0,
            committed_batches: 0,
            anomalies,
        };
        for (readings, uncommitted) in &segments {
            for batch in
                readings.chunk_by(|a, b| a.stream == b.stream && a.batch_seq == b.batch_seq)
            {
                let Some(first) = batch.first() else { continue };
                let stream_id = *loaded
                    .streams
                    .get(&first.stream)
                    .ok_or_else(|| Error::Index {
                        path: first.stream.clone(),
                        message: "internal: a stream without an ID".to_owned(),
                    })?;
                let received_at = shift.apply(first.received_at).map_err(Error::Time)?;
                let mut rows = Vec::with_capacity(batch.len());
                for reading in batch {
                    let device_id =
                        *loaded
                            .devices
                            .get(&reading.device)
                            .ok_or_else(|| Error::Index {
                                path: reading.device.clone(),
                                message: "internal: a device without an ID".to_owned(),
                            })?;
                    rows.push(TelemetryRow::new(
                        target.tenant,
                        target.gateway,
                        stream_id,
                        reading.batch_seq,
                        device_id,
                        &reading.point,
                        shift.apply(reading.ts).map_err(Error::Time)?,
                        received_at,
                        &reading.value,
                    ));
                }
                let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
                clickhouse.write_rows(&rows).await?;
                loaded.rows += row_count;
                loaded.batches += 1;
                if !uncommitted.contains(&(first.stream.as_str(), first.batch_seq)) {
                    clickhouse
                        .commit_batch(&BatchCommit {
                            tenant_id: target.tenant,
                            binding_id: loaded.binding,
                            gateway_id: target.gateway,
                            stream_id,
                            seq: first.batch_seq,
                            row_count,
                            committed_at: received_at,
                        })
                        .await?;
                    loaded.committed_batches += 1;
                }
            }
        }
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_depend_on_every_input() {
        let (t1, t2, g) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let id = derived_id("device", "synthetic", t1, g, "meter-1");
        assert_eq!(id, derived_id("device", "synthetic", t1, g, "meter-1"));
        assert_eq!(id.get_version_num(), 8);
        for other in [
            derived_id("stream", "synthetic", t1, g, "meter-1"),
            derived_id("binding", "synthetic", t1, g, "meter-1"),
            derived_id("device", "lab", t1, g, "meter-1"),
            derived_id("device", "synthetic", t2, g, "meter-1"),
            derived_id("device", "synthetic", t1, t2, "meter-1"),
            derived_id("device", "synthetic", t1, g, "meter-2"),
        ] {
            assert_ne!(id, other);
        }
        // The length prefix keeps shifted boundaries apart.
        assert_ne!(
            derived_id("device", "ab", t1, g, "c"),
            derived_id("device", "a", t1, g, "bc")
        );
    }
}
