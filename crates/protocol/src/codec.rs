//! CBOR encoding, payload limits, versioning and validation shared by every message.
//!
//! Every payload is one CBOR data item (RFC 8949): a map whose keys are field names. Decoding
//! is strict about framing and lenient about content a newer peer may add:
//!
//! - rejected: an empty or oversized payload, malformed CBOR, bytes after the item, nesting
//!   deeper than [`MAX_NESTING`], a missing or wrongly typed field, an invalid identifier, a
//!   field that appears twice in one message map, a major version other than
//!   [`PROTOCOL_VERSION`], or a `rev` greater than [`PROTOCOL_REVISION`];
//! - accepted: map keys this build does not know (they are skipped), and absent optional
//!   fields (they take their default).

use std::convert::Infallible;
use std::fmt;

use cbor4ii::core::dec::{Read, Reference};
use cbor4ii::core::enc::Write;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::open_enum::open_enum;
use crate::topic::Channel;
use crate::{PROTOCOL_REVISION, PROTOCOL_VERSION};

/// Deepest nesting of CBOR arrays and maps a decoder accepts. The deepest message in this
/// version nests 6 levels; the margin leaves room for fields a newer peer adds while keeping a
/// hostile payload from exhausting the stack.
pub const MAX_NESTING: usize = 32;

/// Longest [`Issue::path`], in bytes.
pub const MAX_ISSUE_PATH_BYTES: usize = 256;
/// Longest [`Issue::message`], in bytes.
pub const MAX_ISSUE_MESSAGE_BYTES: usize = 1024;

/// The `v` field every payload starts with: the major version of the wire format. It is always
/// 1 in this crate; a receiver rejects any other value with
/// [`DecodeError::UnsupportedVersion`] before decoding the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct V1;

impl Serialize for V1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for V1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = u64::deserialize(deserializer)?;
        if version == u64::from(PROTOCOL_VERSION) {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported protocol version {version}; this build implements {PROTOCOL_VERSION}"
            )))
        }
    }
}

/// A payload type and the channel it travels on.
pub trait Message: Serialize + DeserializeOwned {
    /// The channel, which fixes the topic, the delivery rules and the size limit.
    const CHANNEL: Channel;

    /// Checks the invariants that the field types cannot express. [`encode`] refuses to send a
    /// message that fails, and [`decode`] refuses to accept one.
    fn validate(&self) -> Result<(), Invalid> {
        Ok(())
    }

    /// Checks what only a sender must ensure, such as never sending an `Unknown` placeholder
    /// that a receiver uses for content from a newer peer. Only [`encode`] calls it.
    fn validate_outgoing(&self) -> Result<(), Invalid> {
        Ok(())
    }
}

/// Validates `message` and encodes it as CBOR within its channel's size limit.
pub fn encode<M: Message>(message: &M) -> Result<Vec<u8>, EncodeError> {
    message.validate().map_err(EncodeError::Invalid)?;
    message.validate_outgoing().map_err(EncodeError::Invalid)?;
    let payload = to_vec(message)?;
    let limit = M::CHANNEL.max_payload_bytes();
    if payload.len() > limit {
        return Err(EncodeError::TooLarge {
            size: payload.len(),
            limit,
        });
    }
    Ok(payload)
}

/// Decodes and validates a payload received on `M`'s channel.
///
/// An empty payload is [`DecodeError::Empty`]: on a retained topic it is how MQTT deletes the
/// retained message, and the caller decides what that means.
pub fn decode<M: Message>(payload: &[u8]) -> Result<M, DecodeError> {
    let limit = M::CHANNEL.max_payload_bytes();
    if payload.is_empty() {
        return Err(DecodeError::Empty);
    }
    if payload.len() > limit {
        return Err(DecodeError::TooLarge {
            size: payload.len(),
            limit,
        });
    }
    check_version(payload)?;
    let message: M = from_slice(payload)?;
    message.validate().map_err(DecodeError::Invalid)?;
    Ok(message)
}

/// The major version every payload carries. Every other field, `rev` included, is skipped
/// without looking at its type: another major version may give it any shape.
#[derive(Deserialize)]
struct MajorProbe {
    v: u64,
}

