//! Gateway presence and health: the retained status with its last will, and heartbeats with
//! operating metrics and per-device read statistics (rebuild checklist C3, H1).
//!
//! On every connection the gateway generates a new [`SessionId`], sets the MQTT last will to
//! [`Status::Offline`] with reason [`OfflineReason::ConnectionLost`] for that session, and
//! publishes [`Status::Online`] for that session as its first message. Both are retained on
//! `up/status`. A will from an earlier session can arrive after a newer session's `online`, so
//! the cloud ignores an `offline` whose session is not the session of the latest `online`, and
//! also treats a gateway as offline when heartbeats stop for longer than their announced
//! interval allows.
//!
//! Heartbeat counters are cumulative since the agent started; the cloud computes rates from the
//! difference between heartbeats and detects a restart when `agent_uptime_s` decreases.
//!
//! [`Heartbeat::validate`] does not cross-check fields against each other (available memory
//! above the total, more failures than requests, no requests at all): a heartbeat is also the
//! liveness signal, and refusing one over a counter would make the gateway look offline. The
//! cloud clamps such values and treats a zero request delta as "no success rate"; the crate
//! README names who does what (Laysi/trustiot-v2#269).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::codec::sealed::Token;
use crate::codec::{self, DecodeError, EncodeError, Invalid, IssueCode, Issues, Message, V1};
use crate::config::MAX_DEVICES;
use crate::ids::{AgentVersion, ConfigVersion, DeviceId, SessionId, UnixMillis};
use crate::open_enum::open_enum;
use crate::topic::Channel;

/// Longest [`LastFailure::detail`], in bytes.
pub const MAX_FAILURE_DETAIL_BYTES: usize = 256;
/// Most entries in [`DeviceStats::exception_codes`]. Modbus defines 9 exception codes; a
/// device that answers with more distinct codes is counted in [`FailureCounts::exception`]
/// only, for the codes that do not fit.
pub const MAX_EXCEPTION_CODES: usize = 16;

/// `up/status` (retained; also the last will): whether a gateway is connected. Encoded as a map
/// whose `state` names the variant; [`crate::decode`] reads the state first and then only the
/// fields of that variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Published right after connecting.
    Online(Online),
    /// The last will, or published before a clean disconnect.
    Offline(Offline),
}

/// The fields of [`Status::Online`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Online {
    /// Wire format major version.
    pub v: V1,
    /// The connection.
    pub session: SessionId,
    /// The agent's release.
    pub agent_version: AgentVersion,
    /// The highest protocol revision the agent implements ([`crate::PROTOCOL_REVISION`]); the
    /// cloud never sends it a configuration with a higher `rev`.
    pub protocol_rev: u16,
}

/// The fields of [`Status::Offline`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offline {
    /// Wire format major version.
    pub v: V1,
    /// The connection that ended.
    pub session: SessionId,
    /// Why.
    pub reason: OfflineReason,
}

open_enum! {
    /// Why a gateway went offline.
    pub enum OfflineReason {
        /// The agent stopped on purpose (shutdown, restart, update).
        Shutdown => "shutdown",
        /// The broker lost the connection and published the last will.
        ConnectionLost => "connection_lost",
    }
}

impl Status {
    /// The session the status belongs to.
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Online(online) => &online.session,
            Self::Offline(offline) => &offline.session,
        }
    }

    /// The last will to register when connecting with `session`.
    pub fn last_will(session: SessionId) -> Self {
        Self::Offline(Offline {
            v: V1,
            session,
            reason: OfflineReason::ConnectionLost,
        })
    }

    /// Encodes the status; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a status; see [`codec::decode`]. An empty payload means the retained status was
    /// cleared (for example when a gateway is decommissioned).
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for Status {
    const CHANNEL: Channel = Channel::Status;

    /// Reads `state` first, then decodes the payload as that variant's fields, so that no
    /// entry of the map is buffered (Laysi/trustiot-v2#290). Both passes skip unknown keys
    /// without building anything; each refuses a repeated field, as a single pass would.
    fn decode_item(payload: &[u8], _: Token) -> Result<Self, DecodeError> {
        /// The variant names of [`Status`], read like serde reads them for the derived enum.
        #[derive(Deserialize)]
        #[serde(variant_identifier, rename_all = "snake_case")]
        enum State {
            Online,
            Offline,
        }

        #[derive(Deserialize)]
        struct Probe {
            state: State,
        }

        let Probe { state } = codec::from_slice(payload)?;
        match state {
            State::Online => codec::from_slice(payload).map(Self::Online),
            State::Offline => codec::from_slice(payload).map(Self::Offline),
        }
    }
}

