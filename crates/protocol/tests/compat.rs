//! Version and compatibility rules of the wire format (crate README, "版本與相容性"):
//!
//! - forward: a receiver ignores fields, sample elements and open-enum codes a newer peer adds;
//! - backward: absent optional fields take their default (`tests/golden.rs` additionally keeps
//!   payloads of the first v1 release decoding);
//! - refused: another major version `v`, or a `rev` above this build's revision.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use cbor4ii::core::Value as Cbor;
use common::cases;
use protocol::config::{ConfigOutcome, ConfigReport, DesiredConfig};
use protocol::health::{FailureKind, Heartbeat, OfflineReason, RootCrlStatus, Status};
use protocol::live::{LiveAction, LiveControl};
use protocol::logs::{LogBatch, LogCoverage, LogDump};
use protocol::telemetry::{AckOutcome, TelemetryAck};
use protocol::{DecodeError, EncodeError, IssueCode, Message, PROTOCOL_REVISION, decode, encode};

fn to_cbor(payload: &[u8]) -> Cbor {
    cbor4ii::serde::from_slice(payload).unwrap()
}

fn from_cbor(value: &Cbor) -> Vec<u8> {
    cbor4ii::serde::to_vec(Vec::new(), value).unwrap()
}

fn text(value: &str) -> Cbor {
    Cbor::Text(value.to_owned())
}

/// The item at a dot-separated path of map keys and array indexes; `""` is the root.
fn at<'a>(value: &'a mut Cbor, path: &str) -> &'a mut Cbor {
    let mut current = value;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        current = match current {
            Cbor::Map(entries) => entries
                .iter_mut()
                .find_map(|(key, value)| (*key == text(segment)).then_some(value))
                .unwrap_or_else(|| panic!("no field {segment} in {path}")),
            Cbor::Array(items) => items.get_mut(segment.parse::<usize>().unwrap()).unwrap(),
            other => panic!("{other:?} has no {segment}"),
        };
    }
    current
}

fn entries<'a>(value: &'a mut Cbor, path: &str) -> &'a mut Vec<(Cbor, Cbor)> {
    match at(value, path) {
        Cbor::Map(entries) => entries,
        other => panic!("{path} is not a map: {other:?}"),
    }
}

/// Sets `key` in the map at `path`, adding it when absent.
fn set(value: &mut Cbor, path: &str, key: &str, new: Cbor) {
    let entries = entries(value, path);
    match entries.iter_mut().find(|(name, _)| *name == text(key)) {
        Some((_, old)) => *old = new,
        None => entries.push((text(key), new)),
    }
}

fn remove(value: &mut Cbor, path: &str, key: &str) {
    let entries = entries(value, path);
    let before = entries.len();
    entries.retain(|(name, _)| *name != text(key));
    assert_eq!(entries.len() + 1, before, "no field {key} at {path:?}");
}

/// A field value of every CBOR type serde can produce, nested.
fn future_value() -> Cbor {
    Cbor::Map(vec![(
        text("nested"),
        Cbor::Array(vec![
            Cbor::Integer(1),
            Cbor::Integer(-1),
            Cbor::Integer(i128::from(u64::MAX)),
            Cbor::Float(2.5),
            text("text"),
            Cbor::Bytes(vec![0x00, 0xff]),
            Cbor::Null,
            Cbor::Bool(true),
            Cbor::Map(vec![(text("deep"), Cbor::Array(Vec::new()))]),
        ]),
    )])
}

/// What a newer peer could send: an unknown field first and last in every message map, and an
/// extra element in every sample. Leaves the data maps `fields` and `exception_codes` alone.
fn add_future_content(value: &mut Cbor, data_map: bool) {
    match value {
        Cbor::Map(entries) => {
            for (key, inner) in entries.iter_mut() {
                if let Cbor::Text(name) = key {
                    if name == "samples"
                        && let Cbor::Array(samples) = &mut *inner
                    {
                        for sample in samples {
                            if let Cbor::Array(elements) = sample {
                                elements.push(text("quality: good"));
                            }
                        }
                    }
                    add_future_content(
                        inner,
                        matches!(name.as_str(), "fields" | "exception_codes"),
                    );
                }
            }
            if !data_map {
                entries.insert(0, (text("a_future_field"), future_value()));
                entries.push((text("z_future_field"), future_value()));
            }
        }
        Cbor::Array(items) => {
            for item in items {
                add_future_content(item, false);
            }
        }
        _ => {}
    }
}