/// The revision a payload of this major version may carry.
#[derive(Deserialize)]
struct RevisionProbe {
    #[serde(default)]
    rev: Option<u64>,
}

/// Checks `v`, then `rev`, without decoding the rest, so that a payload from a newer or older
/// peer fails with a precise error instead of whatever field happens to differ.
fn check_version(payload: &[u8]) -> Result<(), DecodeError> {
    let major: MajorProbe = from_slice(payload)?;
    if major.v != u64::from(PROTOCOL_VERSION) {
        return Err(DecodeError::UnsupportedVersion { found: major.v });
    }
    let probe: RevisionProbe = from_slice(payload)?;
    match probe.rev {
        Some(required) if required > u64::from(PROTOCOL_REVISION) => {
            Err(DecodeError::UnsupportedRevision { required })
        }
        _ => Ok(()),
    }
}

/// Decodes one CBOR item that must span all of `bytes`, with nesting limited to
/// [`MAX_NESTING`]. Does not check `v`; header peeks use it to read what they can from a
/// payload that fails [`decode`].
pub(crate) fn from_slice<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, DecodeError> {
    check_structure(bytes)?;
    let mut input = Input {
        rest: bytes,
        depth_left: MAX_NESTING,
    };
    let value =
        T::deserialize(&mut cbor4ii::serde::Deserializer::new(&mut input)).map_err(|error| {
            match error {
                cbor4ii::serde::DecodeError::Custom(message) => {
                    DecodeError::Malformed(message.into())
                }
                cbor4ii::serde::DecodeError::Core(core) => {
                    DecodeError::Malformed(format!("{core:?}"))
                }
            }
        })?;
    if input.rest.is_empty() {
        Ok(value)
    } else {
        Err(DecodeError::TrailingBytes {
            count: input.rest.len(),
        })
    }
}

/// Walks the CBOR structure of `bytes` without building anything, before serde sees it.
///
/// Serde sizes collections from the declared item count (capped at 1 MiB per collection), so a
/// payload that declares billions of items and then ends would still make the decoder allocate
/// before it fails. This walk refuses such a payload first: every item takes at least one byte,
/// so a count or length larger than the bytes left is impossible. It also bounds nesting
/// (arrays, maps and tags) by [`MAX_NESTING`], refuses reserved and stray
/// `break` bytes, and requires exactly one item.
fn check_structure(bytes: &[u8]) -> Result<(), DecodeError> {
    let mut rest = bytes;
    skip_item(&mut rest, 0).map_err(DecodeError::Malformed)?;
    if rest.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::TrailingBytes { count: rest.len() })
    }
}

/// The CBOR `break` stop code that ends an indefinite-length item.
const BREAK: u8 = 0xff;

/// Takes `count` bytes from the front of `rest`.
fn take<'a>(rest: &mut &'a [u8], count: usize) -> Result<&'a [u8], String> {
    if count > rest.len() {
        return Err(format!(
            "an item needs {count} bytes but only {} are left",
            rest.len()
        ));
    }
    let (taken, left) = rest.split_at(count);
    *rest = left;
    Ok(taken)
}