/// `up/heartbeat`: the gateway's operating metrics, sent every `interval_s` seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Wire format major version.
    pub v: V1,
    /// The connection that sends it.
    pub session: SessionId,
    /// The gateway's clock when it sent the heartbeat.
    pub sent_at: UnixMillis,
    /// Seconds until the next heartbeat; the cloud considers the gateway gone after a few
    /// missed intervals.
    pub interval_s: u32,
    /// The agent's release.
    pub agent_version: AgentVersion,
    /// The configuration version in use; `None` before the first one is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<ConfigVersion>,
    /// Host metrics.
    pub system: SystemStats,
    /// The offline buffer (rebuild checklist G1).
    pub buffer: BufferStats,
    /// Whether the clock is synchronized now (Laysi/trustiot#180).
    pub clock_synced: bool,
    /// When the gateway's client certificate expires.
    pub cert_not_after: UnixMillis,
    /// Read statistics, one entry per configured device.
    #[serde(deserialize_with = "codec::seq")]
    pub devices: Vec<DeviceStats>,
}

/// Host metrics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemStats {
    /// Average CPU use since the previous heartbeat, 0–100.
    #[serde(deserialize_with = "crate::float::any")]
    pub cpu_percent: f64,
    /// Total memory.
    pub memory_total_bytes: u64,
    /// Memory available to programs.
    pub memory_available_bytes: u64,
    /// Size of the data partition that holds the buffer and the applied configuration.
    pub data_total_bytes: u64,
    /// Free space on the data partition.
    pub data_available_bytes: u64,
    /// SoC temperature; `None` where the hardware does not report it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::float::optional"
    )]
    pub soc_temp_celsius: Option<f64>,
    /// Power and throttling flags; `None` where the hardware does not report them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttling: Option<Throttling>,
    /// When the OS booted, from the gateway's clock (wrong while the clock is unsynchronized).
    pub booted_at: UnixMillis,
    /// Seconds since the OS booted, from the monotonic clock.
    pub uptime_s: u64,
    /// How many times the OS has booted, counted on the data partition.
    pub boot_count: u64,
    /// Seconds since the agent started.
    pub agent_uptime_s: u64,
    /// How many times the agent has started since the OS booted; above 1 means it restarted.
    pub agent_starts_since_boot: u32,
}

/// The gateway's power and throttling flags, as `vcgencmd get_throttled` reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Throttling {
    /// The flags right now.
    pub now: ThrottleFlags,
    /// The flags that have been set at any time since boot.
    pub since_boot: ThrottleFlags,
}

/// One set of power and throttling flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleFlags {
    /// The supply voltage is too low.
    pub under_voltage: bool,
    /// The ARM frequency is capped.
    pub frequency_capped: bool,
    /// The CPU is throttled.
    pub throttled: bool,
    /// The soft temperature limit is active.
    pub soft_temp_limit: bool,
}

/// The offline buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferStats {
    /// Readings not yet acknowledged by the cloud.
    pub pending_records: u64,
    /// Formed batches not yet acknowledged.
    pub pending_batches: u64,
    /// The time of the oldest pending reading; `None` when nothing is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending: Option<UnixMillis>,
    /// Bytes of the buffer written to the data partition.
    pub disk_bytes: u64,
    /// Readings dropped since the agent started because the buffer was full or they exceeded
    /// the longest buffering time.
    pub dropped_records: u64,
}

/// One device's read statistics, cumulative since the agent started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStats {
    /// The device.
    pub device: DeviceId,
    /// When a read of this device last succeeded; `None` if never since the agent started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<UnixMillis>,
    /// Data points the latest poll tried to read.
    pub points_total: u32,
    /// Of those, how many failed. 0 is healthy; all of them means the device is unreachable.
    pub points_failed: u32,
    /// Modbus read requests sent, including those that could not connect.
    pub requests: u64,
    /// Failures by kind; a request that failed counts once, under its kind.
    pub failures: FailureCounts,
    /// Requests answered with a Modbus exception, by exception code; at most
    /// [`MAX_EXCEPTION_CODES`] codes. A payload that repeats a code is refused.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "codec::unique_map"
    )]
    pub exception_codes: BTreeMap<u8, u64>,
    /// The most recent failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<LastFailure>,
}

