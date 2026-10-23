//! The event envelope: its wire format, and that a consumer built with the current version of
//! an event still decodes every older version (the fixtures under `tests/fixtures/events/`).
//!
//! Every real event gets a test here that calls [`decode_fixtures`]; `example.widget_renamed`
//! below shows the whole evolution from version 1 to version 2.

mod common;

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Timelike, Utc};
use contract::event::{Actor, parse_payload};
use contract::events::assets::{
    DeviceModelCreated, DeviceModelDeleted, DeviceModelUpdated, GatewayBound,
};
use contract::events::certs::GatewayRegistered;
use contract::events::identity::TenantCreated;
use contract::{
    DecodeError, DeviceModelId, EncodeError, Event, EventEnvelope, GatewayBindingId, ServiceName,
    TenantId, UserId,
};
use protocol::config::{ByteOrder, DataType};
use protocol::{GatewayId, ModelName};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Version;

/// Decodes every fixture of `E`, `tests/fixtures/events/<TYPE>/v<N>[-<slug>].json`, and
/// checks that each version from 1 to [`Event::VERSION`] has at least one. Fixtures are
/// append-only: never edit or delete one, so an incompatible change without a version bump
/// fails here.
fn decode_fixtures<E: Event + Debug>() -> Vec<(String, E)> {
    let dir = common::fixture_path(&format!("events/{}", E::TYPE));
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();

    let mut versions = BTreeSet::new();
    let mut decoded = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("UTF-8 file name")
            .to_owned();
        let version: u32 = name
            .strip_prefix('v')
            .and_then(|rest| rest.split('-').next())
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("{}: name it v<N>[-<slug>].json", path.display()));
        let text = fs::read_to_string(&path).expect("read fixture");
        let envelope: EventEnvelope = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: not an envelope: {error}", path.display()));
        assert_eq!(envelope.version, version, "{}", path.display());
        assert_eq!(envelope.event_type, E::TYPE, "{}", path.display());
        let event = envelope
            .decode::<E>()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        versions.insert(version);
        decoded.push((name, event));
    }
    let expected: BTreeSet<u32> = (1..=E::VERSION).collect();
    assert_eq!(
        versions,
        expected,
        "every version of `{}` needs a fixture in {}",
        E::TYPE,
        dir.display()
    );
    decoded
}

/// Version 2, the current one: `name` became `display_name`, and `renamed_by` was added.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct WidgetRenamed {
    widget_id: String,
    display_name: String,
    renamed_by: Option<UserId>,
}

/// Version 1, kept only to decode old events.
#[derive(Deserialize)]
struct WidgetRenamedV1 {
    widget_id: String,
    name: String,
}

impl From<WidgetRenamedV1> for WidgetRenamed {
    fn from(v1: WidgetRenamedV1) -> Self {
        Self {
            widget_id: v1.widget_id,
            display_name: v1.name,
            renamed_by: None,
        }
    }
}

impl Event for WidgetRenamed {
    const TYPE: &'static str = "example.widget_renamed";
    const VERSION: u32 = 2;
    const PRODUCER: ServiceName = ServiceName::Assets;

    fn decode_payload(version: u32, payload: Value) -> Result<Self, DecodeError> {
        match version {
            1 => parse_payload::<WidgetRenamedV1>(Self::TYPE, version, payload).map(Self::from),
            2 => parse_payload(Self::TYPE, version, payload),
            _ => Err(DecodeError::UnknownVersion {
                event_type: Self::TYPE,
                version,
            }),
        }
    }
}

fn tenant() -> TenantId {
    "01928f6e-7a3c-7d4e-8f00-00000000000a"
        .parse()
        .expect("tenant id")
}

fn widget_renamed() -> WidgetRenamed {
    WidgetRenamed {
        widget_id: "w-1".to_owned(),
        display_name: "Pump B".to_owned(),
        renamed_by: Some(
            "01928f6e-7a3c-7d4e-8f00-0123456789ab"
                .parse()
                .expect("user id"),
        ),
    }
}

#[test]
fn old_versions_decode_with_the_current_consumer() {
    let decoded = decode_fixtures::<WidgetRenamed>();
    let names: Vec<&str> = decoded.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["v1", "v2"]);
    assert_eq!(
        decoded[0].1,
        WidgetRenamed {
            widget_id: "w-1".to_owned(),
            display_name: "Pump A".to_owned(),
            renamed_by: None,
        },
        "version 1 is upgraded to the current type"
    );
    assert_eq!(decoded[1].1, widget_renamed());
}

