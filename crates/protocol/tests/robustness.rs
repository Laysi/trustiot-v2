//! Hostile and broken payloads: decoding returns an error and never panics, allocates beyond
//! the payload, or accepts something the types forbid.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use cbor4ii::core::Value as Cbor;
use common::{cases, device, point, sample};
use protocol::telemetry::{BatchHeader, Series, TelemetryBatch};
use protocol::{DecodeError, Seq, Value, encode};

fn text(value: &str) -> Cbor {
    Cbor::Text(value.to_owned())
}

fn to_cbor(payload: &[u8]) -> Cbor {
    cbor4ii::serde::from_slice(payload).unwrap()
}

fn from_cbor(value: &Cbor) -> Vec<u8> {
    cbor4ii::serde::to_vec(Vec::new(), value).unwrap()
}

/// Replaces the only occurrence of the encoded text `marker` in `payload` with `raw` CBOR bytes,
/// to build payloads that no well-behaved encoder produces.
fn splice(payload: &[u8], marker: &str, raw: &[u8]) -> Vec<u8> {
    let needle = from_cbor(&text(marker));
    let at = payload
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("marker {marker} not found"));
    assert!(
        payload[at + 1..]
            .windows(needle.len())
            .all(|window| window != needle),
        "marker {marker} appears twice"
    );
    [&payload[..at], raw, &payload[at + needle.len()..]].concat()
}

/// A telemetry batch whose single sample value is the text `marker`.
fn batch_with_value(marker: &str) -> Vec<u8> {
    let batch = TelemetryBatch {
        series: vec![Series {
            device: device(1),
            point: point("p"),
            samples: vec![sample(0, Value::Text(marker.to_owned()))],
        }],
        ..common::telemetry_batch()
    };
    encode(&batch).unwrap()
}

/// A telemetry batch with an extra top-level field whose value is the text `marker`.
fn batch_with_unknown_field(marker: &str) -> Vec<u8> {
    let mut value = to_cbor(&encode(&common::telemetry_batch()).unwrap());
    let Cbor::Map(entries) = &mut value else {
        panic!("a batch is a map");
    };
    entries.push((text("future"), text(marker)));
    from_cbor(&value)
}

fn assert_malformed(result: Result<TelemetryBatch, DecodeError>, needle: &str) {
    match result {
        Err(DecodeError::Malformed(message)) => {
            assert!(
                message.contains(needle),
                "{message:?} does not mention {needle:?}"
            );
        }
        other => panic!("expected a malformed payload, got {other:?}"),
    }
}

#[test]
fn empty_and_oversized_payloads_are_refused_before_parsing() {
    for case in cases() {
        assert_eq!(case.decode(&[]), Err(DecodeError::Empty), "{}", case.name());
        let limit = case.channel().max_payload_bytes();
        let oversized = vec![0xa0; limit + 1];
        assert_eq!(
            case.decode(&oversized),
            Err(DecodeError::TooLarge {
                size: limit + 1,
                limit
            }),
            "{}",
            case.name()
        );
    }
}

#[test]
fn every_truncation_is_refused() {
    for case in cases() {
        let payload = case.encode();
        for len in 1..payload.len() {
            assert!(
                case.decode(&payload[..len]).is_err(),
                "{} cut to {len} bytes decoded",
                case.name()
            );
        }
    }
}

#[test]
fn corrupted_bytes_never_panic() {
    let mut decoded = 0;
    for case in cases() {
        let payload = case.encode();
        for index in 0..payload.len() {
            for mask in [0x01, 0x20, 0x80, 0xff] {
                let mut corrupted = payload.clone();
                corrupted[index] ^= mask;
                if case.decode(&corrupted).is_ok() {
                    decoded += 1;
                }
            }
        }
    }
    // Some flips only change a value (a digit of an ID, a timestamp); most break the payload.
    assert!(decoded > 0);
}

#[test]
fn trailing_bytes_are_refused() {
    for case in cases() {
        let payload = case.encode();
        let mut padded = payload.clone();
        padded.push(0x00);
        assert_eq!(
            case.decode(&padded),
            Err(DecodeError::TrailingBytes { count: 1 }),
            "{}",
            case.name()
        );
        let doubled = [payload.as_slice(), payload.as_slice()].concat();
        if doubled.len() <= case.channel().max_payload_bytes() {
            assert_eq!(
                case.decode(&doubled),
                Err(DecodeError::TrailingBytes {
                    count: payload.len()
                }),
                "{}",
                case.name()
            );
        }
    }
}

