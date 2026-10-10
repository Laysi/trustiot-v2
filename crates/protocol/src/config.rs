//! Desired configuration and apply reports (decision 7; rebuild checklist C1, C2, E1, F1, F2,
//! F5).
//!
//! The cloud publishes each gateway's whole desired configuration as one retained message with
//! a version that increases per gateway. The gateway validates it, applies it atomically or
//! keeps the previous version, and answers with a [`ConfigReport`]. Postgres is the source of
//! truth (decision 7); the retained message is only the delivery, and the cloud republishes it
//! when the broker may have lost it.
//!
//! The gateway compares a received version with its active one: a higher version is validated
//! and applied (or rejected with issues); the same version is not applied again but reported as
//! applied again, so a lost report heals on the next delivery; a lower version is never applied
//! and is answered with [`ConfigReport::stale`], so that an outdated retained message cannot
//! roll a gateway back (Laysi/trustiot#139).
//!
//! [`DesiredConfig::validate`] holds every structural rule for models, data points and devices,
//! so the assets service (on every write), the configuration generator (before publishing) and
//! the gateway (before applying) enforce the same rules.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_REVISION;
use crate::codec::{
    self, DecodeError, EncodeError, Invalid, Issue, IssueCode, Issues, Message, V1, truncate,
};
use crate::ids::{ConfigVersion, DeviceId, ModelName, PointKey};
use crate::open_enum::open_enum;
use crate::topic::Channel;

/// Shortest poll interval a device may have, in milliseconds. A plan's shortest sampling
/// interval (rebuild checklist B3) is enforced by the assets service on top of this.
pub const MIN_POLL_INTERVAL_MS: u32 = 100;
/// Longest poll interval a device may have, in milliseconds: one day.
pub const MAX_POLL_INTERVAL_MS: u32 = 86_400_000;
/// Longest string data point, in registers: what one Modbus read returns (rebuild checklist F1).
pub const MAX_STRING_REGISTERS: u16 = 125;
/// Most devices one configuration holds. It is the protocol's ceiling for the per-gateway
/// technical device limit (rebuild checklist B2, enforced by the assets service): a heartbeat
/// carries statistics for every device and cannot be split, and with this many devices its worst
/// case still fits [`crate::topic::MAX_LARGE_BYTES`] (`tests/limits.rs`).
pub const MAX_DEVICES: usize = 1000;
/// Most issues a [`ConfigReport`] carries.
pub const MAX_REPORT_ISSUES: usize = 64;
/// Longest host name, in bytes.
pub const MAX_HOST_LEN: usize = 253;

/// `down/config` (retained): everything a gateway polls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesiredConfig {
    /// Wire format major version.
    pub v: V1,
    /// The lowest protocol revision that applies this configuration correctly
    /// ([`DesiredConfig::minimum_revision`]). A gateway implementing an older revision rejects
    /// the configuration with `unsupported_revision` instead of misreading a feature it does not
    /// know, and keeps its previous configuration.
    pub rev: u16,
    /// This configuration's version for the gateway.
    pub version: ConfigVersion,
    /// The device models the devices use, each once.
    pub models: Vec<DeviceModel>,
    /// The devices to poll. A device missing here is no longer polled and its state is cleared.
    pub devices: Vec<Device>,
}

/// A device model: the register map shared by devices of one type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceModel {
    /// The model's name, unique within the configuration.
    pub name: ModelName,
    /// The data points to read.
    pub points: Vec<DataPoint>,
}

/// One value a device model exposes, and how to read and decode it.
///
/// The value sent in telemetry is [`crate::Value::Float`] when `scale` or `offset` is set
/// (`raw * scale + offset`), and otherwise follows [`DataType`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataPoint {
    /// The key telemetry uses, unique within the model.
    pub key: PointKey,
    /// How to read it. Only the read function codes 1–4 exist: the gateway never writes to a
    /// device.
    pub function_code: FunctionCode,
    /// The 0-based protocol address of the first coil, input or register (a register shown as
    /// 40001 in a device manual is address 0).
    pub address: u16,
    /// How to decode the registers.
    pub data_type: DataType,
    /// How the bytes of a multi-byte value are ordered; `abcd` when absent.
    #[serde(default, skip_serializing_if = "ByteOrder::is_default")]
    pub byte_order: ByteOrder,
    /// For `string` only: the number of registers (2 characters each).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u16>,
    /// Multiplier applied to the raw number (rebuild checklist F5).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::float::optional"
    )]
    pub scale: Option<f64>,
    /// Added after `scale`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::float::optional"
    )]
    pub offset: Option<f64>,
}

