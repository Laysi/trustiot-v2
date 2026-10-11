//! Gateway logs (decision 22): warnings and errors uploaded continuously, and full logs of a
//! time range uploaded on request.
//!
//! The agent keeps its full log in a memory ring buffer and never writes it to the SD card. It
//! publishes `warn` and `error` entries to `up/log` within a rate limit, counting what the limit
//! drops. A [`LogRequest`] on `down/log-request` asks for every entry of a time range; the agent
//! answers with one or more [`LogDump`] parts on `up/log-dump` and says whether its buffer still
//! covered the whole range. Entries must not contain personal data (Laysi/trustiot#110).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::codec::{
    self, DecodeError, EncodeError, Invalid, IssueCode, Issues, Message, V1, encoded_len, has_nul,
    head_len, replace_nul, truncate,
};
use crate::ids::{RequestId, UnixMillis};
use crate::open_enum::open_enum;
use crate::topic::Channel;

/// Longest [`LogEntry::target`], in bytes.
pub const MAX_TARGET_BYTES: usize = 128;
/// Longest [`LogEntry::message`], in bytes.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// Most [`LogEntry::fields`].
pub const MAX_FIELDS: usize = 32;
/// Longest field name, in bytes.
pub const MAX_FIELD_NAME_BYTES: usize = 64;
/// Longest field value, in bytes.
pub const MAX_FIELD_VALUE_BYTES: usize = 1024;

/// A log level, as `tracing` defines them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// An error.
    Error,
    /// A warning.
    Warn,
    /// Information.
    Info,
    /// Debugging detail.
    Debug,
    /// Tracing detail.
    Trace,
}

impl LogLevel {
    /// Higher is more severe: `error` is 5 and `trace` is 1.
    pub const fn severity(self) -> u8 {
        match self {
            Self::Error => 5,
            Self::Warn => 4,
            Self::Info => 3,
            Self::Debug => 2,
            Self::Trace => 1,
        }
    }

    /// Whether an entry of `level` passes a `self` minimum.
    pub const fn admits(self, level: Self) -> bool {
        level.severity() >= self.severity()
    }
}

/// One log entry. No text in it may contain U+0000 ([`LogEntry::new`] and
/// [`LogEntry::with_field`] replace it); other control characters, such as line breaks, are
/// kept, and whoever displays the entry escapes them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// When it was logged, from the gateway's clock.
    pub at: UnixMillis,
    /// Its level.
    pub level: LogLevel,
    /// The module that logged it, at most [`MAX_TARGET_BYTES`].
    pub target: String,
    /// The message, at most [`MAX_MESSAGE_BYTES`].
    pub message: String,
    /// Structured fields, at most [`MAX_FIELDS`]. A payload that repeats a name is refused.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "codec::unique_map"
    )]
    pub fields: BTreeMap<String, String>,
}

