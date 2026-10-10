//! Size limits of the messages that are never split: their largest valid content still fits
//! the channel's limit, so a sender never has to choose between dropping fields and not sending.
//! The split messages (telemetry, logs, live values) are covered by `tests/split.rs`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use std::collections::BTreeMap;

use protocol::codec::{MAX_ISSUE_MESSAGE_BYTES, MAX_ISSUE_PATH_BYTES};
use protocol::config::{ConfigReport, MAX_DEVICES, MAX_REPORT_ISSUES};
use protocol::health::{
    DeviceStats, FailureCounts, FailureKind, LastFailure, MAX_EXCEPTION_CODES,
    MAX_FAILURE_DETAIL_BYTES, Offline, OfflineReason, Online, Status,
};
use protocol::live::{LiveAction, LiveControl, LiveSpec, MAX_LIVE_DEVICES, MAX_LIVE_TTL_S};
use protocol::logs::{LogLevel, LogRequest};
use protocol::telemetry::{AckOutcome, TelemetryAck};
use protocol::topic::{Channel, MIN_MAX_PACKET_BYTES};
use protocol::{
    AgentVersion, ConfigVersion, DeviceId, Issue, IssueCode, LeaseId, MAX_AGENT_VERSION_LEN,
    MAX_ID_LEN, Message, RequestId, Seq, SessionId, StreamId, UnixMillis, V1, encode,
};

/// The longest code an open enum accepts.
const LONGEST_CODE: usize = 64;

fn longest_id(prefix: char, index: usize) -> String {
    let index = index.to_string();
    let mut id = prefix.to_string().repeat(MAX_ID_LEN - index.len());
    id.push_str(&index);
    id
}

fn code() -> String {
    "c".repeat(LONGEST_CODE)
}

/// Encodes `message`, which must be valid, and checks that it fits its channel with room left
/// for the MQTT packet around it.
fn assert_fits<M: Message>(name: &str, message: &M) -> usize {
    let size = encode(message)
        .unwrap_or_else(|error| panic!("{name} must encode: {error}"))
        .len();
    let limit = M::CHANNEL.max_payload_bytes();
    assert!(size <= limit, "{name}: {size} bytes > {limit}");
    assert!(limit + 8 * 1024 <= MIN_MAX_PACKET_BYTES);
    size
}

#[test]
fn the_largest_telemetry_ack_fits() {
    let ack = TelemetryAck {
        v: V1,
        stream: StreamId::new(longest_id('s', 0)).unwrap(),
        seq: Seq::new(u64::MAX).unwrap(),
        outcome: AckOutcome::Rejected {
            code: IssueCode::Other(code()),
            // Three bytes per character, the most a code point below U+10000 takes.
            message: "測".repeat(MAX_ISSUE_MESSAGE_BYTES / 3),
        },
        server_time: UnixMillis::new(u64::MAX),
    };
    assert_fits("telemetry ack", &ack);
}

#[test]
fn the_largest_config_report_fits() {
    let issue = Issue::new(
        IssueCode::Other(code()),
        "p".repeat(MAX_ISSUE_PATH_BYTES),
        "m".repeat(MAX_ISSUE_MESSAGE_BYTES),
    );
    let version = ConfigVersion::new(u64::MAX).unwrap();
    // More issues than allowed: `rejected` keeps the limit.
    let report =
        ConfigReport::rejected(version, Some(version), vec![issue; MAX_REPORT_ISSUES + 10]);
    assert_eq!(report.issues.len(), MAX_REPORT_ISSUES);
    assert_eq!(report.issues.last().unwrap().code, IssueCode::TooMany);
    assert_fits("config report", &report);
}

#[test]
fn the_largest_status_fits() {
    let session = SessionId::new(longest_id('c', 0)).unwrap();
    let online = Status::Online(Online {
        v: V1,
        session: session.clone(),
        agent_version: AgentVersion::new("1".repeat(MAX_AGENT_VERSION_LEN)).unwrap(),
        protocol_rev: u16::MAX,
    });
    let offline = Status::Offline(Offline {
        v: V1,
        session,
        reason: OfflineReason::Other(code()),
    });
    assert_fits("online", &online);
    assert_fits("offline", &offline);
}

#[test]
fn the_largest_live_control_and_log_request_fit() {
    let start = LiveControl {
        v: V1,
        lease: LeaseId::new(longest_id('l', 0)).unwrap(),
        action: LiveAction::Start(LiveSpec {
            devices: (0..MAX_LIVE_DEVICES)
                .map(|index| DeviceId::new(longest_id('d', index)).unwrap())
                .collect(),
            interval_ms: 1_000,
            ttl_s: MAX_LIVE_TTL_S,
        }),
    };
    assert_fits("live control", &start);
    let request = LogRequest {
        v: V1,
        request: RequestId::new(longest_id('r', 0)).unwrap(),
        from: UnixMillis::new(0),
        to: UnixMillis::new(u64::MAX),
        min_level: LogLevel::Trace,
    };
    assert_fits("log request", &request);
}

/// One device's statistics with every field at its largest.
fn largest_device_stats(index: usize) -> DeviceStats {
    DeviceStats {
        device: DeviceId::new(longest_id('d', index)).unwrap(),
        last_success: Some(UnixMillis::new(u64::MAX)),
        points_total: u32::MAX,
        points_failed: u32::MAX,
        requests: u64::MAX,
        failures: FailureCounts {
            connect: u64::MAX,
            timeout: u64::MAX,
            exception: u64::MAX,
            unit_mismatch: u64::MAX,
            invalid_value: u64::MAX,
        },
        exception_codes: (0..MAX_EXCEPTION_CODES)
            .map(|offset| (u8::MAX - u8::try_from(offset).unwrap(), u64::MAX))
            .collect::<BTreeMap<_, _>>(),
        last_failure: Some(LastFailure {
            kind: FailureKind::Other(code()),
            at: UnixMillis::new(u64::MAX),
            exception_code: Some(u8::MAX),
            detail: Some("x".repeat(MAX_FAILURE_DETAIL_BYTES)),
        }),
    }
}

#[test]
fn a_heartbeat_for_the_largest_configuration_fits() {
    let mut heartbeat = common::heartbeat();
    heartbeat.devices = (0..MAX_DEVICES).map(largest_device_stats).collect();
    let size = assert_fits("heartbeat", &heartbeat);
    assert_eq!(Channel::Heartbeat.max_payload_bytes(), 1024 * 1024);
    // The bound is not wildly loose either: the worst case uses most of the limit.
    assert!(size > Channel::Heartbeat.max_payload_bytes() / 2, "{size}");

    heartbeat.devices.push(largest_device_stats(MAX_DEVICES));
    assert!(encode(&heartbeat).is_err(), "more than MAX_DEVICES devices");
}