/// A Modbus read function. Encoded as its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum FunctionCode {
    /// 1: read coils (one bit each).
    ReadCoils,
    /// 2: read discrete inputs (one bit each).
    ReadDiscreteInputs,
    /// 3: read holding registers (16 bits each).
    ReadHoldingRegisters,
    /// 4: read input registers (16 bits each).
    ReadInputRegisters,
}

impl FunctionCode {
    /// The function code number.
    pub const fn number(self) -> u8 {
        match self {
            Self::ReadCoils => 1,
            Self::ReadDiscreteInputs => 2,
            Self::ReadHoldingRegisters => 3,
            Self::ReadInputRegisters => 4,
        }
    }

    /// Whether the function reads bits (coils, discrete inputs) rather than 16-bit registers.
    pub const fn reads_bits(self) -> bool {
        matches!(self, Self::ReadCoils | Self::ReadDiscreteInputs)
    }
}

impl From<FunctionCode> for u8 {
    fn from(code: FunctionCode) -> Self {
        code.number()
    }
}

/// A function code outside 1–4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionCodeError(pub u8);

impl fmt::Display for FunctionCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "function code {} is not a read function; only 1-4 are allowed",
            self.0
        )
    }
}

impl std::error::Error for FunctionCodeError {}

impl TryFrom<u8> for FunctionCode {
    type Error = FunctionCodeError;

    fn try_from(number: u8) -> Result<Self, Self::Error> {
        match number {
            1 => Ok(Self::ReadCoils),
            2 => Ok(Self::ReadDiscreteInputs),
            3 => Ok(Self::ReadHoldingRegisters),
            4 => Ok(Self::ReadInputRegisters),
            other => Err(FunctionCodeError(other)),
        }
    }
}

/// How a data point's registers decode into a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType {
    /// A coil or discrete input; on a register, `true` when the register is not 0. Sent as
    /// [`crate::Value::Bool`].
    Bool,
    /// One register, unsigned. Sent as [`crate::Value::Int`].
    U16,
    /// One register, two's complement. Sent as [`crate::Value::Int`].
    I16,
    /// Two registers, unsigned. Sent as [`crate::Value::Int`].
    U32,
    /// Two registers, two's complement. Sent as [`crate::Value::Int`].
    I32,
    /// Four registers, unsigned. Sent as [`crate::Value::Int`]; a value above `i64::MAX` is a
    /// read failure.
    U64,
    /// Four registers, two's complement. Sent as [`crate::Value::Int`].
    I64,
    /// Two registers, IEEE 754 single precision. Sent as [`crate::Value::Float`].
    F32,
    /// Four registers, IEEE 754 double precision. Sent as [`crate::Value::Float`].
    F64,
    /// `length` registers of ASCII, two characters per register, ending at the first NUL and
    /// trimmed. Sent as [`crate::Value::Text`].
    String,
}

impl DataType {
    /// How many registers a value of this type spans; `None` for `string`, whose size is its
    /// `length`.
    pub const fn fixed_registers(self) -> Option<u16> {
        match self {
            Self::Bool | Self::U16 | Self::I16 => Some(1),
            Self::U32 | Self::I32 | Self::F32 => Some(2),
            Self::U64 | Self::I64 | Self::F64 => Some(4),
            Self::String => None,
        }
    }

    /// Whether scale and offset apply.
    pub const fn is_numeric(self) -> bool {
        !matches!(self, Self::Bool | Self::String)
    }
}

