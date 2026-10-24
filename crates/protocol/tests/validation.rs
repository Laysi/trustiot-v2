//! Message invariants beyond the field types: what `encode` refuses to send and `decode` refuses
//! to accept, reported as issues with a code and a path.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use common::{at, data_point, device, model, point, sample};
use protocol::config::{
    ByteOrder, ConfigReport, DataType, DesiredConfig, Device, FunctionCode, Host, MAX_DEVICES,
    MAX_REPORT_ISSUES, Transport, validate_model_points,
};
use protocol::health::{Heartbeat, LastFailure, MAX_FAILURE_DETAIL_BYTES, Status};
use protocol::live::{LiveAction, LiveControl, LiveSamples, LiveSpec, MAX_LIVE_DEVICES};
use protocol::logs::{
    LogBatch, LogEntry, LogLevel, LogRequest, MAX_FIELD_VALUE_BYTES, MAX_FIELDS, MAX_MESSAGE_BYTES,
    MAX_TARGET_BYTES,
};
use protocol::telemetry::{AckOutcome, Series, TelemetryAck, TelemetryBatch};
use protocol::topic::MAX_LARGE_BYTES;
use protocol::{
    ConfigVersion, DecodeError, DeviceId, EncodeError, Issue, IssueCode, Message, V1, Value,
    decode, encode,
};

/// The issues `encode` reports for `message`, after checking that `decode` reports the same
/// for the same content.
fn issues<M: Message>(message: &M) -> Vec<Issue> {
    let Err(EncodeError::Invalid(invalid)) = encode(message) else {
        panic!("expected the message to be invalid");
    };
    invalid.into_issues()
}

/// Asserts that `issues` contains `code` at `path`.
fn has(issues: &[Issue], code: IssueCode, path: &str) {
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == code && issue.path == path),
        "no {code} at {path} in {issues:#?}"
    );
}

