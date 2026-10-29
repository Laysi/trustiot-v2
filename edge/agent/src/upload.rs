//! Telemetry from readings to the cloud's acknowledgement (rebuild checklist G1; decisions 8
//! and 9; Laysi/trustiot-v2#90): the collection window, batching, the offline buffer
//! ([`crate::buffer`]) and the uploader, in one task ([`Pipeline::run`]).
//!
//! - **Readings** go into the open window through [`Readings::push`], which never waits. Every
//!   upload interval (or earlier, when the window holds about [`WINDOW_LIMIT`] bytes) the window
//!   closes: its readings are grouped by device and data point and pushed series by series into
//!   a [`BatchBuilder`] (per-series batching, about a quarter of the bytes of arrival order),
//!   one run of the same clock state after the other, in the order they came. The builder closes
//!   a batch at the size limit and whenever the clock state changes, so a batch holds one run
//!   at most and every series goes up in the order it was read. A window that reaches twice the limit drops
//!   further readings until it closes.
//! - **Outages.** While uploads do not go through (the link is not connected, or more than one
//!   batch waits), the window closes only every [`OUTAGE_WINDOW`] (five minutes) or once it
//!   holds about one batch's worth ([`OUTAGE_CLOSE_FACTOR`] times the batch limit in the
//!   window's estimate): a long outage forms one batch per five minutes rather than one per
//!   upload interval, which bounds the number of buffered batches and the round trips to catch
//!   up, whatever the upload interval.
//! - **Streams.** Every process start uses a new random [`StreamId`] whose sequence numbers
//!   start at 1 and live only in memory; batches on disk carry their own stream and sequence
//!   number and are resent unchanged after a restart. A used-up stream is replaced by a new one
//!   (the same random digits with a count). Without randomness from the OS the pipeline is not
//!   created ([`NoRandomness`]): nothing else makes an ID that cannot repeat an earlier run's.
//! - **Sending.** One batch is in flight at a time, the oldest first, so a stream's batches go
//!   out in ascending sequence numbers. The uploader reserves a place in the link's queue
//!   ([`Link::reserve`]), only then reads the payload, and publishes it. A PUBACK only means the
//!   broker has it: the batch stays until the cloud's [`TelemetryAck`] for its stream and
//!   sequence number arrives (see [`Pipeline::run`] for every outcome).
//! - **Stopping** closes the open window and writes every batch in memory to disk, within
//!   [`FLUSH_LIMIT`] of the stop: what does not fit in that time is reported as lost.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use protocol::health::BufferStats;
use protocol::telemetry::{
    AckOutcome, BatchBuilder, PushError, Reading, Sample, TelemetryAck, TelemetryBatch,
};
use protocol::topic::{Channel, MAX_BULK_BYTES};
use protocol::{DeviceId, PointKey, Seq, SessionId, StreamId, UnixMillis, Value};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::buffer::{self, Buffer, DropReason, Event, Events, Formed, Meta, Source};
use crate::clock::{AgeClock, GatewayClock};
use crate::desired::fleet::Sink;
use crate::link::{Link, LinkState, PublishError, Published, printable};
use crate::polling::Polled;

/// The subdirectory of the agent's state directory ([`crate::desired::store::state_directory`])
/// that holds the offline buffer's files.
pub const BUFFER_DIRECTORY: &str = "buffer";
/// The default memory limit: encoded batches held in memory, all devices together
/// (docs/feasibility/2026-10-13-gateway-memory.md, "G1 的記憶體上限").
pub const DEFAULT_MEMORY_LIMIT: usize = 16 * 1024 * 1024;
/// The largest memory limit accepted.
pub const MAX_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// The default disk limit: bytes of buffer files on the data partition.
pub const DEFAULT_DISK_LIMIT: u64 = 1024 * 1024 * 1024;
/// The largest disk limit accepted.
pub const MAX_DISK_LIMIT: u64 = 64 * 1024 * 1024 * 1024;
/// The default upload interval: how often the collection window closes.
pub const DEFAULT_UPLOAD_INTERVAL: Duration = Duration::from_secs(10);
/// The shortest upload interval accepted.
pub const MIN_UPLOAD_INTERVAL: Duration = Duration::from_secs(1);
/// The longest upload interval accepted.
pub const MAX_UPLOAD_INTERVAL: Duration = Duration::from_secs(60);
/// How long the uploader waits for the cloud's acknowledgement of a published batch before it
/// sends the batch again. Well above a commit's time in the cloud, and unrelated to the link's
/// own deadlines (the dispatcher takes every message from the link at once).
pub const ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// The shortest wait a `retry` acknowledgement can ask for.
pub const RETRY_AFTER_MIN: Duration = Duration::from_secs(1);
/// The longest wait a `retry` acknowledgement can ask for.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(600);
/// The first wait after the broker refused a batch (PUBACK 0x80 or above, or a batch above its
/// Maximum Packet Size); it doubles with every refusal in a row.
pub const REFUSED_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// The longest wait after refusals.
pub const REFUSED_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// When the open window's readings reach about this many bytes in memory it closes early; at
/// twice as many it drops further readings until it closes.
pub const WINDOW_LIMIT: usize = 4 * 1024 * 1024;
/// Acknowledgements waiting for the uploader; the oldest goes when another arrives.
pub const ACK_QUEUE: usize = 8;
/// While uploads do not go through, the longest a collection window stays open (unless the
/// upload interval is longer): one batch per five minutes in a long outage.
pub const OUTAGE_WINDOW: Duration = Duration::from_secs(300);
/// While uploads do not go through, the window also closes once its estimated size reaches
/// this many times the batch limit: about one full batch (a reading takes about twice as many
/// bytes in the window as encoded in a batch).
pub const OUTAGE_CLOSE_FACTOR: usize = 2;
/// How long a stop may spend writing the batches in memory to disk, from the moment it was
/// asked for; the last segment's sync comes after it. Well inside the agent's stop limit (10 s),
/// so the report of what could not be written is logged before the process exits.
pub const FLUSH_LIMIT: Duration = Duration::from_secs(5);
/// How often a repeated problem is logged at most; the messages in between are counted.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// What the pipeline needs to know; [`Settings::new`] has the defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The buffer directory ([`BUFFER_DIRECTORY`] in the agent's state directory outside
    /// tests); created if missing, its parent must exist.
    pub directory: PathBuf,
    /// Encoded batches held in memory, in bytes.
    pub memory_limit: usize,
    /// Buffer files on disk, in bytes; 0 never writes to disk.
    pub disk_limit: u64,
    /// How often the collection window closes.
    pub upload_interval: Duration,
    /// How long to wait for an acknowledgement before sending a batch again.
    pub ack_timeout: Duration,
    /// The largest encoded batch; the telemetry channel's limit outside tests.
    pub batch_limit: usize,
    /// When the window closes early, in estimated bytes.
    pub window_limit: usize,
    /// The first sequence number of every new stream; 1 outside tests.
    pub first_seq: Seq,
    /// While uploads do not go through, the longest a window stays open; [`OUTAGE_WINDOW`]
    /// outside tests.
    pub outage_window: Duration,
    /// The most batches buffered; [`buffer::MAX_BATCHES`] outside tests.
    pub max_batches: usize,
    /// How long a stop may spend writing memory to disk; [`FLUSH_LIMIT`] outside tests.
    pub flush_limit: Duration,
}

impl Settings {
    /// The defaults, with the buffer in `directory`.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            memory_limit: DEFAULT_MEMORY_LIMIT,
            disk_limit: DEFAULT_DISK_LIMIT,
            upload_interval: DEFAULT_UPLOAD_INTERVAL,
            ack_timeout: ACK_TIMEOUT,
            batch_limit: MAX_BULK_BYTES,
            window_limit: WINDOW_LIMIT,
            first_seq: Seq::FIRST,
            outage_window: OUTAGE_WINDOW,
            max_batches: buffer::MAX_BATCHES,
            flush_limit: FLUSH_LIMIT,
        }
    }
}

/// The OS gave no randomness for this process's stream IDs, so the pipeline is not created
/// ([`pipeline`]): an ID that could repeat an earlier process's would make the cloud take new
/// batches for resends of old ones and discard them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRandomness;

impl std::fmt::Display for NoRandomness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the operating system gave no randomness for a new telemetry stream ID; an ID that could repeat an earlier run's would make the cloud discard new batches as resends")
    }
}

impl std::error::Error for NoRandomness {}

/// The IDs of this process's telemetry streams: 16 bytes of the OS's randomness, drawn once
/// when the pipeline is created (without them it is not). The first stream is their 32 hex
/// digits; each stream after it (once one has used every sequence number) adds `-1`, `-2` and
/// so on. They are distinct within the process, and from every other process's as far as the
/// random bytes are: nothing falls back to the clock or the process ID, which can both repeat
/// after a restart.
#[derive(Debug)]
struct StreamIds {
    random: String,
    /// Streams started so far.
    started: u64,
}

impl StreamIds {
    /// Draws the random part from `randomness`; [`NoRandomness`] when it has none.
    fn new(randomness: fn(&mut [u8]) -> bool) -> Result<Self, NoRandomness> {
        let mut bytes = [0_u8; 16];
        if !randomness(&mut bytes) {
            return Err(NoRandomness);
        }
        let random = bytes
            .iter()
            .fold(String::with_capacity(32), |mut text, byte| {
                text.push_str(&format!("{byte:02x}"));
                text
            });
        Ok(Self { random, started: 0 })
    }