/// Byte order of a multi-register value, named after where the bytes of the 32-bit value
/// `0xAABBCCDD` (`A` most significant) end up in the registers as read: `abcd` is register 0 =
/// `AA BB`, register 1 = `CC DD`.
///
/// It is two independent swaps, which also define the other sizes: `badc` and `dcba` swap the
/// two bytes inside every register; `cdab` and `dcba` reverse the order of the registers. A
/// 64-bit value in `cdab` is therefore `GH EF CD AB`, and a one-register value is affected only
/// by the byte swap. Strings use the byte swap only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ByteOrder {
    /// Big-endian, most significant register first: the Modbus default.
    #[default]
    Abcd,
    /// Bytes swapped inside each register.
    Badc,
    /// Registers reversed (word swap).
    Cdab,
    /// Little-endian: bytes swapped and registers reversed.
    Dcba,
}

impl ByteOrder {
    /// Whether this is `abcd`, the default that is left out of the encoding.
    pub fn is_default(&self) -> bool {
        *self == Self::Abcd
    }

    /// Whether the two bytes inside each register are swapped.
    pub const fn swaps_bytes(self) -> bool {
        matches!(self, Self::Badc | Self::Dcba)
    }

    /// Whether the order of the registers is reversed.
    pub const fn swaps_words(self) -> bool {
        matches!(self, Self::Cdab | Self::Dcba)
    }
}

/// One device to poll.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The device.
    pub id: DeviceId,
    /// The model it uses; must be in [`DesiredConfig::models`].
    pub model: ModelName,
    /// How Modbus frames travel to the device.
    pub transport: Transport,
    /// The device's (or its serial converter's) address.
    pub host: Host,
    /// The TCP port, 1–65535.
    pub port: u16,
    /// The Modbus unit identifier: 1–247 behind an RTU-over-TCP converter, any value for
    /// Modbus TCP.
    pub unit_id: u8,
    /// How often to poll, in milliseconds.
    pub poll_interval_ms: u32,
}

/// How Modbus frames travel to a device. Only active polling exists (rebuild checklist F3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Modbus TCP (MBAP framing).
    ModbusTcp,
    /// RTU framing over a TCP connection to an RS485 converter. Devices on one converter
    /// (same host and port) share one connection and are polled in turn (rebuild checklist F2).
    RtuOverTcp,
}

/// An IPv4 or IPv6 address literal or a DNS host name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Host(String);

/// Why a string is not a usable host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    excerpt: String,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid host {:?}: must be a unicast IP address or a DNS name of at most {MAX_HOST_LEN} bytes",
            self.excerpt
        )
    }
}

impl std::error::Error for HostError {}

impl Host {
    /// Validates and wraps a host.
    pub fn new(host: impl Into<String>) -> Result<Self, HostError> {
        let host = host.into();
        if is_valid_host(&host) {
            Ok(Self(host))
        } else {
            Err(HostError {
                excerpt: truncate(host, 64),
            })
        }
    }

    /// The host as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host in a form that is equal for equal hosts: an IP address in its canonical text
    /// form, a DNS name in lowercase.
    fn canonical(&self) -> String {
        match IpAddr::from_str(&self.0) {
            Ok(ip) => ip.to_string(),
            Err(_) => self.0.to_ascii_lowercase(),
        }
    }
}

fn is_valid_host(host: &str) -> bool {
    if let Ok(ip) = IpAddr::from_str(host) {
        return !ip.is_unspecified()
            && !ip.is_multicast()
            && ip != IpAddr::from([255, 255, 255, 255]);
    }
    // A name whose last label is all digits reads as a mistyped IPv4 address such as
    // 192.168.1.300 (RFC 3696 section 2); refuse it rather than resolve it as a name.
    !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && host
            .rsplit('.')
            .next()
            .is_some_and(|last| !last.bytes().all(|byte| byte.is_ascii_digit()))
        && host.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
                && bytes.first() != Some(&b'-')
                && bytes.last() != Some(&b'-')
        })
}

impl TryFrom<String> for Host {
    type Error = HostError;

    fn try_from(host: String) -> Result<Self, Self::Error> {
        Self::new(host)
    }
}

