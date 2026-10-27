//! The bodies of the two device write entries (create, update) and their rules (rebuild
//! checklist C2). Both entries read the JSON by hand, so every problem becomes a field error of
//! the M1 format with a JSON Pointer and a `device.*` message key, in two steps:
//!
//! 1. Each field on its own: unknown fields, the name (the model naming rule), the gateway's
//!    hardware ID, the model's ID, and the connection settings and poll interval.
//! 2. When every field passed, crates/protocol's configuration rules for the device, the same
//!    function the gateway runs before it applies a configuration: the settings that pass step 1
//!    always pass it, and this keeps it so if the protocol's rules tighten.
//!
//! The rules across devices of one gateway (one transport per endpoint, one device per unit ID
//! on it) and whether the gateway and the model belong to the tenant are checked against the
//! database afterwards (`store.rs`).
//!
//! A body that is not a JSON object is a 400 `invalid_request`; every rule broken is a 422
//! `validation_failed` listing up to [`MAX_FIELD_ERRORS`] field errors. Nothing is written unless
//! both steps pass.

use std::net::IpAddr;
use std::str::FromStr;

use contract::{DeviceModelId, ErrorBody, ErrorCode, FieldError, MessageKey};
use protocol::config::{
    DesiredConfig, Device, DeviceModel, Host, MAX_HOST_LEN, MAX_POLL_INTERVAL_MS, Transport,
};
use protocol::{
    ConfigVersion, DeviceId as GatewayDeviceId, GatewayId, IssueCode, MAX_ID_LEN,
    MAX_MODEL_NAME_LEN, Message, ModelName, PROTOCOL_REVISION, V1,
};
use serde_json::{Map, Value};

use crate::device_models::document::{from_wire_name, wire_name};

/// The poll interval of a device whose body does not set one, in milliseconds: one minute, the
/// shortest interval every plan allows (docs/product/plans.md: Starter 1 minute, Business
/// 10 seconds, Enterprise 1 second), so a device created with the default never needs changing
/// when the plans' limits are checked (Laysi/trustiot-v2#116). Rebuild checklist C2 asks for one
/// default for every device; the legacy system had none (each entry set its own).
pub const DEFAULT_POLL_INTERVAL_MS: u32 = 60_000;

/// The shortest poll interval the API accepts, in milliseconds: one second, the shortest any
/// plan allows. crates/protocol accepts 100 ms; no plan sells that, so the API refuses it rather
/// than store a setting a later plan check would have to undo. A plan's own minimum is checked on
/// top (Laysi/trustiot-v2#116).
pub const MIN_POLL_INTERVAL_MS: u32 = 1_000;

/// The unit ID of a device whose body does not set one: 1, the legacy system's default and the
/// first RTU address.
pub const DEFAULT_UNIT_ID: u8 = 1;

/// The unit IDs an RS485 device behind an RTU-over-TCP converter can have (the Modbus RTU
/// address range; 0 is broadcast, 248-255 are reserved).
pub const RTU_UNIT_IDS: (u8, u8) = (1, 247);

/// Most field errors one response lists; `details.omitted_field_errors` counts the rest.
pub const MAX_FIELD_ERRORS: usize = 20;

/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("device.body_invalid");
/// A field the entry does not know.
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("device.unknown_field");
/// A required field is missing or `null`.
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("device.field_required");
/// The name breaks the naming rule of models; `details.max` is 40.
pub const NAME_INVALID: MessageKey = MessageKey::new("device.name_invalid");
/// The hardware ID breaks the hardware ID rule; `details.max` is 64.
pub const HARDWARE_ID_INVALID: MessageKey = MessageKey::new("device.hardware_id_invalid");
/// The model ID is not a UUID.
pub const MODEL_ID_INVALID: MessageKey = MessageKey::new("device.model_id_invalid");
/// The transport is not one of `details.allowed`.
pub const TRANSPORT_INVALID: MessageKey = MessageKey::new("device.transport_invalid");
/// The host is not a unicast IP address or a DNS name of at most `details.max` bytes, or it is
/// the gateway itself (a loopback address or `localhost`).
pub const HOST_INVALID: MessageKey = MessageKey::new("device.host_invalid");
/// The port is not an integer `details.min`-`details.max`.
pub const PORT_INVALID: MessageKey = MessageKey::new("device.port_invalid");
/// The unit ID is not an integer `details.min`-`details.max` (1-247 for RTU over TCP).
pub const UNIT_ID_INVALID: MessageKey = MessageKey::new("device.unit_id_invalid");
/// The poll interval is not an integer `details.min`-`details.max` milliseconds.
pub const POLL_INTERVAL_INVALID: MessageKey = MessageKey::new("device.poll_interval_invalid");
/// An update's `revision` is not a positive integer.
pub const REVISION_INVALID: MessageKey = MessageKey::new("device.revision_invalid");
/// The settings break one of crates/protocol's configuration rules that the field rules above do
/// not already cover; reaching it means the two sets of rules drifted apart.
pub const SETTINGS_INVALID: MessageKey = MessageKey::new("device.settings_invalid");

