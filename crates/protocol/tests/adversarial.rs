//! Hand-built hostile inputs for the boundaries the hostile-payload fuzz loop (`fuzz.rs`) can
//! only reach by chance: exact size limits, version and revision fields in every odd shape,
//! nesting at the limit, repeated keys, integer and float extremes, identifier and text limits,
//! topics that try to become wildcards, and byte-for-byte golden stability.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use cbor4ii::core::Value as Cbor;
use common::{cases, golden_dir, read_hex};
use protocol::acl::{filter_matches, gateway_acl_topic};
use protocol::config::DesiredConfig;
use protocol::health::Status;
use protocol::live::LiveControl;
use protocol::telemetry::{TelemetryAck, TelemetryBatch};
use protocol::topic::{Channel, Direction, TopicError, parse_topic};
use protocol::{DecodeError, GatewayId, Message, decode};

fn text(value: &str) -> Cbor {
    Cbor::Text(value.to_owned())
}

/// Canonical CBOR of a tree. (`cbor4ii::serde` cannot serialize tags, the core encoder can.)
fn encode_tree(value: &Cbor) -> Vec<u8> {
    use cbor4ii::core::enc::Encode;
    let mut writer = cbor4ii::core::utils::BufWriter::new(Vec::new());
    value.encode(&mut writer).unwrap();
    writer.into_inner()
}

fn golden(name: &str) -> Cbor {
    let bytes = read_hex(&golden_dir("current").join(format!("{name}.cbor.hex")));
    cbor4ii::serde::from_slice(&bytes).unwrap()
}

fn entries(value: &mut Cbor) -> &mut Vec<(Cbor, Cbor)> {
    let Cbor::Map(entries) = value else {
        panic!("not a map")
    };
    entries
}

/// Sets `key` of the map `value` to `new`, or removes the key for `None`.
fn set(value: &mut Cbor, key: &str, new: Option<Cbor>) {
    let map = entries(value);
    map.retain(|(name, _)| *name != text(key));
    if let Some(new) = new {
        map.push((text(key), new));
    }
}

fn child<'a>(value: &'a mut Cbor, key: &str) -> &'a mut Cbor {
    &mut entries(value)
        .iter_mut()
        .find(|(name, _)| *name == text(key))
        .unwrap()
        .1
}

fn is_malformed<M: Message>(payload: &[u8]) -> bool {
    matches!(decode::<M>(payload), Err(DecodeError::Malformed(_)))
}

#[test]
fn the_major_version_is_checked_first_and_exactly() {
    for (label, version, found) in [
        ("0", Cbor::Integer(0), Some(0)),
        ("2", Cbor::Integer(2), Some(2)),
        ("u64::MAX", Cbor::Integer(u64::MAX.into()), Some(u64::MAX)),
    ] {
        let mut batch = golden("telemetry");
        set(&mut batch, "v", Some(version));
        assert_eq!(
            TelemetryBatch::decode(&encode_tree(&batch)),
            Err(DecodeError::UnsupportedVersion {
                found: found.unwrap()
            }),
            "v = {label}"
        );
    }
    // Any other shape is malformed, never a silently accepted 1: a float 1.0, text, a tag, null.
    for version in [
        Cbor::Integer(-1),
        Cbor::Float(1.0),
        text("1"),
        Cbor::Bool(true),
        Cbor::Null,
        Cbor::Tag(0, Box::new(Cbor::Integer(1))),
        Cbor::Tag(55799, Box::new(Cbor::Integer(1))),
    ] {
        let mut batch = golden("telemetry");
        set(&mut batch, "v", Some(version.clone()));
        assert!(
            is_malformed::<TelemetryBatch>(&encode_tree(&batch)),
            "v = {version:?}"
        );
    }
    let mut batch = golden("telemetry");
    set(&mut batch, "v", None);
    assert!(is_malformed::<TelemetryBatch>(&encode_tree(&batch)));
    // The wrong version wins over a repeated key: both orders are refused as repeated.
    for (first, second) in [(1, 2), (2, 1)] {
        let mut batch = golden("telemetry");
        set(&mut batch, "v", Some(Cbor::Integer(first)));
        entries(&mut batch).push((text("v"), Cbor::Integer(second)));
        assert!(is_malformed::<TelemetryBatch>(&encode_tree(&batch)));
    }
}