/// A head's argument (RFC 8949 section 3): `None` for the indefinite length (31).
fn read_argument(rest: &mut &[u8], info: u8) -> Result<Option<u64>, String> {
    let width = match info {
        0..=23 => return Ok(Some(u64::from(info))),
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        31 => return Ok(None),
        _ => return Err(format!("reserved additional information {info}")),
    };
    let mut value = 0u64;
    for byte in take(rest, width)? {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(Some(value))
}

/// A declared count of items, each at least `min_bytes` long, that the rest can hold.
fn checked_count(rest: &[u8], count: u64, min_bytes: u64) -> Result<u64, String> {
    let needed = count.checked_mul(min_bytes);
    match needed {
        Some(needed) if needed <= rest.len() as u64 => Ok(count),
        _ => Err(format!(
            "declared count {count} exceeds the {} bytes left",
            rest.len()
        )),
    }
}

fn skip_item(rest: &mut &[u8], depth: usize) -> Result<(), String> {
    let [initial] = take(rest, 1)? else {
        return Err("missing initial byte".to_owned());
    };
    let (major, info) = (initial >> 5, initial & 0x1f);
    let argument = read_argument(rest, info)?;
    let nested = || {
        if depth >= MAX_NESTING {
            Err(format!("nesting deeper than {MAX_NESTING}"))
        } else {
            Ok(depth + 1)
        }
    };
    match (major, argument) {
        // Unsigned and negative integers: the argument is the value.
        (0 | 1, Some(_)) => Ok(()),
        // Byte and text strings.
        (2 | 3, Some(len)) => {
            let len = usize::try_from(checked_count(rest, len, 1)?)
                .map_err(|_| "string length does not fit usize".to_owned())?;
            take(rest, len).map(|_| ())
        }
        // Chunked (indefinite-length) strings are refused: cbor4ii 1.2.3 leaves their `break`
        // unread when it decodes them into a value, so the rest of the payload would be read
        // out of step. No encoder produces them for strings of known length, which every
        // string in this protocol is.
        (2 | 3, None) => Err("indefinite-length strings are not supported".to_owned()),
        // Arrays and maps of definite length.
        (4 | 5, Some(count)) => {
            let inner = nested()?;
            let per_entry = if major == 4 { 1 } else { 2 };
            let items = checked_count(rest, count, per_entry)? * per_entry;
            for _ in 0..items {
                skip_item(rest, inner)?;
            }
            Ok(())
        }
        // Arrays and maps of indefinite length, ended by `break`.
        (4 | 5, None) => {
            let inner = nested()?;
            let mut items = 0u64;
            loop {
                match rest.first() {
                    Some(&BREAK) => break,
                    Some(_) => {
                        skip_item(rest, inner)?;
                        items += 1;
                    }
                    None => return Err("unterminated indefinite container".to_owned()),
                }
            }
            take(rest, 1)?;
            if major == 5 && items % 2 == 1 {
                return Err("an indefinite map ends after a key".to_owned());
            }
            Ok(())
        }
        // A tag and the item it wraps.
        (6, Some(_)) => skip_item(rest, nested()?),
        // Simple values and floats; the head already consumed their bytes.
        (7, Some(_)) => Ok(()),
        // `break` outside an indefinite item, or an indefinite integer or tag.
        _ => Err(format!("unexpected initial byte {initial:#04x}")),
    }
}

/// A byte slice reader that tracks the unread rest and the nesting depth.
struct Input<'de> {
    rest: &'de [u8],
    depth_left: usize,
}

impl<'de> Read<'de> for Input<'de> {
    type Error = Infallible;

    fn fill<'short>(&'short mut self, want: usize) -> Result<Reference<'de, 'short>, Self::Error> {
        let (available, _) = self.rest.split_at(want.min(self.rest.len()));
        Ok(Reference::Long(available))
    }

    fn advance(&mut self, n: usize) {
        let (_, rest) = self.rest.split_at(n.min(self.rest.len()));
        self.rest = rest;
    }

    fn step_in(&mut self) -> bool {
        match self.depth_left.checked_sub(1) {
            Some(depth_left) => {
                self.depth_left = depth_left;
                true
            }
            None => false,
        }
    }

    fn step_out(&mut self) {
        self.depth_left = self.depth_left.saturating_add(1);
    }
}

/// Encodes without validation or size checks; [`encode`] adds both.
pub(crate) fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    cbor4ii::serde::to_vec(Vec::new(), value).map_err(|error| match error {
        cbor4ii::serde::EncodeError::Custom(message) => EncodeError::Serialize(message.into()),
        cbor4ii::serde::EncodeError::Core(core) => EncodeError::Serialize(format!("{core:?}")),
    })
}