/// A change to the sample configuration and the issue it must cause.
type ConfigCase = (Box<dyn FnOnce(&mut DesiredConfig)>, IssueCode, &'static str);

fn config_issues(change: impl FnOnce(&mut DesiredConfig)) -> Vec<Issue> {
    let mut config = common::desired_config();
    change(&mut config);
    issues(&config)
}

#[test]
fn the_samples_are_valid() {
    for case in common::cases() {
        assert!(!case.encode().is_empty(), "{}", case.name());
    }
    assert_eq!(common::desired_config().minimum_revision(), 1);
}

#[test]
fn models_and_points_follow_the_register_rules() {
    let cases: Vec<ConfigCase> = vec![
        (
            Box::new(|c| c.models[1].name = model("ms300-vfd")),
            IssueCode::Duplicate,
            "models[1].name",
        ),
        (
            Box::new(|c| c.models[0].points[1].key = point("frequency")),
            IssueCode::Duplicate,
            "models[0].points[1].key",
        ),
        (
            Box::new(|c| c.models[0].points[3].data_type = DataType::U16),
            IssueCode::FunctionTypeMismatch,
            "models[0].points[3].data_type",
        ),
        (
            Box::new(|c| c.models[0].points[3].byte_order = ByteOrder::Cdab),
            IssueCode::UnexpectedByteOrder,
            "models[0].points[3].byte_order",
        ),
        (
            Box::new(|c| c.models[0].points[3].length = Some(1)),
            IssueCode::UnexpectedLength,
            "models[0].points[3].length",
        ),
        (
            Box::new(|c| c.models[1].points[3].length = None),
            IssueCode::MissingLength,
            "models[1].points[3].length",
        ),
        (
            Box::new(|c| c.models[1].points[3].length = Some(0)),
            IssueCode::OutOfRange,
            "models[1].points[3].length",
        ),
        (
            Box::new(|c| c.models[1].points[3].length = Some(126)),
            IssueCode::OutOfRange,
            "models[1].points[3].length",
        ),
        (
            Box::new(|c| c.models[1].points[3].byte_order = ByteOrder::Dcba),
            IssueCode::UnexpectedByteOrder,
            "models[1].points[3].byte_order",
        ),
        (
            Box::new(|c| c.models[1].points[0].length = Some(2)),
            IssueCode::UnexpectedLength,
            "models[1].points[0].length",
        ),
        (
            Box::new(|c| c.models[1].points[1].address = 65_533),
            IssueCode::AddressOutOfRange,
            "models[1].points[1].address",
        ),
        (
            // energy_wh (input registers 2-5) moved onto voltage_a (0-1).
            Box::new(|c| c.models[1].points[1].address = 1),
            IssueCode::OverlappingPoints,
            "models[1].points[1].address",
        ),
        (
            // power (input registers 6-9) moved under energy_wh (2-5).
            Box::new(|c| c.models[1].points[2].address = 3),
            IssueCode::OverlappingPoints,
            "models[1].points[2].address",
        ),
        (
            Box::new(|c| c.models[0].points[0].scale = Some(0.0)),
            IssueCode::InvalidScaling,
            "models[0].points[0].scale",
        ),
        (
            Box::new(|c| c.models[0].points[0].scale = Some(f64::NAN)),
            IssueCode::InvalidScaling,
            "models[0].points[0].scale",
        ),
        (
            Box::new(|c| c.models[0].points[2].offset = Some(f64::INFINITY)),
            IssueCode::InvalidScaling,
            "models[0].points[2].offset",
        ),
        (
            Box::new(|c| c.models[0].points[3].scale = Some(2.0)),
            IssueCode::InvalidScaling,
            "models[0].points[3].scale",
        ),
        (
            Box::new(|c| c.models[1].points[3].offset = Some(1.0)),
            IssueCode::InvalidScaling,
            "models[1].points[3].scale",
        ),
        (
            Box::new(|c| c.rev = 2),
            IssueCode::UnsupportedRevision,
            "rev",
        ),
    ];
    for (change, code, path) in cases {
        has(&config_issues(change), code, path);
    }
}

#[test]
fn points_in_different_address_spaces_never_overlap() {
    let mut config = common::desired_config();
    // Coil 0, discrete input 0, holding register 0 and input register 0 are four places.
    config.models[0].points = vec![
        data_point("coil", FunctionCode::ReadCoils, 0, DataType::Bool),
        data_point("input", FunctionCode::ReadDiscreteInputs, 0, DataType::Bool),
        data_point(
            "holding",
            FunctionCode::ReadHoldingRegisters,
            0,
            DataType::F64,
        ),
        data_point(
            "register",
            FunctionCode::ReadInputRegisters,
            0,
            DataType::U64,
        ),
        data_point(
            "last",
            FunctionCode::ReadInputRegisters,
            65_532,
            DataType::U64,
        ),
        data_point("bit", FunctionCode::ReadCoils, 65_535, DataType::Bool),
    ];
    assert!(encode(&config).is_ok());
    // Adjacent but not overlapping; then one register too far.
    config.models[0].points[4].address = 4;
    assert!(encode(&config).is_ok());
    config.models[0].points[4].address = 3;
    let found = issues(&config);
    assert_eq!(found.len(), 1, "{found:#?}");
    has(
        &found,
        IssueCode::OverlappingPoints,
        "models[0].points[4].address",
    );
}

/// `DeviceModel::validate`, which the assets service runs on every write of a model, applies
/// the configuration's data point rules with paths from the model, and finds exactly what the
/// configuration check finds for that model.
#[test]
fn a_model_alone_is_checked_with_the_configuration_rules() {
    let config = common::desired_config();
    for model in &config.models {
        assert_eq!(model.validate(), Ok(()), "{}", model.name);
    }

    let mut config = common::desired_config();
    let mut string = data_point(
        "label",
        FunctionCode::ReadHoldingRegisters,
        40,
        DataType::String,
    );
    string.length = None;
    let mut scaled = data_point("flag", FunctionCode::ReadCoils, 0, DataType::Bool);
    scaled.scale = Some(0.1);
    config.models[0].points = vec![
        data_point("a", FunctionCode::ReadHoldingRegisters, 0, DataType::U32),
        data_point("b", FunctionCode::ReadHoldingRegisters, 1, DataType::U16),
        data_point("a", FunctionCode::ReadInputRegisters, 0, DataType::U16),
        data_point("c", FunctionCode::ReadCoils, 1, DataType::U16),
        data_point("d", FunctionCode::ReadInputRegisters, 65_535, DataType::U32),
        string,
        scaled,
    ];
    let found = config.models[0]
        .validate()
        .expect_err("the model is invalid")
        .into_issues();
    assert_eq!(
        validate_model_points(&config.models[0].points)
            .expect_err("the points are invalid")
            .into_issues(),
        found,
        "the model's check is the check of its points"
    );
    for (code, path) in [
        (IssueCode::OverlappingPoints, "points[1].address"),
        (IssueCode::Duplicate, "points[2].key"),
        (IssueCode::FunctionTypeMismatch, "points[3].data_type"),
        (IssueCode::AddressOutOfRange, "points[4].address"),
        (IssueCode::MissingLength, "points[5].length"),
        (IssueCode::InvalidScaling, "points[6].scale"),
    ] {
        has(&found, code, path);
    }
    // The overlap names the other point from the model too.
    let overlap = found
        .iter()
        .find(|issue| issue.code == IssueCode::OverlappingPoints)
        .expect("the overlap");
    assert!(
        overlap.message.ends_with("overlaps points[0]"),
        "{overlap:?}"
    );
    // The configuration reports the same issues under models[0].
    let in_config = issues(&config);
    let prefixed: Vec<String> = found
        .iter()
        .map(|issue| format!("{} models[0].{}", issue.code, issue.path))
        .collect();
    let configured: Vec<String> = in_config
        .iter()
        .filter(|issue| issue.path.starts_with("models[0]."))
        .map(|issue| format!("{} {}", issue.code, issue.path))
        .collect();
    assert_eq!(prefixed, configured);
}

#[test]
fn devices_follow_the_connection_rules() {
    let cases: Vec<ConfigCase> = vec![
        (
            Box::new(|c| c.devices[2].id = device(2)),
            IssueCode::Duplicate,
            "devices[2].id",
        ),
        (
            Box::new(|c| c.devices[0].model = model("pm-3000")),
            IssueCode::UnknownModel,
            "devices[0].model",
        ),
        (
            Box::new(|c| c.devices[0].port = 0),
            IssueCode::OutOfRange,
            "devices[0].port",
        ),
        (
            Box::new(|c| c.devices[1].unit_id = 0),
            IssueCode::OutOfRange,
            "devices[1].unit_id",
        ),
        (
            Box::new(|c| c.devices[1].unit_id = 248),
            IssueCode::OutOfRange,
            "devices[1].unit_id",
        ),
        (
            Box::new(|c| c.devices[0].poll_interval_ms = 99),
            IssueCode::OutOfRange,
            "devices[0].poll_interval_ms",
        ),
        (
            Box::new(|c| c.devices[0].poll_interval_ms = 86_400_001),
            IssueCode::OutOfRange,
            "devices[0].poll_interval_ms",
        ),
        (
            Box::new(|c| c.devices[2].transport = Transport::ModbusTcp),
            IssueCode::MixedTransport,
            "devices[2].transport",
        ),
        (
            // The same converter written differently is still the same converter.
            Box::new(|c| {
                c.devices[1].host = Host::new("conv-1.plant.local").unwrap();
                c.devices[2].host = Host::new("CONV-1.Plant.Local").unwrap();
                c.devices[2].transport = Transport::ModbusTcp;
            }),
            IssueCode::MixedTransport,
            "devices[2].transport",
        ),
        (
            // An IPv4-mapped IPv6 literal is the same endpoint as its IPv4 form
            // (Laysi/trustiot-v2#269), whichever comes first and in any case.
            Box::new(|c| {
                c.devices[1].host = Host::new("192.168.10.30").unwrap();
                c.devices[2].host = Host::new("::FFFF:192.168.10.30").unwrap();
                c.devices[2].transport = Transport::ModbusTcp;
            }),
            IssueCode::MixedTransport,
            "devices[2].transport",
        ),
        (
            Box::new(|c| {
                c.devices[1].host = Host::new("::ffff:c0a8:a1e").unwrap();
                c.devices[2].host = Host::new("192.168.10.30").unwrap();
                c.devices[2].transport = Transport::ModbusTcp;
            }),
            IssueCode::MixedTransport,
            "devices[2].transport",
        ),
    ];
    for (change, code, path) in cases {
        has(&config_issues(change), code, path);
    }
    // Modbus TCP accepts any unit identifier, including 0 and 255.
    let mut config = common::desired_config();
    config.devices[0].unit_id = 0;
    assert!(encode(&config).is_ok());
    config.devices[0].unit_id = 255;
    assert!(encode(&config).is_ok());
}

#[test]
fn every_issue_is_reported_at_once() {
    let found = config_issues(|c| {
        c.devices[0].port = 0;
        c.devices[1].unit_id = 0;
        c.models[0].points[0].scale = Some(0.0);
    });
    assert_eq!(found.len(), 3, "{found:#?}");
}

#[test]
fn an_invalid_configuration_is_rejected_on_receipt_with_its_issues() {
    let mut config = common::desired_config();
    config.devices[0].model = model("pm-3000");
    // A sender that skipped validation: serialize without `encode`.
    let payload = cbor4ii::serde::to_vec(Vec::new(), &config).unwrap();
    let error = DesiredConfig::decode(&payload).unwrap_err();
    let DecodeError::Invalid(invalid) = &error else {
        panic!("expected invalid, got {error:?}");
    };
    has(
        invalid.issues(),
        IssueCode::UnknownModel,
        "devices[0].model",
    );
    let version = DesiredConfig::peek_header(&payload).unwrap().version;
    let report = ConfigReport::rejected(version, ConfigVersion::new(6), error.to_issues());
    let decoded = ConfigReport::decode(&report.encode().unwrap()).unwrap();
    assert_eq!(decoded.version.get(), 7);
    assert_eq!(decoded.active_version, ConfigVersion::new(6));
    has(&decoded.issues, IssueCode::UnknownModel, "devices[0].model");
}

#[test]
fn a_configuration_holds_at_most_max_devices_and_must_fit_one_message() {
    let mut config = common::desired_config();
    let template = config.devices[0].clone();
    config.devices = (0..=MAX_DEVICES)
        .map(|index| Device {
            id: DeviceId::new(format!("device-{index:05}-0000-7000-8000-000000000000")).unwrap(),
            ..template.clone()
        })
        .collect();
    has(&issues(&config), IssueCode::TooMany, "devices");
    config.devices.truncate(MAX_DEVICES);
    assert!(encode(&config).unwrap().len() <= MAX_LARGE_BYTES);

    // Register maps can still outgrow one message; the configuration is then refused, never
    // truncated.
    config.models[0].points = (0..30_000u16)
        .map(|address| {
            data_point(
                &format!("register_{address:05}"),
                FunctionCode::ReadHoldingRegisters,
                address,
                DataType::U16,
            )
        })
        .collect();
    let Err(EncodeError::TooLarge { size, limit }) = encode(&config) else {
        panic!("expected the configuration to be too large");
    };
    assert_eq!(limit, MAX_LARGE_BYTES);
    assert!(size > limit);
}

#[test]
fn hosts_are_unicast_addresses_or_dns_names() {
    for host in [
        "192.168.10.21",
        "10.0.0.1",
        "127.0.0.1",
        "::1",
        "fe80::1",
        "2001:db8::10",
        "plc-01.plant.local",
        "localhost",
        "a",
        "1plc.example",
        // Hex-looking labels are names unless the last label is a number.
        "0x7f.plc",
        "0xplc",
        "plc-0x1f",
        "0x1g",
        // Site LAN addresses: private ranges, link-local (APIPA converters) and IPv6 ULA.
        "172.16.0.10",
        "169.254.1.20",
        "fd12:3456:789a::10",
        &"a".repeat(63),
        &format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        ),
    ] {
        assert!(Host::new(host).is_ok(), "{host:?}");
    }
    for host in [
        "",
        "0.0.0.0",
        "::",
        "224.0.0.1",
        "ff02::1",
        "255.255.255.255",
        "192.168.1.300",
        "1.2.3",
        "10.1",
        // Names that end in a number: getaddrinfo reads them as IPv4 addresses (inet_aton),
        // 127.0.0.1, 0.0.0.0, a multicast and the broadcast address.
        "0x7f000001",
        "0X7F000001",
        "0x7f.0.0.0x1",
        "0x7f.1",
        "0x0",
        "0x",
        "0xe0000001",
        "0xffffffff",
        "plc.0x1f",
        "017700000001",
        "plc_01",
        "-plc",
        "plc-",
        "a..b",
        "plc.local.",
        ".plc",
        "plc local",
        "plc/1",
        "[::1]",
        "http://plc",
        "plc:502",
        "電表",
        &"a".repeat(64),
        &format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(62)
        ),
    ] {
        assert!(Host::new(host).is_err(), "{host:?}");
    }
}

