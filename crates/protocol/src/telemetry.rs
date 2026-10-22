//! Telemetry batches and their end-to-end acknowledgement (decision 8; checklist G1, I1).
//!
//! A gateway numbers its batches per stream: 1, 2, 3, … Sequence numbers are assigned when a
//! batch is formed and are never reused within a stream. The gateway sends one batch of a
//! stream at a time and sends the next only after the cloud acknowledges the previous one with
//! [`AckOutcome::Committed`], [`AckOutcome::Duplicate`] or [`AckOutcome::Rejected`]. The cloud
//! acknowledges only after the whole batch is durable, and treats a batch whose sequence number
//! is not above the stream's committed high-water mark as a duplicate. Both rules together make
//! resending always safe; sending a stream's batches out of order would make the cloud drop data
//! as duplicates.
//!
//! The cloud keys streams by the gateway in the topic *and* the stream ID: a gateway can name
//! any stream ID, so a stream ID alone must never select a high-water mark.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use serde::de::{self, DeserializeOwned, IgnoredAny, SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::codec::sealed::Token;
use crate::codec::{
    self, DecodeError, EncodeError, Invalid, IssueCode, Issues, MAX_ISSUE_MESSAGE_BYTES, Message,
    V1, encoded_len, head_len,
};
use crate::ids::{DeviceId, PointKey, Seq, StreamId, UnixMillis};
use crate::topic::{Channel, MAX_BULK_BYTES};
use crate::value::{Value, ValueError};

/// `up/telemetry`: readings from one stream, grouped by data point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryBatch {
    /// Wire format major version.
    pub v: V1,
    /// The stream the batch belongs to.
    pub stream: StreamId,
    /// The batch's number within the stream.
    pub seq: Seq,
    /// Whether the gateway's clock was synchronized when every sample in the batch was taken.
    /// A batch never mixes samples taken before and after the clock state changed, so the
    /// cloud can flag or correct the timestamps of a whole batch (Laysi/trustiot#180).
    pub clock_synced: bool,
    /// The readings, one entry per data point, each data point at most once.
    #[serde(deserialize_with = "codec::seq")]
    pub series: Vec<Series>,
}

/// The samples of one data point of one device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Series {
    /// The device, as listed in the desired configuration.
    pub device: DeviceId,
    /// The data point, as listed in the device's model.
    pub point: PointKey,
    /// The samples in the order they were taken. Never empty.
    #[serde(deserialize_with = "codec::seq")]
    pub samples: Vec<Sample>,
}

/// One reading's time and value. Encoded as the two-element array `[at, value]`; a decoder
/// ignores further elements, which a newer revision may append.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// When the device was read, from the gateway's clock, in milliseconds.
    pub at: UnixMillis,
    /// The value after decoding and scaling.
    pub value: Value,
}

impl Serialize for Sample {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&self.at)?;
        tuple.serialize_element(&self.value)?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for Sample {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SampleVisitor;

        impl<'de> Visitor<'de> for SampleVisitor {
            type Value = Sample;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array [timestamp, value]")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Sample, A::Error> {
                let at = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let value = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Sample { at, value })
            }
        }

        deserializer.deserialize_seq(SampleVisitor)
    }
}

impl Message for TelemetryBatch {
    const CHANNEL: Channel = Channel::Telemetry;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.series.is_empty() {
            issues.push(
                IssueCode::Empty,
                "series",
                "a batch holds at least one series",
            );
        }
        validate_series(&self.series, "series", &mut issues);
        issues.finish()
    }
}

/// Checks that every data point has one series, every series has samples and every value can
/// be sent.
pub(crate) fn validate_series(series: &[Series], path: &str, issues: &mut Issues) {
    let mut seen = HashMap::with_capacity(series.len());
    for (index, entry) in series.iter().enumerate() {
        if let Some(first) = seen.insert((&entry.device, &entry.point), index) {
            issues.push(
                IssueCode::Duplicate,
                format!("{path}[{index}]"),
                format!(
                    "data point {} of device {} is also {path}[{first}]",
                    entry.point, entry.device
                ),
            );
        }
        if entry.samples.is_empty() {
            issues.push(
                IssueCode::Empty,
                format!("{path}[{index}].samples"),
                "a series holds at least one sample",
            );
        }
        for (sample_index, sample) in entry.samples.iter().enumerate() {
            if let Err(error) = sample.value.validate() {
                issues.push(
                    IssueCode::InvalidValue,
                    format!("{path}[{index}].samples[{sample_index}]"),
                    error.to_string(),
                );
            }
        }
    }
}