impl LogEntry {
    /// An entry without fields, replacing U+0000 with U+FFFD and cutting `target` and
    /// `message` to their limits.
    pub fn new(
        at: UnixMillis,
        level: LogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            at,
            level,
            target: truncate(replace_nul(target.into()), MAX_TARGET_BYTES),
            message: truncate(replace_nul(message.into()), MAX_MESSAGE_BYTES),
            fields: BTreeMap::new(),
        }
    }

    /// Adds a field, replacing U+0000 with U+FFFD and cutting its name and value to their
    /// limits; ignored once the entry has [`MAX_FIELDS`] fields.
    #[must_use]
    pub fn with_field(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if self.fields.len() < MAX_FIELDS {
            self.fields.insert(
                truncate(replace_nul(name.into()), MAX_FIELD_NAME_BYTES),
                truncate(replace_nul(value.into()), MAX_FIELD_VALUE_BYTES),
            );
        }
        self
    }

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.target.len() > MAX_TARGET_BYTES {
            issues.push(
                IssueCode::TooLong,
                format!("{path}.target"),
                format!("longer than {MAX_TARGET_BYTES} bytes"),
            );
        }
        if self.message.len() > MAX_MESSAGE_BYTES {
            issues.push(
                IssueCode::TooLong,
                format!("{path}.message"),
                format!("longer than {MAX_MESSAGE_BYTES} bytes"),
            );
        }
        for (field, text) in [("target", &self.target), ("message", &self.message)] {
            if has_nul(text) {
                issues.push(
                    IssueCode::InvalidValue,
                    format!("{path}.{field}"),
                    "contains U+0000",
                );
            }
        }
        if self.fields.len() > MAX_FIELDS {
            issues.push(
                IssueCode::TooMany,
                format!("{path}.fields"),
                format!("at most {MAX_FIELDS} fields"),
            );
        }
        for (name, value) in &self.fields {
            if name.is_empty() {
                issues.push(
                    IssueCode::Empty,
                    format!("{path}.fields"),
                    "a field name is empty",
                );
            }
            if name.len() > MAX_FIELD_NAME_BYTES {
                issues.push(
                    IssueCode::TooLong,
                    format!("{path}.fields"),
                    format!("field names are at most {MAX_FIELD_NAME_BYTES} bytes"),
                );
            }
            if value.len() > MAX_FIELD_VALUE_BYTES {
                issues.push(
                    IssueCode::TooLong,
                    format!("{path}.fields"),
                    format!("field values are at most {MAX_FIELD_VALUE_BYTES} bytes"),
                );
            }
            if has_nul(name) || has_nul(value) {
                issues.push(
                    IssueCode::InvalidValue,
                    format!("{path}.fields"),
                    "a field name or value contains U+0000",
                );
            }
        }
    }
}

fn check_entries(entries: &[LogEntry], issues: &mut Issues) {
    for (index, entry) in entries.iter().enumerate() {
        entry.check(&format!("entries[{index}]"), issues);
    }
}

/// Splits `entries` into consecutive groups that fit a message whose encoding without entries
/// (and without the entries array head) is `fixed_len`.
fn pack(
    entries: Vec<LogEntry>,
    fixed_len: usize,
    limit: usize,
) -> Result<Vec<Vec<LogEntry>>, EncodeError> {
    let mut groups = Vec::new();
    let mut group: Vec<LogEntry> = Vec::new();
    let mut entries_len = 0;
    for entry in entries {
        let len = encoded_len(&entry)?;
        let alone = fixed_len + head_len(1) + len;
        if alone > limit {
            return Err(EncodeError::TooLarge { size: alone, limit });
        }
        let grown = fixed_len + head_len(group.len() + 1) + entries_len + len;
        if grown > limit {
            groups.push(std::mem::take(&mut group));
            entries_len = 0;
        }
        entries_len += len;
        group.push(entry);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    Ok(groups)
}

/// `up/log`: entries at `warn` or above, uploaded as they are logged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogBatch {
    /// Wire format major version.
    pub v: V1,
    /// The entries, oldest first.
    #[serde(deserialize_with = "codec::seq")]
    pub entries: Vec<LogEntry>,
    /// Entries the rate limit dropped since the previous batch.
    #[serde(default)]
    pub dropped: u64,
}

impl LogBatch {
    /// Splits entries into batches within the channel's size limit. `dropped` goes into the
    /// first batch. With no entries, returns one batch only if something was dropped.
    pub fn split(entries: Vec<LogEntry>, dropped: u64) -> Result<Vec<Self>, EncodeError> {
        if entries.is_empty() {
            return Ok(if dropped > 0 {
                vec![Self {
                    v: V1,
                    entries,
                    dropped,
                }]
            } else {
                Vec::new()
            });
        }
        let empty = Self {
            v: V1,
            entries: Vec::new(),
            dropped,
        };
        let fixed_len = encoded_len(&empty)? - head_len(0);
        let groups = pack(entries, fixed_len, Self::CHANNEL.max_payload_bytes())?;
        Ok(groups
            .into_iter()
            .enumerate()
            .map(|(index, entries)| Self {
                v: V1,
                entries,
                dropped: if index == 0 { dropped } else { 0 },
            })
            .collect())
    }