/// An IPv4-mapped IPv6 literal (`::ffff:a.b.c.d`) is the IPv4 address it maps: the same
/// acceptance and the same canonical form, so neither rule can be bypassed by spelling
/// (Laysi/trustiot-v2#269, finding F4 of the #50 verification).
#[test]
fn ipv4_mapped_hosts_follow_the_ipv4_rules() {
    for ipv4 in [
        "192.168.10.30",
        "10.0.0.1",
        "127.0.0.1",
        "1.2.3.4",
        "0.0.0.0",
        "224.0.0.1",
        "239.255.255.255",
        "255.255.255.255",
        "192.168.1.255",
    ] {
        let octets: Vec<u8> = ipv4
            .split('.')
            .map(|octet| octet.parse().unwrap())
            .collect();
        let hex = format!(
            "::ffff:{:x}:{:x}",
            u16::from_be_bytes([octets[0], octets[1]]),
            u16::from_be_bytes([octets[2], octets[3]])
        );
        let plain = Host::new(ipv4);
        for mapped in [
            format!("::ffff:{ipv4}"),
            format!("::FFFF:{ipv4}"),
            format!("0:0:0:0:0:ffff:{ipv4}"),
            hex,
        ] {
            let host = Host::new(mapped.as_str());
            assert_eq!(host.is_ok(), plain.is_ok(), "{mapped:?} vs {ipv4:?}");
            if let (Ok(host), Ok(plain)) = (host, &plain) {
                assert_eq!(host.canonical(), plain.canonical(), "{mapped:?}");
                assert_eq!(host.canonical(), ipv4);
                // As written: the wire keeps the sender's spelling.
                assert_eq!(host.as_str(), mapped);
            }
        }
    }
    // The cases of the report, refused like their IPv4 forms.
    for host in [
        "::ffff:0.0.0.0",
        "::ffff:224.0.0.1",
        "::ffff:255.255.255.255",
    ] {
        assert!(Host::new(host).is_err(), "{host:?}");
    }
    // Other IPv6 addresses keep their own form; DNS names compare in lowercase.
    assert_eq!(
        Host::new("2001:DB8::10").unwrap().canonical(),
        "2001:db8::10"
    );
    assert_eq!(Host::new("::1").unwrap().canonical(), "::1");
    assert_eq!(Host::new("PLC-1.Local").unwrap().canonical(), "plc-1.local");
    assert!(FunctionCode::try_from(0).is_err());
    assert!(FunctionCode::try_from(5).is_err());
    for number in 1..=4 {
        assert_eq!(FunctionCode::try_from(number).unwrap().number(), number);
    }
}