#[test]
fn a_newer_revision_is_refused_in_every_encoding_a_peer_might_use() {
    for required in [2, 3, 65_535, 65_536, u64::MAX] {
        let mut status = golden("status-online");
        set(&mut status, "rev", Some(Cbor::Integer(required.into())));
        assert_eq!(
            Status::decode(&encode_tree(&status)),
            Err(DecodeError::UnsupportedRevision { required }),
            "rev = {required}"
        );
        let mut config = golden("config");
        set(&mut config, "rev", Some(Cbor::Integer(required.into())));
        assert!(matches!(
            DesiredConfig::decode(&encode_tree(&config)),
            Err(DecodeError::UnsupportedRevision { .. })
        ));
    }
    // A malformed `rev` is malformed, not ignored; a repeated `rev` cannot hide the larger one.
    for rev in [
        Cbor::Integer(-1),
        Cbor::Float(2.0),
        text("x"),
        Cbor::Tag(1, Box::new(Cbor::Integer(2))),
    ] {
        let mut status = golden("status-online");
        set(&mut status, "rev", Some(rev));
        assert!(is_malformed::<Status>(&encode_tree(&status)));
    }
    for (first, second) in [(1, 99), (99, 1)] {
        let mut config = golden("config");
        set(&mut config, "rev", Some(Cbor::Integer(first)));
        entries(&mut config).push((text("rev"), Cbor::Integer(second)));
        assert!(is_malformed::<DesiredConfig>(&encode_tree(&config)));
    }
    // A configuration that says rev 0, or omits rev, is not accepted.
    let mut config = golden("config");
    set(&mut config, "rev", Some(Cbor::Integer(0)));
    assert!(matches!(
        DesiredConfig::decode(&encode_tree(&config)),
        Err(DecodeError::Invalid(_))
    ));
    set(&mut config, "rev", None);
    assert!(is_malformed::<DesiredConfig>(&encode_tree(&config)));
}

#[test]
fn only_a_map_is_a_message() {
    let batch = golden("telemetry");
    let values: Vec<Cbor> = entries(&mut batch.clone())
        .iter()
        .map(|(_, v)| v.clone())
        .collect();
    for root in [
        // Positional (array) form of a struct, which serde would otherwise take.
        Cbor::Array(values),
        Cbor::Tag(55799, Box::new(batch.clone())),
        Cbor::Tag(0, Box::new(batch)),
        Cbor::Integer(1),
        Cbor::Null,
        Cbor::Bytes(vec![0xa0]),
        Cbor::Array(Vec::new()),
    ] {
        assert!(
            is_malformed::<TelemetryBatch>(&encode_tree(&root)),
            "{root:?}"
        );
    }
    assert!(
        is_malformed::<TelemetryBatch>(&[0xa0]),
        "an empty map misses v"
    );
}

/// A telemetry payload whose unknown field `future` is `depth` containers deep.
fn nested_unknown_field(kind: &str, depth: usize) -> Vec<u8> {
    let mut batch = golden("telemetry");
    set(&mut batch, "future", Some(text("MARKER")));
    let mut bytes = encode_tree(&batch);
    let needle = encode_tree(&text("MARKER"));
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap();
    let mut nested = Vec::new();
    for _ in 0..depth {
        match kind {
            "array" => nested.push(0x81),
            "map" => nested.extend([0xa1, 0x61, b'k']),
            "tag" => nested.push(0xc6),
            "indefinite array" => nested.push(0x9f),
            _ => nested.extend([0xbf, 0x61, b'k']),
        }
    }
    nested.push(0x00);
    if kind.starts_with("indefinite") {
        nested.extend(std::iter::repeat_n(0xff, depth));
    }
    bytes.splice(at..at + needle.len(), nested);
    bytes
}

#[test]
fn nesting_beyond_the_limit_is_refused_for_every_container_kind_and_cheap_to_refuse() {
    for kind in ["array", "map", "tag", "indefinite array", "indefinite map"] {
        // A few levels are fine; at and beyond MAX_NESTING (counting the message map) the
        // payload is refused, with a bounded, small answer.
        assert!(
            TelemetryBatch::decode(&nested_unknown_field(kind, 10)).is_ok(),
            "{kind}"
        );
        for depth in [
            protocol::codec::MAX_NESTING,
            protocol::codec::MAX_NESTING + 1,
            1_000,
            50_000,
        ] {
            assert!(
                is_malformed::<TelemetryBatch>(&nested_unknown_field(kind, depth)),
                "{kind} at depth {depth}"
            );
        }
    }
}

