//! Sample messages shared by the integration tests. The golden files under `tests/golden/` are
//! these samples encoded; change a sample only together with its golden files.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the samples"
)]

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use protocol::config::{
    ByteOrder, ConfigOutcome, ConfigReport, DataPoint, DataType, DesiredConfig, Device,
    DeviceModel, FunctionCode, Host, Transport,
};
use protocol::health::{
    BufferStats, DeviceStats, FailureCounts, FailureKind, Heartbeat, LastFailure, Offline,
    OfflineReason, Online, Status, SystemStats, ThrottleFlags, Throttling,
};
use protocol::live::{LiveAction, LiveControl, LiveSamples, LiveSpec};
use protocol::logs::{LogBatch, LogCoverage, LogDump, LogEntry, LogLevel, LogRequest};
use protocol::telemetry::{AckOutcome, Sample, Series, TelemetryAck, TelemetryBatch};
use protocol::topic::Channel;
use protocol::{
    AgentVersion, ConfigVersion, DecodeError, DeviceId, Issue, IssueCode, LeaseId, Message,
    ModelName, PointKey, RequestId, Seq, SessionId, StreamId, UnixMillis, V1, Value,
};

/// 2026-10-10T00:00:00Z.
pub const T0: u64 = 1_791_590_400_000;

pub fn at(offset_ms: u64) -> UnixMillis {
    UnixMillis::new(T0 + offset_ms)
}

pub fn device(n: u8) -> DeviceId {
    DeviceId::new(format!("0199d0a0-0000-7000-8000-0000000000{n:02x}")).unwrap()
}

pub fn point(key: &str) -> PointKey {
    PointKey::new(key).unwrap()
}

pub fn model(name: &str) -> ModelName {
    ModelName::new(name).unwrap()
}

pub fn stream() -> StreamId {
    StreamId::new("0199d0a0-0000-7000-8000-00000000a001").unwrap()
}

pub fn session() -> SessionId {
    SessionId::new("0199d0a0-0000-7000-8000-00000000c001").unwrap()
}

pub fn sample(offset_ms: u64, value: Value) -> Sample {
    Sample {
        at: at(offset_ms),
        value,
    }
}

pub fn telemetry_batch() -> TelemetryBatch {
    TelemetryBatch {
        v: V1,
        stream: stream(),
        seq: Seq::new(42).unwrap(),
        clock_synced: true,
        series: vec![
            Series {
                device: device(1),
                point: point("frequency"),
                samples: vec![
                    sample(0, Value::Float(59.98)),
                    sample(1_000, Value::Float(60.0)),
                ],
            },
            Series {
                device: device(1),
                point: point("running"),
                samples: vec![sample(0, Value::Bool(true))],
            },
            Series {
                device: device(2),
                point: point("energy_wh"),
                samples: vec![sample(0, Value::Int(1_234_567_890_123))],
            },
            Series {
                device: device(2),
                point: point("power_factor"),
                samples: vec![sample(5, Value::Float(-0.5))],
            },
            Series {
                device: device(2),
                point: point("serial"),
                samples: vec![sample(5, Value::Text("PM2K-0001".to_owned()))],
            },
            Series {
                device: device(2),
                point: point("temperature"),
                samples: vec![sample(5, Value::Int(-12))],
            },
        ],
    }
}

fn ack(outcome: AckOutcome) -> TelemetryAck {
    TelemetryAck {
        v: V1,
        stream: stream(),
        seq: Seq::new(42).unwrap(),
        outcome,
        server_time: at(1_500),
    }
}

pub fn ack_committed() -> TelemetryAck {
    ack(AckOutcome::Committed { records: 7 })
}

pub fn ack_duplicate() -> TelemetryAck {
    ack(AckOutcome::Duplicate { records: 7 })
}

pub fn ack_rejected() -> TelemetryAck {
    ack(AckOutcome::Rejected {
        code: IssueCode::DecodeFailed,
        message: "malformed payload: missing field `series`".to_owned(),
    })
}

pub fn ack_retry() -> TelemetryAck {
    ack(AckOutcome::Retry { after_ms: 5_000 })
}