#[test]
fn fields_and_sample_elements_from_newer_peers_are_ignored() {
    for case in cases() {
        let mut value = to_cbor(&case.encode());
        add_future_content(&mut value, false);
        let payload = from_cbor(&value);
        assert!(payload.len() > case.encode().len());
        case.decodes_to_sample(&payload)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name()));
    }
}

fn reencode<M: Message>(message: &M, change: impl FnOnce(&mut Cbor)) -> Vec<u8> {
    let mut value = to_cbor(&encode(message).unwrap());
    change(&mut value);
    from_cbor(&value)
}

#[test]
fn unknown_codes_of_open_enums_are_kept() {
    let payload = reencode(&common::config_rejected(), |value| {
        set(value, "", "outcome", text("partially_applied"));
        set(value, "issues.0", "code", text("register_gap"));
    });
    let report: ConfigReport = decode(&payload).unwrap();
    assert_eq!(
        report.outcome,
        ConfigOutcome::Other("partially_applied".to_owned())
    );
    assert_eq!(
        report.issues[0].code,
        IssueCode::Other("register_gap".to_owned())
    );
    // A relay can pass the codes on unchanged.
    assert_eq!(encode(&report).unwrap(), payload);

    let payload = reencode(&common::status_offline(), |value| {
        set(value, "", "reason", text("power_loss"));
    });
    let Status::Offline(offline) = decode::<Status>(&payload).unwrap() else {
        panic!("expected offline");
    };
    assert_eq!(
        offline.reason,
        OfflineReason::Other("power_loss".to_owned())
    );

    let payload = reencode(&common::heartbeat(), |value| {
        set(value, "devices.1.last_failure", "kind", text("crc_error"));
    });
    let heartbeat: Heartbeat = decode(&payload).unwrap();
    assert_eq!(
        heartbeat.devices[1].last_failure.as_ref().unwrap().kind,
        FailureKind::Other("crc_error".to_owned())
    );

    let payload = reencode(&common::log_dump(), |value| {
        set(value, "", "coverage", text("sampled"));
    });
    let dump: LogDump = decode(&payload).unwrap();
    assert_eq!(dump.coverage, LogCoverage::Other("sampled".to_owned()));

    let payload = reencode(&common::ack_rejected(), |value| {
        set(value, "outcome", "code", text("quota_exceeded"));
    });
    let ack: TelemetryAck = decode(&payload).unwrap();
    assert!(
        matches!(&ack.outcome, AckOutcome::Rejected { code, .. } if *code == IssueCode::Other("quota_exceeded".to_owned()))
    );
}

#[test]
fn malformed_open_enum_codes_are_refused() {
    for code in ["", "Applied", "has space", "drop table", &"a".repeat(65)] {
        let payload = reencode(&common::config_applied(), |value| {
            set(value, "", "outcome", text(code));
        });
        assert!(
            matches!(
                decode::<ConfigReport>(&payload),
                Err(DecodeError::Malformed(_))
            ),
            "{code:?}"
        );
    }
    let mut report = common::config_applied();
    report.outcome = ConfigOutcome::Other("Not A Code".to_owned());
    assert!(matches!(encode(&report), Err(EncodeError::Serialize(_))));
}

#[test]
fn unknown_operations_and_ack_statuses_decode_as_unknown() {
    let payload = reencode(&common::ack_committed(), |value| {
        set(value, "outcome", "status", text("deferred"));
        set(value, "outcome", "until", Cbor::Integer(1_791_590_460_000));
    });
    let ack: TelemetryAck = decode(&payload).unwrap();
    assert_eq!(ack.outcome, AckOutcome::Unknown);
    // The gateway keeps the batch and resends it later.
    assert!(!ack.outcome.releases_batch());

    let payload = reencode(&common::live_start(), |value| {
        set(value, "action", "op", text("pause"));
    });
    let control: LiveControl = decode(&payload).unwrap();
    assert_eq!(control.action, LiveAction::Unknown);

    // Unknown is a receive-side placeholder and never sent.
    let mut ack = common::ack_committed();
    ack.outcome = AckOutcome::Unknown;
    assert!(matches!(encode(&ack), Err(EncodeError::Invalid(_))));
    let mut control = common::live_stop();
    control.action = LiveAction::Unknown;
    assert!(matches!(encode(&control), Err(EncodeError::Invalid(_))));
}