    /// The next stream's ID.
    fn next(&mut self) -> StreamId {
        let id = if self.started == 0 {
            self.random.clone()
        } else {
            format!("{}-{}", self.random, self.started)
        };
        // u64::MAX streams of u64::MAX batches each never come to pass.
        self.started = self.started.saturating_add(1);
        StreamId::new(id).unwrap_or_else(|error| {
            unreachable!("32 hex digits, a hyphen and a number are a valid stream ID: {error}")
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding these locks in a way that leaves them inconsistent.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The readings of one series in the window, each with the run of its clock state
/// ([`Window::runs`]).
#[derive(Debug)]
struct Group {
    device: DeviceId,
    point: PointKey,
    samples: Vec<(Sample, usize)>,
}

/// The estimated memory of one reading in the window, besides its text.
const SAMPLE_COST: usize = size_of::<(Sample, usize)>();
/// The estimated memory of a new series in the window, besides its IDs.
const GROUP_COST: usize = size_of::<Group>() + 64;

/// The open collection window.
#[derive(Debug, Default)]
struct Window {
    groups: Vec<Group>,
    index: HashMap<DeviceId, HashMap<PointKey, usize>>,
    /// The clock state of each run of readings, in the order they came: a reading whose state
    /// differs from the one before it starts a new run.
    runs: Vec<bool>,
    readings: u64,
    bytes: usize,
    oldest: Option<UnixMillis>,
}

impl Window {
    fn add(&mut self, reading: Reading) {
        let Reading {
            device,
            point,
            sample,
            clock_synced,
        } = reading;
        if self.runs.last() != Some(&clock_synced) {
            self.runs.push(clock_synced);
            self.bytes += size_of::<bool>();
        }
        let run = self.runs.len() - 1;
        self.oldest = Some(
            self.oldest
                .map_or(sample.at, |oldest| oldest.min(sample.at)),
        );
        let text = match &sample.value {
            Value::Text(text) => text.len(),
            _ => 0,
        };
        let index = match self
            .index
            .get(&device)
            .and_then(|points| points.get(&point))
        {
            Some(index) => *index,
            None => {
                let index = self.groups.len();
                self.bytes += GROUP_COST + device.as_str().len() + point.as_str().len();
                self.index
                    .entry(device.clone())
                    .or_default()
                    .insert(point.clone(), index);
                self.groups.push(Group {
                    device,
                    point,
                    samples: Vec::new(),
                });
                index
            }
        };
        self.groups[index].samples.push((sample, run));
        self.bytes += SAMPLE_COST + text;
        self.readings += 1;
    }
}

/// What [`Readings`] and the pipeline share.
#[derive(Debug)]
struct Shared {
    window: Mutex<Window>,
    /// Wakes the pipeline to close the window early.
    early: Notify,
    /// Readings dropped because the window was full.
    dropped: AtomicU64,
    /// At twice this estimate the window drops readings.
    limit: usize,
    /// At this estimate the window wakes the pipeline to close it: the limit, or about one
    /// batch while uploads do not go through.
    close_at: AtomicUsize,
}

/// Where the device polling hands its readings over, as the fleet's [`Sink`] in the service:
/// cheap to clone, and [`Readings::push`] never waits.
#[derive(Clone, Debug)]
pub struct Readings {
    shared: Arc<Shared>,
}

impl Readings {
    /// Adds readings to the open window, keeping their timestamps and clock states. Holds a lock
    /// only to append. When the window reaches its limit (about one batch during an outage) the
    /// pipeline is woken to close it; a window at twice its limit drops the readings, which
    /// count in `dropped_records`.
    pub fn push(&self, readings: impl IntoIterator<Item = Reading>) {
        let mut dropped = 0_u64;
        let full = {
            let mut window = lock(&self.shared.window);
            for reading in readings {
                if window.bytes >= self.shared.limit.saturating_mul(2) {
                    dropped += 1;
                } else {
                    window.add(reading);
                }
            }
            window.bytes >= self.shared.close_at.load(Ordering::Relaxed)
        };
        if dropped > 0 {
            self.shared.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        if full {
            self.shared.early.notify_one();
        }
    }
}

/// The service's [`Sink`]: every poll's readings go into the open window, as they were stamped
/// (by the [`GatewayClock`] the pipeline compares with the cloud's time). The rest of a poll
/// (its failures, the polls it missed) is for the device statistics (Laysi/trustiot-v2#117);
/// until then it is logged at debug level.
impl Sink for Readings {
    fn polled(&self, device: &DeviceId, polled: Polled) {
        tracing::debug!(
            %device,
            readings = polled.poll.readings.len(),
            failed = polled.poll.failed.len(),
            missed = polled.missed,
            "polled"
        );
        self.push(polled.poll.readings);
    }

    fn removed(&self, device: &DeviceId) {
        tracing::debug!(%device, "no longer polled");
    }
}

/// The acknowledgements on their way from the dispatcher to the uploader.
#[derive(Debug, Default)]
struct AckQueue {
    queue: Mutex<VecDeque<TelemetryAck>>,
    notify: Notify,
}

impl AckQueue {
    async fn recv(&self) -> TelemetryAck {
        loop {
            if let Some(ack) = lock(&self.queue).pop_front() {
                return ack;
            }
            self.notify.notified().await;
        }
    }
}

/// Where the dispatcher hands `down/telemetry-ack` messages over: cheap to clone, and
/// [`Acks::offer`] never waits.
#[derive(Clone, Debug)]
pub struct Acks {
    queue: Arc<AckQueue>,
}

impl Acks {
    /// Hands an acknowledgement to the uploader. When [`ACK_QUEUE`] are already waiting the
    /// oldest is dropped: a lost acknowledgement only makes its batch go again after the
    /// acknowledgement timeout, and the cloud answers that one `duplicate`.
    pub fn offer(&self, ack: TelemetryAck) {
        {
            let mut queue = lock(&self.queue.queue);
            if queue.len() >= ACK_QUEUE
                && let Some(stale) = queue.pop_front()
            {
                tracing::debug!(stream = %stale.stream, seq = %stale.seq, "dropped a stale telemetry acknowledgement: the queue is full");
            }
            queue.push_back(ack);
        }
        self.queue.notify.notify_one();
    }
}

/// The buffer's numbers for the heartbeat; cheap to clone. The default reports an empty buffer.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    buffer: Arc<Mutex<Option<BufferStats>>>,
    window: Option<Arc<Shared>>,
}

impl Stats {
    /// The heartbeat's `buffer`: the batches and the open window's readings.
    pub fn snapshot(&self) -> BufferStats {
        let mut stats = lock(&self.buffer).clone().unwrap_or(BufferStats {
            pending_records: 0,
            pending_batches: 0,
            oldest_pending: None,
            disk_bytes: 0,
            dropped_records: 0,
        });
        if let Some(shared) = &self.window {
            let (readings, oldest) = {
                let window = lock(&shared.window);
                (window.readings, window.oldest)
            };
            stats.pending_records += readings;
            stats.oldest_pending = match (stats.oldest_pending, oldest) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            stats.dropped_records += shared.dropped.load(Ordering::Relaxed);
        }
        stats
    }
}

/// The handles the rest of the agent uses.
#[derive(Clone, Debug)]
pub struct Handles {
    /// For the polling.
    pub readings: Readings,
    /// For the dispatcher.
    pub acks: Acks,
    /// For the heartbeat.
    pub stats: Stats,
}

/// Forms batches from a closed window.
#[derive(Debug)]
struct Collector {
    builder: BatchBuilder,
    /// The builder's stream, shared by the index entries of its batches.
    stream: Arc<StreamId>,
    limit: usize,
    first_seq: Seq,
    ids: StreamIds,
}

/// What forming batches noticed, for the log.
#[derive(Debug, Default)]
struct Formation {
    batches: Vec<Formed>,
    /// Readings the builder refused (cannot happen with readings from the Modbus reader).
    refused: Vec<String>,
    /// New streams started because the old one used up its sequence numbers.
    rotated: Vec<StreamId>,
    /// Batches that did not encode (a bug in the protocol crate), with their readings.
    unencodable: Vec<(String, u64)>,
}

impl Collector {
    fn new(limit: usize, first_seq: Seq, mut ids: StreamIds) -> Self {
        let builder = Self::builder(&mut ids, limit, first_seq);
        Self {
            stream: Arc::new(builder.stream().clone()),
            builder,
            limit,
            first_seq,
            ids,
        }
    }

    fn builder(ids: &mut StreamIds, limit: usize, first_seq: Seq) -> BatchBuilder {
        let stream = ids.next();
        tracing::info!(%stream, first_seq = %first_seq, "telemetry batches use a new stream");
        BatchBuilder::with_limit(stream.clone(), first_seq, limit)
            .unwrap_or_else(|_| BatchBuilder::new(stream, first_seq))
    }

    /// Forms the window's batches, at `formed_at` on the age clock: run by run of the clock
    /// state, in the order the runs came, and within a run series by series (in the order each
    /// series first came). Consecutive runs differ in state, so the builder starts a new batch
    /// at every run: a batch never holds readings of two runs, and every series goes up in the
    /// order it was read, also when the state changes back within the window.
    fn form(&mut self, window: Window, formed_at: u64) -> Formation {
        let mut formation = Formation::default();
        let Window { groups, runs, .. } = window;
        // Each series' samples in one run are contiguous, because runs follow the order of
        // arrival: one segment (run, series, count) each. Usually one run, so one segment per
        // series.
        let mut segments = Vec::new();
        for (series, group) in groups.iter().enumerate() {
            let mut start = 0;
            while let Some(&(_, run)) = group.samples.get(start) {
                let count = group.samples[start..]
                    .iter()
                    .take_while(|(_, other)| *other == run)
                    .count();
                segments.push((run, series, count));
                start += count;
            }
        }
        segments.sort_unstable_by_key(|&(run, series, _)| (run, series));
        let mut groups: Vec<_> = groups
            .into_iter()
            .map(|group| (group.device, group.point, group.samples.into_iter()))
            .collect();
        for (run, series, count) in segments {
            let synced = runs.get(run).copied().unwrap_or_default();
            let (device, point, samples) = &mut groups[series];
            for (sample, _) in samples.by_ref().take(count) {
                self.push(device, point, sample, synced, formed_at, &mut formation);
            }
        }
        if let Some(batch) = self.builder.finish() {
            self.keep(batch, formed_at, &mut formation);
        }
        formation
    }

    fn push(
        &mut self,
        device: &DeviceId,
        point: &PointKey,
        sample: Sample,
        clock_synced: bool,
        formed_at: u64,
        formation: &mut Formation,
    ) {
        let reading = Reading {
            device: device.clone(),
            point: point.clone(),
            sample,
            clock_synced,
        };
        // Only when the stream may be used up (its last sequence number is open, or none is
        // left) is the reading kept for a second try on a new stream.
        let exhausted = self
            .builder
            .next_seq()
            .is_none_or(|seq| seq.next().is_none());
        let retry = exhausted.then(|| reading.clone());
        match self.builder.push(reading) {
            Ok(Some(batch)) => self.keep(batch, formed_at, formation),
            Ok(None) => {}
            Err(PushError::SeqExhausted) => {
                if let Some(batch) = self.builder.finish() {
                    self.keep(batch, formed_at, formation);
                }
                self.builder = Self::builder(&mut self.ids, self.limit, self.first_seq);
                self.stream = Arc::new(self.builder.stream().clone());
                formation.rotated.push(self.builder.stream().clone());
                if let Some(reading) = retry {
                    match self.builder.push(reading) {
                        Ok(Some(batch)) => self.keep(batch, formed_at, formation),
                        Ok(None) => {}
                        Err(error) => formation.refused.push(error.to_string()),
                    }
                }
            }
            Err(error) => formation.refused.push(error.to_string()),
        }
    }

    fn keep(&mut self, batch: TelemetryBatch, formed_at: u64, formation: &mut Formation) {
        let records = u32::try_from(batch.record_count()).unwrap_or(u32::MAX);
        let oldest = batch
            .series
            .iter()
            .flat_map(|series| series.samples.iter().map(|sample| sample.at))
            .min()
            .unwrap_or(UnixMillis::new(0));
        match batch.encode() {
            Ok(mut payload) => {
                payload.shrink_to_fit();
                let stream = if *self.stream == batch.stream {
                    Arc::clone(&self.stream)
                } else {
                    Arc::new(batch.stream)
                };
                formation.batches.push(Formed {
                    meta: Meta {
                        stream,
                        seq: batch.seq,
                        records,
                        oldest,
                        formed_at,
                    },
                    payload,
                });
            }
            Err(error) => formation.unencodable.push((
                format!("{}/{}: {error}", batch.stream, batch.seq),
                u64::from(records),
            )),
        }
    }
}

/// The buffer and the collector, used one operation at a time on a blocking thread.
#[derive(Debug)]
struct Core {
    buffer: Buffer,
    collector: Collector,
}

/// A batch in flight.
#[derive(Debug)]
struct InFlight {
    id: u64,
    stream: StreamId,
    seq: Seq,
    /// When it was formed, on the age clock: past [`protocol::telemetry::BUFFER_MAX_AGE`] it is
    /// never sent again.
    formed_at: u64,
    /// Readings in the batch, for the log when the cloud rejects it.
    records: u32,
    /// Its first publish the broker accepted on the connection it last went out on: an
    /// acknowledgement can only answer a publish made on its own connection (each starts a
    /// clean session) that reached the cloud. `None` until one is accepted on that connection.
    published: Arc<Mutex<Option<FirstPublish>>>,
    /// Refusals by the broker in a row.
    refusals: u32,
}

/// The first publish of a batch that the broker accepted on one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FirstPublish {
    session: Option<SessionId>,
    /// The wall clock right before it, in ms.
    at: u64,
    /// The monotonic clock at the same moment: the round trip to the acknowledgement is
    /// bounded on both clocks ([`crate::clock::SYNC_ROUND_TRIP_LIMIT`]).
    sent: Instant,
}

/// One send of a batch: reserve, read, publish.
#[derive(Debug)]
struct Attempt {
    /// The connection the place was reserved on, or that refused it.
    session: Option<SessionId>,
    outcome: AttemptOutcome,
}

#[derive(Debug)]
enum AttemptOutcome {
    Published(Result<Published, PublishError>),
    /// The payload could not be read from disk, or is not the batch it should be.
    Unreadable(String),
    /// Not published: the batch crossed the longest buffering time while it waited for its
    /// place in the link's queue.
    Expired,
}

/// What the uploader is doing.
#[derive(Debug)]
enum Upload {
    /// Nothing in flight.
    Idle,
    /// A send is under way.
    Sending {
        batch: InFlight,
        task: JoinHandle<Attempt>,
    },
    /// The broker has the batch; waiting for the cloud's acknowledgement.
    AwaitingAck {
        batch: InFlight,
        session: Option<SessionId>,
        deadline: Instant,
    },
    /// The connection went away; send again on the next connection other than `failed`, or at
    /// `until` (an acknowledgement timeout later) on whatever connection is up then, so that a
    /// connection wrongly taken for the failed one cannot hold the batch forever.
    Reconnecting {
        batch: InFlight,
        failed: Option<SessionId>,
        until: Instant,
    },
    /// Send again at `until` (after `retry`, or after a refusal).
    Waiting { batch: InFlight, until: Instant },
}

impl Upload {
    fn batch(&self) -> Option<&InFlight> {
        match self {
            Self::Idle => None,
            Self::Sending { batch, .. }
            | Self::AwaitingAck { batch, .. }
            | Self::Reconnecting { batch, .. }
            | Self::Waiting { batch, .. } => Some(batch),
        }
    }

    fn into_batch(self) -> Option<InFlight> {
        match self {
            Self::Idle => None,
            Self::Sending { batch, task } => {
                task.abort();
                Some(batch)
            }
            Self::AwaitingAck { batch, .. }
            | Self::Reconnecting { batch, .. }
            | Self::Waiting { batch, .. } => Some(batch),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::AwaitingAck { deadline, .. } => Some(*deadline),
            Self::Waiting { until, .. } | Self::Reconnecting { until, .. } => Some(*until),
            _ => None,
        }
    }
}

/// Why the pipeline woke.
enum Wake {
    Stop,
    Attempt(Result<Attempt, tokio::task::JoinError>),
    Ack(TelemetryAck),
    State(bool),
    /// The upload interval passed.
    Tick,
    /// The window reached its size to close.
    Window,
    Deadline,
}

/// Logs a repeated problem at most every [`LOG_EVERY`], counting the rest.
#[derive(Debug, Default)]
struct Throttle {
    last: Option<Instant>,
    suppressed: u64,
}

impl Throttle {
    /// `Some(messages held back since the last one)` when this one may be logged.
    fn ready(&mut self) -> Option<u64> {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < LOG_EVERY)
        {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Totals of a repeated loss of readings, logged at most every [`LOG_EVERY`] with everything
/// since the last log line (and the rest at the stop), so that every reading lost this way is
/// in some line's `readings`.
#[derive(Debug, Default)]
struct Losses {
    last: Option<Instant>,
    /// Since the last log line: occurrences and their readings.
    count: u64,
    readings: u64,
    /// The first occurrence since the last log line.
    first: Option<String>,
}

/// What a [`Losses`] line says: occurrences, readings, and the first occurrence.
type LossTotals = (u64, u64, String);

impl Losses {
    /// Counts one occurrence; the totals when a line is due.
    fn add(&mut self, readings: u64, detail: impl FnOnce() -> String) -> Option<LossTotals> {
        self.count += 1;
        self.readings += readings;
        if self.first.is_none() {
            self.first = Some(detail());
        }
        self.take(false)
    }

    /// The totals since the last line, if any, when a line is due (`now`: at any time).
    fn take(&mut self, now: bool) -> Option<LossTotals> {
        if self.count == 0 {
            return None;
        }
        let due = now || self.last.is_none_or(|last| last.elapsed() >= LOG_EVERY);
        if !due {
            return None;
        }
        self.last = Some(Instant::now());
        Some((
            std::mem::take(&mut self.count),
            std::mem::take(&mut self.readings),
            self.first.take().unwrap_or_default(),
        ))
    }
}

/// Logs what the buffer reports, without flooding the journal.
#[derive(Debug, Default)]
struct Reporter {
    /// Drops not logged yet, by reason: (batches, readings).
    drops: BTreeMap<DropReason, (u64, u64)>,
    drops_logged: Option<Instant>,
    disk: Throttle,
    clock: Throttle,
    refused: Throttle,
    unknown: Throttle,
    unacknowledged: Throttle,
    /// Batches the cloud rejected.
    rejected: Losses,
    /// Readings the batch builder refused.
    refused_readings: Losses,
    /// Batches that did not encode.
    unencodable: Losses,
    /// Window drops already counted in `drops`.
    window_dropped: u64,
}

impl Reporter {
    fn report(&mut self, events: Events) {
        for event in events.0 {
            match event {
                Event::Dropped {
                    reason,
                    batches,
                    records,
                } => {
                    let entry = self.drops.entry(reason).or_default();
                    entry.0 += batches;
                    entry.1 += records;
                }
                Event::DiskFailed(error) => {
                    if let Some(suppressed) = self.disk.ready() {
                        tracing::error!(%error, suppressed, retry_in_s = buffer::DISK_RETRY.as_secs(), "the buffer cannot write to the data partition; batches stay in memory, and the oldest are dropped when it is full");
                    }
                }
                Event::DiskOpenFailed(error) => {
                    if let Some(suppressed) = self.disk.ready() {
                        tracing::error!(%error, suppressed, retry_in_s = buffer::DISK_RETRY.as_secs(), "cannot open the buffer directory; batches stay in memory until it opens");
                    }
                }
                Event::ClockNotSaved(error) => {
                    if let Some(suppressed) = self.clock.ready() {
                        tracing::warn!(%error, suppressed, retry_in_s = buffer::CLOCK_SAVE_EVERY.as_secs(), "cannot save the age clock's time in the buffer directory; after a restart, batches on disk may count as younger than they are by up to the time since it was last saved");
                    }
                }
                Event::DiskOpenedForStop { bytes } => tracing::info!(
                    bytes,
                    "opened the buffer directory for the stop only; the batches already there are read at the next start"
                ),
                Event::DiskOpened {
                    batches,
                    records,
                    bytes,
                    removed_temporary,
                    ignored,
                    unreadable_bytes,
                } => {
                    tracing::info!(
                        batches,
                        records,
                        bytes,
                        removed_temporary,
                        "opened the buffer directory; batches found there are sent first"
                    );
                    if !ignored.is_empty() {
                        tracing::warn!(files = ?ignored, "files in the buffer directory that are not buffer segments are left alone");
                    }
                    if unreadable_bytes > 0 {
                        tracing::warn!(
                            bytes = unreadable_bytes,
                            "the buffer directory held unreadable records; they were dropped"
                        );
                    }
                }
                Event::NotFlushed {
                    batches,
                    records,
                    error,
                } => {
                    // Never throttled: it happens once, at the stop.
                    tracing::error!(
                        batches,
                        records,
                        error = error.as_deref().unwrap_or("the disk limit leaves no room"),
                        "stopping: these buffered batches could not be made durable on disk and are lost"
                    );
                }
            }
        }
        self.log_drops(false);
    }

    /// Counts readings the window dropped since the last call.
    fn window(&mut self, total: u64) {
        let new = total.saturating_sub(self.window_dropped);
        if new > 0 {
            self.window_dropped = total;
            let entry = self.drops.entry(DropReason::WindowFull).or_default();
            entry.1 += new;
        }
    }

    /// A batch of `readings` the cloud rejected (`detail`: which, and why).
    fn rejected(&mut self, readings: u64, detail: impl FnOnce() -> String) {
        if let Some(totals) = self.rejected.add(readings, detail) {
            log_rejected(totals);
        }
    }

    /// `readings` the batch builder refused, the first with `detail`.
    fn refused_readings(&mut self, readings: u64, detail: impl FnOnce() -> String) {
        if let Some(totals) = self.refused_readings.add(readings, detail) {
            log_refused_readings(totals);
        }
    }

    /// A batch of `readings` that did not encode.
    fn unencodable(&mut self, readings: u64, detail: impl FnOnce() -> String) {
        if let Some(totals) = self.unencodable.add(readings, detail) {
            log_unencodable(totals);
        }
    }

    /// Logs the drops and losses collected so far, at most every [`LOG_EVERY`] unless `now`.
    fn log_drops(&mut self, now: bool) {
        if let Some(totals) = self.rejected.take(now) {
            log_rejected(totals);
        }
        if let Some(totals) = self.refused_readings.take(now) {
            log_refused_readings(totals);
        }
        if let Some(totals) = self.unencodable.take(now) {
            log_unencodable(totals);
        }
        if self.drops.is_empty() {
            return;
        }
        let due = self
            .drops_logged
            .is_none_or(|last| last.elapsed() >= LOG_EVERY);
        if !due && !now {
            return;
        }
        self.drops_logged = Some(Instant::now());
        for (reason, (batches, records)) in std::mem::take(&mut self.drops) {
            tracing::warn!(
                %reason,
                batches,
                records,
                "dropped buffered telemetry: {}; the readings are counted in the heartbeat's dropped_records",
                reason.explain()
            );
        }
    }
}

fn log_rejected((batches, readings, first): LossTotals) {
    tracing::error!(
        batches,
        readings,
        first = %first,
        "the cloud rejected telemetry batches; they will never be accepted, so they were released and their readings are lost (not counted in dropped_records)"
    );
}

fn log_refused_readings((windows, readings, first): LossTotals) {
    tracing::warn!(
        readings,
        windows,
        first = %first,
        "readings that cannot be sent were left out of the batches"
    );
}

fn log_unencodable((batches, readings, first): LossTotals) {
    tracing::error!(
        batches,
        readings,
        first = %first,
        "telemetry batches that do not encode; their readings are lost"
    );
}

/// The pipeline's task; run it with [`Pipeline::run`].
#[derive(Debug)]
pub struct Pipeline {
    settings: Settings,
    link: Link,
    clock: GatewayClock,
    age: AgeClock,
    core: Arc<Mutex<Core>>,
    shared: Arc<Shared>,
    acks: Arc<AckQueue>,
    stats: Arc<Mutex<Option<BufferStats>>>,
    stop: CancellationToken,
    /// Set as soon as the stop is asked for; the buffer reads no more of the disk then.
    stopping: Arc<AtomicBool>,
}

/// Creates the pipeline: the handles for the polling, the dispatcher and the heartbeat, and the
/// task, which does nothing until [`Pipeline::run`] is awaited. It sends over `link`, stamps
/// and checks time with `clock`, and stops (flushing to disk) when `stop` is cancelled.
///
/// # Errors
///
/// [`NoRandomness`]: the OS gave no randomness for the stream IDs.
pub fn pipeline(
    settings: Settings,
    link: Link,
    clock: GatewayClock,
    stop: CancellationToken,
) -> Result<(Handles, Pipeline), NoRandomness> {
    pipeline_with(settings, link, clock, stop, crate::link::fill_random)
}

/// [`pipeline`], with the stream IDs' randomness from `randomness` (it fills the bytes, `false`
/// when it has none).
fn pipeline_with(
    settings: Settings,
    link: Link,
    clock: GatewayClock,
    stop: CancellationToken,
    randomness: fn(&mut [u8]) -> bool,
) -> Result<(Handles, Pipeline), NoRandomness> {
    let ids = StreamIds::new(randomness)?;
    let shared = Arc::new(Shared {
        window: Mutex::new(Window::default()),
        early: Notify::new(),
        dropped: AtomicU64::new(0),
        limit: settings.window_limit,
        close_at: AtomicUsize::new(settings.window_limit),
    });
    let acks = Arc::new(AckQueue::default());
    let stats = Arc::new(Mutex::new(None));
    let age = AgeClock::new(clock.clone());
    let stopping = Arc::new(AtomicBool::new(false));
    let core = Core {
        buffer: Buffer::new(
            settings.directory.clone(),
            settings.memory_limit,
            settings.disk_limit,
        )
        .with_batch_limit(settings.max_batches)
        .with_age_clock(age.clone())
        .with_stop(Arc::clone(&stopping)),
        collector: Collector::new(settings.batch_limit, settings.first_seq, ids),
    };
    let handles = Handles {
        readings: Readings {
            shared: Arc::clone(&shared),
        },
        acks: Acks {
            queue: Arc::clone(&acks),
        },
        stats: Stats {
            buffer: Arc::clone(&stats),
            window: Some(Arc::clone(&shared)),
        },
    };
    let pipeline = Pipeline {
        age,
        settings,
        link,
        clock,
        core: Arc::new(Mutex::new(core)),
        shared,
        acks,
        stats,
        stop,
        stopping,
    };
    Ok((handles, pipeline))
}

/// The session of the connection that is up now, if any.
fn connected(state: &LinkState) -> Option<SessionId> {
    match state {
        LinkState::Connected { session } => Some(session.clone()),
        _ => None,
    }
}

/// One send: a place in the link's queue first, then the payload, then the publish, unless
/// the batch formed at `formed_at` has expired on the `age` clock by then. The session is the
/// connection the place is in or, when no place was had, the connection that was up before the
/// wait: never one that came up during it, which would then be taken for the failed one.
async fn attempt(
    link: Link,
    source: Source,
    expected: (StreamId, Seq),
    formed_at: u64,
    published: Arc<Mutex<Option<FirstPublish>>>,
    clock: GatewayClock,
    age: AgeClock,
) -> Attempt {
    let before = connected(&link.state().borrow());
    let permit = match link.reserve(Channel::Telemetry).await {
        Ok(permit) => permit,
        Err(error) => {
            return Attempt {
                session: before,
                outcome: AttemptOutcome::Published(Err(error)),
            };
        }
    };
    let session = Some(permit.session().clone());
    let payload = match source {
        Source::Memory(payload) => Vec::from(payload),
        Source::Disk { dir, at } => {
            let read = tokio::task::spawn_blocking(move || buffer::read_payload(&dir, &at)).await;
            let payload = match read {
                Ok(Ok(payload)) => payload,
                Ok(Err(error)) => {
                    return Attempt {
                        session,
                        outcome: AttemptOutcome::Unreadable(error),
                    };
                }
                Err(error) => {
                    return Attempt {
                        session,
                        outcome: AttemptOutcome::Unreadable(format!("reading failed: {error}")),
                    };
                }
            };
            // The checksum matched; still make sure the record holds the batch it says.
            match TelemetryBatch::peek_header(&payload) {
                Ok(header) if (header.stream.clone(), header.seq) == expected => payload,
                Ok(header) => {
                    return Attempt {
                        session,
                        outcome: AttemptOutcome::Unreadable(format!(
                            "the record holds batch {}/{}",
                            header.stream, header.seq
                        )),
                    };
                }
                Err(error) => {
                    return Attempt {
                        session,
                        outcome: AttemptOutcome::Unreadable(format!(
                            "the record is not a telemetry batch: {error}"
                        )),
                    };
                }
            }
        }
    };
    // The wait for a place can be long; never publish a batch that crossed the limit meanwhile.
    if buffer::expired(formed_at, age.now()) {
        return Attempt {
            session,
            outcome: AttemptOutcome::Expired,
        };
    }
    // An acknowledgement can only answer a publish on its own connection: the first publish on
    // an earlier one stops counting before anything is sent on this one.
    {
        let mut first = lock(&published);
        if first.as_ref().is_some_and(|first| first.session != session) {
            *first = None;
        }
    }
    // The batch cannot reach the cloud before this. The wait inside `publish` and everything
    // after it until the acknowledgement counts in the round trip, which bounds the comparison
    // (`SYNC_ROUND_TRIP_LIMIT`).
    let at = clock.wall().get();
    let sent = Instant::now();
    let outcome = permit.publish(payload).await;
    // Only a publish the broker accepted (PUBACK 0x00 or 0x10) can reach the cloud and be
    // acknowledged. One it refused, one too large for it, or one that never went out does not
    // count, so that the acknowledgement of a later publish on this connection is not compared
    // from its earlier time; one whose connection ended before the PUBACK is not counted either
    // (the next publish goes on another connection, which drops it first).
    if outcome.is_ok() {
        let mut first = lock(&published);
        if first.as_ref().is_none_or(|first| first.session != session) {
            *first = Some(FirstPublish {
                session: session.clone(),
                at,
                sent,
            });
        }
    }
    Attempt {
        session,
        outcome: AttemptOutcome::Published(outcome),
    }
}

fn clamp_retry(after_ms: u32) -> Duration {
    Duration::from_millis(u64::from(after_ms)).clamp(RETRY_AFTER_MIN, RETRY_AFTER_MAX)
}

fn refused_backoff(refusals: u32) -> Duration {
    let doublings = refusals.saturating_sub(1).min(16);
    REFUSED_BACKOFF_INITIAL
        .saturating_mul(1 << doublings)
        .min(REFUSED_BACKOFF_MAX)
}

impl Pipeline {
    /// Runs until the stop token is cancelled, then closes the window and writes every batch
    /// in memory to disk.
    ///
    /// The disk buffer is opened first, so batches from an earlier process go before new ones.
    /// Then, for the batch in flight:
    ///
    /// | What happens | What the uploader does |
    /// | --- | --- |
    /// | PUBACK 0x00 or 0x10 | waits up to the acknowledgement timeout for the cloud's ack |
    /// | ack `committed`, `duplicate` | releases the batch, sends the next |
    /// | ack `rejected` | releases the batch, logs an error with `code` and `message` (not a drop) |
    /// | ack `retry` | keeps the batch, sends it again after `after_ms` (1 s–10 min) |
    /// | ack with an unknown status | keeps the batch, sends it again after the timeout |
    /// | ack for another batch | ignored |
    /// | ack for the batch while its send is under way (before the publish has its PUBACK) | held until the send ends, then handled as above |
    /// | no ack within the timeout | sends the same batch again |
    /// | PUBACK 0x80 or above, or too large for the broker | keeps the batch, logs, sends again after 1 s, doubling to 5 min |
    /// | not connected, connection lost, link stopping | sends the same batch again on the next connection, or after the acknowledgement timeout on the connection that is up then |
    /// | record on disk unreadable or corrupt | drops the batch (counted) and sends the next |
    /// | older than 30 days on the age clock | never sent, first or again: dropped (counted, `expired`) |
    ///
    /// Batches found on disk are checked for age as soon as they are loaded, before anything
    /// is sent, and every send checks the batch's age again, the last time right before the
    /// publish. Every acknowledgement of the batch in flight is also compared with the clock
    /// ([`GatewayClock::compare`]), from the batch's first publish the broker accepted on the
    /// connection it last went out on: a refused publish never reached the cloud, and an
    /// acknowledgement with no accepted publish to compare with leaves the clock's state as it
    /// was, as does one that arrives more than [`crate::clock::SYNC_ROUND_TRIP_LIMIT`] after
    /// that publish.
    ///
    /// Every upload interval the window closes, except while uploads do not go through (see
    /// the module documentation): then it closes every [`OUTAGE_WINDOW`] or at about one
    /// batch, and the interval only does the buffer's housekeeping.
    pub async fn run(self) {
        let mut reporter = Reporter::default();
        // When the stop was asked for: the stop's time counts from then, even when the stop
        // comes while a buffer operation runs.
        let stopped_at = Arc::new(OnceLock::new());
        let watcher = {
            let stop = self.stop.clone();
            let stopped_at = Arc::clone(&stopped_at);
            let stopping = Arc::clone(&self.stopping);
            tokio::spawn(async move {
                stop.cancelled().await;
                let _ = stopped_at.set(std::time::Instant::now());
                stopping.store(true, Ordering::Relaxed);
            })
        };
        // Opening the disk raises the age clock's floor to the newest batch found there.
        let now = self.age.now();
        self.core(&mut reporter, move |core, events| {
            core.buffer.open_disk(now, events);
        })
        .await;
        self.expire(&mut reporter).await;
        let interval = self.settings.upload_interval;
        let mut ticks = tokio::time::interval_at(Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut state = self.link.state();
        let mut link_running = true;
        let mut upload = Upload::Idle;
        // An acknowledgement of the batch that came while its send was under way: the send's
        // first accepted publish is recorded only when the send ends, and the comparison with
        // the cloud's time needs it, so the acknowledgement waits for the send to end.
        let mut held: Option<TelemetryAck> = None;
        let mut window_opened = Instant::now();
        loop {
            if matches!(upload, Upload::Idle) {
                upload = self.start_next(&state, &mut reporter).await;
            }
            let deadline = upload.deadline();
            let wake = tokio::select! {
                biased;
                () = self.stop.cancelled() => Wake::Stop,
                attempt = async {
                    match &mut upload {
                        Upload::Sending { task, .. } => task.await,
                        _ => std::future::pending().await,
                    }
                }, if matches!(upload, Upload::Sending { .. }) => Wake::Attempt(attempt),
                ack = self.acks.recv() => Wake::Ack(ack),
                changed = state.changed(), if link_running => Wake::State(changed.is_ok()),
                _ = ticks.tick() => Wake::Tick,
                () = self.shared.early.notified() => Wake::Window,
                () = async {
                    match deadline {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                }, if deadline.is_some() => Wake::Deadline,
            };
            upload = match wake {
                Wake::Stop => {
                    let _ = upload.into_batch();
                    break;
                }
                Wake::Tick => {
                    let outage = self.outage(&state);
                    self.shared
                        .close_at
                        .store(self.close_at(outage), Ordering::Relaxed);
                    let form = !outage || window_opened.elapsed() >= self.settings.outage_window;
                    self.close_window(&mut reporter, form).await;
                    if form {
                        window_opened = Instant::now();
                    }
                    upload
                }
                Wake::Window => {
                    self.close_window(&mut reporter, true).await;
                    window_opened = Instant::now();
                    upload
                }
                Wake::Attempt(result) => {
                    let upload = self.attempted(upload, result, &state, &mut reporter).await;
                    match held.take() {
                        Some(ack) => self.acknowledged(upload, ack, &mut reporter).await,
                        None => upload,
                    }
                }
                Wake::Ack(ack) => {
                    if let Upload::Sending { batch, .. } = &upload
                        && ack.stream == batch.stream
                        && ack.seq == batch.seq
                    {
                        tracing::debug!(stream = %ack.stream, seq = %ack.seq, "a telemetry acknowledgement came while its batch is being sent; handling it once the send ends");
                        // The latest one says what the cloud did last.
                        held = Some(ack);
                        upload
                    } else {
                        self.acknowledged(upload, ack, &mut reporter).await
                    }
                }
                Wake::State(running) => {
                    link_running = running;
                    let current = state.borrow_and_update().clone();
                    self.link_changed(upload, &current)
                }
                Wake::Deadline => {
                    let current = state.borrow().clone();
                    self.deadline_passed(upload, &current, &mut reporter)
                }
            };
        }
        // Stopping: what is in the window becomes batches, and memory goes to disk within the
        // stop's time.
        let deadline = stopped_at
            .get()
            .copied()
            .unwrap_or_else(std::time::Instant::now)
            + self.settings.flush_limit;
        watcher.abort();
        // Whatever the watcher got to: no full read of the disk from here on.
        self.stopping.store(true, Ordering::Relaxed);
        self.close_window(&mut reporter, true).await;
        self.core(&mut reporter, move |core, events| {
            core.buffer
                .flush(&mut || std::time::Instant::now() < deadline, events);
        })
        .await;
        reporter.window(self.shared.dropped.load(Ordering::Relaxed));
        reporter.log_drops(true);
        tracing::info!("the telemetry pipeline has stopped");
    }

    /// Runs `work` on the core on a blocking thread (the buffer does file I/O), logs its
    /// events and publishes the new numbers. `None` if the work panicked.
    async fn core<T: Send + 'static>(
        &self,
        reporter: &mut Reporter,
        work: impl FnOnce(&mut Core, &mut Events) -> T + Send + 'static,
    ) -> Option<T> {
        let core = Arc::clone(&self.core);
        let done = tokio::task::spawn_blocking(move || {
            let mut core = lock(&core);
            let mut events = Events::default();
            let value = work(&mut core, &mut events);
            (value, events, core.buffer.stats())
        })
        .await;
        match done {
            Ok((value, events, stats)) => {
                *lock(&self.stats) = Some(stats);
                reporter.report(events);
                Some(value)
            }
            Err(error) => {
                tracing::error!(%error, "a buffer operation failed");
                None
            }
        }
    }

    /// Whether uploads do not go through: the link is not connected, or more than one batch
    /// waits (the one in flight is buffered until its acknowledgement).
    fn outage(&self, state: &watch::Receiver<LinkState>) -> bool {
        connected(&state.borrow()).is_none()
            || lock(&self.stats)
                .as_ref()
                .is_some_and(|stats| stats.pending_batches > 1)
    }

    /// The window's estimate at which it closes early.
    fn close_at(&self, outage: bool) -> usize {
        if outage {
            self.settings
                .batch_limit
                .saturating_mul(OUTAGE_CLOSE_FACTOR)
                .min(self.settings.window_limit)
        } else {
            self.settings.window_limit
        }
    }

    /// Closes the window when `form` (forms its batches and buffers them), then drops what is
    /// too old and does the buffer's housekeeping.
    async fn close_window(&self, reporter: &mut Reporter, form: bool) {
        let window = if form {
            std::mem::take(&mut *lock(&self.shared.window))
        } else {
            Window::default()
        };
        reporter.window(self.shared.dropped.load(Ordering::Relaxed));
        let now = self.age.now();
        let formation = self
            .core(reporter, move |core, events| {
                let Formation {
                    batches,
                    refused,
                    rotated,
                    unencodable,
                } = core.collector.form(window, now);
                for batch in batches {
                    core.buffer.push(batch, now, events);
                }
                core.buffer.expire(now, events);
                let floor = core.buffer.tick(now, events);
                let report = Formation {
                    batches: Vec::new(),
                    refused,
                    rotated,
                    unencodable,
                };
                (report, floor)
            })
            .await;
        let Some((formation, floor)) = formation else {
            return;
        };
        if floor.is_some() {
            // The disk opened only now (raising the age clock's floor; the batches formed
            // before kept their age): check the age of what it held before anything is sent.
            self.expire(reporter).await;
        }
        for stream in formation.rotated {
            tracing::info!(%stream, "the telemetry stream used every sequence number; continuing on a new stream");
        }
        for (error, readings) in formation.unencodable {
            reporter.unencodable(readings, || error);
        }
        let refused = formation.refused.len() as u64;
        if let Some(first) = formation.refused.into_iter().next() {
            reporter.refused_readings(refused, || first);
        }
        reporter.log_drops(false);
    }

    /// Drops every batch older than the longest buffering time on the age clock (counted and
    /// logged as `expired`).
    async fn expire(&self, reporter: &mut Reporter) {
        let now = self.age.now();
        self.core(reporter, move |core, events| {
            core.buffer.expire(now, events)
        })
        .await;
        reporter.log_drops(false);
    }

    /// Starts sending the oldest batch when the link is connected. An oldest batch past the
    /// longest buffering time is never sent: the expired batches are dropped first.
    async fn start_next(
        &self,
        state: &watch::Receiver<LinkState>,
        reporter: &mut Reporter,
    ) -> Upload {
        if connected(&state.borrow()).is_none() {
            return Upload::Idle;
        }
        let mut outgoing = lock(&self.core).buffer.front();
        if outgoing
            .as_ref()
            .is_some_and(|next| buffer::expired(next.formed_at, self.age.now()))
        {
            self.expire(reporter).await;
            outgoing = lock(&self.core).buffer.front();
        }
        let Some(outgoing) = outgoing else {
            return Upload::Idle;
        };
        if buffer::expired(outgoing.formed_at, self.age.now()) {
            // Only if expiring failed (the buffer's task panicked); the next wake tries again.
            return Upload::Idle;
        }
        self.spawn(
            InFlight {
                id: outgoing.id,
                stream: outgoing.stream.clone(),
                seq: outgoing.seq,
                formed_at: outgoing.formed_at,
                records: outgoing.records,
                published: Arc::new(Mutex::new(None)),
                refusals: 0,
            },
            outgoing.source,
        )
    }

    /// Sends `batch` again, if it is still buffered and has not expired. An expired batch is
    /// given up; [`Pipeline::start_next`] then drops it.
    fn send(&self, batch: InFlight) -> Upload {
        if buffer::expired(batch.formed_at, self.age.now()) {
            tracing::debug!(stream = %batch.stream, seq = %batch.seq, "a batch in flight passed the longest buffering time; it is not sent again");
            return Upload::Idle;
        }
        let source = lock(&self.core).buffer.source(batch.id);
        match source {
            Some(source) => self.spawn(batch, source),
            None => Upload::Idle,
        }
    }

    fn spawn(&self, batch: InFlight, source: Source) -> Upload {
        let task = tokio::spawn(attempt(
            self.link.clone(),
            source,
            (batch.stream.clone(), batch.seq),
            batch.formed_at,
            Arc::clone(&batch.published),
            self.clock.clone(),
            self.age.clone(),
        ));
        Upload::Sending { batch, task }
    }

    /// A send ended.
    async fn attempted(
        &self,
        upload: Upload,
        result: Result<Attempt, tokio::task::JoinError>,
        state: &watch::Receiver<LinkState>,
        reporter: &mut Reporter,
    ) -> Upload {
        let Upload::Sending { mut batch, .. } = upload else {
            return upload;
        };
        let attempt = match result {
            Ok(attempt) => attempt,
            Err(error) => {
                tracing::error!(%error, "a telemetry send failed");
                return Upload::Waiting {
                    batch,
                    until: Instant::now() + REFUSED_BACKOFF_INITIAL,
                };
            }
        };
        match attempt.outcome {
            AttemptOutcome::Unreadable(error) => {
                tracing::error!(stream = %batch.stream, seq = %batch.seq, %error, "a buffered batch on disk is unreadable; dropping it");
                let id = batch.id;
                self.core(reporter, move |core, events| {
                    core.buffer.discard(id, events)
                })
                .await;
                Upload::Idle
            }
            AttemptOutcome::Expired => {
                tracing::debug!(stream = %batch.stream, seq = %batch.seq, "a batch passed the longest buffering time before its publish; it is not sent");
                // The next start drops it.
                Upload::Idle
            }
            AttemptOutcome::Published(Ok(_)) => {
                batch.refusals = 0;
                Upload::AwaitingAck {
                    batch,
                    session: attempt.session,
                    deadline: Instant::now() + self.settings.ack_timeout,
                }
            }
            AttemptOutcome::Published(Err(
                PublishError::NotConnected | PublishError::ConnectionLost | PublishError::Stopped,
            )) => {
                let waiting = Upload::Reconnecting {
                    batch,
                    failed: attempt.session,
                    until: Instant::now() + self.settings.ack_timeout,
                };
                self.link_changed(waiting, &state.borrow())
            }
            AttemptOutcome::Published(Err(PublishError::Busy)) => Upload::Waiting {
                batch,
                until: Instant::now() + REFUSED_BACKOFF_INITIAL,
            },
            AttemptOutcome::Published(Err(error)) => {
                batch.refusals = batch.refusals.saturating_add(1);
                let delay = refused_backoff(batch.refusals);
                if let Some(suppressed) = reporter.refused.ready() {
                    tracing::error!(stream = %batch.stream, seq = %batch.seq, %error, refusals = batch.refusals, retry_in_s = delay.as_secs(), suppressed, "the broker refused a telemetry batch; keeping it and trying again");
                }
                Upload::Waiting {
                    batch,
                    until: Instant::now() + delay,
                }
            }
        }
    }

    /// The deadline of the batch in flight passed (the link is in `state`).
    fn deadline_passed(
        &self,
        upload: Upload,
        state: &LinkState,
        reporter: &mut Reporter,
    ) -> Upload {
        match upload {
            Upload::AwaitingAck { batch, .. } => {
                if let Some(suppressed) = reporter.unacknowledged.ready() {
                    tracing::warn!(stream = %batch.stream, seq = %batch.seq, suppressed, timeout_s = self.settings.ack_timeout.as_secs(), "no acknowledgement from the cloud in time; sending the batch again");
                }
                self.send(batch)
            }
            Upload::Waiting { batch, .. } => self.send(batch),
            // No other connection in an acknowledgement timeout: send on the one that is up,
            // even if it was taken for the failed one; without one, wait again.
            Upload::Reconnecting { batch, failed, .. } => {
                if connected(state).is_some() {
                    self.send(batch)
                } else {
                    Upload::Reconnecting {
                        batch,
                        failed,
                        until: Instant::now() + self.settings.ack_timeout,
                    }
                }
            }
            other => other,
        }
    }

    /// The link's state changed: a batch whose connection is gone goes again on the next one
    /// (or, failing that, when its `Reconnecting` time is up: [`Pipeline::deadline_passed`]).
    fn link_changed(&self, upload: Upload, state: &LinkState) -> Upload {
        let now = connected(state);
        match upload {
            Upload::AwaitingAck {
                batch,
                session,
                deadline,
            } => {
                if now.is_some() && now == session {
                    Upload::AwaitingAck {
                        batch,
                        session,
                        deadline,
                    }
                } else {
                    self.link_changed(
                        Upload::Reconnecting {
                            batch,
                            failed: session,
                            until: Instant::now() + self.settings.ack_timeout,
                        },
                        state,
                    )
                }
            }
            Upload::Reconnecting {
                batch,
                failed,
                until,
            } => match now {
                Some(session) if Some(&session) != failed.as_ref() => self.send(batch),
                _ => Upload::Reconnecting {
                    batch,
                    failed,
                    until,
                },
            },
            other => other,
        }
    }

    /// An acknowledgement arrived.
    async fn acknowledged(
        &self,
        upload: Upload,
        ack: TelemetryAck,
        reporter: &mut Reporter,
    ) -> Upload {
        let Some(batch) = upload.batch() else {
            tracing::debug!(stream = %ack.stream, seq = %ack.seq, "ignored a telemetry acknowledgement: no batch is in flight");
            return upload;
        };
        if ack.stream != batch.stream || ack.seq != batch.seq {
            tracing::debug!(stream = %ack.stream, seq = %ack.seq, in_flight_stream = %batch.stream, in_flight_seq = %batch.seq, "ignored a telemetry acknowledgement for another batch");
            return upload;
        }
        // From the first publish the broker accepted on the connection the batch last went out
        // on: an ack cannot answer a publish made on an earlier connection, or one the broker
        // refused. Without one (none accepted yet) nothing is compared, nor when the ack comes
        // more than `SYNC_ROUND_TRIP_LIMIT` after it (`GatewayClock::compare`).
        let published = lock(&batch.published)
            .as_ref()
            .map(|first| (first.at, first.sent));
        if let Some((published, sent)) = published {
            self.clock.compare(
                ack.server_time,
                UnixMillis::new(published),
                self.clock.wall(),
                sent.elapsed(),
            );
        }
        match ack.outcome {
            AckOutcome::Committed { records } | AckOutcome::Duplicate { records } => {
                let Some(batch) = upload.into_batch() else {
                    return Upload::Idle;
                };
                tracing::debug!(stream = %batch.stream, seq = %batch.seq, records, "the cloud has the batch");
                self.release(batch, reporter).await
            }
            AckOutcome::Rejected { code, message } => {
                let Some(batch) = upload.into_batch() else {
                    return Upload::Idle;
                };
                reporter.rejected(u64::from(batch.records), || {
                    format!(
                        "{}/{}: {code}: {}",
                        batch.stream,
                        batch.seq,
                        printable(&message, 1024)
                    )
                });
                self.release(batch, reporter).await
            }
            AckOutcome::Retry { after_ms } => {
                let Some(batch) = upload.into_batch() else {
                    return Upload::Idle;
                };
                let delay = clamp_retry(after_ms);
                tracing::debug!(stream = %batch.stream, seq = %batch.seq, after_ms, delay_ms = delay.as_millis(), "the cloud asked to send the batch again later");
                Upload::Waiting {
                    batch,
                    until: Instant::now() + delay,
                }
            }
            AckOutcome::Unknown => {
                if let Some(suppressed) = reporter.unknown.ready() {
                    tracing::warn!(stream = %batch.stream, seq = %batch.seq, suppressed, "a telemetry acknowledgement with a status this agent does not know; keeping the batch until the acknowledgement timeout");
                }
                upload
            }
        }
    }

    async fn release(&self, batch: InFlight, reporter: &mut Reporter) -> Upload {
        let id = batch.id;
        self.core(reporter, move |core, _| core.buffer.release(id))
            .await;
        Upload::Idle
    }
}

impl DropReason {
    fn explain(self) -> &'static str {
        match self {
            Self::MemoryFull => {
                "memory is full and the disk takes no batches (it failed or the disk limit is 0)"
            }
            Self::DiskFull => "the memory and disk limits are reached",
            Self::TooManyBatches => {
                "more batches than the buffer keeps track of (65,536 in memory and on disk)"
            }
            Self::Expired => "older than the longest buffering time (30 days)",
            Self::Corrupt => "a record on disk was torn or corrupt",
            Self::WindowFull => "the collection window is full (the buffer is not keeping up)",
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::telemetry::Sample;

    use super::*;

    fn reading(device: &str, point: &str, at: u64, synced: bool) -> Reading {
        Reading {
            device: DeviceId::new(device).unwrap(),
            point: PointKey::new(point).unwrap(),
            sample: Sample {
                at: UnixMillis::new(at),
                value: Value::Int(i64::try_from(at).unwrap()),
            },
            clock_synced: synced,
        }
    }

    fn window(readings: impl IntoIterator<Item = Reading>) -> Window {
        let mut window = Window::default();
        for reading in readings {
            window.add(reading);
        }
        window
    }

    /// A collector whose stream IDs come from the OS.
    fn collector(limit: usize, first_seq: Seq) -> Collector {
        Collector::new(
            limit,
            first_seq,
            StreamIds::new(crate::link::fill_random).unwrap(),
        )
    }

    #[test]
    fn stream_ids_are_valid_random_and_distinct() {
        let mut ids = StreamIds::new(crate::link::fill_random).unwrap();
        let first = ids.next();
        let other = StreamIds::new(crate::link::fill_random).unwrap().next();
        assert_ne!(first, other);
        assert_eq!(first.as_str().len(), 32);
        assert!(first.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
        // The streams after the first in a process: the same random digits and a count.
        assert_eq!(ids.next().as_str(), format!("{first}-1"));
        assert_eq!(ids.next().as_str(), format!("{first}-2"));
        assert!(StreamId::new(format!("{first}-{}", u64::MAX)).is_ok());
    }

    /// Codex review (sixth round): without the OS's randomness the pipeline is not created.
    /// The stream ID used to fall back to the clock in nanoseconds and the process ID, which
    /// can both repeat after a restart (a clock that is not set, a process ID used again):
    /// the cloud would then take the new batches for resends of the old stream's and discard
    /// them.
    #[test]
    fn without_randomness_the_pipeline_is_not_created() {
        assert_eq!(StreamIds::new(|_| false).unwrap_err(), NoRandomness);
        let scratch = testkit::mqtt::ScratchDir::new("agent-upload-unit");
        let settings = Settings::new(scratch.path().join("buffer"));
        assert_eq!(
            idle_pipeline_with(settings, |_| false).err(),
            Some(NoRandomness),
            "a pipeline was created without randomness"
        );
    }

    #[test]
    fn a_window_becomes_batches_grouped_by_series_and_split_by_clock_state() {
        let mut collector = collector(MAX_BULK_BYTES, Seq::FIRST);
        // Arrival order interleaves the series; the clock becomes synchronized in the middle.
        let readings = [
            reading("d1", "p1", 1, false),
            reading("d2", "p1", 2, false),
            reading("d1", "p1", 3, false),
            reading("d1", "p1", 4, true),
            reading("d2", "p1", 5, true),
        ];
        let formation = collector.form(window(readings), 77);
        assert!(formation.refused.is_empty() && formation.unencodable.is_empty());
        let batches: Vec<TelemetryBatch> = formation
            .batches
            .iter()
            .map(|formed| TelemetryBatch::decode(&formed.payload).unwrap())
            .collect();
        assert_eq!(batches.len(), 2);
        assert_eq!((batches[0].seq.get(), batches[0].clock_synced), (1, false));
        assert_eq!((batches[1].seq.get(), batches[1].clock_synced), (2, true));
        let series: Vec<(String, Vec<u64>)> = batches[0]
            .series
            .iter()
            .map(|series| {
                (
                    series.device.to_string(),
                    series
                        .samples
                        .iter()
                        .map(|sample| sample.at.get())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            series,
            [("d1".to_owned(), vec![1, 3]), ("d2".to_owned(), vec![2])]
        );
        assert_eq!(formation.batches[0].meta.records, 3);
        assert_eq!(formation.batches[0].meta.oldest, UnixMillis::new(1));
        assert_eq!(formation.batches[1].meta.oldest, UnixMillis::new(4));
        assert_eq!(formation.batches[0].meta.formed_at, 77);
        // The next window continues the stream.
        let next = collector.form(window([reading("d1", "p1", 6, true)]), 78);
        assert_eq!(next.batches[0].meta.seq.get(), 3);
        assert_eq!(
            next.batches[0].meta.stream,
            formation.batches[0].meta.stream
        );
    }

    /// Codex review before the commit (the first round after the integration with
    /// Laysi/trustiot-v2#94): a clock state that changes and changes back within one window
    /// gives one batch per run of readings with the same state, in the order they came. The
    /// window used to put every reading of the first state first, so the second run that was
    /// not synchronized went into the first batch: a series' last reading went up ahead of one
    /// read before it, and that batch mixed two separate periods without synchronization.
    #[test]
    fn a_clock_state_that_changes_back_within_a_window_keeps_every_series_in_order() {
        let mut collector = collector(MAX_BULK_BYTES, Seq::FIRST);
        let readings = [
            reading("d1", "p1", 1, false),
            reading("d2", "p1", 2, false),
            reading("d1", "p1", 3, true),
            reading("d2", "p1", 4, true),
            reading("d1", "p1", 5, false),
        ];
        let formation = collector.form(window(readings), 77);
        assert!(formation.refused.is_empty() && formation.unencodable.is_empty());
        // Per batch: its sequence number, its clock state, and each series' device and times.
        type Summary = (u64, bool, Vec<(String, Vec<u64>)>);
        let batches: Vec<Summary> = formation
            .batches
            .iter()
            .map(|formed| {
                let batch = TelemetryBatch::decode(&formed.payload).unwrap();
                let series = batch
                    .series
                    .iter()
                    .map(|series| {
                        (
                            series.device.to_string(),
                            series
                                .samples
                                .iter()
                                .map(|sample| sample.at.get())
                                .collect(),
                        )
                    })
                    .collect();
                (batch.seq.get(), batch.clock_synced, series)
            })
            .collect();
        let d = |device: &str, at: &[u64]| (device.to_owned(), at.to_vec());
        assert_eq!(
            batches,
            [
                (1, false, vec![d("d1", &[1]), d("d2", &[2])]),
                (2, true, vec![d("d1", &[3]), d("d2", &[4])]),
                (3, false, vec![d("d1", &[5])]),
            ]
        );
    }

    #[test]
    fn a_used_up_stream_continues_on_a_new_one_without_losing_a_reading() {
        let mut collector = collector(4096, Seq::new(u64::MAX).unwrap());
        let first_stream = collector.builder.stream().clone();
        // Enough readings for several 4 KiB batches.
        let readings = (0..2_000).map(|at| reading("d1", "p1", 1_000_000 + at, true));
        let formation = collector.form(window(readings), 1);
        let batches: Vec<TelemetryBatch> = formation
            .batches
            .iter()
            .map(|formed| TelemetryBatch::decode(&formed.payload).unwrap())
            .collect();
        assert!(batches.len() >= 3, "{}", batches.len());
        // Every stream here has room for one batch: each batch is on a stream of its own.
        assert_eq!(formation.rotated.len(), batches.len() - 1);
        assert_eq!(batches[0].stream, first_stream);
        let streams: std::collections::HashSet<&StreamId> =
            batches.iter().map(|batch| &batch.stream).collect();
        assert_eq!(streams.len(), batches.len());
        assert!(batches.iter().all(|batch| batch.seq.get() == u64::MAX));
        let times: Vec<u64> = batches
            .iter()
            .flat_map(|batch| batch.series.iter().flat_map(|series| series.samples.iter()))
            .map(|sample| sample.at.get())
            .collect();
        assert_eq!(
            times,
            (1_000_000..1_002_000).collect::<Vec<_>>(),
            "no reading lost"
        );
    }

    #[test]
    fn a_full_window_wakes_the_pipeline_and_a_window_at_twice_its_limit_drops() {
        let shared = Arc::new(Shared {
            window: Mutex::new(Window::default()),
            early: Notify::new(),
            dropped: AtomicU64::new(0),
            limit: 10 * SAMPLE_COST,
            close_at: AtomicUsize::new(10 * SAMPLE_COST),
        });
        let readings = Readings {
            shared: Arc::clone(&shared),
        };
        let stats = Stats {
            buffer: Arc::default(),
            window: Some(Arc::clone(&shared)),
        };
        readings.push((0..5).map(|at| reading("d1", "p1", 100 + at, true)));
        assert_eq!(stats.snapshot().pending_records, 5);
        assert_eq!(stats.snapshot().oldest_pending, Some(UnixMillis::new(100)));
        readings.push((0..100).map(|at| reading("d1", "p1", 200 + at, true)));
        let snapshot = stats.snapshot();
        assert!(snapshot.dropped_records > 0);
        assert_eq!(snapshot.pending_records + snapshot.dropped_records, 105);
        assert!(lock(&shared.window).bytes < 2 * shared.limit + SAMPLE_COST + GROUP_COST + 8);
    }

    #[test]
    fn the_ack_queue_keeps_the_newest() {
        let queue = Arc::new(AckQueue::default());
        let acks = Acks {
            queue: Arc::clone(&queue),
        };
        for seq in 1..=(ACK_QUEUE as u64 + 3) {
            acks.offer(TelemetryAck {
                v: protocol::V1,
                stream: StreamId::new("s").unwrap(),
                seq: Seq::new(seq).unwrap(),
                outcome: AckOutcome::Committed { records: 1 },
                server_time: UnixMillis::new(0),
            });
        }
        let seqs: Vec<u64> = lock(&queue.queue).iter().map(|ack| ack.seq.get()).collect();
        assert_eq!(seqs, (4..=(ACK_QUEUE as u64 + 3)).collect::<Vec<_>>());
    }

    #[test]
    fn waits_are_clamped_and_backoff_doubles_to_its_cap() {
        assert_eq!(clamp_retry(0), RETRY_AFTER_MIN);
        assert_eq!(clamp_retry(5_000), Duration::from_secs(5));
        assert_eq!(clamp_retry(u32::MAX), RETRY_AFTER_MAX);
        assert_eq!(refused_backoff(1), Duration::from_secs(1));
        assert_eq!(refused_backoff(2), Duration::from_secs(2));
        assert_eq!(refused_backoff(4), Duration::from_secs(8));
        assert_eq!(refused_backoff(100), REFUSED_BACKOFF_MAX);
    }

    /// A connector that never connects: the link stays Connecting.
    struct Nowhere;

    impl crate::link::Connector for Nowhere {
        type Stream = tokio::io::DuplexStream;

        async fn connect(&self) -> Result<Self::Stream, crate::link::ConnectError> {
            std::future::pending().await
        }
    }

    /// A pipeline over a link that is never connected, not running.
    fn idle_pipeline(directory: PathBuf) -> Pipeline {
        idle_pipeline_with(Settings::new(directory), crate::link::fill_random).unwrap()
    }

    /// [`idle_pipeline`] with `settings` and the stream IDs' `randomness`.
    fn idle_pipeline_with(
        settings: Settings,
        randomness: fn(&mut [u8]) -> bool,
    ) -> Result<Pipeline, NoRandomness> {
        let link_settings = crate::link::LinkSettings::new(
            protocol::GatewayId::new("gw-1").unwrap(),
            protocol::AgentVersion::new("0.1.0-test").unwrap(),
        );
        let (link, _incoming, _supervisor) =
            crate::link::link(link_settings, Nowhere, CancellationToken::new());
        let clock = GatewayClock::new(
            crate::clock::SystemWall,
            PathBuf::from("/nonexistent/trustiot-agent-test/synchronized"),
        );
        pipeline_with(settings, link, clock, CancellationToken::new(), randomness)
            .map(|(_handles, pipeline)| pipeline)
    }

    /// Independent review of Laysi/trustiot-v2#90 (2026-10-28), finding 8: a batch waiting for
    /// a new connection must not wait forever when the connection that is up was taken for the
    /// one that failed: an acknowledgement timeout later it goes on the connection that is up.
    #[tokio::test(start_paused = true)]
    async fn a_batch_waiting_for_a_new_connection_goes_on_the_one_up_after_the_ack_timeout() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-upload-unit");
        let pipeline = idle_pipeline(scratch.path().join("buffer"));
        let now = pipeline.age.now();
        let outgoing = {
            let mut core = lock(&pipeline.core);
            let mut events = Events::default();
            let mut formation = core
                .collector
                .form(window([reading("d1", "p1", 1, true)]), now);
            core.buffer
                .push(formation.batches.remove(0), now, &mut events);
            core.buffer.front().unwrap()
        };
        let batch = InFlight {
            id: outgoing.id,
            stream: outgoing.stream.clone(),
            seq: outgoing.seq,
            formed_at: outgoing.formed_at,
            records: outgoing.records,
            published: Arc::new(Mutex::new(None)),
            refusals: 0,
        };
        let session = SessionId::new("session-up").unwrap();
        let up = LinkState::Connected {
            session: session.clone(),
        };
        let mut reporter = Reporter::default();
        // The connection that is up was recorded as the one that failed.
        let waiting = pipeline.link_changed(
            Upload::Reconnecting {
                batch,
                failed: Some(session),
                until: Instant::now() + ACK_TIMEOUT,
            },
            &up,
        );
        assert!(matches!(waiting, Upload::Reconnecting { .. }));
        assert_eq!(
            waiting.deadline(),
            Some(Instant::now() + ACK_TIMEOUT),
            "waiting for another connection has a deadline"
        );
        tokio::time::advance(ACK_TIMEOUT).await;
        let sending = pipeline.deadline_passed(waiting, &up, &mut reporter);
        assert!(
            matches!(sending, Upload::Sending { .. }),
            "sent on the connection that is up: {sending:?}"
        );
        // Without any connection it waits for another timeout instead.
        let batch = sending.into_batch().unwrap();
        let waiting = pipeline.deadline_passed(
            Upload::Reconnecting {
                batch,
                failed: None,
                until: Instant::now(),
            },
            &LinkState::Connecting,
            &mut reporter,
        );
        assert_eq!(waiting.deadline(), Some(Instant::now() + ACK_TIMEOUT));
    }

    /// Log lines written while a test runs.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The sum of `readings=` over the captured lines that contain `message`.
    fn readings_logged(captured: &Captured, message: &str) -> u64 {
        String::from_utf8(lock(&captured.0).clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains(message))
            .map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("readings="))
                    .unwrap_or_else(|| panic!("no readings in {line}"))
                    .parse::<u64>()
                    .unwrap()
            })
            .sum()
    }

    /// Finding 9: every reading lost because the cloud rejected its batch, the batch builder
    /// refused it or its batch did not encode is in some log line's `readings`, however many
    /// come within a minute: the totals since the last line, and the rest at the stop.
    #[tokio::test(start_paused = true)]
    async fn every_lost_reading_is_in_some_log_line() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut reporter = Reporter::default();
        for readings in [10, 20, 30] {
            reporter.rejected(readings, || "s/1: invalid: test".to_owned());
        }
        for readings in [1, 2] {
            reporter.refused_readings(readings, || "too large".to_owned());
        }
        for readings in [5, 6] {
            reporter.unencodable(readings, || "s/2: test".to_owned());
        }
        tokio::time::advance(LOG_EVERY).await;
        reporter.rejected(40, || "s/9: invalid: test".to_owned());
        // The stop logs whatever is left.
        reporter.log_drops(true);
        assert_eq!(readings_logged(&captured, "the cloud rejected"), 100);
        assert_eq!(
            readings_logged(&captured, "readings that cannot be sent"),
            3
        );
        assert_eq!(readings_logged(&captured, "do not encode"), 11);
    }
}