/// The fields of a create body.
const CREATE_FIELDS: &[&str] = &[
    "name",
    "hardware_id",
    "model_id",
    "transport",
    "host",
    "port",
    "unit_id",
    "poll_interval_ms",
];
/// The fields of an update body: the settings, the revision, and the fields that never change.
const UPDATE_FIELDS: &[&str] = &[
    "revision",
    "name",
    "hardware_id",
    "model_id",
    "transport",
    "host",
    "port",
    "unit_id",
    "poll_interval_ms",
];

/// An unknown field's name is shown in its pointer only when it is this short and printable
/// ASCII; otherwise the error points at the body, so a response never echoes an unbounded or
/// invisible string.
const MAX_ECHOED_FIELD_LEN: usize = 64;

/// Every transport, in the order the API documents them.
pub const TRANSPORTS: [Transport; 2] = [Transport::ModbusTcp, Transport::RtuOverTcp];

/// How the gateway reaches a device and how often it polls it: everything about a device an
/// update can change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Modbus TCP, or RTU over TCP to an RS485 converter.
    pub transport: Transport,
    /// The device's or its converter's address; never the gateway itself.
    pub host: Host,
    /// The TCP port, 1-65535.
    pub port: u16,
    /// The Modbus unit ID: 0-255 for Modbus TCP, 1-247 for RTU over TCP.
    pub unit_id: u8,
    /// How often the gateway polls the device, in milliseconds.
    pub poll_interval_ms: u32,
}

/// A create body that passed every rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDevice {
    /// The device's name.
    pub name: ModelName,
    /// The gateway that polls it.
    pub hardware_id: GatewayId,
    /// The model it uses.
    pub model_id: DeviceModelId,
    /// Its settings.
    pub settings: Settings,
}

/// The fields of an update that never change, as the client sent them: each must be the stored
/// value, which only the database knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Unchanged {
    /// `name`, if sent.
    pub name: Option<Value>,
    /// `hardware_id`, if sent.
    pub hardware_id: Option<Value>,
    /// `model_id`, if sent.
    pub model_id: Option<Value>,
}

/// An update body that passed every rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The revision the client last read; the update is refused when the device changed since.
    pub revision: u64,
    /// The new settings, which replace all the old ones.
    pub settings: Settings,
    /// The fields that never change, as sent.
    pub unchanged: Unchanged,
}

/// `POST /api/assets/v1/devices`.
///
/// # Errors
///
/// The response body: 400 when the body is not a JSON object, 422 with the field errors.
pub fn parse_create(body: &[u8]) -> Result<NewDevice, ErrorBody> {
    let map = object(body)?;
    let mut errors = Errors::default();
    reject_unknown(&map, CREATE_FIELDS, &mut errors);
    let name = required(&map, "name", &mut errors).and_then(|value| {
        let name = value.as_str().and_then(|text| ModelName::new(text).ok());
        if name.is_none() {
            errors
                .add(FieldError::new("/name", NAME_INVALID).with_detail("max", MAX_MODEL_NAME_LEN));
        }
        name
    });
    let hardware_id = required(&map, "hardware_id", &mut errors).and_then(|value| {
        let id = value.as_str().and_then(|text| GatewayId::new(text).ok());
        if id.is_none() {
            errors.add(
                FieldError::new("/hardware_id", HARDWARE_ID_INVALID).with_detail("max", MAX_ID_LEN),
            );
        }
        id
    });
    let model_id = required(&map, "model_id", &mut errors).and_then(|value| {
        let id = value
            .as_str()
            .and_then(|text| text.parse::<DeviceModelId>().ok());
        if id.is_none() {
            errors.add(FieldError::new("/model_id", MODEL_ID_INVALID));
        }
        id
    });
    let settings = settings(&map, &mut errors);
    errors.finish()?;
    match (name, hardware_id, model_id, settings) {
        (Some(name), Some(hardware_id), Some(model_id), Some(settings)) => Ok(NewDevice {
            name,
            hardware_id,
            model_id,
            settings,
        }),
        _ => Err(Errors::default().into_body()),
    }
}