/// A data point with the default byte order and no length, scale or offset.
pub fn data_point(
    key: &str,
    function_code: FunctionCode,
    address: u16,
    data_type: DataType,
) -> DataPoint {
    DataPoint {
        key: point(key),
        function_code,
        address,
        data_type,
        byte_order: ByteOrder::Abcd,
        length: None,
        scale: None,
        offset: None,
    }
}

pub fn desired_config() -> DesiredConfig {
    DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(7).unwrap(),
        models: vec![
            DeviceModel {
                name: model("ms300-vfd"),
                points: vec![
                    DataPoint {
                        scale: Some(0.01),
                        ..data_point(
                            "frequency",
                            FunctionCode::ReadHoldingRegisters,
                            8451,
                            DataType::U16,
                        )
                    },
                    DataPoint {
                        byte_order: ByteOrder::Cdab,
                        ..data_point(
                            "energy_wh",
                            FunctionCode::ReadHoldingRegisters,
                            8460,
                            DataType::U32,
                        )
                    },
                    DataPoint {
                        scale: Some(0.1),
                        offset: Some(-40.0),
                        ..data_point(
                            "temperature",
                            FunctionCode::ReadInputRegisters,
                            0,
                            DataType::I16,
                        )
                    },
                    data_point("running", FunctionCode::ReadCoils, 0, DataType::Bool),
                ],
            },
            DeviceModel {
                name: model("pm-2000"),
                points: vec![
                    data_point(
                        "voltage_a",
                        FunctionCode::ReadInputRegisters,
                        0,
                        DataType::F32,
                    ),
                    DataPoint {
                        byte_order: ByteOrder::Dcba,
                        ..data_point(
                            "energy_wh",
                            FunctionCode::ReadInputRegisters,
                            2,
                            DataType::U64,
                        )
                    },
                    DataPoint {
                        byte_order: ByteOrder::Badc,
                        ..data_point("power", FunctionCode::ReadInputRegisters, 6, DataType::F64)
                    },
                    DataPoint {
                        length: Some(5),
                        ..data_point(
                            "serial",
                            FunctionCode::ReadHoldingRegisters,
                            100,
                            DataType::String,
                        )
                    },
                    data_point(
                        "breaker",
                        FunctionCode::ReadDiscreteInputs,
                        3,
                        DataType::Bool,
                    ),
                ],
            },
        ],
        devices: vec![
            Device {
                id: device(1),
                model: model("ms300-vfd"),
                transport: Transport::ModbusTcp,
                host: Host::new("192.168.10.21").unwrap(),
                port: 502,
                unit_id: 1,
                poll_interval_ms: 1_000,
            },
            Device {
                id: device(2),
                model: model("pm-2000"),
                transport: Transport::RtuOverTcp,
                host: Host::new("192.168.10.30").unwrap(),
                port: 4196,
                unit_id: 3,
                poll_interval_ms: 5_000,
            },
            Device {
                id: device(3),
                model: model("pm-2000"),
                transport: Transport::RtuOverTcp,
                host: Host::new("192.168.10.30").unwrap(),
                port: 4196,
                unit_id: 4,
                poll_interval_ms: 5_000,
            },
        ],
    }
}

pub fn config_applied() -> ConfigReport {
    ConfigReport::applied(ConfigVersion::new(7).unwrap())
}

pub fn config_rejected() -> ConfigReport {
    ConfigReport {
        v: V1,
        version: ConfigVersion::new(8).unwrap(),
        outcome: ConfigOutcome::Rejected,
        active_version: ConfigVersion::new(7),
        issues: vec![Issue::new(
            IssueCode::UnknownModel,
            "devices[2].model",
            "model pm-3000 is not in the configuration",
        )],
    }
}

pub fn status_online() -> Status {
    Status::Online(Online {
        v: V1,
        session: session(),
        agent_version: AgentVersion::new("0.1.0+3a94ff1").unwrap(),
        protocol_rev: 1,
    })
}