/// `identity.tenant_created` (Laysi/trustiot-v2#65): the payload is an empty object, and the
/// tenant is the envelope's. `v1-actor` is the same version with the envelope's `actor`
/// (Laysi/trustiot-v2#130).
#[test]
fn tenant_created_decodes_every_version() {
    let decoded = decode_fixtures::<TenantCreated>();
    let names: Vec<&str> = decoded.iter().map(|(name, _)| name.as_str()).collect();
    // Sorted by path: `-` sorts before `.`.
    assert_eq!(names, ["v1-actor", "v1"]);
    let tenant: TenantId = "01928f6e-7a3c-7d4e-8f00-00000000000b"
        .parse()
        .expect("tenant id");
    let envelope = EventEnvelope::new(&TenantCreated {}, Some(tenant)).expect("encode");
    assert_eq!(envelope.producer, ServiceName::Identity);
    assert_eq!(envelope.payload, json!({}));
    assert_eq!(envelope.tenant_id, Some(tenant));
}

/// `certs.gateway_registered` (Laysi/trustiot-v2#79): platform-wide, so no tenant, written by
/// the certs service, with the platform administrator as the actor.
#[test]
fn gateway_registered_decodes_every_version() {
    let decoded = decode_fixtures::<GatewayRegistered>();
    let names: Vec<&str> = decoded.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["v1"]);
    let registered = GatewayRegistered {
        hardware_id: GatewayId::new("tg-0001").expect("hardware id"),
        public_key_sha256: "1f2d4c6a8e0b3d5f7a9c1e3b5d7f9a2c4e6b8d0f2a4c6e8b0d2f4a6c8e0b2d4f"
            .to_owned(),
        endorsement_key_sha256: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
            .to_owned(),
        certificate_serial: "4c2a19e0f3b85d7a6e01c2d3b4a59687".to_owned(),
        workstation: "line-1".to_owned(),
    };
    assert_eq!(decoded[0].1, registered);

    let path = common::fixture_path("events/certs.gateway_registered/v1.json");
    let fixture: EventEnvelope =
        serde_json::from_str(&fs::read_to_string(path).expect("read fixture")).expect("envelope");
    assert_eq!(fixture.tenant_id, None);
    let user: UserId = "01928f6e-7a3c-7d4e-8f00-0000000000ad"
        .parse()
        .expect("user id");
    assert_eq!(fixture.actor, Some(Actor::User { user_id: user }));

    let envelope = EventEnvelope::new(&registered, None).expect("encode");
    assert_eq!(envelope.producer, ServiceName::Certs);
    assert_eq!(envelope.event_type, "certs.gateway_registered");
    assert_eq!(envelope.tenant_id, None);
    assert_eq!(
        envelope.payload,
        json!({
            "hardware_id": "tg-0001",
            "public_key_sha256": "1f2d4c6a8e0b3d5f7a9c1e3b5d7f9a2c4e6b8d0f2a4c6e8b0d2f4a6c8e0b2d4f",
            "endorsement_key_sha256": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "certificate_serial": "4c2a19e0f3b85d7a6e01c2d3b4a59687",
            "workstation": "line-1",
        })
    );
}