impl TelemetryBatch {
    /// Encodes the batch; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a batch; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }

    /// The number of samples in the batch.
    pub fn record_count(&self) -> usize {
        self.series.iter().map(|series| series.samples.len()).sum()
    }

    /// Reads only the stream and sequence number of a payload that [`Self::decode`] refused, so
    /// the cloud can still answer it with [`AckOutcome::Rejected`] instead of leaving the
    /// gateway to resend it forever. Fails only when the payload is larger than
    /// [`codec::MAX_PEEK_BYTES`] or is not a CBOR map with those two fields.
    pub fn peek_header(payload: &[u8]) -> Result<BatchHeader, DecodeError> {
        codec::peek(payload)
    }
}

/// The identifying fields of a telemetry payload.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BatchHeader {
    /// The stream.
    pub stream: StreamId,
    /// The sequence number.
    pub seq: Seq,
}

/// `down/telemetry-ack`: the cloud's answer to one batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryAck {
    /// Wire format major version.
    pub v: V1,
    /// The acknowledged batch's stream.
    pub stream: StreamId,
    /// The acknowledged batch's sequence number.
    pub seq: Seq,
    /// What happened to the batch.
    pub outcome: AckOutcome,
    /// The cloud's clock when it sent the acknowledgement. Informational; a gateway may use it
    /// to notice that its own clock is wrong.
    pub server_time: UnixMillis,
}

/// What the cloud did with a batch. Encoded as a map whose `status` names the variant;
/// [`crate::decode`] reads the status first and then only the fields of that variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AckOutcome {
    /// Every record of the batch is durable. The gateway drops the batch from its buffer.
    Committed {
        /// The number of records stored; equal to the batch's record count.
        records: u32,
    },
    /// The batch was committed before (its sequence number is not above the stream's
    /// high-water mark). The gateway drops the batch from its buffer.
    Duplicate {
        /// The number of records stored when the batch was first committed.
        records: u32,
    },
    /// The batch can never be accepted, for example because it is malformed. The gateway drops
    /// it and logs the reason; resending would block the stream forever.
    Rejected {
        /// Why.
        code: IssueCode,
        /// A diagnostic, at most [`MAX_ISSUE_MESSAGE_BYTES`], without U+0000. A diagnostic
        /// derived from the refused payload may quote it; take the message of an
        /// [`crate::Issue`] (from [`DecodeError::to_issues`]), which [`crate::Issue::new`]
        /// already cleaned and cut to size.
        message: String,
    },
    /// The cloud cannot store the batch now. The gateway resends the same batch after the
    /// given delay.
    Retry {
        /// How long to wait before resending.
        after_ms: u32,
    },
    /// A status this build does not know. The gateway keeps the batch and resends it after its
    /// usual acknowledgement timeout. Never sent.
    #[serde(other)]
    Unknown,
}

impl AckOutcome {
    /// Whether the gateway may drop the batch from its buffer.
    pub fn releases_batch(&self) -> bool {
        matches!(
            self,
            Self::Committed { .. } | Self::Duplicate { .. } | Self::Rejected { .. }
        )
    }
}

impl Message for TelemetryAck {
    const CHANNEL: Channel = Channel::TelemetryAck;

    fn validate(&self) -> Result<(), Invalid> {
        match &self.outcome {
            AckOutcome::Rejected { message, .. } if message.len() > MAX_ISSUE_MESSAGE_BYTES => {
                Err(Invalid::single(
                    IssueCode::TooLong,
                    "outcome.message",
                    format!("longer than {MAX_ISSUE_MESSAGE_BYTES} bytes"),
                ))
            }
            AckOutcome::Rejected { message, .. } if codec::has_nul(message) => {
                Err(Invalid::single(
                    IssueCode::InvalidValue,
                    "outcome.message",
                    "contains U+0000",
                ))
            }
            _ => Ok(()),
        }
    }