#[test]
fn a_payload_of_exactly_the_limit_is_accepted_and_one_byte_more_is_not() {
    for case in cases() {
        let limit = case.channel().max_payload_bytes();
        let sample: Cbor = cbor4ii::serde::from_slice(&case.encode()).unwrap();
        for target in [limit - 1, limit, limit + 1] {
            // Pad with an unknown byte-string field until the payload is `target` bytes long.
            let mut pad = target - case.encode().len() - 24;
            let payload = loop {
                let mut padded = sample.clone();
                entries(&mut padded).push((text("pad"), Cbor::Bytes(vec![0; pad])));
                let payload = encode_tree(&padded);
                if payload.len() >= target {
                    break payload;
                }
                pad += 1;
            };
            assert_eq!(payload.len(), target, "{}", case.name());
            let result = case.decode(&payload);
            if target <= limit {
                assert!(result.is_ok(), "{} at {target}: {result:?}", case.name());
            } else {
                assert_eq!(
                    result,
                    Err(DecodeError::TooLarge {
                        size: target,
                        limit
                    }),
                    "{}",
                    case.name()
                );
            }
        }
    }
}

#[test]
fn repeated_keys_are_refused_wherever_a_field_could_be_chosen() {
    type Case = (&'static str, Vec<u8>, fn(&[u8]) -> bool);
    let mut cases: Vec<Case> = Vec::new();
    let mut batch = golden("telemetry");
    entries(&mut batch).push((text("seq"), Cbor::Integer(2)));
    cases.push((
        "telemetry seq",
        encode_tree(&batch),
        is_malformed::<TelemetryBatch>,
    ));
    let mut status = golden("status-online");
    entries(&mut status).push((text("state"), text("offline")));
    cases.push(("status state", encode_tree(&status), is_malformed::<Status>));
    let mut ack = golden("telemetry-ack-committed");
    entries(child(&mut ack, "outcome")).push((text("status"), text("retry")));
    cases.push((
        "ack outcome status",
        encode_tree(&ack),
        is_malformed::<TelemetryAck>,
    ));
    let mut ack = golden("telemetry-ack-committed");
    entries(child(&mut ack, "outcome")).push((text("records"), Cbor::Integer(99)));
    cases.push((
        "ack outcome records",
        encode_tree(&ack),
        is_malformed::<TelemetryAck>,
    ));
    let mut live = golden("live-control-start");
    entries(child(&mut live, "action")).push((text("op"), text("stop")));
    cases.push((
        "live action op",
        encode_tree(&live),
        is_malformed::<LiveControl>,
    ));
    let mut live = golden("live-control-start");
    entries(child(&mut live, "action")).push((text("ttl_s"), Cbor::Integer(5)));
    cases.push((
        "live action ttl",
        encode_tree(&live),
        is_malformed::<LiveControl>,
    ));
    for (label, payload, refused) in cases {
        assert!(refused(&payload), "{label}");
    }
    // A repeated key that no field uses is skipped like any unknown key.
    let mut batch = golden("telemetry");
    entries(&mut batch).push((text("future"), Cbor::Integer(1)));
    entries(&mut batch).push((text("future"), Cbor::Integer(2)));
    assert!(TelemetryBatch::decode(&encode_tree(&batch)).is_ok());
}

#[test]
fn chunked_strings_and_broken_indefinite_items_are_refused() {
    let batch = golden("telemetry");
    let mut base = vec![0xa0 | u8::try_from(entries(&mut batch.clone()).len() + 1).unwrap()];
    for (key, value) in entries(&mut batch.clone()) {
        base.extend(encode_tree(key));
        base.extend(encode_tree(value));
    }
    base.extend(encode_tree(&text("future")));
    for chunked in [
        &[0x7f, 0x61, b'a', 0xff][..],
        &[0x5f, 0x41, 0x00, 0xff],
        &[0x7f, 0xff],
    ] {
        let payload = [base.as_slice(), chunked].concat();
        assert!(is_malformed::<TelemetryBatch>(&payload), "{chunked:02x?}");
    }
    for broken in [
        &[0xff][..],
        &[0x1f],
        &[0xdf],
        &[0x9f],
        &[0xbf, 0x61, b'v', 0x01],
        &[0xbf, 0x61, b'v', 0x01, 0x61, b'x', 0xff],
        &[0x1c],
        &[0xfc],
        &[0xfe],
        &[0xf8],
    ] {
        assert!(is_malformed::<TelemetryBatch>(broken), "{broken:02x?}");
    }
    // An indefinite-length message map is accepted and decodes to the same message.
    let mut indefinite = vec![0xbf];
    for (key, value) in entries(&mut batch.clone()) {
        indefinite.extend(encode_tree(key));
        indefinite.extend(encode_tree(value));
    }
    indefinite.push(0xff);
    let decoded = TelemetryBatch::decode(&indefinite).unwrap();
    assert_eq!(decoded, common::telemetry_batch());
}

fn telemetry_with_first_value(value: Cbor) -> Vec<u8> {
    let mut batch = golden("telemetry");
    let Cbor::Array(series) = child(&mut batch, "series") else {
        panic!()
    };
    let Cbor::Array(samples) = child(&mut series[0], "samples") else {
        panic!()
    };
    let Cbor::Array(sample) = &mut samples[0] else {
        panic!()
    };
    sample[1] = value;
    encode_tree(&batch)
}

#[test]
fn integer_and_float_extremes_are_accepted_or_refused_exactly() {
    let accepted = |value: Cbor| TelemetryBatch::decode(&telemetry_with_first_value(value));
    assert!(accepted(Cbor::Integer(i64::MAX.into())).is_ok());
    assert!(accepted(Cbor::Integer(i64::MIN.into())).is_ok());
    for refused in [
        i128::from(i64::MAX) + 1,
        i128::from(i64::MIN) - 1,
        u64::MAX.into(),
        -i128::from(u64::MAX) - 1,
    ] {
        assert!(accepted(Cbor::Integer(refused)).is_err(), "{refused}");
    }
    // Bignums (tag 2 and 3) and other tags never reach a value.
    for tagged in [
        Cbor::Tag(2, Box::new(Cbor::Bytes(vec![1, 0, 0, 0, 0, 0, 0, 0, 0]))),
        Cbor::Tag(3, Box::new(Cbor::Bytes(vec![1, 0, 0, 0, 0, 0, 0, 0, 0]))),
        Cbor::Tag(1, Box::new(Cbor::Integer(5))),
    ] {
        assert!(accepted(tagged).is_err());
    }
    // NaN, infinities and half floats, spliced in as raw CBOR.
    let marker = "MARKERMARKER";
    let template = telemetry_with_first_value(text(marker));
    let needle = encode_tree(&text(marker));
    let at = template
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap();
    let with_raw = |raw: &[u8]| {
        TelemetryBatch::decode(&[&template[..at], raw, &template[at + needle.len()..]].concat())
    };
    for (label, raw) in [
        ("f16 NaN", vec![0xf9, 0x7e, 0x00]),
        ("f16 inf", vec![0xf9, 0x7c, 0x00]),
        ("f16 -inf", vec![0xf9, 0xfc, 0x00]),
        ("f32 NaN", vec![0xfa, 0x7f, 0xc0, 0, 0]),
        ("f32 inf", vec![0xfa, 0x7f, 0x80, 0, 0]),
        (
            "f64 signalling NaN",
            vec![0xfb, 0x7f, 0xf0, 0, 0, 0, 0, 0, 1],
        ),
        ("f64 -NaN", vec![0xfb, 0xff, 0xf8, 0, 0, 0, 0, 0, 0]),
        ("undefined", vec![0xf7]),
        ("simple 16", vec![0xf0]),
        ("simple 255", vec![0xf8, 0xff]),
    ] {
        assert!(with_raw(&raw).is_err(), "{label}");
    }
    for (label, raw) in [
        ("f16 1.5", vec![0xf9, 0x3e, 0x00]),
        ("f16 max", vec![0xf9, 0x7b, 0xff]),
        ("f16 subnormal", vec![0xf9, 0x00, 0x01]),
        ("f64 -0.0", vec![0xfb, 0x80, 0, 0, 0, 0, 0, 0, 0]),
    ] {
        assert!(with_raw(&raw).is_ok(), "{label}");
    }
}

#[test]
fn identifier_and_text_limits_hold_at_the_byte() {
    let with_stream = |stream: &str| {
        let mut batch = golden("telemetry");
        set(&mut batch, "stream", Some(text(stream)));
        TelemetryBatch::decode(&encode_tree(&batch))
    };
    assert!(with_stream(&"a".repeat(64)).is_ok());
    assert!(with_stream(&"a".repeat(65)).is_err());
    for bad in [
        "",
        "a/b",
        "a+b",
        "a#",
        "$SYS",
        ".a",
        "-a",
        "_a",
        "ａｂｃ",
        "\u{430}bc",
        "a\u{200b}b",
        "a\u{202e}b",
        "a\0b",
        "a\nb",
        "ab ",
        " ab",
        "é",
    ] {
        assert!(with_stream(bad).is_err(), "{bad:?}");
    }
    let with_text =
        |value: String| TelemetryBatch::decode(&telemetry_with_first_value(Cbor::Text(value)));
    assert!(with_text("a".repeat(1024)).is_ok());
    assert!(with_text("a".repeat(1025)).is_err());
    assert!(
        with_text("é".repeat(512)).is_ok(),
        "1024 bytes of two-byte characters"
    );
    assert!(with_text("é".repeat(513)).is_err(), "1026 bytes");
    // Text that is not UTF-8 is refused, including overlong encodings and surrogates.
    let marker = "MARKERMARKER";
    let template = telemetry_with_first_value(text(marker));
    let needle = encode_tree(&text(marker));
    let at = template
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap();
    for raw in [
        &[0x62, 0xff, 0xfe][..],
        &[0x62, 0xc0, 0x80],
        &[0x63, 0xed, 0xa0, 0x80],
        &[0x64, 0xf4, 0x90, 0x80, 0x80],
    ] {
        let payload = [&template[..at], raw, &template[at + needle.len()..]].concat();
        assert!(is_malformed::<TelemetryBatch>(&payload), "{raw:02x?}");
    }
}

#[test]
fn topics_never_parse_into_wildcards_or_other_gateways() {
    for topic in [
        "trustiot/v1/gw/+/up/telemetry",
        "trustiot/v1/gw/#",
        "trustiot/v1/gw/gw-1/up/#",
        "trustiot/v1/gw/gw-1/up/+",
        "trustiot/v1/gw/gw-1/+/telemetry",
        "trustiot/v1/gw/gw-1/up/telemetry/",
        "trustiot/v1/gw/gw-1/up/telemetry/extra",
        "/trustiot/v1/gw/gw-1/up/telemetry",
        "trustiot/v1/gw//up/telemetry",
        "trustiot//gw/gw-1/up/telemetry",
        "trustiot/v1/gw/gw-1//telemetry",
        "TRUSTIOT/v1/gw/gw-1/up/telemetry",
        "trustiot/V1/gw/gw-1/up/telemetry",
        "trustiot/v1/GW/gw-1/up/telemetry",
        "trustiot/v1/gw/gw-1/UP/telemetry",
        "trustiot/v1/gw/gw-1/up/Telemetry",
        "trustiot/v1/gw/gw-1/down/telemetry",
        "trustiot/v1/gw/gw-1/up/telemetry-ack",
        "trustiot/v10/gw/gw-1/up/telemetry",
        "trustiot/v2/gw/gw-1/up/telemetry",
        "trustiot/v1/gw/gw-1/up/telemetry\0",
        "trustiot/v1/gw/gw\0/up/telemetry",
        "trustiot/v1/gw/gw-1 /up/telemetry",
        "trustiot/v1/gw/gw\u{ff0f}1/up/telemetry",
        "trustiot/v1/gw/gw\u{2215}1/up/telemetry",
        "trustiot/v1/gw/ｇｗ/up/telemetry",
        "trustiot/v1/gw/gw\u{430}/up/telemetry",
        "$share/g/trustiot/v1/gw/gw-1/up/telemetry",
        "$SYS/trustiot/v1/gw/gw-1/up/telemetry",
        "",
        "/",
        "trustiot",
    ] {
        assert!(parse_topic(topic).is_err(), "{topic:?}");
    }
    // A gateway ID at the length limit parses; one byte more does not.
    let long = "g".repeat(64);
    assert!(parse_topic(&format!("trustiot/v1/gw/{long}/up/telemetry")).is_ok());
    assert!(matches!(
        parse_topic(&format!("trustiot/v1/gw/{long}x/up/telemetry")),
        Err(TopicError::InvalidGateway(_))
    ));
    // The error never echoes an unbounded topic level.
    let huge = format!("trustiot/{}/gw/g/up/telemetry", "v".repeat(100_000));
    assert!(parse_topic(&huge).unwrap_err().to_string().len() < 200);
    let huge = format!("trustiot/v1/gw/{}/up/telemetry", "é".repeat(100_000));
    assert!(parse_topic(&huge).unwrap_err().to_string().len() < 400);
    // Shared subscription groups cannot smuggle levels or wildcards.
    for group in ["", "a/b", "+", "#", "$x", "a b", "g\0", &"g".repeat(65)] {
        assert_eq!(
            Channel::Telemetry.shared_filter(group),
            Err(TopicError::InvalidShareGroup),
            "{group:?}"
        );
    }
    assert!(Channel::Telemetry.shared_filter(&"g".repeat(64)).is_ok());
}

#[test]
fn a_gateways_acl_topics_contain_no_wildcard_except_the_trailing_hash() {
    for direction in [Direction::Up, Direction::Down] {
        let acl = gateway_acl_topic(direction);
        assert_eq!(acl.matches('%').count(), 1, "{acl}");
        assert!(acl.ends_with("/#") && acl.matches('#').count() == 1 && !acl.contains('+'));
    }
    for rule in protocol::acl::gateway_role_acls() {
        assert!(rule.allow);
        assert!(
            rule.topic.starts_with("trustiot/v1/gw/%u/"),
            "{}",
            rule.topic
        );
    }
    // Substituting any valid CN keeps the ACL inside that CN's own tree.
    for (cn, other) in [
        ("gw-1", "gw-10"),
        ("gw-1", "gw-1.x"),
        ("a", "A"),
        ("gw", "gw-1"),
    ] {
        let own = GatewayId::new(cn).unwrap();
        let other = GatewayId::new(other).unwrap();
        let acl = gateway_acl_topic(Direction::Up).replace("%u", own.as_str());
        assert!(filter_matches(&acl, &Channel::Telemetry.topic(&own)));
        assert!(!filter_matches(&acl, &Channel::Telemetry.topic(&other)));
        assert!(!filter_matches(&acl, &Channel::TelemetryAck.topic(&own)));
    }
    // CNs that would need to be wildcards or extra levels are not gateway IDs at all.
    for cn in [
        "+", "#", "a/b", "a/#", "$SYS", "%u", "", "a b", "gw\n", "gw\0",
    ] {
        assert!(GatewayId::new(cn).is_err(), "{cn:?}");
    }
}

fn reencode<M: Message>(payload: &[u8]) -> Option<Vec<u8>> {
    protocol::encode(&decode::<M>(payload).ok()?).ok()
}

#[test]
fn golden_payloads_survive_a_decode_and_encode_byte_for_byte() {
    use protocol::config::ConfigReport;
    use protocol::health::Heartbeat;
    use protocol::live::LiveSamples;
    use protocol::logs::{LogBatch, LogDump, LogRequest};
    for case in cases() {
        let name = case.name();
        let golden = read_hex(&golden_dir("current").join(format!("{name}.cbor.hex")));
        let again = match name {
            "telemetry" => reencode::<TelemetryBatch>(&golden),
            n if n.starts_with("telemetry-ack") => reencode::<TelemetryAck>(&golden),
            "config" => reencode::<DesiredConfig>(&golden),
            n if n.starts_with("config-report") => reencode::<ConfigReport>(&golden),
            n if n.starts_with("status") => reencode::<Status>(&golden),
            "heartbeat" => reencode::<Heartbeat>(&golden),
            "live" => reencode::<LiveSamples>(&golden),
            n if n.starts_with("live-control") => reencode::<LiveControl>(&golden),
            "log" => reencode::<LogBatch>(&golden),
            "log-request" => reencode::<LogRequest>(&golden),
            "log-dump" => reencode::<LogDump>(&golden),
            other => panic!("no decoder for {other}"),
        };
        assert_eq!(again, Some(golden), "{name}");
    }
}

/// The map of the first `devices[]` entry of a golden heartbeat that has `key`.
fn heartbeat_device_with<'a>(heartbeat: &'a mut Cbor, key: &str) -> &'a mut Cbor {
    let Cbor::Array(devices) = child(heartbeat, "devices") else {
        panic!("devices is not an array")
    };
    devices
        .iter_mut()
        .find(|device| {
            entries(&mut (*device).clone())
                .iter()
                .any(|(name, _)| *name == text(key))
        })
        .unwrap()
}