/// The device model events (Laysi/trustiot-v2#99) carry a model the gateway's configuration
/// accepts, in the gateway's own format, and the assets service produces them.
#[test]
fn device_model_events_decode_every_version() {
    let model_id: DeviceModelId = "01928f6e-7a3c-7d4e-8f00-000000000a01"
        .parse()
        .expect("model id");

    let created = decode_fixtures::<DeviceModelCreated>();
    assert_eq!(created.len(), 1);
    let created = &created[0].1;
    assert_eq!(created.model_id, model_id);
    assert_eq!(created.revision, 1);
    assert_eq!(created.model.name.as_str(), "power-meter");
    assert_eq!(created.model.points.len(), 4);
    assert_eq!(created.model.points[1].byte_order, ByteOrder::Cdab);
    assert_eq!(created.model.points[1].offset, Some(-40.0));
    assert_eq!(created.model.points[2].data_type, DataType::String);
    assert_eq!(created.model.points[2].length, Some(8));
    created.model.validate().expect("a valid model");

    let updated = decode_fixtures::<DeviceModelUpdated>();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].1.model_id, model_id);
    assert_eq!(updated[0].1.revision, 2);
    updated[0].1.model.validate().expect("a valid model");

    let deleted = decode_fixtures::<DeviceModelDeleted>();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        deleted[0].1,
        DeviceModelDeleted {
            model_id,
            name: ModelName::new("power-meter").expect("name"),
            revision: 2,
        }
    );

    let envelope = EventEnvelope::new(&deleted[0].1, Some(tenant())).expect("encode");
    assert_eq!(envelope.producer, ServiceName::Assets);
    assert_eq!(envelope.event_type, "assets.device_model_deleted");
    assert_eq!(
        envelope.payload,
        json!({
            "model_id": "01928f6e-7a3c-7d4e-8f00-000000000a01",
            "name": "power-meter",
            "revision": 2,
        })
    );
    let envelope = EventEnvelope::new(created, Some(tenant())).expect("encode");
    assert_eq!(envelope.producer, ServiceName::Assets);
    assert_eq!(
        envelope.decode::<DeviceModelCreated>().expect("round trip"),
        *created
    );
}

/// `assets.gateway_bound` (Laysi/trustiot-v2#101): a tenant event of the tenant the gateway is
/// bound to, written by the assets service with the platform administrator as the actor; the
/// payload has the binding and the hardware ID, nothing secret.
#[test]
fn gateway_bound_decodes_every_version() {
    let decoded = decode_fixtures::<GatewayBound>();
    let names: Vec<&str> = decoded.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["v1"]);
    let binding: GatewayBindingId = "01928f6e-7a3c-7d4e-8f00-000000000b01"
        .parse()
        .expect("binding id");
    let bound = GatewayBound {
        binding_id: binding,
        hardware_id: GatewayId::new("tg-0001").expect("hardware id"),
    };
    assert_eq!(decoded[0].1, bound);

    let path = common::fixture_path("events/assets.gateway_bound/v1.json");
    let fixture: EventEnvelope =
        serde_json::from_str(&fs::read_to_string(path).expect("read fixture")).expect("envelope");
    let bound_to: TenantId = "01928f6e-7a3c-7d4e-8f00-00000000000b"
        .parse()
        .expect("tenant id");
    assert_eq!(fixture.tenant_id, Some(bound_to));
    let user: UserId = "01928f6e-7a3c-7d4e-8f00-0000000000ad"
        .parse()
        .expect("user id");
    assert_eq!(fixture.actor, Some(Actor::User { user_id: user }));

    let envelope = EventEnvelope::new(&bound, Some(tenant())).expect("encode");
    assert_eq!(envelope.producer, ServiceName::Assets);
    assert_eq!(envelope.event_type, "assets.gateway_bound");
    assert_eq!(envelope.tenant_id, Some(tenant()));
    assert_eq!(
        envelope.payload,
        json!({
            "binding_id": "01928f6e-7a3c-7d4e-8f00-000000000b01",
            "hardware_id": "tg-0001",
        })
    );
}

#[test]
fn new_envelopes_carry_type_version_tenant_time_and_producer() {
    let before = Utc::now();
    let envelope = EventEnvelope::new(&widget_renamed(), Some(tenant())).expect("encode");
    assert_eq!(envelope.id.as_uuid().get_version(), Some(Version::SortRand));
    assert_eq!(envelope.event_type, "example.widget_renamed");
    assert_eq!(envelope.version, 2);
    assert_eq!(envelope.tenant_id, Some(tenant()));
    assert_eq!(envelope.producer, ServiceName::Assets);
    assert_eq!(
        envelope.occurred_at.nanosecond() % 1_000_000,
        0,
        "millisecond precision"
    );
    assert!(envelope.occurred_at >= before - chrono::Duration::milliseconds(1));
    assert!(envelope.is::<WidgetRenamed>());
    assert_eq!(
        envelope.decode::<WidgetRenamed>().expect("decode"),
        widget_renamed()
    );
    assert_eq!(
        envelope.into_event::<WidgetRenamed>().expect("decode"),
        widget_renamed()
    );
}