impl From<Host> for String {
    fn from(host: Host) -> Self {
        host.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl DesiredConfig {
    /// The lowest protocol revision whose rules this configuration needs. Revision 1 is this
    /// crate's first; a later revision that adds a field a gateway must not ignore raises this
    /// whenever the field is used, so that older gateways reject the configuration instead of
    /// misreading it.
    pub fn minimum_revision(&self) -> u16 {
        1
    }

    /// The model a device uses.
    pub fn model(&self, name: &ModelName) -> Option<&DeviceModel> {
        self.models.iter().find(|model| &model.name == name)
    }

    /// Encodes the configuration; see [`codec::encode`]. Refuses an invalid configuration, so
    /// that a configuration that references a missing model is never sent (rebuild checklist
    /// E1).
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes and validates a configuration; see [`codec::decode`]. On failure,
    /// [`DecodeError::to_issues`] gives the issues to report and [`Self::peek_header`] the
    /// version to report them for.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }

    /// Reads only the version of a configuration that [`Self::decode`] refused, so the gateway
    /// can report the rejection for the right version.
    pub fn peek_header(payload: &[u8]) -> Result<ConfigHeader, DecodeError> {
        codec::from_slice(payload)
    }
}

/// The identifying fields of a configuration payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct ConfigHeader {
    /// The configuration's version.
    pub version: ConfigVersion,
}

impl Message for DesiredConfig {
    const CHANNEL: Channel = Channel::Config;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.rev == 0 || self.rev > PROTOCOL_REVISION {
            issues.push(
                IssueCode::UnsupportedRevision,
                "rev",
                format!("revision {} is outside 1..={PROTOCOL_REVISION}", self.rev),
            );
            return issues.finish();
        }
        if self.rev < self.minimum_revision() {
            issues.push(
                IssueCode::UnsupportedRevision,
                "rev",
                format!(
                    "the configuration needs revision {}; rev says {}",
                    self.minimum_revision(),
                    self.rev
                ),
            );
        }
        validate_models(&self.models, &mut issues);
        validate_devices(&self.devices, &self.models, &mut issues);
        issues.finish()
    }
}

fn validate_models(models: &[DeviceModel], issues: &mut Issues) {
    let mut names = HashMap::new();
    for (index, model) in models.iter().enumerate() {
        if let Some(first) = names.insert(&model.name, index) {
            issues.push(
                IssueCode::Duplicate,
                format!("models[{index}].name"),
                format!("model {} is also models[{first}]", model.name),
            );
        }
        validate_points(&model.points, &format!("models[{index}]"), issues);
    }
}

fn validate_points(points: &[DataPoint], path: &str, issues: &mut Issues) {
    let mut keys = HashMap::new();
    // (function code, first address, last address, point index) of every point that spans a
    // valid range, for the overlap check.
    let mut ranges = Vec::with_capacity(points.len());
    for (index, point) in points.iter().enumerate() {
        let at = format!("{path}.points[{index}]");
        if let Some(first) = keys.insert(&point.key, index) {
            issues.push(
                IssueCode::Duplicate,
                format!("{at}.key"),
                format!("data point {} is also points[{first}]", point.key),
            );
        }
        if let Some(count) = check_point(point, &at, issues) {
            let last = u32::from(point.address) + u32::from(count) - 1;
            if last > u32::from(u16::MAX) {
                issues.push(
                    IssueCode::AddressOutOfRange,
                    format!("{at}.address"),
                    format!(
                        "{} registers from address {} run past 65535",
                        count, point.address
                    ),
                );
            } else {
                ranges.push((point.function_code, u32::from(point.address), last, index));
            }
        }
    }
    // Coils, discrete inputs, holding registers and input registers are separate address
    // spaces. After sorting, a point overlaps an earlier one exactly when it starts at or before
    // the furthest end seen so far in its space.
    ranges.sort_unstable();
    let mut furthest: Option<(FunctionCode, u32, usize)> = None;
    for (function_code, first, last, index) in ranges {
        match furthest {
            Some((code, end, other)) if code == function_code && first <= end => {
                issues.push(
                    IssueCode::OverlappingPoints,
                    format!("{path}.points[{index}].address"),
                    format!(
                        "{} {} overlaps {path}.points[{other}]",
                        function_label(function_code),
                        first
                    ),
                );
                if last > end {
                    furthest = Some((code, last, index));
                }
            }
            _ => furthest = Some((function_code, last, index)),
        }
    }
}