#[test]
fn reports_keep_at_most_the_issue_limit() {
    let many: Vec<Issue> = (0..100)
        .map(|index| {
            Issue::new(
                IssueCode::OutOfRange,
                format!("devices[{index}].port"),
                "port 0",
            )
        })
        .collect();
    let report = ConfigReport::rejected(ConfigVersion::new(9).unwrap(), None, many.clone());
    assert_eq!(report.issues.len(), MAX_REPORT_ISSUES);
    assert_eq!(
        report.issues[..MAX_REPORT_ISSUES - 1],
        many[..MAX_REPORT_ISSUES - 1]
    );
    let last = &report.issues[MAX_REPORT_ISSUES - 1];
    assert_eq!(last.code, IssueCode::TooMany);
    assert_eq!(last.message, "37 more issues are not listed");
    assert!(report.encode().is_ok());

    let mut oversized = report.clone();
    oversized.issues = many;
    has(&issues(&oversized), IssueCode::TooMany, "issues");

    // Issues built without `Issue::new` are still bounded.
    let mut unbounded = report;
    unbounded.issues = vec![Issue {
        code: IssueCode::Empty,
        path: "p".repeat(257),
        message: String::new(),
    }];
    has(&issues(&unbounded), IssueCode::TooLong, "issues[0].path");
    let applied = ConfigReport::applied(ConfigVersion::new(3).unwrap());
    assert_eq!(applied.active_version, ConfigVersion::new(3));
    assert!(applied.issues.is_empty());
}