fn first_log_entry(log: &mut Cbor) -> &mut Cbor {
    let Cbor::Array(items) = child(log, "entries") else {
        panic!("entries is not an array")
    };
    &mut items[0]
}

/// Finding F1 of the #50 verification: the data maps kept the last of a repeated key
/// (Laysi/trustiot-v2#269). They now refuse it like message structs refuse a repeated field.
#[test]
fn a_repeated_key_in_a_data_map_is_refused() {
    use protocol::health::Heartbeat;
    use protocol::logs::LogBatch;

    // The reproduction of the report: `timeout_ms` twice, the second "OVERRIDDEN".
    let mut log = golden("log");
    entries(child(first_log_entry(&mut log), "fields"))
        .push((text("timeout_ms"), text("OVERRIDDEN")));
    assert!(is_malformed::<LogBatch>(&encode_tree(&log)));
    // The repeated key first, then the original: refused in either order.
    let mut log = golden("log");
    entries(child(first_log_entry(&mut log), "fields")).insert(0, (text("timeout_ms"), text("1")));
    assert!(is_malformed::<LogBatch>(&encode_tree(&log)));
    // A new key is still a new field.
    let mut log = golden("log");
    entries(child(first_log_entry(&mut log), "fields")).push((text("retry"), text("2")));
    let batch = LogBatch::decode(&encode_tree(&log)).unwrap();
    assert_eq!(batch.entries[0].fields.len(), 3);

    // Exception code 2 twice, the second 999.
    let mut heartbeat = golden("heartbeat");
    let device = heartbeat_device_with(&mut heartbeat, "exception_codes");
    entries(child(device, "exception_codes")).push((Cbor::Integer(2), Cbor::Integer(999)));
    assert!(is_malformed::<Heartbeat>(&encode_tree(&heartbeat)));
    let mut heartbeat = golden("heartbeat");
    let device = heartbeat_device_with(&mut heartbeat, "exception_codes");
    entries(child(device, "exception_codes")).push((Cbor::Integer(3), Cbor::Integer(1)));
    assert!(Heartbeat::decode(&encode_tree(&heartbeat)).is_ok());

    // The same code written with a wider head (0x18 0x02 instead of 0x02) is the same key.
    let mut heartbeat = golden("heartbeat");
    let device = heartbeat_device_with(&mut heartbeat, "exception_codes");
    entries(child(device, "exception_codes")).push((Cbor::Integer(0x77), Cbor::Integer(1)));
    let mut payload = encode_tree(&heartbeat);
    // Rewrite the added key 0x77 (`18 77`) as a one-byte-argument 2 (`18 02`).
    let at = payload
        .windows(3)
        .position(|window| window == [0x18, 0x77, 0x01])
        .unwrap();
    payload[at + 1] = 0x02;
    assert!(is_malformed::<Heartbeat>(&payload));
}