#[test]
fn deep_nesting_is_refused_even_in_unknown_fields() {
    let mut bomb = vec![0x81; 10_000];
    bomb.push(0x00);
    for marker_payload in [batch_with_unknown_field("BOMB"), batch_with_value("BOMB")] {
        let payload = splice(&marker_payload, "BOMB", &bomb);
        assert!(matches!(
            TelemetryBatch::decode(&payload),
            Err(DecodeError::Malformed(_))
        ));
    }
    // The status is an internally tagged enum, which buffers the whole map first. Its channel
    // allows 4 KiB, so the bomb is smaller there.
    let mut value = to_cbor(&encode(&common::status_online()).unwrap());
    let Cbor::Map(entries) = &mut value else {
        panic!("a status is a map");
    };
    entries.push((text("future"), text("BOMB")));
    let payload = splice(&from_cbor(&value), "BOMB", &bomb[bomb.len() - 1_001..]);
    let case = cases()
        .into_iter()
        .find(|case| case.name() == "status-online")
        .unwrap();
    assert!(matches!(
        case.decode(&payload),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn huge_declared_lengths_fail_without_allocating() {
    let max = [0xff; 8];
    for head in [0x5b, 0x7b, 0x9b, 0xbb] {
        // A byte string, text, array or map that claims u64::MAX bytes or entries.
        let raw = [&[head][..], &max].concat();
        let payload = splice(&batch_with_unknown_field("HUGE"), "HUGE", &raw);
        assert!(TelemetryBatch::decode(&payload).is_err(), "{head:#x}");
        let payload = splice(&batch_with_value("HUGE"), "HUGE", &raw);
        assert!(TelemetryBatch::decode(&payload).is_err(), "{head:#x}");
    }
    // A known string field claiming 4 GiB.
    let payload = splice(
        &batch_with_value("HUGE"),
        "HUGE",
        &[0x7a, 0xff, 0xff, 0xff, 0xff],
    );
    assert!(TelemetryBatch::decode(&payload).is_err());
}

#[test]
fn payloads_that_are_not_maps_are_refused() {
    for root in [
        Cbor::Array(vec![Cbor::Integer(1)]),
        Cbor::Integer(1),
        text("v"),
        Cbor::Null,
        Cbor::Bool(true),
        Cbor::Bytes(vec![0xa1]),
    ] {
        for case in cases() {
            assert!(
                matches!(
                    case.decode(&from_cbor(&root)),
                    Err(DecodeError::Malformed(_))
                ),
                "{} accepted {root:?}",
                case.name()
            );
        }
    }
}

#[test]
fn duplicate_fields_are_refused() {
    for (key, duplicate) in [
        ("v", Cbor::Integer(1)),
        ("seq", Cbor::Integer(43)),
        ("series", Cbor::Array(Vec::new())),
    ] {
        let mut value = to_cbor(&encode(&common::telemetry_batch()).unwrap());
        let Cbor::Map(entries) = &mut value else {
            panic!("a batch is a map");
        };
        entries.push((text(key), duplicate));
        assert_malformed(
            TelemetryBatch::decode(&from_cbor(&value)),
            &format!("duplicate field `{key}`"),
        );
    }
}

#[test]
fn identifiers_with_topic_syntax_are_refused() {
    for bad in ["gw-b/up/telemetry", "+", "#", "", "$SYS", "a b", "x\u{0}"] {
        let mut value = to_cbor(&encode(&common::telemetry_batch()).unwrap());
        let Cbor::Map(entries) = &mut value else {
            panic!("a batch is a map");
        };
        for (key, field) in entries.iter_mut() {
            if *key == text("stream") {
                *field = text(bad);
            }
        }
        assert_malformed(
            TelemetryBatch::decode(&from_cbor(&value)),
            "invalid stream ID",
        );
    }
}

#[test]
fn values_outside_the_types_are_refused() {
    let cases: [(&[u8], &str); 9] = [
        // NaN and infinities are not measurements, in any float width.
        (&[0xfb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0], "NaN"),
        (&[0xfa, 0x7f, 0x80, 0, 0], "NaN or infinite"),
        (&[0xfb, 0xff, 0xf0, 0, 0, 0, 0, 0, 0], "NaN or infinite"),
        (&[0xf9, 0x7e, 0x00], "NaN or infinite"),
        (&[0xf9, 0xfc, 0x00], "NaN or infinite"),
        // Integers beyond i64.
        (&[0x1b, 0x80, 0, 0, 0, 0, 0, 0, 0], "does not fit"),
        (&[0x3b, 0x80, 0, 0, 0, 0, 0, 0, 0], ""),
        // Null, undefined.
        (&[0xf6], ""),
        (&[0xf7], ""),
    ];
    for (raw, needle) in cases {
        let payload = splice(&batch_with_value("VALUE"), "VALUE", raw);
        assert_malformed(TelemetryBatch::decode(&payload), needle);
    }
    // Text values are bounded.
    let long = "x".repeat(protocol::MAX_TEXT_BYTES + 1);
    let payload = splice(
        &batch_with_value("VALUE"),
        "VALUE",
        &from_cbor(&text(&long)),
    );
    assert_malformed(TelemetryBatch::decode(&payload), "limit is 1024 bytes");

    // Sequence numbers start at 1; versions too.
    for (key, bad) in [
        ("seq", Cbor::Integer(0)),
        ("seq", Cbor::Integer(-1)),
        ("seq", text("1")),
    ] {
        let mut value = to_cbor(&encode(&common::telemetry_batch()).unwrap());
        let Cbor::Map(entries) = &mut value else {
            panic!("a batch is a map");
        };
        for (name, field) in entries.iter_mut() {
            if *name == text(key) {
                *field = bad.clone();
            }
        }
        assert_malformed(TelemetryBatch::decode(&from_cbor(&value)), "");
    }
}

#[test]
fn values_round_trip_exactly() {
    let values = [
        Value::Bool(false),
        Value::Int(0),
        Value::Int(-1),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(f64::MIN_POSITIVE),
        Value::Float(5e-324),
        Value::Float(f64::MAX),
        Value::Float(f64::from(f32::MAX)),
        Value::Float(f64::from(f32::MIN_POSITIVE) / 2.0),
        Value::Float(0.1),
        Value::Float(230.5),
        Value::Text(String::new()),
        Value::Text("電壓 A 相".to_owned()),
        Value::Text("x".repeat(protocol::MAX_TEXT_BYTES)),
    ];
    for value in values {
        let batch = TelemetryBatch {
            series: vec![Series {
                device: device(1),
                point: point("p"),
                samples: vec![sample(0, value.clone())],
            }],
            ..common::telemetry_batch()
        };
        let decoded = TelemetryBatch::decode(&encode(&batch).unwrap()).unwrap();
        let decoded = &decoded.series[0].samples[0].value;
        match (&value, decoded) {
            (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits(), "{a}"),
            _ => assert_eq!(&value, decoded),
        }
    }
    // A float that a single holds exactly takes 5 bytes instead of 9.
    let size = |value: f64| {
        encode(&TelemetryBatch {
            series: vec![Series {
                device: device(1),
                point: point("p"),
                samples: vec![sample(0, Value::Float(value))],
            }],
            ..common::telemetry_batch()
        })
        .unwrap()
        .len()
    };
    assert_eq!(size(0.1) - size(0.5), 4);

    // Other CBOR encoders send short floats as half precision (RFC 8949 section 4.2.2); they
    // decode to the same value.
    for (raw, expected) in [
        (&[0xf9, 0x3e, 0x00][..], 1.5),
        (&[0xf9, 0x80, 0x00][..], -0.0),
        (&[0xf9, 0x7b, 0xff][..], 65_504.0),
        (&[0xf9, 0x00, 0x01][..], 5.960_464_477_539_063e-8),
    ] {
        let payload = splice(&batch_with_value("VALUE"), "VALUE", raw);
        let decoded = TelemetryBatch::decode(&payload).unwrap();
        let Value::Float(value) = decoded.series[0].samples[0].value else {
            panic!("{raw:02x?} is not a float");
        };
        assert_eq!(value.to_bits(), f64::to_bits(expected), "{raw:02x?}");
    }
}

#[test]
fn the_header_of_a_refused_batch_can_still_be_read() {
    // Valid CBOR that breaks the batch's invariants: answered with `rejected`.
    let empty = TelemetryBatch {
        series: Vec::new(),
        ..common::telemetry_batch()
    };
    let mut value = to_cbor(&encode(&common::telemetry_batch()).unwrap());
    let Cbor::Map(entries) = &mut value else {
        panic!("a batch is a map");
    };
    for (key, field) in entries.iter_mut() {
        if *key == text("series") {
            *field = Cbor::Array(Vec::new());
        }
    }
    let payload = from_cbor(&value);
    assert!(matches!(
        TelemetryBatch::decode(&payload),
        Err(DecodeError::Invalid(_))
    ));
    assert!(encode(&empty).is_err());
    assert_eq!(
        TelemetryBatch::peek_header(&payload).unwrap(),
        BatchHeader {
            stream: common::stream(),
            seq: Seq::new(42).unwrap()
        }
    );

    // A malformed sample: still answered with `rejected` for the right batch.
    let payload = splice(&batch_with_value("VALUE"), "VALUE", &[0xf6]);
    assert!(TelemetryBatch::decode(&payload).is_err());
    assert_eq!(TelemetryBatch::peek_header(&payload).unwrap().seq.get(), 42);

    // Not even a map: nothing to answer, the gateway's ack timeout applies.
    assert!(TelemetryBatch::peek_header(&[0x01]).is_err());
}