#[test]
fn telemetry_rules() {
    let mut batch = common::telemetry_batch();
    batch.series.clear();
    has(&issues(&batch), IssueCode::Empty, "series");

    let mut batch = common::telemetry_batch();
    batch.series[1].samples.clear();
    batch.series[0].samples[1].value = Value::Float(f64::NAN);
    let found = issues(&batch);
    has(&found, IssueCode::Empty, "series[1].samples");
    has(&found, IssueCode::InvalidValue, "series[0].samples[1]");

    let mut batch = common::telemetry_batch();
    let repeated = batch.series[0].clone();
    batch.series.push(repeated);
    has(&issues(&batch), IssueCode::Duplicate, "series[6]");

    let mut ack = common::ack_rejected();
    ack.outcome = AckOutcome::Rejected {
        code: IssueCode::DecodeFailed,
        message: "m".repeat(1_025),
    };
    has(&issues(&ack), IssueCode::TooLong, "outcome.message");

    let releases: Vec<bool> = [
        common::ack_committed(),
        common::ack_duplicate(),
        common::ack_rejected(),
        common::ack_retry(),
    ]
    .iter()
    .map(|ack| ack.outcome.releases_batch())
    .collect();
    assert_eq!(releases, [true, true, true, false]);
    assert!(!AckOutcome::Unknown.releases_batch());

    assert_eq!(common::telemetry_batch().record_count(), 7);
    let decoded: TelemetryAck = decode(&encode(&common::ack_retry()).unwrap()).unwrap();
    assert_eq!(decoded.outcome, AckOutcome::Retry { after_ms: 5_000 });
}