    /// Encodes the batch; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a batch; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for LogBatch {
    const CHANNEL: Channel = Channel::Log;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.entries.is_empty() && self.dropped == 0 {
            issues.push(
                IssueCode::Empty,
                "entries",
                "a batch carries entries or a drop count",
            );
        }
        check_entries(&self.entries, &mut issues);
        issues.finish()
    }
}

/// `down/log-request`: asks for every entry of a time range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRequest {
    /// Wire format major version.
    pub v: V1,
    /// Echoed in every [`LogDump`] part.
    pub request: RequestId,
    /// The range's start, inclusive, on the gateway's clock.
    pub from: UnixMillis,
    /// The range's end, inclusive, on the gateway's clock.
    pub to: UnixMillis,
    /// The least severe level to include.
    pub min_level: LogLevel,
}

impl LogRequest {
    /// Encodes the request; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a request; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for LogRequest {
    const CHANNEL: Channel = Channel::LogRequest;

    fn validate(&self) -> Result<(), Invalid> {
        if self.from > self.to {
            return Err(Invalid::single(
                IssueCode::InvalidRange,
                "to",
                "the range ends before it starts",
            ));
        }
        Ok(())
    }
}

open_enum! {
    /// How much of a requested range the ring buffer still held.
    pub enum LogCoverage {
        /// The buffer held the whole range.
        Complete => "complete",
        /// The range starts before the oldest buffered entry; only the rest is included.
        Partial => "partial",
        /// The range ends before the oldest buffered entry; nothing is included.
        Unavailable => "unavailable",
    }
}

/// `up/log-dump`: one part of the answer to a [`LogRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogDump {
    /// Wire format major version.
    pub v: V1,
    /// The request this answers.
    pub request: RequestId,
    /// The part's index, from 0.
    pub part: u32,
    /// Whether this is the final part.
    pub last: bool,
    /// How much of the range the buffer held; the same in every part.
    pub coverage: LogCoverage,
    /// The time of the oldest entry in the buffer when the request was served; `None` when the
    /// buffer was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_from: Option<UnixMillis>,
    /// The entries, oldest first.
    #[serde(deserialize_with = "codec::seq")]
    pub entries: Vec<LogEntry>,
}

impl LogDump {
    /// Splits the answer to `request` into parts within the channel's size limit. Always
    /// returns at least one part, so the cloud learns the coverage even when nothing matched.
    pub fn split(
        request: RequestId,
        coverage: LogCoverage,
        available_from: Option<UnixMillis>,
        entries: Vec<LogEntry>,
    ) -> Result<Vec<Self>, EncodeError> {
        let part = |part, last, entries| Self {
            v: V1,
            request: request.clone(),
            part,
            last,
            coverage: coverage.clone(),
            available_from,
            entries,
        };
        // Measure with the widest part number, so every part fits whatever its index.
        let fixed_len = encoded_len(&part(u32::MAX, false, Vec::new()))? - head_len(0);
        let mut groups = pack(entries, fixed_len, Self::CHANNEL.max_payload_bytes())?;
        if groups.is_empty() {
            groups.push(Vec::new());
        }
        let count = groups.len();
        groups
            .into_iter()
            .enumerate()
            .map(|(index, entries)| {
                let number = u32::try_from(index)
                    .map_err(|_| EncodeError::Serialize("more than u32::MAX parts".to_owned()))?;
                Ok(part(number, index + 1 == count, entries))
            })
            .collect()
    }

    /// Encodes the part; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a part; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for LogDump {
    const CHANNEL: Channel = Channel::LogDump;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        check_entries(&self.entries, &mut issues);
        issues.finish()
    }
}