#[test]
fn envelopes_round_trip_in_the_documented_shape() {
    let mut envelope = EventEnvelope::new(&widget_renamed(), Some(tenant())).expect("encode");
    envelope.occurred_at = DateTime::parse_from_rfc3339("2026-10-10T08:05:00.456Z")
        .expect("time")
        .with_timezone(&Utc);
    let value = serde_json::to_value(&envelope).expect("serialize");
    assert_eq!(
        value,
        json!({
            "id": envelope.id.to_string(),
            "type": "example.widget_renamed",
            "version": 2,
            "tenant_id": "01928f6e-7a3c-7d4e-8f00-00000000000a",
            "occurred_at": "2026-10-10T08:05:00.456Z",
            "producer": "assets",
            "payload": {
                "widget_id": "w-1",
                "display_name": "Pump B",
                "renamed_by": "01928f6e-7a3c-7d4e-8f00-0123456789ab"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(value).expect("deserialize"),
        envelope
    );
}

/// The envelope's `actor` (Laysi/trustiot-v2#130): written only when set, so envelopes without
/// one keep the shape above; an envelope written before the field existed decodes with none; a
/// kind added later decodes as `Unrecognized` and the event itself still decodes.
#[test]
fn actors_are_optional_tagged_and_open_to_new_kinds() {
    let user: UserId = "01928f6e-7a3c-7d4e-8f00-0000000000ad"
        .parse()
        .expect("user id");
    let envelope = EventEnvelope::new(&widget_renamed(), Some(tenant()))
        .expect("encode")
        .with_actor(Actor::User { user_id: user });
    let value = serde_json::to_value(&envelope).expect("serialize");
    assert_eq!(
        value["actor"],
        json!({"kind": "user", "user_id": "01928f6e-7a3c-7d4e-8f00-0000000000ad"})
    );
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(value).expect("deserialize"),
        envelope
    );

    let service = EventEnvelope::new(&widget_renamed(), None)
        .expect("encode")
        .with_actor(Actor::Service {
            service: ServiceName::Assets,
        });
    assert_eq!(
        serde_json::to_value(&service).expect("serialize")["actor"],
        json!({"kind": "service", "service": "assets"})
    );

    let without = EventEnvelope::new(&widget_renamed(), None).expect("encode");
    assert_eq!(without.actor, None);
    let value = serde_json::to_value(&without).expect("serialize");
    assert!(value.get("actor").is_none(), "no actor key: {value}");

    let mut newer = value.clone();
    newer["actor"] = json!({"kind": "scheduler", "schedule": "nightly"});
    let envelope: EventEnvelope = serde_json::from_value(newer).expect("deserialize");
    assert_eq!(envelope.actor, Some(Actor::Unrecognized));
    assert_eq!(
        envelope.decode::<WidgetRenamed>().expect("decode"),
        widget_renamed()
    );
    // Decode only: an envelope carrying it cannot be serialized, so it is never written again.
    assert!(
        serde_json::to_string(&envelope).is_err(),
        "an Unrecognized actor must not serialize"
    );
    assert!(
        serde_json::to_string(
            &EventEnvelope::new(&widget_renamed(), None)
                .expect("encode")
                .with_actor(Actor::Unrecognized)
        )
        .is_err(),
        "a producer cannot write an Unrecognized actor"
    );

    // An actor this build cannot decode never fails the envelope (Laysi/trustiot-v2#130).
    let mut malformed = value;
    malformed["actor"] = json!({"kind": "user"});
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(malformed)
            .expect("a malformed actor does not fail the envelope")
            .actor,
        Some(Actor::Unrecognized)
    );
}

#[test]
fn platform_wide_events_have_no_tenant() {
    let envelope = EventEnvelope::new(&widget_renamed(), None).expect("encode");
    let value = serde_json::to_value(&envelope).expect("serialize");
    assert_eq!(value["tenant_id"], Value::Null);
}

#[test]
fn fields_added_later_are_ignored_by_older_consumers() {
    let mut value =
        serde_json::to_value(EventEnvelope::new(&widget_renamed(), None).expect("encode"))
            .expect("serialize");
    value["traceparent"] = json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    value["payload"]["color"] = json!("red");
    let envelope: EventEnvelope = serde_json::from_value(value).expect("deserialize");
    assert_eq!(
        envelope.decode::<WidgetRenamed>().expect("decode"),
        widget_renamed()
    );
}

#[test]
fn a_newer_version_waits_for_the_consumer_upgrade() {
    let mut envelope = EventEnvelope::new(&widget_renamed(), None).expect("encode");
    envelope.version = 3;
    let error = envelope
        .decode::<WidgetRenamed>()
        .expect_err("version 3 is unknown");
    assert!(matches!(
        error,
        DecodeError::NewerVersion {
            found: 3,
            newest_known: 2,
            ..
        }
    ));
    assert!(error.is_retryable());
}

#[test]
fn mismatched_envelopes_are_rejected_for_good() {
    let good = EventEnvelope::new(&widget_renamed(), None).expect("encode");

    let mut other_type = good.clone();
    other_type.event_type = "example.widget_deleted".to_owned();
    assert!(!other_type.is::<WidgetRenamed>());
    assert!(matches!(
        other_type.decode::<WidgetRenamed>(),
        Err(DecodeError::WrongType { .. })
    ));

    let mut other_producer = good.clone();
    other_producer.producer = ServiceName::Identity;
    assert!(matches!(
        other_producer.decode::<WidgetRenamed>(),
        Err(DecodeError::WrongProducer {
            found: ServiceName::Identity,
            ..
        })
    ));

    let mut version_zero = good.clone();
    version_zero.version = 0;
    assert!(matches!(
        version_zero.decode::<WidgetRenamed>(),
        Err(DecodeError::UnknownVersion { version: 0, .. })
    ));

    let mut bad_payload = good;
    bad_payload.payload =
        json!({"widget_id": "w-1", "display_name": 42, "renamed_by": "alice@example.com"});
    let error = bad_payload
        .decode::<WidgetRenamed>()
        .expect_err("invalid payload");
    assert!(matches!(
        error,
        DecodeError::InvalidPayload { version: 2, .. }
    ));
    assert!(!error.is_retryable());
    let message = error.to_string();
    assert!(
        !message.contains("42") && !message.contains("alice"),
        "{message}"
    );
}

/// Decodes without `parse_payload`, to show the envelope checks the payload shape itself.
#[derive(Debug, Serialize, Deserialize)]
struct RawDecoded {
    widget_id: String,
}

impl Event for RawDecoded {
    const TYPE: &'static str = "example.raw_decoded";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;

    fn decode_payload(_version: u32, payload: Value) -> Result<Self, DecodeError> {
        serde_json::from_value(payload).map_err(|_| DecodeError::UnknownVersion {
            event_type: Self::TYPE,
            version: 1,
        })
    }
}

#[test]
fn positional_array_payloads_are_rejected() {
    let array = json!(["w-1", "Pump B", null]);
    // serde alone would accept the array as the struct's fields in order.
    assert!(serde_json::from_value::<WidgetRenamed>(array.clone()).is_ok());

    let mut envelope = EventEnvelope::new(&widget_renamed(), None).expect("encode");
    envelope.payload = array.clone();
    let error = envelope
        .decode::<WidgetRenamed>()
        .expect_err("array payload");
    assert!(matches!(
        error,
        DecodeError::PayloadNotAnObject { version: 2, .. }
    ));
    assert!(!error.is_retryable());
    assert!(matches!(
        parse_payload::<WidgetRenamed>(WidgetRenamed::TYPE, 2, array.clone()),
        Err(DecodeError::PayloadNotAnObject { .. })
    ));

    let mut raw = EventEnvelope::new(
        &RawDecoded {
            widget_id: "w-1".to_owned(),
        },
        None,
    )
    .expect("encode");
    raw.payload = json!(["w-1"]);
    assert!(serde_json::from_value::<RawDecoded>(raw.payload.clone()).is_ok());
    assert!(matches!(
        raw.into_event::<RawDecoded>(),
        Err(DecodeError::PayloadNotAnObject { .. })
    ));
}

#[derive(Debug, Serialize, Deserialize)]
struct Counter(u32);

impl Event for Counter {
    const TYPE: &'static str = "example.counter";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

#[test]
fn payloads_must_be_json_objects() {
    assert!(matches!(
        EventEnvelope::new(&Counter(3), None),
        Err(EncodeError::NotAnObject {
            event_type: "example.counter"
        })
    ));
}