fn function_label(code: FunctionCode) -> &'static str {
    match code {
        FunctionCode::ReadCoils => "coil",
        FunctionCode::ReadDiscreteInputs => "discrete input",
        FunctionCode::ReadHoldingRegisters => "holding register",
        FunctionCode::ReadInputRegisters => "input register",
    }
}

/// Checks one point's own fields and returns how many coils or registers it spans, when that
/// is known.
fn check_point(point: &DataPoint, at: &str, issues: &mut Issues) -> Option<u16> {
    let scaled = point.scale.is_some() || point.offset.is_some();
    for (field, value) in [("scale", point.scale), ("offset", point.offset)] {
        if value.is_some_and(|value| !value.is_finite()) {
            issues.push(
                IssueCode::InvalidScaling,
                format!("{at}.{field}"),
                "must be a finite number",
            );
        }
    }
    if point.scale == Some(0.0) {
        issues.push(
            IssueCode::InvalidScaling,
            format!("{at}.scale"),
            "0 would turn every reading into the offset",
        );
    }
    if scaled && !point.data_type.is_numeric() {
        issues.push(
            IssueCode::InvalidScaling,
            format!("{at}.scale"),
            "bool and string data points cannot be scaled",
        );
    }

    if point.function_code.reads_bits() {
        if point.data_type != DataType::Bool {
            issues.push(
                IssueCode::FunctionTypeMismatch,
                format!("{at}.data_type"),
                format!(
                    "function code {} reads single bits; the type must be bool",
                    point.function_code.number()
                ),
            );
        }
        if !point.byte_order.is_default() {
            issues.push(
                IssueCode::UnexpectedByteOrder,
                format!("{at}.byte_order"),
                "single bits have no byte order",
            );
        }
        if point.length.is_some() {
            issues.push(
                IssueCode::UnexpectedLength,
                format!("{at}.length"),
                "only string data points have a length",
            );
        }
        return Some(1);
    }

    match (point.data_type.fixed_registers(), point.length) {
        (Some(count), None) => Some(count),
        (Some(_), Some(_)) => {
            issues.push(
                IssueCode::UnexpectedLength,
                format!("{at}.length"),
                "only string data points have a length",
            );
            None
        }
        (None, None) => {
            issues.push(
                IssueCode::MissingLength,
                format!("{at}.length"),
                "a string data point needs its length in registers",
            );
            None
        }
        (None, Some(length)) if (1..=MAX_STRING_REGISTERS).contains(&length) => {
            if point.byte_order.swaps_words() {
                issues.push(
                    IssueCode::UnexpectedByteOrder,
                    format!("{at}.byte_order"),
                    "strings support only abcd and badc",
                );
            }
            Some(length)
        }
        (None, Some(length)) => {
            issues.push(
                IssueCode::OutOfRange,
                format!("{at}.length"),
                format!("{length} registers is outside 1..={MAX_STRING_REGISTERS}"),
            );
            None
        }
    }
}