/// U+0000 is refused in every free text of the protocol (Laysi/trustiot-v2#269); other control
/// characters are left to whoever renders the text.
#[test]
fn nul_is_refused_in_every_free_text() {
    use protocol::config::ConfigReport;
    use protocol::health::Heartbeat;
    use protocol::logs::{LogBatch, LogDump};
    use protocol::{Issue, IssueCode};

    fn invalid_at(result: Result<impl std::fmt::Debug, DecodeError>, path: &str) {
        let Err(DecodeError::Invalid(invalid)) = &result else {
            panic!("expected {path} to be invalid: {result:?}");
        };
        assert!(
            invalid
                .issues()
                .iter()
                .any(|issue| issue.code == IssueCode::InvalidValue && issue.path == path),
            "{path}: {invalid}"
        );
    }

    // Telemetry text values (and live values, which share the type).
    assert!(TelemetryBatch::decode(&telemetry_with_first_value(text("AB\0"))).is_err());
    assert!(TelemetryBatch::decode(&telemetry_with_first_value(text("a\0b"))).is_err());

    // Log entries: target, message, field names and values.
    for (key, path) in [
        ("target", "entries[0].target"),
        ("message", "entries[0].message"),
    ] {
        let mut log = golden("log");
        set(first_log_entry(&mut log), key, Some(text("x\0y")));
        invalid_at(LogBatch::decode(&encode_tree(&log)), path);
    }
    for (name, value) in [("bad\0name", "v"), ("name", "bad\0value")] {
        let mut log = golden("log");
        entries(child(first_log_entry(&mut log), "fields")).push((text(name), text(value)));
        invalid_at(LogBatch::decode(&encode_tree(&log)), "entries[0].fields");
    }
    let mut dump = golden("log-dump");
    if let Cbor::Array(items) = child(&mut dump, "entries")
        && let Some(entry) = items.first_mut()
    {
        set(entry, "message", Some(text("\0")));
        invalid_at(LogDump::decode(&encode_tree(&dump)), "entries[0].message");
    }

    // Heartbeat failure detail.
    let mut heartbeat = golden("heartbeat");
    let device = heartbeat_device_with(&mut heartbeat, "last_failure");
    set(child(device, "last_failure"), "detail", Some(text("eof\0")));
    let Err(DecodeError::Invalid(invalid)) = Heartbeat::decode(&encode_tree(&heartbeat)) else {
        panic!("a NUL in the failure detail must be refused");
    };
    assert!(
        invalid
            .issues()
            .iter()
            .any(|issue| issue.code == IssueCode::InvalidValue
                && issue.path.ends_with(".last_failure.detail")),
        "{invalid}"
    );

    // The rejection message of an acknowledgement, and the issues of a configuration report.
    let mut ack = golden("telemetry-ack-rejected");
    set(child(&mut ack, "outcome"), "message", Some(text("bad\0")));
    invalid_at(
        decode::<TelemetryAck>(&encode_tree(&ack)),
        "outcome.message",
    );
    for (key, path) in [("message", "issues[0].message"), ("path", "issues[0].path")] {
        let mut report = golden("config-report-rejected");
        let Cbor::Array(issues) = child(&mut report, "issues") else {
            panic!()
        };
        set(&mut issues[0], key, Some(text("a\0")));
        invalid_at(ConfigReport::decode(&encode_tree(&report)), path);
    }

    // Tabs, line breaks and escape sequences pass: escaping them depends on where the text
    // is shown, so the renderer does it.
    let mut log = golden("log");
    set(
        first_log_entry(&mut log),
        "message",
        Some(text("line 1\nline 2\t\r\u{1b}[31m\u{7f}\u{202e}")),
    );
    assert!(LogBatch::decode(&encode_tree(&log)).is_ok());
    assert!(TelemetryBatch::decode(&telemetry_with_first_value(text("a\tb\u{1}"))).is_ok());

    // The constructors make text sendable: U+0000 becomes U+FFFD before the length cut.
    let issue = Issue::new(IssueCode::DecodeFailed, "a\0", "unknown variant `\0x`");
    assert_eq!(issue.path, "a\u{fffd}");
    assert_eq!(issue.message, "unknown variant `\u{fffd}x`");
    let entry = protocol::logs::LogEntry::new(
        protocol::UnixMillis::new(1),
        protocol::logs::LogLevel::Warn,
        "t\0",
        "m\0",
    )
    .with_field("k\0", "v\0");
    assert_eq!(entry.target, "t\u{fffd}");
    assert_eq!(entry.message, "m\u{fffd}");
    assert_eq!(
        entry.fields.get("k\u{fffd}").map(String::as_str),
        Some("v\u{fffd}")
    );
    let batch = LogBatch {
        v: protocol::V1,
        entries: vec![entry],
        dropped: 0,
    };
    assert_eq!(LogBatch::decode(&batch.encode().unwrap()).unwrap(), batch);
}