    fn validate_outgoing(&self) -> Result<(), Invalid> {
        if self.outcome == AckOutcome::Unknown {
            return Err(Invalid::single(
                IssueCode::InvalidValue,
                "outcome.status",
                "an unknown status cannot be sent",
            ));
        }
        Ok(())
    }

    /// Reads `outcome.status` first, then decodes the payload with the fields of the variant
    /// it names, so that no entry of the outcome map is buffered (Laysi/trustiot-v2#290).
    /// Both passes skip unknown keys without building anything; each refuses a repeated
    /// field, as a single pass would.
    fn decode_item(payload: &[u8], _: Token) -> Result<Self, DecodeError> {
        /// The variant names of [`AckOutcome`], read like serde reads them for the derived
        /// enum: an unknown name is [`AckOutcome::Unknown`].
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum AckStatus {
            Committed,
            Duplicate,
            Rejected,
            Retry,
            #[serde(other)]
            Unknown,
        }

        #[derive(Deserialize)]
        struct Probe {
            outcome: OutcomeProbe,
        }

        #[derive(Deserialize)]
        struct OutcomeProbe {
            status: AckStatus,
        }

        let Probe {
            outcome: OutcomeProbe { status },
        } = codec::from_slice(payload)?;
        match status {
            AckStatus::Committed => AckWire::decode(payload, |Records { records }| {
                AckOutcome::Committed { records }
            }),
            AckStatus::Duplicate => AckWire::decode(payload, |Records { records }| {
                AckOutcome::Duplicate { records }
            }),
            AckStatus::Rejected => AckWire::decode(payload, |Rejected { code, message }| {
                AckOutcome::Rejected { code, message }
            }),
            AckStatus::Retry => {
                AckWire::decode(payload, |Retry { after_ms }| AckOutcome::Retry { after_ms })
            }
            AckStatus::Unknown => AckWire::decode(payload, |IgnoredAny| AckOutcome::Unknown),
        }
    }
}

/// A [`TelemetryAck`] whose outcome holds the fields `F` of one [`AckOutcome`] variant: the
/// second decoding pass, once the status is known. [`AckWire::decode`] takes it apart and
/// builds the [`TelemetryAck`] field by field, so a field added to one and not the other does
/// not compile.
#[derive(Deserialize)]
struct AckWire<F> {
    v: V1,
    stream: StreamId,
    seq: Seq,
    outcome: F,
    server_time: UnixMillis,
}

impl<F: DeserializeOwned> AckWire<F> {
    fn decode(
        payload: &[u8],
        outcome: impl FnOnce(F) -> AckOutcome,
    ) -> Result<TelemetryAck, DecodeError> {
        let Self {
            v,
            stream,
            seq,
            outcome: fields,
            server_time,
        } = codec::from_slice(payload)?;
        Ok(TelemetryAck {
            v,
            stream,
            seq,
            outcome: outcome(fields),
            server_time,
        })
    }
}

/// The fields of [`AckOutcome::Committed`] and [`AckOutcome::Duplicate`].
#[derive(Deserialize)]
struct Records {
    records: u32,
}

/// The fields of [`AckOutcome::Rejected`].
#[derive(Deserialize)]
struct Rejected {
    code: IssueCode,
    message: String,
}

/// The fields of [`AckOutcome::Retry`].
#[derive(Deserialize)]
struct Retry {
    after_ms: u32,
}

/// The longest a gateway keeps an unacknowledged batch, in days (checklist G1, decided in
/// Laysi/trustiot-v2#92, reasoning in docs/architecture/data-retention.md). Past it the gateway
/// drops the batch and counts the dropped readings in its heartbeat.
///
/// The cloud's raw data retention must be longer (the migration Job refuses a shorter setting):
/// a stream's committed high-water mark lives in the batch completion records, which expire with
/// the raw data, so a batch the cloud committed but whose acknowledgement was lost must come back
/// before its completion record expires, or it would be written twice.
pub const BUFFER_MAX_AGE_DAYS: u32 = 30;

/// [`BUFFER_MAX_AGE_DAYS`] as a duration.
pub const BUFFER_MAX_AGE: Duration = Duration::from_secs(BUFFER_MAX_AGE_DAYS as u64 * 86_400);