/// `PUT /api/assets/v1/devices/{device_id}`.
///
/// # Errors
///
/// As [`parse_create`]. Whether `revision` is the current one and the unchanging fields are the
/// stored ones is checked against the database afterwards.
pub fn parse_update(body: &[u8]) -> Result<Change, ErrorBody> {
    let map = object(body)?;
    let mut errors = Errors::default();
    reject_unknown(&map, UPDATE_FIELDS, &mut errors);
    let revision = required(&map, "revision", &mut errors).and_then(|value| {
        let revision = value.as_u64().filter(|revision| *revision >= 1);
        if revision.is_none() {
            errors.add(FieldError::new("/revision", REVISION_INVALID));
        }
        revision
    });
    let unchanged = Unchanged {
        name: optional(&map, "name").cloned(),
        hardware_id: optional(&map, "hardware_id").cloned(),
        model_id: optional(&map, "model_id").cloned(),
    };
    let settings = settings(&map, &mut errors);
    errors.finish()?;
    match (revision, settings) {
        (Some(revision), Some(settings)) => Ok(Change {
            revision,
            settings,
            unchanged,
        }),
        _ => Err(Errors::default().into_body()),
    }
}

/// The settings, when every field passed its rule and the device passes crates/protocol's.
fn settings(map: &Map<String, Value>, errors: &mut Errors) -> Option<Settings> {
    let before = errors.count();
    let transport = required(map, "transport", errors).and_then(|value| {
        let transport = value.as_str().and_then(from_wire_name::<Transport>);
        if transport.is_none() {
            errors.add(
                FieldError::new("/transport", TRANSPORT_INVALID)
                    .with_detail("allowed", TRANSPORTS.map(wire_name).to_vec()),
            );
        }
        transport
    });
    let host = required(map, "host", errors).and_then(|value| {
        let host = value.as_str().and_then(parse_host);
        if host.is_none() {
            errors.add(FieldError::new("/host", HOST_INVALID).with_detail("max", MAX_HOST_LEN));
        }
        host
    });
    let port = required(map, "port", errors).and_then(|value| {
        let port = value
            .as_u64()
            .and_then(|number| u16::try_from(number).ok())
            .filter(|port| *port != 0);
        if port.is_none() {
            errors.add(
                FieldError::new("/port", PORT_INVALID)
                    .with_detail("min", 1)
                    .with_detail("max", u16::MAX),
            );
        }
        port
    });
    // The unit IDs the transport allows; Modbus TCP's whole byte when the transport is unknown,
    // which then has its own error.
    let (min_unit, max_unit) = match transport {
        Some(Transport::RtuOverTcp) => RTU_UNIT_IDS,
        _ => (u8::MIN, u8::MAX),
    };
    let unit_id = match optional(map, "unit_id") {
        None => Some(DEFAULT_UNIT_ID).filter(|unit| (min_unit..=max_unit).contains(unit)),
        Some(value) => {
            let unit = value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .filter(|unit| (min_unit..=max_unit).contains(unit));
            if unit.is_none() {
                errors.add(
                    FieldError::new("/unit_id", UNIT_ID_INVALID)
                        .with_detail("min", min_unit)
                        .with_detail("max", max_unit),
                );
            }
            unit
        }
    };
    let poll_interval_ms = match optional(map, "poll_interval_ms") {
        None => Some(DEFAULT_POLL_INTERVAL_MS),
        Some(value) => {
            let interval = value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .filter(|ms| (MIN_POLL_INTERVAL_MS..=MAX_POLL_INTERVAL_MS).contains(ms));
            if interval.is_none() {
                errors.add(
                    FieldError::new("/poll_interval_ms", POLL_INTERVAL_INVALID)
                        .with_detail("min", MIN_POLL_INTERVAL_MS)
                        .with_detail("max", MAX_POLL_INTERVAL_MS),
                );
            }
            interval
        }
    };
    if errors.count() != before {
        return None;
    }
    let settings = Settings {
        transport: transport?,
        host: host?,
        port: port?,
        unit_id: unit_id?,
        poll_interval_ms: poll_interval_ms?,
    };
    // Step 2: crates/protocol's rules, on a configuration of this one device.
    let before = errors.count();
    for issue in protocol_issues(&settings) {
        errors.add(issue);
    }
    (errors.count() == before).then_some(settings)
}