#[test]
fn closed_enums_refuse_unknown_values() {
    // Values a receiver must understand to act correctly; adding one needs a new revision.
    let payload = reencode(&common::status_online(), |value| {
        set(value, "", "state", text("maintenance"));
    });
    assert!(matches!(
        decode::<Status>(&payload),
        Err(DecodeError::Malformed(_))
    ));

    for (path, key, unknown) in [
        ("models.1.points.0", "data_type", "f16"),
        ("models.1.points.1", "byte_order", "bacd"),
        ("devices.0", "transport", "modbus_udp"),
    ] {
        let payload = reencode(&common::desired_config(), |value| {
            set(value, path, key, text(unknown));
        });
        assert!(
            matches!(
                DesiredConfig::decode(&payload),
                Err(DecodeError::Malformed(_))
            ),
            "{key}: {unknown}"
        );
    }
    let payload = reencode(&common::desired_config(), |value| {
        set(
            value,
            "models.0.points.0",
            "function_code",
            Cbor::Integer(5),
        );
    });
    let error = DesiredConfig::decode(&payload).unwrap_err();
    assert!(
        error.to_string().contains("only 1-4 are allowed"),
        "{error}"
    );
}

#[test]
fn other_major_versions_are_refused_before_anything_else() {
    for case in cases() {
        let encoded = case.encode();
        for found in [0, 2, 3, u64::MAX] {
            let expected = DecodeError::UnsupportedVersion { found };
            // The same content under another major version.
            let mut value = to_cbor(&encoded);
            set(&mut value, "", "v", Cbor::Integer(i128::from(found)));
            assert_eq!(
                case.decode(&from_cbor(&value)),
                Err(expected.clone()),
                "{}",
                case.name()
            );
            // Another major version may have any shape; the version check comes first.
            // That includes `rev`, which another major version may encode differently.
            let other = Cbor::Map(vec![
                (text("payload"), future_value()),
                (text("rev"), text("2026-10")),
                (text("v"), Cbor::Integer(i128::from(found))),
            ]);
            assert_eq!(
                case.decode(&from_cbor(&other)),
                Err(expected),
                "{}",
                case.name()
            );
        }

        let mut value = to_cbor(&encoded);
        remove(&mut value, "", "v");
        assert_eq!(
            case.decode(&from_cbor(&value)),
            Err(DecodeError::Malformed("missing field `v`".to_owned())),
            "{}",
            case.name()
        );

        let mut value = to_cbor(&encoded);
        set(&mut value, "", "v", text("1"));
        assert!(
            matches!(
                case.decode(&from_cbor(&value)),
                Err(DecodeError::Malformed(_))
            ),
            "{}",
            case.name()
        );
    }
}

#[test]
fn newer_revisions_are_refused_and_the_current_one_accepted() {
    let newer = u64::from(PROTOCOL_REVISION) + 1;
    for case in cases() {
        let mut value = to_cbor(&case.encode());
        set(&mut value, "", "rev", Cbor::Integer(i128::from(newer)));
        assert_eq!(
            case.decode(&from_cbor(&value)),
            Err(DecodeError::UnsupportedRevision { required: newer }),
            "{}",
            case.name()
        );

        // Messages without a `rev` field accept one that this build implements.
        let mut value = to_cbor(&case.encode());
        set(
            &mut value,
            "",
            "rev",
            Cbor::Integer(i128::from(PROTOCOL_REVISION)),
        );
        case.decodes_to_sample(&from_cbor(&value))
            .unwrap_or_else(|error| panic!("{}: {error}", case.name()));
    }
}

#[test]
fn a_configuration_the_gateway_cannot_apply_is_reported_for_its_version() {
    let payload = reencode(&common::desired_config(), |value| {
        set(value, "", "rev", Cbor::Integer(2));
        set(value, "", "version", Cbor::Integer(8));
        set(value, "", "watchdog", future_value());
    });
    let error = DesiredConfig::decode(&payload).unwrap_err();
    assert_eq!(error, DecodeError::UnsupportedRevision { required: 2 });
    let header = DesiredConfig::peek_header(&payload).unwrap();
    assert_eq!(header.version.get(), 8);
    let report = ConfigReport::rejected(
        header.version,
        common::config_applied().active_version,
        error.to_issues(),
    );
    assert_eq!(report.issues[0].code, IssueCode::UnsupportedRevision);
    assert_eq!(report.issues[0].path, "rev");
    let decoded: ConfigReport = decode(&encode(&report).unwrap()).unwrap();
    assert_eq!(decoded, report);

    // rev 0 is not a revision.
    let payload = reencode(&common::desired_config(), |value| {
        set(value, "", "rev", Cbor::Integer(0));
    });
    let DecodeError::Invalid(invalid) = DesiredConfig::decode(&payload).unwrap_err() else {
        panic!("rev 0 must be invalid");
    };
    assert_eq!(invalid.issues()[0].code, IssueCode::UnsupportedRevision);
}