/// The smallest batch size limit a [`BatchBuilder`] accepts. Every valid reading fits in an
/// otherwise empty batch of this size, so with a valid limit [`PushError::TooLarge`] cannot
/// occur.
pub const MIN_BATCH_LIMIT: usize = 4 * 1024;

/// One buffered reading, as the agent hands it to a [`BatchBuilder`].
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// The device.
    pub device: DeviceId,
    /// The data point.
    pub point: PointKey,
    /// The time and value.
    pub sample: Sample,
    /// Whether the clock was synchronized when the sample was taken.
    pub clock_synced: bool,
}

/// Why a [`BatchBuilder`] refused a reading. The builder is unchanged after an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The value cannot be sent.
    InvalidValue(ValueError),
    /// Even a batch holding only this reading would exceed the limit.
    TooLarge {
        /// The size of that batch.
        size: usize,
        /// The builder's limit.
        limit: usize,
    },
    /// The stream has used every sequence number; start a new stream.
    SeqExhausted,
    /// The encoder failed on a value that passed validation. This is a bug in this crate.
    Encode(EncodeError),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue(error) => write!(f, "reading cannot be sent: {error}"),
            Self::TooLarge { size, limit } => write!(
                f,
                "a batch holding only this reading is {size} bytes; the limit is {limit} bytes"
            ),
            Self::SeqExhausted => f.write_str("the stream has no sequence numbers left"),
            Self::Encode(error) => write!(f, "cannot measure the reading: {error}"),
        }
    }
}

impl std::error::Error for PushError {}

/// Why a batch size limit is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitError {
    /// The rejected limit.
    pub limit: usize,
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "batch limit {} is outside {MIN_BATCH_LIMIT}..={MAX_BULK_BYTES} bytes",
            self.limit
        )
    }
}

impl std::error::Error for LimitError {}

/// Forms numbered batches that never exceed a size limit (rebuild checklist G1: split by the
/// limit, never crash).
///
/// Readings go in one at a time. When a reading does not fit in the open batch, or its clock
/// state differs from the open batch's, the open batch is closed and returned, and the reading
/// starts the next batch. Batches therefore come out numbered consecutively from the first
/// sequence number, each encoding to at most the limit: splitting an oversized set of readings
/// is just pushing them all. The size accounting is exact, so batches are filled to the limit.
///
/// Sequence numbers are assigned when a batch is opened. A returned batch must be stored (in
/// memory or on disk) before its sequence number is considered used; see the crate README for
/// the rule that a stream never reuses a sequence number.
#[derive(Clone, Debug)]
pub struct BatchBuilder {
    stream: StreamId,
    /// The sequence number of the next batch to open; `None` once `u64` is used up.
    next_seq: Option<Seq>,
    limit: usize,
    open: Option<OpenBatch>,
}

#[derive(Clone, Debug)]
struct OpenBatch {
    seq: Seq,
    clock_synced: bool,
    series: Vec<Series>,
    /// Where each device's data points are in `series`.
    index: HashMap<DeviceId, HashMap<PointKey, usize>>,
    /// The exact encoded size of the batch as it stands.
    size: usize,
}

impl BatchBuilder {
    /// A builder whose first batch gets `first_seq` and whose batches stay within the telemetry
    /// channel's limit.
    pub fn new(stream: StreamId, first_seq: Seq) -> Self {
        Self {
            stream,
            next_seq: Some(first_seq),
            limit: MAX_BULK_BYTES,
            open: None,
        }
    }

    /// A builder with a smaller limit, for example to lose less on a flaky uplink.
    pub fn with_limit(stream: StreamId, first_seq: Seq, limit: usize) -> Result<Self, LimitError> {
        if !(MIN_BATCH_LIMIT..=MAX_BULK_BYTES).contains(&limit) {
            return Err(LimitError { limit });
        }
        Ok(Self {
            limit,
            ..Self::new(stream, first_seq)
        })
    }

    /// The stream.
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// The size limit.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The sequence number the next returned batch will carry: the open batch's, or the next
    /// one when no batch is open. `None` when the stream is used up.
    pub fn next_seq(&self) -> Option<Seq> {
        self.open.as_ref().map(|open| open.seq).or(self.next_seq)
    }