/// Failures by kind (rebuild checklist H1). A request's success rate is
/// `1 - (connect + timeout + exception) / requests`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCounts {
    /// Requests that could not connect.
    pub connect: u64,
    /// Requests without a matching response in time.
    pub timeout: u64,
    /// Requests answered with a Modbus exception.
    pub exception: u64,
    /// Responses discarded because they came from another unit (a converter shares responses
    /// among connections, Laysi/trustiot#183); the request then usually times out.
    pub unit_mismatch: u64,
    /// Data points whose registers did not decode into a value that can be sent, such as a
    /// NaN float.
    pub invalid_value: u64,
}

/// The most recent read failure of a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastFailure {
    /// What failed.
    pub kind: FailureKind,
    /// When.
    pub at: UnixMillis,
    /// The Modbus exception code, for [`FailureKind::Exception`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_code: Option<u8>,
    /// A short diagnostic such as the OS error; at most [`MAX_FAILURE_DETAIL_BYTES`], without
    /// U+0000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

open_enum! {
    /// What kind of read failure happened.
    pub enum FailureKind {
        /// The TCP connection could not be established.
        Connect => "connect",
        /// No matching response arrived in time.
        Timeout => "timeout",
        /// The device answered with a Modbus exception.
        Exception => "exception",
        /// A response from another unit was discarded.
        UnitMismatch => "unit_mismatch",
        /// The registers did not decode into a value that can be sent.
        InvalidValue => "invalid_value",
    }
}

impl Heartbeat {
    /// Encodes the heartbeat; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a heartbeat; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for Heartbeat {
    const CHANNEL: Channel = Channel::Heartbeat;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.interval_s == 0 {
            issues.push(IssueCode::OutOfRange, "interval_s", "must be at least 1");
        }
        let cpu = self.system.cpu_percent;
        if !(cpu.is_finite() && (0.0..=100.0).contains(&cpu)) {
            issues.push(
                IssueCode::OutOfRange,
                "system.cpu_percent",
                "must be within 0..=100",
            );
        }
        if self
            .system
            .soc_temp_celsius
            .is_some_and(|celsius| !celsius.is_finite())
        {
            issues.push(
                IssueCode::OutOfRange,
                "system.soc_temp_celsius",
                "must be a finite number",
            );
        }
        if self.devices.len() > MAX_DEVICES {
            issues.push(
                IssueCode::TooMany,
                "devices",
                format!("at most {MAX_DEVICES} devices"),
            );
        }
        let mut seen = HashMap::with_capacity(self.devices.len());
        for (index, stats) in self.devices.iter().enumerate() {
            if let Some(first) = seen.insert(&stats.device, index) {
                issues.push(
                    IssueCode::Duplicate,
                    format!("devices[{index}].device"),
                    format!("device {} is also devices[{first}]", stats.device),
                );
            }
            if stats.exception_codes.len() > MAX_EXCEPTION_CODES {
                issues.push(
                    IssueCode::TooMany,
                    format!("devices[{index}].exception_codes"),
                    format!("at most {MAX_EXCEPTION_CODES} exception codes"),
                );
            }
            if stats.points_failed > stats.points_total {
                issues.push(
                    IssueCode::OutOfRange,
                    format!("devices[{index}].points_failed"),
                    "more failed points than points read",
                );
            }
            let detail = stats
                .last_failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref());
            if detail.is_some_and(|detail| detail.len() > MAX_FAILURE_DETAIL_BYTES) {
                issues.push(
                    IssueCode::TooLong,
                    format!("devices[{index}].last_failure.detail"),
                    format!("longer than {MAX_FAILURE_DETAIL_BYTES} bytes"),
                );
            }
            if detail.is_some_and(codec::has_nul) {
                issues.push(
                    IssueCode::InvalidValue,
                    format!("devices[{index}].last_failure.detail"),
                    "contains U+0000",
                );
            }
        }
        issues.finish()
    }
}