#[test]
fn absent_optional_fields_take_their_defaults() {
    // What an older peer sends: a payload without the optional fields.
    let payload = reencode(&common::heartbeat(), |value| {
        remove(value, "", "config_version");
        remove(value, "system", "soc_temp_celsius");
        remove(value, "system", "throttling");
        remove(value, "buffer", "oldest_pending");
        remove(value, "devices.1", "exception_codes");
        remove(value, "devices.1", "last_failure");
        remove(value, "devices.1", "last_success");
    });
    let heartbeat: Heartbeat = decode(&payload).unwrap();
    assert_eq!(heartbeat.config_version, None);
    assert_eq!(heartbeat.system.soc_temp_celsius, None);
    assert_eq!(heartbeat.system.throttling, None);
    assert_eq!(heartbeat.buffer.oldest_pending, None);
    assert!(heartbeat.devices[1].exception_codes.is_empty());
    assert_eq!(heartbeat.devices[1].last_failure, None);
    assert_eq!(heartbeat.devices[1].last_success, None);

    // `root_crl` came later (Laysi/trustiot-v2#563): optional, and so are its own optional fields.
    let mut with_root_crl = common::heartbeat();
    with_root_crl.root_crl = Some(RootCrlStatus {
        number: Some("12".to_owned()),
        next_update: Some(common::at(90_000)),
        expired: true,
    });
    let mut value = to_cbor(&encode(&with_root_crl).unwrap());
    let fields: Vec<_> = entries(&mut value, "root_crl")
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(
        fields,
        [text("number"), text("next_update"), text("expired")],
        "the field names on the wire"
    );
    let payload = reencode(&with_root_crl, |value| {
        remove(value, "root_crl", "number");
        remove(value, "root_crl", "next_update");
    });
    let heartbeat: Heartbeat = decode(&payload).unwrap();
    let status = heartbeat.root_crl.unwrap();
    assert_eq!((status.number, status.next_update), (None, None));
    assert!(status.expired);
    let payload = reencode(&with_root_crl, |value| remove(value, "", "root_crl"));
    assert_eq!(decode::<Heartbeat>(&payload).unwrap(), common::heartbeat());

    let payload = reencode(&common::log_batch(), |value| remove(value, "", "dropped"));
    assert_eq!(decode::<LogBatch>(&payload).unwrap().dropped, 0);

    let payload = reencode(&common::config_rejected(), |value| {
        remove(value, "", "active_version");
        remove(value, "", "issues");
    });
    let report: ConfigReport = decode(&payload).unwrap();
    assert_eq!(report.active_version, None);
    assert!(report.issues.is_empty());

    // Defaults are left out of the encoding, so an older receiver never sees them.
    let mut value = to_cbor(&encode(&common::desired_config()).unwrap());
    let point = entries(&mut value, "models.1.points.0");
    for key in ["byte_order", "length", "scale", "offset"] {
        assert!(point.iter().all(|(name, _)| *name != text(key)), "{key}");
    }
}

#[test]
fn required_fields_are_required() {
    for (name, path, key) in [
        ("telemetry", "", "seq"),
        ("telemetry", "", "stream"),
        ("telemetry", "", "clock_synced"),
        ("telemetry", "series.0", "device"),
        ("telemetry-ack-committed", "outcome", "records"),
        ("config", "", "version"),
        ("config", "", "rev"),
        ("config", "devices.0", "unit_id"),
        ("status-online", "", "session"),
        ("status-online", "", "state"),
        ("heartbeat", "system", "uptime_s"),
        ("live-control-start", "action", "ttl_s"),
        ("log-request", "", "from"),
    ] {
        let case = cases()
            .into_iter()
            .find(|case| case.name() == name)
            .unwrap();
        let mut value = to_cbor(&case.encode());
        remove(&mut value, path, key);
        assert_eq!(
            case.decode(&from_cbor(&value)),
            Err(DecodeError::Malformed(format!("missing field `{key}`"))),
            "{name}"
        );
    }
}