    /// The exact encoded size of the open batch, if any.
    pub fn open_len(&self) -> Option<usize> {
        self.open.as_ref().map(|open| open.size)
    }

    /// The number of readings in the open batch.
    pub fn open_records(&self) -> usize {
        self.open.as_ref().map_or(0, |open| {
            open.series.iter().map(|series| series.samples.len()).sum()
        })
    }

    /// Adds a reading. Returns the batch that adding it closed, if any.
    pub fn push(&mut self, reading: Reading) -> Result<Option<TelemetryBatch>, PushError> {
        reading
            .sample
            .value
            .validate()
            .map_err(PushError::InvalidValue)?;
        let sample_len = encoded_len(&reading.sample).map_err(PushError::Encode)?;

        if let Some(open) = &mut self.open
            && open.clock_synced == reading.clock_synced
        {
            let growth = match open.position(&reading) {
                Some(index) => {
                    let count = open.series[index].samples.len();
                    sample_len + head_len(count + 1) - head_len(count)
                }
                None => {
                    let count = open.series.len();
                    series_head_len(&reading)? + head_len(1) + sample_len + head_len(count + 1)
                        - head_len(count)
                }
            };
            if open.size + growth <= self.limit {
                open.size += growth;
                open.insert(reading);
                return Ok(None);
            }
        }

        // The reading starts a new batch. Work out its number and size before closing the open
        // batch, so that an error leaves the builder unchanged.
        let seq = self.next_seq.ok_or(PushError::SeqExhausted)?;
        // Batch fields, a one-entry series array, the series' fields, a one-sample array, the
        // sample.
        let size = batch_head_len(&self.stream, seq, reading.clock_synced)?
            + head_len(1)
            + series_head_len(&reading)?
            + head_len(1)
            + sample_len;
        if size > self.limit {
            return Err(PushError::TooLarge {
                size,
                limit: self.limit,
            });
        }
        let closed = self.finish();
        self.next_seq = seq.next();
        let mut open = OpenBatch {
            seq,
            clock_synced: reading.clock_synced,
            series: Vec::new(),
            index: HashMap::new(),
            size,
        };
        open.insert(reading);
        self.open = Some(open);
        Ok(closed)
    }

    /// Closes and returns the open batch, if any; for example when the upload interval ends.
    pub fn finish(&mut self) -> Option<TelemetryBatch> {
        let open = self.open.take()?;
        Some(TelemetryBatch {
            v: V1,
            stream: self.stream.clone(),
            seq: open.seq,
            clock_synced: open.clock_synced,
            series: open.series,
        })
    }
}

impl OpenBatch {
    fn position(&self, reading: &Reading) -> Option<usize> {
        self.index
            .get(&reading.device)
            .and_then(|points| points.get(&reading.point))
            .copied()
    }

    fn insert(&mut self, reading: Reading) {
        match self.position(&reading) {
            Some(index) => self.series[index].samples.push(reading.sample),
            None => {
                self.index
                    .entry(reading.device.clone())
                    .or_default()
                    .insert(reading.point.clone(), self.series.len());
                self.series.push(Series {
                    device: reading.device,
                    point: reading.point,
                    samples: vec![reading.sample],
                });
            }
        }
    }
}

/// The encoded size of the reading's series without the samples array: everything a new series
/// adds before its first sample.
fn series_head_len(reading: &Reading) -> Result<usize, PushError> {
    let series = Series {
        device: reading.device.clone(),
        point: reading.point.clone(),
        samples: Vec::new(),
    };
    let len = encoded_len(&series).map_err(PushError::Encode)?;
    Ok(len - head_len(0))
}

/// The encoded size of a batch without the series array.
fn batch_head_len(stream: &StreamId, seq: Seq, clock_synced: bool) -> Result<usize, PushError> {
    let empty = TelemetryBatch {
        v: V1,
        stream: stream.clone(),
        seq,
        clock_synced,
        series: Vec::new(),
    };
    let len = encoded_len(&empty).map_err(PushError::Encode)?;
    Ok(len - head_len(0))
}