/// The exact number of bytes [`to_vec`] produces for `value`, without allocating them.
pub(crate) fn encoded_len<T: Serialize>(value: &T) -> Result<usize, EncodeError> {
    struct Counter(usize);
    impl Write for Counter {
        type Error = Infallible;
        fn push(&mut self, input: &[u8]) -> Result<(), Self::Error> {
            self.0 = self.0.saturating_add(input.len());
            Ok(())
        }
    }
    let mut serializer = cbor4ii::serde::Serializer::new(Counter(0));
    value
        .serialize(&mut serializer)
        .map_err(|error| match error {
            cbor4ii::serde::EncodeError::Custom(message) => EncodeError::Serialize(message.into()),
            cbor4ii::serde::EncodeError::Core(core) => EncodeError::Serialize(format!("{core:?}")),
        })?;
    Ok(serializer.into_inner().0)
}

/// The size of a CBOR head that carries `count`: the initial byte plus the shortest argument
/// (RFC 8949 section 3; the encoder always uses the shortest form).
pub(crate) const fn head_len(count: usize) -> usize {
    let count = count as u64;
    if count < 24 {
        1
    } else if count <= 0xff {
        2
    } else if count <= 0xffff {
        3
    } else if count <= 0xffff_ffff {
        5
    } else {
        9
    }
}

open_enum! {
    /// Machine-readable reason in an [`Issue`]. The cloud stores and the frontend translates
    /// codes; messages are English diagnostics for logs.
    pub enum IssueCode {
        /// The payload's `v` is not a major version this build implements.
        UnsupportedVersion => "unsupported_version",
        /// The payload's `rev` needs a newer protocol revision than the receiver implements.
        UnsupportedRevision => "unsupported_revision",
        /// The payload is not valid CBOR for its message type.
        DecodeFailed => "decode_failed",
        /// The payload exceeds its channel's size limit.
        TooLarge => "too_large",
        /// A configuration's version is lower than the one the gateway already applied: the
        /// broker delivered an outdated retained message, or the cloud lost track of the
        /// gateway's version. The cloud republishes with a version above `active_version`.
        StaleVersion => "stale_version",
        /// A payload or a required list is empty.
        Empty => "empty",
        /// A list has more entries than allowed.
        TooMany => "too_many",
        /// A string is longer than allowed.
        TooLong => "too_long",
        /// An entry that must be unique repeats.
        Duplicate => "duplicate",
        /// A number is outside its allowed range.
        OutOfRange => "out_of_range",
        /// A time range ends before it starts.
        InvalidRange => "invalid_range",
        /// A value cannot be sent, for example a code this build does not know.
        InvalidValue => "invalid_value",
        /// A device names a model the configuration does not contain.
        UnknownModel => "unknown_model",
        /// A coil or discrete input (function code 1 or 2) has a type other than `bool`.
        FunctionTypeMismatch => "function_type_mismatch",
        /// A `string` data point has no `length`.
        MissingLength => "missing_length",
        /// A data point other than `string` has a `length`.
        UnexpectedLength => "unexpected_length",
        /// A data point extends past address 65535.
        AddressOutOfRange => "address_out_of_range",
        /// Two data points of a model read overlapping addresses with the same function code.
        OverlappingPoints => "overlapping_points",
        /// Scale or offset is not finite, scale is 0, or the type cannot be scaled.
        InvalidScaling => "invalid_scaling",
        /// A coil or discrete input sets a byte order.
        UnexpectedByteOrder => "unexpected_byte_order",
        /// One host and port is used with both Modbus TCP and RTU over TCP.
        MixedTransport => "mixed_transport",
    }
}

/// One problem found in a message, addressed by a path such as `devices[2].unit_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// What is wrong.
    pub code: IssueCode,
    /// Where, as field names and list indexes from the message root; empty for the whole
    /// payload. At most [`MAX_ISSUE_PATH_BYTES`].
    pub path: String,
    /// A diagnostic in English. At most [`MAX_ISSUE_MESSAGE_BYTES`].
    pub message: String,
}