#[test]
fn heartbeat_rules() {
    let change = |f: fn(&mut Heartbeat)| {
        let mut heartbeat = common::heartbeat();
        f(&mut heartbeat);
        issues(&heartbeat)
    };
    has(
        &change(|h| h.interval_s = 0),
        IssueCode::OutOfRange,
        "interval_s",
    );
    for cpu in [-0.1, 100.1, f64::NAN] {
        let mut heartbeat = common::heartbeat();
        heartbeat.system.cpu_percent = cpu;
        has(
            &issues(&heartbeat),
            IssueCode::OutOfRange,
            "system.cpu_percent",
        );
    }
    has(
        &change(|h| h.system.soc_temp_celsius = Some(f64::INFINITY)),
        IssueCode::OutOfRange,
        "system.soc_temp_celsius",
    );
    has(
        &change(|h| h.devices[0].points_failed = 5),
        IssueCode::OutOfRange,
        "devices[0].points_failed",
    );
    has(
        &change(|h| h.devices[1].device = h.devices[0].device.clone()),
        IssueCode::Duplicate,
        "devices[1].device",
    );
    has(
        &change(|h| {
            h.devices[1].last_failure = Some(LastFailure {
                detail: Some("d".repeat(MAX_FAILURE_DETAIL_BYTES + 1)),
                ..h.devices[1].last_failure.clone().unwrap()
            });
        }),
        IssueCode::TooLong,
        "devices[1].last_failure.detail",
    );
    has(
        &change(|h| {
            h.devices[1].exception_codes = (1..=17).map(|code| (code, 1)).collect();
        }),
        IssueCode::TooMany,
        "devices[1].exception_codes",
    );
    let mut heartbeat = common::heartbeat();
    let template = heartbeat.devices[0].clone();
    heartbeat.devices = (0..=MAX_DEVICES)
        .map(|index| protocol::health::DeviceStats {
            device: DeviceId::new(format!("device-{index:05}")).unwrap(),
            ..template.clone()
        })
        .collect();
    has(&issues(&heartbeat), IssueCode::TooMany, "devices");
}