/// A header peek reads payloads that `decode` refused, but not more than any conforming MQTT
/// connection carries (Laysi/trustiot-v2#269).
#[test]
fn a_header_peek_reads_at_most_max_peek_bytes() {
    use protocol::codec::MAX_PEEK_BYTES;

    let padded = |golden_name: &str, target: usize| {
        let sample = golden(golden_name);
        let mut pad = target - encode_tree(&sample).len() - 24;
        loop {
            let mut padded = sample.clone();
            entries(&mut padded).push((text("pad"), Cbor::Bytes(vec![0; pad])));
            let payload = encode_tree(&padded);
            if payload.len() >= target {
                assert_eq!(payload.len(), target);
                break payload;
            }
            pad += 1;
        }
    };
    // Over the channel limit, so `decode` refuses it, but the peek still answers.
    let at_limit = padded("telemetry", MAX_PEEK_BYTES);
    assert!(matches!(
        TelemetryBatch::decode(&at_limit),
        Err(DecodeError::TooLarge { .. })
    ));
    assert_eq!(
        TelemetryBatch::peek_header(&at_limit).unwrap().seq.get(),
        42
    );
    let over = padded("telemetry", MAX_PEEK_BYTES + 1);
    assert_eq!(
        TelemetryBatch::peek_header(&over),
        Err(DecodeError::TooLarge {
            size: MAX_PEEK_BYTES + 1,
            limit: MAX_PEEK_BYTES
        })
    );
    let at_limit = padded("config", MAX_PEEK_BYTES);
    assert!(DesiredConfig::peek_header(&at_limit).is_ok());
    let over = padded("config", MAX_PEEK_BYTES + 1);
    assert!(matches!(
        DesiredConfig::peek_header(&over),
        Err(DecodeError::TooLarge { .. })
    ));
}