pub fn status_offline() -> Status {
    Status::Offline(Offline {
        v: V1,
        session: session(),
        reason: OfflineReason::ConnectionLost,
    })
}

pub fn heartbeat() -> Heartbeat {
    Heartbeat {
        v: V1,
        session: session(),
        sent_at: at(60_000),
        interval_s: 60,
        agent_version: AgentVersion::new("0.1.0+3a94ff1").unwrap(),
        config_version: ConfigVersion::new(7),
        system: SystemStats {
            cpu_percent: 12.5,
            memory_total_bytes: 2_147_483_648,
            memory_available_bytes: 1_610_612_736,
            data_total_bytes: 16_000_000_000,
            data_available_bytes: 15_000_000_000,
            soc_temp_celsius: Some(48.5),
            throttling: Some(Throttling {
                now: ThrottleFlags::default(),
                since_boot: ThrottleFlags {
                    under_voltage: true,
                    ..ThrottleFlags::default()
                },
            }),
            booted_at: at(0),
            uptime_s: 86_400,
            boot_count: 12,
            agent_uptime_s: 3_600,
            agent_starts_since_boot: 2,
        },
        buffer: BufferStats {
            pending_records: 1_200,
            pending_batches: 1,
            oldest_pending: Some(at(50_000)),
            disk_bytes: 0,
            dropped_records: 0,
        },
        clock_synced: true,
        cert_not_after: UnixMillis::new(T0 + 90 * 86_400_000),
        // Left out: an optional field added later (Laysi/trustiot-v2#563), so the golden
        // heartbeat stays as it was; the field has its own tests (`compat.rs`, `validation.rs`).
        root_crl: None,
        devices: vec![
            DeviceStats {
                device: device(1),
                last_success: Some(at(59_000)),
                points_total: 4,
                points_failed: 0,
                requests: 7_200,
                failures: FailureCounts::default(),
                exception_codes: BTreeMap::new(),
                last_failure: None,
            },
            DeviceStats {
                device: device(2),
                last_success: Some(at(55_000)),
                points_total: 5,
                points_failed: 1,
                requests: 3_600,
                failures: FailureCounts {
                    connect: 1,
                    timeout: 4,
                    exception: 2,
                    unit_mismatch: 3,
                    invalid_value: 0,
                },
                exception_codes: BTreeMap::from([(2, 2)]),
                last_failure: Some(LastFailure {
                    kind: FailureKind::Exception,
                    at: at(58_000),
                    exception_code: Some(2),
                    detail: Some("illegal data address".to_owned()),
                }),
            },
        ],
    }
}

fn lease() -> LeaseId {
    LeaseId::new("0199d0a0-0000-7000-8000-00000000e001").unwrap()
}

fn live_spec() -> LiveSpec {
    LiveSpec {
        devices: vec![device(1), device(2)],
        interval_ms: 1_000,
        ttl_s: 60,
    }
}

pub fn live_start() -> LiveControl {
    LiveControl {
        v: V1,
        lease: lease(),
        action: LiveAction::Start(live_spec()),
    }
}

pub fn live_renew() -> LiveControl {
    LiveControl {
        v: V1,
        lease: lease(),
        action: LiveAction::Renew(live_spec()),
    }
}

pub fn live_stop() -> LiveControl {
    LiveControl {
        v: V1,
        lease: lease(),
        action: LiveAction::Stop,
    }
}

pub fn live_samples() -> LiveSamples {
    LiveSamples {
        v: V1,
        series: vec![Series {
            device: device(1),
            point: point("frequency"),
            samples: vec![sample(2_000, Value::Float(60.01))],
        }],
    }
}

fn log_entries() -> Vec<LogEntry> {
    vec![
        LogEntry::new(
            at(10),
            LogLevel::Warn,
            "edge_agent::modbus",
            "read timed out; retrying",
        )
        .with_field("device", device(2).to_string())
        .with_field("timeout_ms", "5000"),
        LogEntry::new(
            at(20),
            LogLevel::Error,
            "edge_agent::upload",
            "telemetry publish refused by the broker (PUBACK 0x87)",
        ),
    ]
}