/// The field errors crates/protocol's configuration rules give a configuration of this one
/// device: none for settings that passed the field rules.
fn protocol_issues(settings: &Settings) -> Vec<FieldError> {
    // A placeholder ID and model: only the settings are checked here.
    let (Ok(id), Ok(model)) = (
        GatewayDeviceId::new("00000000-0000-0000-0000-000000000000"),
        ModelName::new("m"),
    ) else {
        return vec![FieldError::new("", SETTINGS_INVALID)];
    };
    let Some(version) = ConfigVersion::new(1) else {
        return vec![FieldError::new("", SETTINGS_INVALID)];
    };
    let config = DesiredConfig {
        v: V1,
        rev: PROTOCOL_REVISION,
        version,
        models: vec![DeviceModel {
            name: model.clone(),
            points: Vec::new(),
        }],
        devices: vec![gateway_device(id, model, settings)],
    };
    match config.validate() {
        Ok(()) => Vec::new(),
        Err(invalid) => invalid
            .issues()
            .iter()
            .map(|issue| {
                let field = issue
                    .path
                    .strip_prefix("devices[0].")
                    .map(|field| format!("/{field}"))
                    .unwrap_or_default();
                let key = match (&issue.code, field.as_str()) {
                    (&IssueCode::OutOfRange, "/port") => PORT_INVALID,
                    (&IssueCode::OutOfRange, "/unit_id") => UNIT_ID_INVALID,
                    (&IssueCode::OutOfRange, "/poll_interval_ms") => POLL_INTERVAL_INVALID,
                    _ => SETTINGS_INVALID,
                };
                FieldError::new(field, key)
            })
            .collect(),
    }
}

/// The device as the gateway's configuration carries it.
#[must_use]
pub fn gateway_device(id: GatewayDeviceId, model: ModelName, settings: &Settings) -> Device {
    Device {
        id,
        model,
        transport: settings.transport,
        host: settings.host.clone(),
        port: settings.port,
        unit_id: settings.unit_id,
        poll_interval_ms: settings.poll_interval_ms,
    }
}

/// A host the gateway may poll: crates/protocol's rule (a unicast IP address or a DNS name),
/// and not spelled as the gateway itself. A loopback address, `localhost`, `*.localhost`, or
/// `ip6-localhost` and `ip6-loopback` (which the default /etc/hosts of Raspberry Pi OS and
/// other Debian systems maps to ::1) would point the gateway's Modbus client at the gateway's
/// own ports, which hold no field device, so it is refused as a mistake. Names that a resolver
/// reads as numbers (`0x7f000001`) are refused by crates/protocol's rule already.
///
/// This is input validation, not a security boundary: the cloud cannot resolve a name the way
/// the gateway will on the site's network, and any other DNS name may resolve to a loopback
/// address there, or start to after the check. Keeping the poller off the gateway's own
/// services is the gateway's to enforce when it resolves and connects: edge/agent's address
/// guard (`modbus/address.rs`, Laysi/trustiot-v2#653) connects only to resolved addresses it
/// checked. This check does not replace it.
#[must_use]
pub fn parse_host(text: &str) -> Option<Host> {
    let host = Host::new(text).ok()?;
    let local = match IpAddr::from_str(host.as_str()) {
        Ok(ip) => ip.to_canonical().is_loopback(),
        Err(_) => {
            let canonical = host.canonical();
            let name = canonical.trim_end_matches('.');
            LOCAL_NAMES.contains(&name) || name.ends_with(".localhost")
        }
    };
    (!local).then_some(host)
}

/// Names that point at the gateway itself on every gateway, in lowercase: `localhost`, and the
/// names the default /etc/hosts of Debian systems (Raspberry Pi OS included) gives ::1.
const LOCAL_NAMES: [&str; 3] = ["localhost", "ip6-localhost", "ip6-loopback"];

/// The body as a JSON object.
fn object(body: &[u8]) -> Result<Map<String, Value>, ErrorBody> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID)),
    }
}

/// The field errors found so far.
#[derive(Default)]
struct Errors {
    listed: Vec<FieldError>,
    omitted: usize,
}

impl Errors {
    fn add(&mut self, error: FieldError) {
        if self.listed.len() < MAX_FIELD_ERRORS {
            self.listed.push(error);
        } else {
            self.omitted += 1;
        }
    }