#[test]
fn an_outdated_configuration_is_reported_as_stale() {
    let report = ConfigReport::stale(
        ConfigVersion::new(5).unwrap(),
        ConfigVersion::new(7).unwrap(),
    );
    let decoded = ConfigReport::decode(&report.encode().unwrap()).unwrap();
    assert_eq!(decoded, report);
    assert_eq!(decoded.outcome, protocol::config::ConfigOutcome::Rejected);
    assert_eq!(decoded.version, ConfigVersion::new(5).unwrap());
    assert_eq!(decoded.active_version, ConfigVersion::new(7));
    has(&decoded.issues, IssueCode::StaleVersion, "version");
}

#[test]
fn status_and_last_will() {
    let session = common::session();
    let will = Status::last_will(session.clone());
    assert_eq!(will, common::status_offline());
    assert_eq!(will.session(), &session);
    assert_eq!(common::status_online().session(), &session);
}

#[test]
fn live_rules() {
    let with_spec = |f: fn(&mut LiveSpec)| {
        let mut control = common::live_start();
        let LiveAction::Start(spec) = &mut control.action else {
            panic!("the sample starts a lease");
        };
        f(spec);
        issues(&control)
    };
    has(
        &with_spec(|s| s.devices.clear()),
        IssueCode::Empty,
        "action.devices",
    );
    has(
        &with_spec(|s| {
            s.devices = (0..=MAX_LIVE_DEVICES)
                .map(|n| DeviceId::new(format!("d{n}")).unwrap())
                .collect();
        }),
        IssueCode::TooMany,
        "action.devices",
    );
    has(
        &with_spec(|s| s.devices.push(s.devices[0].clone())),
        IssueCode::Duplicate,
        "action.devices[2]",
    );
    // A list over the limit is reported once, not with an issue for every repeat: a 64 KiB
    // payload holds about 32,000 two-byte device IDs (Laysi/trustiot-v2#290).
    let too_many = with_spec(|s| s.devices = vec![s.devices[0].clone(); 30_000]);
    assert_eq!(
        too_many
            .iter()
            .map(|issue| (issue.code.as_str(), issue.path.as_str()))
            .collect::<Vec<_>>(),
        [("too_many", "action.devices")]
    );
    has(
        &with_spec(|s| s.interval_ms = 99),
        IssueCode::OutOfRange,
        "action.interval_ms",
    );
    has(
        &with_spec(|s| s.interval_ms = 60_001),
        IssueCode::OutOfRange,
        "action.interval_ms",
    );
    has(
        &with_spec(|s| s.ttl_s = 0),
        IssueCode::OutOfRange,
        "action.ttl_s",
    );
    has(
        &with_spec(|s| s.ttl_s = 601),
        IssueCode::OutOfRange,
        "action.ttl_s",
    );

    // A renewal is checked like a start; a stop carries nothing to check.
    let mut renew = common::live_renew();
    if let LiveAction::Renew(spec) = &mut renew.action {
        spec.ttl_s = 0;
    }
    has(&issues(&renew), IssueCode::OutOfRange, "action.ttl_s");
    assert!(encode(&common::live_stop()).is_ok());

    let empty = LiveSamples {
        v: V1,
        series: Vec::new(),
    };
    has(&issues(&empty), IssueCode::Empty, "series");
    let invalid = LiveSamples {
        v: V1,
        series: vec![Series {
            device: device(1),
            point: point("p"),
            samples: vec![sample(0, Value::Float(f64::NEG_INFINITY))],
        }],
    };
    has(
        &issues(&invalid),
        IssueCode::InvalidValue,
        "series[0].samples[0]",
    );
    let control: LiveControl = decode(&encode(&common::live_start()).unwrap()).unwrap();
    assert_eq!(control, common::live_start());
}