impl Issue {
    /// Builds an issue, cutting `path` and `message` to their limits at a character boundary.
    pub fn new(code: IssueCode, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path: truncate(path.into(), MAX_ISSUE_PATH_BYTES),
            message: truncate(message.into(), MAX_ISSUE_MESSAGE_BYTES),
        }
    }

    pub(crate) fn check_bounds(&self, path: &str) -> Result<(), Issue> {
        if self.path.len() > MAX_ISSUE_PATH_BYTES {
            return Err(Issue::new(
                IssueCode::TooLong,
                format!("{path}.path"),
                format!("longer than {MAX_ISSUE_PATH_BYTES} bytes"),
            ));
        }
        if self.message.len() > MAX_ISSUE_MESSAGE_BYTES {
            return Err(Issue::new(
                IssueCode::TooLong,
                format!("{path}.message"),
                format!("longer than {MAX_ISSUE_MESSAGE_BYTES} bytes"),
            ));
        }
        Ok(())
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{} at {}: {}", self.code, self.path, self.message)
        }
    }
}

/// Cuts `text` to at most `max` bytes at a character boundary.
pub(crate) fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// A message that breaks its invariants; holds at least one [`Issue`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    issues: Vec<Issue>,
}

impl Invalid {
    /// `None` when `issues` is empty, that is when nothing is invalid.
    pub fn from_issues(issues: Vec<Issue>) -> Option<Self> {
        (!issues.is_empty()).then_some(Self { issues })
    }

    /// One issue.
    pub fn single(code: IssueCode, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            issues: vec![Issue::new(code, path, message)],
        }
    }

    /// The issues, in the order they were found.
    pub fn issues(&self) -> &[Issue] {
        &self.issues
    }

    /// The issues, by value.
    pub fn into_issues(self) -> Vec<Issue> {
        self.issues
    }
}