pub fn log_batch() -> LogBatch {
    LogBatch {
        v: V1,
        entries: log_entries(),
        dropped: 3,
    }
}

fn request() -> RequestId {
    RequestId::new("0199d0a0-0000-7000-8000-00000000f001").unwrap()
}

pub fn log_request() -> LogRequest {
    LogRequest {
        v: V1,
        request: request(),
        from: at(0),
        to: at(3_600_000),
        min_level: LogLevel::Debug,
    }
}

pub fn log_dump() -> LogDump {
    LogDump {
        v: V1,
        request: request(),
        part: 0,
        last: true,
        coverage: LogCoverage::Partial,
        available_from: Some(at(5)),
        entries: log_entries(),
    }
}

/// A sample message whose encoding is pinned by golden files.
pub trait Golden {
    /// The golden files' base name.
    fn name(&self) -> &'static str;
    /// The channel the message travels on.
    fn channel(&self) -> Channel;
    /// The sample encoded.
    fn encode(&self) -> Vec<u8>;
    /// The sample rendered as JSON for human review.
    fn json(&self) -> serde_json::Value;
    /// Decodes `payload` as this message type and compares it with the sample.
    fn decodes_to_sample(&self, payload: &[u8]) -> Result<(), String>;
    /// Decodes `payload` as this message type and renders it as JSON.
    fn decode(&self, payload: &[u8]) -> Result<serde_json::Value, DecodeError>;
}

pub struct Typed<M> {
    pub name: &'static str,
    pub sample: fn() -> M,
}

impl<M: Message + PartialEq + Debug> Golden for Typed<M> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn channel(&self) -> Channel {
        M::CHANNEL
    }

    fn encode(&self) -> Vec<u8> {
        protocol::encode(&(self.sample)()).unwrap()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::to_value((self.sample)()).unwrap()
    }

    fn decodes_to_sample(&self, payload: &[u8]) -> Result<(), String> {
        let decoded: M = protocol::decode(payload).map_err(|error| error.to_string())?;
        let sample = (self.sample)();
        if decoded == sample {
            Ok(())
        } else {
            Err(format!("decoded {decoded:?}\nexpected {sample:?}"))
        }
    }

    fn decode(&self, payload: &[u8]) -> Result<serde_json::Value, DecodeError> {
        let decoded: M = protocol::decode(payload)?;
        Ok(serde_json::to_value(decoded).unwrap())
    }
}

fn typed<M: Message + PartialEq + Debug + 'static>(
    name: &'static str,
    sample: fn() -> M,
) -> Box<dyn Golden> {
    Box::new(Typed { name, sample })
}

/// Every golden case; together they cover every channel.
pub fn cases() -> Vec<Box<dyn Golden>> {
    vec![
        typed("telemetry", telemetry_batch),
        typed("telemetry-ack-committed", ack_committed),
        typed("telemetry-ack-duplicate", ack_duplicate),
        typed("telemetry-ack-rejected", ack_rejected),
        typed("telemetry-ack-retry", ack_retry),
        typed("config", desired_config),
        typed("config-report-applied", config_applied),
        typed("config-report-rejected", config_rejected),
        typed("status-online", status_online),
        typed("status-offline", status_offline),
        typed("heartbeat", heartbeat),
        typed("live", live_samples),
        typed("live-control-start", live_start),
        typed("live-control-renew", live_renew),
        typed("live-control-stop", live_stop),
        typed("log", log_batch),
        typed("log-request", log_request),
        typed("log-dump", log_dump),
    ]
}

pub fn golden_dir(set: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(set)
}

/// Lowercase hex, 32 bytes per line.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2 + bytes.len() / 32 + 1);
    for line in bytes.chunks(32) {
        for byte in line {
            out.push_str(&format!("{byte:02x}"));
        }
        out.push('\n');
    }
    out
}

pub fn from_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert!(digits.len().is_multiple_of(2), "odd number of hex digits");
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

pub fn read_hex(path: &PathBuf) -> Vec<u8> {
    from_hex(
        &fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
}