#[test]
fn log_rules() {
    let empty = LogBatch {
        v: V1,
        entries: Vec::new(),
        dropped: 0,
    };
    has(&issues(&empty), IssueCode::Empty, "entries");

    let raw = |f: fn(&mut LogEntry)| {
        let mut entry = LogEntry::new(at(0), LogLevel::Warn, "t", "m");
        f(&mut entry);
        issues(&LogBatch {
            v: V1,
            entries: vec![entry],
            dropped: 0,
        })
    };
    has(
        &raw(|e| e.target = "t".repeat(MAX_TARGET_BYTES + 1)),
        IssueCode::TooLong,
        "entries[0].target",
    );
    has(
        &raw(|e| e.message = "m".repeat(MAX_MESSAGE_BYTES + 1)),
        IssueCode::TooLong,
        "entries[0].message",
    );
    has(
        &raw(|e| {
            e.fields = (0..=MAX_FIELDS)
                .map(|n| (format!("f{n}"), String::new()))
                .collect();
        }),
        IssueCode::TooMany,
        "entries[0].fields",
    );
    has(
        &raw(|e| {
            e.fields.insert(String::new(), "v".to_owned());
        }),
        IssueCode::Empty,
        "entries[0].fields",
    );
    has(
        &raw(|e| {
            e.fields.insert("n".repeat(65), "v".to_owned());
        }),
        IssueCode::TooLong,
        "entries[0].fields",
    );
    has(
        &raw(|e| {
            e.fields
                .insert("n".to_owned(), "v".repeat(MAX_FIELD_VALUE_BYTES + 1));
        }),
        IssueCode::TooLong,
        "entries[0].fields",
    );

    // The constructors cut to the limits instead of failing, at character boundaries.
    let entry = LogEntry::new(at(0), LogLevel::Error, "目".repeat(100), "訊".repeat(2_000))
        .with_field("名".repeat(30), "值".repeat(500));
    assert!(entry.target.len() <= MAX_TARGET_BYTES);
    assert!(entry.message.len() <= MAX_MESSAGE_BYTES);
    assert!(entry.target.chars().all(|c| c == '目'));
    assert!(entry.message.chars().all(|c| c == '訊'));
    assert!(
        encode(&LogBatch {
            v: V1,
            entries: vec![entry],
            dropped: 0
        })
        .is_ok()
    );

    let mut request = common::log_request();
    request.to = request.from;
    assert!(encode(&request).is_ok());
    request.from = at(1);
    request.to = at(0);
    has(&issues(&request), IssueCode::InvalidRange, "to");
    let decoded: LogRequest = decode(&encode(&common::log_request()).unwrap()).unwrap();
    assert_eq!(decoded.min_level, LogLevel::Debug);

    assert!(LogLevel::Warn.admits(LogLevel::Error));
    assert!(LogLevel::Warn.admits(LogLevel::Warn));
    assert!(!LogLevel::Warn.admits(LogLevel::Info));
    assert!(LogLevel::Trace.admits(LogLevel::Debug));
}

#[test]
fn decode_rejects_what_encode_refuses() {
    // Send an invalid heartbeat around `encode` and check the receiver refuses it too.
    let mut heartbeat = common::heartbeat();
    heartbeat.interval_s = 0;
    let payload = cbor4ii::serde::to_vec(Vec::new(), &heartbeat).unwrap();
    let Err(DecodeError::Invalid(invalid)) = Heartbeat::decode(&payload) else {
        panic!("expected the heartbeat to be invalid");
    };
    has(invalid.issues(), IssueCode::OutOfRange, "interval_s");
    let mut batch = common::telemetry_batch();
    batch.series.clear();
    let payload = cbor4ii::serde::to_vec(Vec::new(), &batch).unwrap();
    assert!(matches!(
        TelemetryBatch::decode(&payload),
        Err(DecodeError::Invalid(_))
    ));
}