impl From<Issue> for Invalid {
    fn from(issue: Issue) -> Self {
        Self {
            issues: vec![issue],
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.issues.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Invalid {}

/// Collects issues while validating a message.
#[derive(Default)]
pub(crate) struct Issues(Vec<Issue>);

impl Issues {
    pub(crate) fn push(
        &mut self,
        code: IssueCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.0.push(Issue::new(code, path, message));
    }

    pub(crate) fn push_issue(&mut self, issue: Issue) {
        self.0.push(issue);
    }

    pub(crate) fn finish(self) -> Result<(), Invalid> {
        Invalid::from_issues(self.0).map_or(Ok(()), Err)
    }
}

/// Why a message cannot be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The message breaks its invariants.
    Invalid(Invalid),
    /// The encoded payload exceeds the channel's limit.
    TooLarge {
        /// The encoded size in bytes.
        size: usize,
        /// The channel's limit in bytes.
        limit: usize,
    },
    /// The serializer failed, for example on a non-finite float.
    Serialize(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(invalid) => write!(f, "invalid message: {invalid}"),
            Self::TooLarge { size, limit } => {
                write!(f, "payload is {size} bytes; the limit is {limit} bytes")
            }
            Self::Serialize(message) => write!(f, "cannot encode: {message}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Why a payload cannot be accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The payload is empty: on a retained topic, the retained message was deleted.
    Empty,
    /// The payload exceeds the channel's limit.
    TooLarge {
        /// The payload size in bytes.
        size: usize,
        /// The channel's limit in bytes.
        limit: usize,
    },
    /// The payload is not a valid CBOR encoding of the message type.
    Malformed(String),
    /// Bytes follow the CBOR item.
    TrailingBytes {
        /// How many.
        count: usize,
    },
    /// The payload's `v` is not [`PROTOCOL_VERSION`].
    UnsupportedVersion {
        /// The payload's `v`.
        found: u64,
    },
    /// The payload's `rev` is greater than [`PROTOCOL_REVISION`]: it uses a feature this build
    /// would silently get wrong, so it is refused instead.
    UnsupportedRevision {
        /// The payload's `rev`.
        required: u64,
    },
    /// The payload decodes but breaks the message's invariants.
    Invalid(Invalid),
}

impl DecodeError {
    /// The error as issues, for reporting it back to the sender (for example in a
    /// [`crate::config::ConfigReport`]).
    pub fn to_issues(&self) -> Vec<Issue> {
        match self {
            Self::Invalid(invalid) => invalid.issues().to_vec(),
            Self::Empty => vec![Issue::new(IssueCode::Empty, "", "payload is empty")],
            Self::TooLarge { .. } => vec![Issue::new(IssueCode::TooLarge, "", self.to_string())],
            Self::Malformed(_) | Self::TrailingBytes { .. } => {
                vec![Issue::new(IssueCode::DecodeFailed, "", self.to_string())]
            }
            Self::UnsupportedVersion { .. } => {
                vec![Issue::new(
                    IssueCode::UnsupportedVersion,
                    "v",
                    self.to_string(),
                )]
            }
            Self::UnsupportedRevision { .. } => {
                vec![Issue::new(
                    IssueCode::UnsupportedRevision,
                    "rev",
                    self.to_string(),
                )]
            }
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("payload is empty"),
            Self::TooLarge { size, limit } => {
                write!(f, "payload is {size} bytes; the limit is {limit} bytes")
            }
            Self::Malformed(message) => write!(f, "malformed payload: {message}"),
            Self::TrailingBytes { count } => {
                write!(f, "{count} bytes follow the CBOR data item")
            }
            Self::UnsupportedVersion { found } => write!(
                f,
                "protocol version {found} is not supported; this build implements {PROTOCOL_VERSION}"
            ),
            Self::UnsupportedRevision { required } => write!(
                f,
                "payload requires protocol revision {required}; this build implements {PROTOCOL_REVISION}"
            ),
            Self::Invalid(invalid) => write!(f, "invalid message: {invalid}"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Probe {
        v: V1,
        name: String,
    }

    #[test]
    fn head_lengths_match_the_encoder() {
        for count in [0, 1, 23, 24, 255, 256, 65_535, 65_536, 1 << 20] {
            let items = vec![0u8; count];
            // A sequence of `count` single-byte integers: head plus `count` bytes.
            assert_eq!(
                encoded_len(&items).unwrap(),
                head_len(count) + count,
                "{count}"
            );
            assert_eq!(to_vec(&items).unwrap().len(), head_len(count) + count);
        }
        assert_eq!(head_len(0x1_0000_0000), 9);
    }

    #[test]
    fn decoding_rejects_trailing_bytes_and_deep_nesting() {
        let mut bytes = to_vec(&Probe {
            v: V1,
            name: "x".to_owned(),
        })
        .unwrap();
        assert!(from_slice::<Probe>(&bytes).is_ok());
        bytes.push(0x00);
        assert_eq!(
            from_slice::<Probe>(&bytes),
            Err(DecodeError::TrailingBytes { count: 1 })
        );

        // 1000 nested one-element arrays around 0.
        let mut nested = vec![0x81; 1000];
        nested.push(0x00);
        assert!(matches!(
            from_slice::<serde::de::IgnoredAny>(&nested),
            Err(DecodeError::Malformed(_))
        ));
        let mut shallow = vec![0x81; MAX_NESTING - 1];
        shallow.push(0x00);
        assert!(from_slice::<serde::de::IgnoredAny>(&shallow).is_ok());
    }

    #[test]
    fn the_structure_check_accepts_every_well_formed_shape() {
        let cases: [&[u8]; 8] = [
            // Indefinite array [1, 2], indefinite map {"a": 1}.
            &[0x9f, 0x01, 0x02, 0xff],
            &[0xbf, 0x61, b'a', 0x01, 0xff],
            // A tagged item, half/single/double floats, a one-byte simple value, null.
            &[0xc1, 0x1a, 0, 0, 0, 1],
            &[0xf9, 0x3c, 0x00],
            &[0xfa, 0x3f, 0x80, 0, 0],
            &[0xfb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0],
            &[0xf8, 0x20],
            &[0xf6],
        ];
        for bytes in cases {
            assert_eq!(check_structure(bytes), Ok(()), "{bytes:02x?}");
        }
        assert_eq!(
            from_slice::<Vec<u8>>(&[0x9f, 0x01, 0x02, 0xff]),
            Ok(vec![1, 2])
        );
        // {_ "v": 1, "name": ""}: an indefinite map decodes like a definite one.
        let indefinite = [
            0xbf, 0x61, b'v', 0x01, 0x64, b'n', b'a', b'm', b'e', 0x60, 0xff,
        ];
        assert_eq!(
            from_slice::<Probe>(&indefinite),
            Ok(Probe {
                v: V1,
                name: String::new()
            })
        );
    }

    #[test]
    fn the_structure_check_refuses_impossible_lengths_before_decoding() {
        let max = [0xff; 8];
        for head in [0x5b, 0x7b, 0x9b, 0xbb] {
            let bytes = [&[head][..], &max].concat();
            let Err(DecodeError::Malformed(message)) = check_structure(&bytes) else {
                panic!("{head:#x} must be refused");
            };
            assert!(message.contains("exceeds"), "{message}");
        }
        // An array that claims 3 items but holds 2; a map that claims 2 entries with 3 bytes.
        for bytes in [&[0x83, 0x01, 0x02][..], &[0xa2, 0x01, 0x02, 0x03]] {
            assert!(check_structure(bytes).is_err(), "{bytes:02x?}");
        }
        let cases: [&[u8]; 9] = [
            // Stray break, reserved additional information, indefinite integer.
            &[0xff],
            &[0x1c],
            &[0x1f],
            // Unterminated indefinite array.
            &[0x9f, 0x01],
            // Chunked text and bytes, which cbor4ii would decode out of step.
            &[0x7f, 0x61, b'a', 0x61, b'b', 0xff],
            &[0x5f, 0x41, 0x00, 0xff],
            &[0xa1, 0x61, b'k', 0x7f, 0xff],
            // An indefinite map that ends after a key, an empty payload.
            &[0xbf, 0x01, 0xff],
            &[],
        ];
        for bytes in cases {
            assert!(
                matches!(check_structure(bytes), Err(DecodeError::Malformed(_))),
                "{bytes:02x?}"
            );
        }
        // Tags count as nesting, so a chain of tags cannot exhaust the stack.
        let mut tags = vec![0xc1; 100_000];
        tags.push(0x00);
        assert!(check_structure(&tags).is_err());
        let mut tags = vec![0xc1; MAX_NESTING];
        tags.push(0x00);
        assert_eq!(check_structure(&tags), Ok(()));
    }

    #[test]
    fn the_version_marker_accepts_only_one() {
        let bytes = to_vec(&Probe {
            v: V1,
            name: String::new(),
        })
        .unwrap();
        // {"v": 1, "name": ""}
        assert_eq!(
            bytes,
            [0xa2, 0x61, b'v', 0x01, 0x64, b'n', b'a', b'm', b'e', 0x60]
        );
        let mut v2 = bytes.clone();
        v2[3] = 0x02;
        assert!(matches!(
            from_slice::<Probe>(&v2),
            Err(DecodeError::Malformed(message)) if message.contains("unsupported protocol version 2")
        ));
    }

    #[test]
    fn issues_are_bounded() {
        let issue = Issue::new(IssueCode::Empty, "p".repeat(1000), "é".repeat(1000));
        assert_eq!(issue.path.len(), MAX_ISSUE_PATH_BYTES);
        assert!(issue.message.len() <= MAX_ISSUE_MESSAGE_BYTES);
        assert!(issue.message.chars().all(|c| c == 'é'));
        assert_eq!(Invalid::from_issues(Vec::new()), None);
        let invalid = Invalid::single(IssueCode::Duplicate, "devices[1].id", "repeats devices[0]");
        assert_eq!(
            invalid.to_string(),
            "duplicate at devices[1].id: repeats devices[0]"
        );
    }

    #[test]
    fn decode_errors_map_to_issue_codes() {
        let cases = [
            (DecodeError::Empty, IssueCode::Empty),
            (
                DecodeError::TooLarge { size: 2, limit: 1 },
                IssueCode::TooLarge,
            ),
            (
                DecodeError::Malformed("x".to_owned()),
                IssueCode::DecodeFailed,
            ),
            (
                DecodeError::TrailingBytes { count: 1 },
                IssueCode::DecodeFailed,
            ),
            (
                DecodeError::UnsupportedVersion { found: 2 },
                IssueCode::UnsupportedVersion,
            ),
            (
                DecodeError::UnsupportedRevision { required: 9 },
                IssueCode::UnsupportedRevision,
            ),
        ];
        for (error, code) in cases {
            let issues = error.to_issues();
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].code, code, "{error}");
        }
    }
}