fn validate_devices(devices: &[Device], models: &[DeviceModel], issues: &mut Issues) {
    let model_names: HashSet<&ModelName> = models.iter().map(|model| &model.name).collect();
    let mut ids = HashMap::new();
    if devices.len() > MAX_DEVICES {
        issues.push(
            IssueCode::TooMany,
            "devices",
            format!(
                "{} devices; a configuration holds at most {MAX_DEVICES}",
                devices.len()
            ),
        );
    }
    let mut transports: HashMap<(String, u16), (Transport, usize)> = HashMap::new();
    for (index, device) in devices.iter().enumerate() {
        let at = format!("devices[{index}]");
        if let Some(first) = ids.insert(&device.id, index) {
            issues.push(
                IssueCode::Duplicate,
                format!("{at}.id"),
                format!("device {} is also devices[{first}]", device.id),
            );
        }
        if !model_names.contains(&device.model) {
            issues.push(
                IssueCode::UnknownModel,
                format!("{at}.model"),
                format!("model {} is not in the configuration", device.model),
            );
        }
        if device.port == 0 {
            issues.push(
                IssueCode::OutOfRange,
                format!("{at}.port"),
                "port 0 is not a usable TCP port",
            );
        }
        if device.transport == Transport::RtuOverTcp && !(1..=247).contains(&device.unit_id) {
            issues.push(
                IssueCode::OutOfRange,
                format!("{at}.unit_id"),
                format!(
                    "unit {} is outside the RTU address range 1..=247",
                    device.unit_id
                ),
            );
        }
        if !(MIN_POLL_INTERVAL_MS..=MAX_POLL_INTERVAL_MS).contains(&device.poll_interval_ms) {
            issues.push(
                IssueCode::OutOfRange,
                format!("{at}.poll_interval_ms"),
                format!(
                    "{} ms is outside {MIN_POLL_INTERVAL_MS}..={MAX_POLL_INTERVAL_MS}",
                    device.poll_interval_ms
                ),
            );
        }
        let endpoint = (device.host.canonical(), device.port);
        match transports.get(&endpoint) {
            Some(&(transport, first)) if transport != device.transport => issues.push(
                IssueCode::MixedTransport,
                format!("{at}.transport"),
                format!(
                    "{}:{} is used with another transport by devices[{first}]",
                    device.host, device.port
                ),
            ),
            Some(_) => {}
            None => {
                transports.insert(endpoint, (device.transport, index));
            }
        }
    }
}

open_enum! {
    /// What the gateway did with a configuration version.
    pub enum ConfigOutcome {
        /// The gateway now polls exactly this configuration.
        Applied => "applied",
        /// The gateway refused this configuration and keeps its active version.
        Rejected => "rejected",
    }
}

/// `up/config-report`: the gateway's answer to one configuration version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReport {
    /// Wire format major version.
    pub v: V1,
    /// The configuration version this report is about.
    pub version: ConfigVersion,
    /// What the gateway did with it.
    pub outcome: ConfigOutcome,
    /// The version the gateway polls with after this report; `None` when it has never applied
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<ConfigVersion>,
    /// Why a configuration was rejected; at most [`MAX_REPORT_ISSUES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<Issue>,
}

impl ConfigReport {
    /// The report for an applied configuration.
    pub fn applied(version: ConfigVersion) -> Self {
        Self {
            v: V1,
            version,
            outcome: ConfigOutcome::Applied,
            active_version: Some(version),
            issues: Vec::new(),
        }
    }

    /// The report for a rejected configuration. Keeps the first [`MAX_REPORT_ISSUES`] issues,
    /// replacing the last kept one with a count of those left out.
    pub fn rejected(
        version: ConfigVersion,
        active_version: Option<ConfigVersion>,
        mut issues: Vec<Issue>,
    ) -> Self {
        if issues.len() > MAX_REPORT_ISSUES {
            let omitted = issues.len() - (MAX_REPORT_ISSUES - 1);
            issues.truncate(MAX_REPORT_ISSUES - 1);
            issues.push(Issue::new(
                IssueCode::TooMany,
                "issues",
                format!("{omitted} more issues are not listed"),
            ));
        }
        Self {
            v: V1,
            version,
            outcome: ConfigOutcome::Rejected,
            active_version,
            issues,
        }
    }

    /// The report for a configuration whose version is lower than the active one: the gateway
    /// keeps `active` and tells the cloud which version to exceed.
    pub fn stale(version: ConfigVersion, active: ConfigVersion) -> Self {
        Self::rejected(
            version,
            Some(active),
            vec![Issue::new(
                IssueCode::StaleVersion,
                "version",
                format!("version {version} is lower than the active version {active}"),
            )],
        )
    }

    /// Encodes the report; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a report; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for ConfigReport {
    const CHANNEL: Channel = Channel::ConfigReport;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.issues.len() > MAX_REPORT_ISSUES {
            issues.push(
                IssueCode::TooMany,
                "issues",
                format!("at most {MAX_REPORT_ISSUES} issues"),
            );
        }
        for (index, issue) in self.issues.iter().enumerate() {
            if let Err(problem) = issue.check_bounds(&format!("issues[{index}]")) {
                issues.push_issue(problem);
            }
        }
        issues.finish()
    }
}