    fn count(&self) -> usize {
        self.listed.len() + self.omitted
    }

    fn finish(self) -> Result<(), ErrorBody> {
        if self.count() == 0 {
            Ok(())
        } else {
            Err(self.into_body())
        }
    }

    fn into_body(self) -> ErrorBody {
        let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
        for error in self.listed {
            body = body.with_field_error(error);
        }
        if self.omitted > 0 {
            body = body.with_detail("omitted_field_errors", self.omitted);
        }
        body
    }
}

/// A field error for every field of the body not in `allowed`.
fn reject_unknown(map: &Map<String, Value>, allowed: &[&str], errors: &mut Errors) {
    for field in map.keys() {
        if !allowed.contains(&field.as_str()) {
            let echo = field.len() <= MAX_ECHOED_FIELD_LEN
                && field.bytes().all(|byte| byte.is_ascii_graphic());
            let pointer = if echo {
                format!("/{}", field.replace('~', "~0").replace('/', "~1"))
            } else {
                String::new()
            };
            errors.add(FieldError::new(pointer, UNKNOWN_FIELD));
        }
    }
}

/// The field `name`, `None` when it is missing or `null`.
fn optional<'a>(map: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    map.get(name).filter(|value| !value.is_null())
}

/// The field `name`; a missing or `null` one is a field error.
fn required<'a>(map: &'a Map<String, Value>, name: &str, errors: &mut Errors) -> Option<&'a Value> {
    let value = optional(map, name);
    if value.is_none() {
        errors.add(FieldError::new(format!("/{name}"), FIELD_REQUIRED));
    }
    value
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const MODEL: &str = "01928f6e-7a3c-7d4e-8f00-000000000a01";

    fn create() -> Value {
        json!({"name": "line-1-meter", "hardware_id": "tg-0001", "model_id": MODEL,
               "transport": "rtu_over_tcp", "host": "192.0.2.10", "port": 8899, "unit_id": 3,
               "poll_interval_ms": 10_000})
    }

    fn update() -> Value {
        json!({"revision": 1, "transport": "modbus_tcp", "host": "198.51.100.7", "port": 502})
    }

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    /// The (field, message key) pairs of a 422.
    fn field_errors(result: Result<impl std::fmt::Debug, ErrorBody>) -> Vec<(String, String)> {
        let body = result.expect_err("the body is refused");
        assert_eq!(body.code(), ErrorCode::ValidationFailed, "{body:?}");
        body.field_errors()
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect()
    }

    fn with(base: &Value, changes: &Value) -> Value {
        let mut body = base.clone();
        for (field, value) in changes.as_object().unwrap() {
            if value.is_null() {
                body.as_object_mut().unwrap().remove(field);
            } else {
                body[field] = value.clone();
            }
        }
        body
    }

    #[test]
    fn a_valid_device_passes() {
        let device = parse_create(&bytes(&create())).unwrap();
        assert_eq!(device.name.as_str(), "line-1-meter");
        assert_eq!(device.hardware_id.as_str(), "tg-0001");
        assert_eq!(device.model_id.to_string(), MODEL);
        assert_eq!(
            device.settings,
            Settings {
                transport: Transport::RtuOverTcp,
                host: Host::new("192.0.2.10").unwrap(),
                port: 8899,
                unit_id: 3,
                poll_interval_ms: 10_000,
            }
        );
        let change = parse_update(&bytes(&update())).unwrap();
        assert_eq!(change.revision, 1);
        assert_eq!(change.unchanged, Unchanged::default());
        assert_eq!(change.settings.transport, Transport::ModbusTcp);
    }

    #[test]
    fn absent_unit_id_and_interval_take_the_unified_defaults() {
        let body = with(
            &create(),
            &json!({"unit_id": null, "poll_interval_ms": null}),
        );
        let device = parse_create(&bytes(&body)).unwrap();
        assert_eq!(device.settings.unit_id, DEFAULT_UNIT_ID);
        assert_eq!(device.settings.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(DEFAULT_POLL_INTERVAL_MS, 60_000);
        // An update is a whole replacement: what it leaves out returns to the default.
        let change = parse_update(&bytes(&update())).unwrap();
        assert_eq!(change.settings.unit_id, 1);
        assert_eq!(change.settings.poll_interval_ms, 60_000);
        // An explicit null is an absent field.
        let body = with(&update(), &json!({"unit_id": Value::Null}));
        assert!(parse_update(&bytes(&body)).is_ok());
    }

    #[test]
    fn the_settings_rules_hold_on_both_entries() {
        let cases: Vec<(Value, &str, MessageKey)> = vec![
            (json!({"transport": null}), "/transport", FIELD_REQUIRED),
            (
                json!({"transport": "mock"}),
                "/transport",
                TRANSPORT_INVALID,
            ),
            (
                json!({"transport": "modbus-tcp"}),
                "/transport",
                TRANSPORT_INVALID,
            ),
            (
                json!({"transport": "ModbusTcp"}),
                "/transport",
                TRANSPORT_INVALID,
            ),
            (json!({"transport": 1}), "/transport", TRANSPORT_INVALID),
            (json!({"host": null}), "/host", FIELD_REQUIRED),
            (json!({"host": ""}), "/host", HOST_INVALID),
            (json!({"host": "192.168.1.300"}), "/host", HOST_INVALID),
            (json!({"host": "0.0.0.0"}), "/host", HOST_INVALID),
            (json!({"host": "255.255.255.255"}), "/host", HOST_INVALID),
            (json!({"host": "224.0.0.1"}), "/host", HOST_INVALID),
            (json!({"host": "127.0.0.1"}), "/host", HOST_INVALID),
            (json!({"host": "127.1.2.3"}), "/host", HOST_INVALID),
            (json!({"host": "::1"}), "/host", HOST_INVALID),
            (json!({"host": "::ffff:127.0.0.1"}), "/host", HOST_INVALID),
            (json!({"host": "localhost"}), "/host", HOST_INVALID),
            (json!({"host": "LocalHost"}), "/host", HOST_INVALID),
            (json!({"host": "plc.localhost"}), "/host", HOST_INVALID),
            (json!({"host": "ip6-localhost"}), "/host", HOST_INVALID),
            (json!({"host": "IP6-Loopback"}), "/host", HOST_INVALID),
            (json!({"host": "0x7f000001"}), "/host", HOST_INVALID),
            (json!({"host": "0x7f.0.0.0x1"}), "/host", HOST_INVALID),
            (json!({"host": "0x0"}), "/host", HOST_INVALID),
            (json!({"host": "0xe0000001"}), "/host", HOST_INVALID),
            (json!({"host": "0xffffffff"}), "/host", HOST_INVALID),
            (json!({"host": "-bad.example"}), "/host", HOST_INVALID),
            (json!({"host": "a b"}), "/host", HOST_INVALID),
            (json!({"host": "http://192.0.2.1"}), "/host", HOST_INVALID),
            (json!({"host": "192.0.2.1:502"}), "/host", HOST_INVALID),
            (
                json!({"host": format!("{}.example", "a".repeat(64))}),
                "/host",
                HOST_INVALID,
            ),
            (json!({"host": 3_221_225_985_u64}), "/host", HOST_INVALID),
            (json!({"port": null}), "/port", FIELD_REQUIRED),
            (json!({"port": 0}), "/port", PORT_INVALID),
            (json!({"port": 65_536}), "/port", PORT_INVALID),
            (json!({"port": -1}), "/port", PORT_INVALID),
            (json!({"port": 502.5}), "/port", PORT_INVALID),
            (json!({"port": "502"}), "/port", PORT_INVALID),
            (json!({"unit_id": 256}), "/unit_id", UNIT_ID_INVALID),
            (json!({"unit_id": -1}), "/unit_id", UNIT_ID_INVALID),
            (json!({"unit_id": "1"}), "/unit_id", UNIT_ID_INVALID),
            (
                json!({"transport": "rtu_over_tcp", "unit_id": 0}),
                "/unit_id",
                UNIT_ID_INVALID,
            ),
            (
                json!({"transport": "rtu_over_tcp", "unit_id": 248}),
                "/unit_id",
                UNIT_ID_INVALID,
            ),
            (
                json!({"poll_interval_ms": 999}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": 100}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": 0}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": 86_400_001}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": 1500.5}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": "60000"}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (
                json!({"poll_interval_ms": 4_294_967_296_u64}),
                "/poll_interval_ms",
                POLL_INTERVAL_INVALID,
            ),
            (json!({"tenant_id": MODEL}), "/tenant_id", UNKNOWN_FIELD),
            (json!({"mode": "server"}), "/mode", UNKNOWN_FIELD),
        ];
        for (changes, field, key) in cases {
            for (entry, errors) in [
                (
                    "create",
                    field_errors(parse_create(&bytes(&with(&create(), &changes)))),
                ),
                (
                    "update",
                    field_errors(parse_update(&bytes(&with(&update(), &changes)))),
                ),
            ] {
                assert_eq!(
                    errors,
                    [(field.to_owned(), key.as_str().to_owned())],
                    "{entry}: {changes}"
                );
            }
        }
    }

    #[test]
    fn the_range_limits_are_accepted() {
        for changes in [
            json!({"transport": "modbus_tcp", "unit_id": 0}),
            json!({"transport": "modbus_tcp", "unit_id": 255}),
            json!({"transport": "rtu_over_tcp", "unit_id": 1}),
            json!({"transport": "rtu_over_tcp", "unit_id": 247}),
            json!({"port": 1}),
            json!({"port": 65_535}),
            json!({"poll_interval_ms": 1_000}),
            json!({"poll_interval_ms": 86_400_000}),
            json!({"host": "2001:db8::10"}),
            json!({"host": "::ffff:192.0.2.1"}),
            json!({"host": "169.254.10.20"}),
            // The site LAN: private ranges and IPv6 ULA.
            json!({"host": "10.1.2.3"}),
            json!({"host": "172.16.0.10"}),
            json!({"host": "192.168.10.21"}),
            json!({"host": "fd12:3456:789a::10"}),
            json!({"host": "0x7f.plc.example"}),
            json!({"host": "localhost-meter"}),
            json!({"host": "Meter-1.Plant.Example"}),
            json!({"host": format!("{}.example", "a".repeat(63))}),
        ] {
            assert!(
                parse_create(&bytes(&with(&create(), &changes))).is_ok(),
                "{changes}"
            );
            assert!(
                parse_update(&bytes(&with(&update(), &changes))).is_ok(),
                "{changes}"
            );
        }
    }

    #[test]
    fn create_needs_a_name_gateway_and_model_under_their_rules() {
        let cases: Vec<(Value, &str, MessageKey)> = vec![
            (json!({"name": null}), "/name", FIELD_REQUIRED),
            (json!({"name": "Line-1"}), "/name", NAME_INVALID),
            (json!({"name": "line_1"}), "/name", NAME_INVALID),
            (json!({"name": "1-line"}), "/name", NAME_INVALID),
            (json!({"name": "line--1"}), "/name", NAME_INVALID),
            (json!({"name": "電表"}), "/name", NAME_INVALID),
            (json!({"name": "a".repeat(41)}), "/name", NAME_INVALID),
            (json!({"name": 7}), "/name", NAME_INVALID),
            (json!({"hardware_id": null}), "/hardware_id", FIELD_REQUIRED),
            (
                json!({"hardware_id": "tg/0001"}),
                "/hardware_id",
                HARDWARE_ID_INVALID,
            ),
            (
                json!({"hardware_id": "+"}),
                "/hardware_id",
                HARDWARE_ID_INVALID,
            ),
            (
                json!({"hardware_id": "x".repeat(65)}),
                "/hardware_id",
                HARDWARE_ID_INVALID,
            ),
            (json!({"model_id": null}), "/model_id", FIELD_REQUIRED),
            (
                json!({"model_id": "power-meter"}),
                "/model_id",
                MODEL_ID_INVALID,
            ),
            (json!({"model_id": 1}), "/model_id", MODEL_ID_INVALID),
            (json!({"revision": 1}), "/revision", UNKNOWN_FIELD),
        ];
        for (changes, field, key) in cases {
            assert_eq!(
                field_errors(parse_create(&bytes(&with(&create(), &changes)))),
                [(field.to_owned(), key.as_str().to_owned())],
                "{changes}"
            );
        }
        assert_eq!(
            field_errors(parse_create(&bytes(&with(
                &create(),
                &json!({"name": "A"})
            )))),
            [("/name".to_owned(), NAME_INVALID.as_str().to_owned())]
        );
        let error = parse_create(&bytes(&with(&create(), &json!({"name": "A"})))).unwrap_err();
        assert_eq!(error.field_errors()[0].details["max"], json!(40));
    }

    #[test]
    fn an_update_needs_a_revision_and_keeps_the_unchanging_fields_as_sent() {
        for revision in [json!(0), json!(-1), json!(1.5), json!("1")] {
            let body = with(&update(), &json!({"revision": revision}));
            assert_eq!(
                field_errors(parse_update(&bytes(&body))),
                [("/revision".to_owned(), REVISION_INVALID.as_str().to_owned())],
                "{revision}"
            );
        }
        assert_eq!(
            field_errors(parse_update(&bytes(&with(
                &update(),
                &json!({"revision": null})
            )))),
            [("/revision".to_owned(), FIELD_REQUIRED.as_str().to_owned())]
        );
        // Any value passes here; the store compares it with the stored one.
        let body = with(
            &update(),
            &json!({"name": "Not Valid", "hardware_id": 5, "model_id": MODEL}),
        );
        let change = parse_update(&bytes(&body)).unwrap();
        assert_eq!(change.unchanged.name, Some(json!("Not Valid")));
        assert_eq!(change.unchanged.hardware_id, Some(json!(5)));
        assert_eq!(change.unchanged.model_id, Some(json!(MODEL)));
    }

    #[test]
    fn a_body_that_is_not_an_object_is_a_bad_request() {
        for body in [&b"not json"[..], b"[]", b"\"x\"", b"null", b"{\"name\": "] {
            for result in [
                parse_create(body).map(|_| ()),
                parse_update(body).map(|_| ()),
            ] {
                let error = result.expect_err("refused");
                assert_eq!(error.code(), ErrorCode::InvalidRequest);
                assert_eq!(error.message_key(), &BODY_INVALID);
            }
        }
    }

    #[test]
    fn unknown_fields_are_refused_without_echoing_odd_names() {
        let mut body = create();
        body["a/b~c"] = json!(1);
        body["x".repeat(65)] = json!(1);
        body["zero\u{200B}width"] = json!(1);
        let mut errors = field_errors(parse_create(&bytes(&body)));
        errors.sort();
        let unknown = UNKNOWN_FIELD.as_str().to_owned();
        assert_eq!(
            errors,
            [
                (String::new(), unknown.clone()),
                (String::new(), unknown.clone()),
                ("/a~1b~0c".to_owned(), unknown),
            ]
        );
    }

    #[test]
    fn many_errors_are_listed_up_to_the_limit() {
        let mut body = json!({});
        for index in 0..30 {
            body[format!("extra{index}")] = json!(1);
        }
        let error = parse_create(&bytes(&body)).unwrap_err();
        assert_eq!(error.field_errors().len(), MAX_FIELD_ERRORS);
        // 30 unknown fields and 6 required ones missing.
        assert_eq!(
            error.details()["omitted_field_errors"],
            json!(30 + 6 - MAX_FIELD_ERRORS)
        );
    }

    /// Every setting the field rules accept passes crates/protocol's configuration rules, so a
    /// stored device always fits the gateway's configuration.
    #[test]
    fn accepted_settings_pass_the_protocols_rules() {
        for transport in TRANSPORTS {
            let (min, max) = match transport {
                Transport::RtuOverTcp => RTU_UNIT_IDS,
                Transport::ModbusTcp => (0, 255),
            };
            for unit_id in [min, max] {
                for poll_interval_ms in [MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS] {
                    let settings = Settings {
                        transport,
                        host: Host::new("192.0.2.1").unwrap(),
                        port: 1,
                        unit_id,
                        poll_interval_ms,
                    };
                    assert!(protocol_issues(&settings).is_empty(), "{settings:?}");
                }
            }
        }
        // A setting protocol refuses, which the field rules never let through, maps to its field.
        let settings = Settings {
            transport: Transport::RtuOverTcp,
            host: Host::new("192.0.2.1").unwrap(),
            port: 0,
            unit_id: 0,
            poll_interval_ms: 1,
        };
        let mut fields: Vec<(String, String)> = protocol_issues(&settings)
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect();
        fields.sort();
        assert_eq!(
            fields,
            [
                (
                    "/poll_interval_ms".to_owned(),
                    POLL_INTERVAL_INVALID.as_str().to_owned()
                ),
                ("/port".to_owned(), PORT_INVALID.as_str().to_owned()),
                ("/unit_id".to_owned(), UNIT_ID_INVALID.as_str().to_owned()),
            ]
        );
    }

    #[test]
    fn transports_use_the_protocols_names() {
        assert_eq!(
            TRANSPORTS.map(wire_name).to_vec(),
            ["modbus_tcp", "rtu_over_tcp"]
        );
        for transport in TRANSPORTS {
            match transport {
                Transport::ModbusTcp | Transport::RtuOverTcp => {}
            }
            assert_eq!(
                from_wire_name::<Transport>(&wire_name(transport)),
                Some(transport)
            );
        }
    }
}
