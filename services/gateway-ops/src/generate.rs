//! One gateway's desired configuration from the projection's rows (services/gateway-ops/README.md,
//! "由事件到設定"). Pure: [`crate::projection`] reads the rows and stores the result.
//!
//! [`build`] puts the gateway's devices and the models they use into a [`Content`], in a fixed
//! order (models by name, devices by ID), so the same rows always give the same configuration
//! and an unchanged gateway gets no new version. It refuses a device whose model the projection
//! does not have (never received, or deleted) or names otherwise, and a configuration that breaks
//! crates/protocol's rules ([`DesiredConfig`]'s validation, the one the gateway runs before it
//! applies a configuration); [`encode`] refuses one too large for its MQTT channel. The caller
//! then keeps the gateway's previous version (rebuild checklist E1: a reference to a missing
//! resource is an explicit error), or stores what [`Content::empty`] or
//! [`Content::without_device`] gives when keeping it would be wrong (README.md, "錯誤").

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use contract::{DeviceId, DeviceModelId};
use protocol::config::{DesiredConfig, Device, DeviceModel};
use protocol::{ConfigVersion, EncodeError, Invalid, Message as _, V1};

/// A model as the projection has it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
    /// The model.
    pub model_id: DeviceModelId,
    /// The name and data points.
    pub model: DeviceModel,
}

/// A device as the projection has it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    /// The device.
    pub device_id: DeviceId,
    /// The model it uses.
    pub model_id: DeviceModelId,
    /// The device as the gateway polls it.
    pub device: Device,
}

/// What a configuration holds besides its version: compared with the previous version to decide
/// whether a new one is needed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    /// The models the devices use, each once, by name.
    pub models: Vec<DeviceModel>,
    /// The devices, by ID.
    pub devices: Vec<Device>,
}

impl Content {
    /// The configuration without devices: what an unbound gateway gets.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The configuration at `version`, at the lowest protocol revision that applies it, so that
    /// gateways of older revisions accept it whenever they can.
    #[must_use]
    pub fn at(&self, version: ConfigVersion) -> DesiredConfig {
        let mut config = DesiredConfig {
            v: V1,
            rev: protocol::PROTOCOL_REVISION,
            version,
            models: self.models.clone(),
            devices: self.devices.clone(),
        };
        config.rev = config.minimum_revision();
        config
    }

    /// Whether `config` holds exactly this content, whatever its version.
    #[must_use]
    pub fn same_as(&self, config: &DesiredConfig) -> bool {
        self.models == config.models && self.devices == config.devices
    }

    /// `config`'s content without the device `device_id` and without the models no remaining
    /// device uses; `None` when `config` does not list the device. What a deletion leaves of a
    /// stored version while the gateway's full configuration cannot be made (README.md, "錯誤"):
    /// everything in it was validated and stored before, and it holds less than that version.
    #[must_use]
    pub fn without_device(config: &DesiredConfig, device_id: DeviceId) -> Option<Self> {
        let id = device_id.to_string();
        let devices: Vec<Device> = config
            .devices
            .iter()
            .filter(|device| device.id.as_str() != id)
            .cloned()
            .collect();
        if devices.len() == config.devices.len() {
            return None;
        }
        let models = config
            .models
            .iter()
            .filter(|model| devices.iter().any(|device| device.model == model.name))
            .cloned()
            .collect();
        Some(Self { models, devices })
    }
}

/// Why no configuration was made. The gateway keeps its previous version.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GenerationError {
    /// A device's model is not in the projection, or was deleted.
    #[error("device {device_id} uses model {model_id}, which the projection does not have")]
    UnknownModel {
        /// The device.
        device_id: DeviceId,
        /// The model it names.
        model_id: DeviceModelId,
    },
    /// A device names its model by another name than the model has.
    #[error("device {device_id} names its model {model_id} by another name")]
    ModelMismatch {
        /// The device.
        device_id: DeviceId,
        /// The model it names.
        model_id: DeviceModelId,
    },
    /// The configuration breaks crates/protocol's rules.
    #[error("the configuration is invalid: {}", issue_summary(.0))]
    Invalid(Invalid),
    /// The configuration cannot be encoded, for example because it is larger than its channel
    /// allows.
    #[error("the configuration cannot be encoded: {0}")]
    Encode(String),
    /// A stored row of the projection could not be read, which the tables' rules should prevent.
    #[error("a stored {0} could not be read")]
    Corrupt(&'static str),
}

impl GenerationError {
    /// The reason for the logs and the metric's `reason` attribute.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::UnknownModel { .. } => "unknown_model",
            Self::ModelMismatch { .. } => "model_mismatch",
            Self::Invalid(_) => "invalid",
            Self::Encode(_) => "encode",
            Self::Corrupt(_) => "corrupt",
        }
    }
}

/// The issues as `code at path`, without their messages: a message may quote a tenant's
/// connection settings, which the logs do not need.
fn issue_summary(invalid: &Invalid) -> String {
    let mut summary = String::new();
    for (index, issue) in invalid.issues().iter().enumerate() {
        if index > 0 {
            summary.push_str(", ");
        }
        let _ = write!(summary, "{} at {}", issue.code, issue.path);
    }
    summary
}

/// The configuration of `devices` (one gateway's, of one binding) with the models they use from
/// `models`; see the module.
///
/// # Errors
///
/// [`GenerationError::UnknownModel`], [`GenerationError::ModelMismatch`] or
/// [`GenerationError::Invalid`].
pub fn build(
    devices: Vec<DeviceRow>,
    models: &HashMap<DeviceModelId, ModelRow>,
) -> Result<Content, GenerationError> {
    let mut used: BTreeMap<String, DeviceModel> = BTreeMap::new();
    let mut listed = Vec::with_capacity(devices.len());
    for row in devices {
        let Some(model) = models.get(&row.model_id) else {
            return Err(GenerationError::UnknownModel {
                device_id: row.device_id,
                model_id: row.model_id,
            });
        };
        if model.model.name != row.device.model {
            return Err(GenerationError::ModelMismatch {
                device_id: row.device_id,
                model_id: row.model_id,
            });
        }
        // Two models of one name (which the assets service never allows in a tenant) are both
        // kept, so the validation below reports the duplicate instead of hiding one of them.
        let key = format!("{}/{}", model.model.name, model.model_id);
        used.entry(key).or_insert_with(|| model.model.clone());
        listed.push(row.device);
    }
    listed.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let content = Content {
        models: used.into_values().collect(),
        devices: listed,
    };
    // Any version: the rules do not depend on it.
    content
        .at(ConfigVersion::new(1).unwrap_or_else(|| unreachable!("1 is a version")))
        .validate()
        .map_err(GenerationError::Invalid)?;
    Ok(content)
}

/// `content` encoded at `version`, as the gateway receives it.
///
/// # Errors
///
/// [`GenerationError::Invalid`] or [`GenerationError::Encode`].
pub fn encode(content: &Content, version: ConfigVersion) -> Result<Vec<u8>, GenerationError> {
    content.at(version).encode().map_err(|error| match error {
        EncodeError::Invalid(invalid) => GenerationError::Invalid(invalid),
        other => GenerationError::Encode(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use protocol::config::{DataPoint, DataType, FunctionCode, Host, Transport};
    use protocol::{IssueCode, ModelName, PointKey};

    use super::*;

    fn model(name: &str) -> DeviceModel {
        DeviceModel {
            name: ModelName::new(name).expect("a name"),
            points: vec![DataPoint {
                key: PointKey::new("voltage").expect("a key"),
                function_code: FunctionCode::ReadHoldingRegisters,
                address: 0,
                data_type: DataType::U16,
                byte_order: protocol::config::ByteOrder::default(),
                length: None,
                scale: Some(0.1),
                offset: None,
            }],
        }
    }

    fn device(id: DeviceId, model: &str, unit_id: u8) -> Device {
        Device {
            id: protocol::DeviceId::new(id.to_string()).expect("an id"),
            model: ModelName::new(model).expect("a name"),
            transport: Transport::RtuOverTcp,
            host: Host::new("192.0.2.10").expect("a host"),
            port: 8899,
            unit_id,
            poll_interval_ms: 60_000,
        }
    }

    fn models(rows: &[(DeviceModelId, &str)]) -> HashMap<DeviceModelId, ModelRow> {
        rows.iter()
            .map(|&(model_id, name)| {
                (
                    model_id,
                    ModelRow {
                        model_id,
                        model: model(name),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn devices_and_models_come_in_a_fixed_order_each_model_once() {
        let (meter, sensor) = (DeviceModelId::generate(), DeviceModelId::generate());
        let ids: Vec<DeviceId> = (0..3).map(|_| DeviceId::generate()).collect();
        let rows = vec![
            DeviceRow {
                device_id: ids[2],
                model_id: sensor,
                device: device(ids[2], "sensor", 3),
            },
            DeviceRow {
                device_id: ids[0],
                model_id: meter,
                device: device(ids[0], "meter", 1),
            },
            DeviceRow {
                device_id: ids[1],
                model_id: meter,
                device: device(ids[1], "meter", 2),
            },
        ];
        let known = models(&[(meter, "meter"), (sensor, "sensor")]);
        let content = build(rows.clone(), &known).expect("a valid configuration");
        let names: Vec<&str> = content.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["meter", "sensor"]);
        let mut expected: Vec<String> = ids.iter().map(ToString::to_string).collect();
        expected.sort();
        let listed: Vec<&str> = content.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(listed, expected);
        let mut reversed = rows;
        reversed.reverse();
        assert_eq!(build(reversed, &known).expect("valid"), content);

        let version = ConfigVersion::new(7).expect("a version");
        let payload = encode(&content, version).expect("encodes");
        let decoded = DesiredConfig::decode(&payload).expect("the gateway accepts it");
        assert_eq!(decoded.version, version);
        assert!(content.same_as(&decoded));
        assert!(!Content::empty().same_as(&decoded));
        let empty = encode(&Content::empty(), version).expect("encodes");
        assert!(
            DesiredConfig::decode(&empty)
                .expect("valid")
                .devices
                .is_empty()
        );
    }

    #[test]
    fn a_missing_or_misnamed_model_and_an_invalid_configuration_are_refused() {
        let (meter, gone) = (DeviceModelId::generate(), DeviceModelId::generate());
        let id = DeviceId::generate();
        let known = models(&[(meter, "meter")]);
        let missing = build(
            vec![DeviceRow {
                device_id: id,
                model_id: gone,
                device: device(id, "meter", 1),
            }],
            &known,
        )
        .expect_err("no such model");
        assert_eq!(
            missing,
            GenerationError::UnknownModel {
                device_id: id,
                model_id: gone
            }
        );
        assert_eq!(missing.reason(), "unknown_model");

        let misnamed = build(
            vec![DeviceRow {
                device_id: id,
                model_id: meter,
                device: device(id, "other", 1),
            }],
            &known,
        )
        .expect_err("another name");
        assert_eq!(misnamed.reason(), "model_mismatch");

        // Two devices on one RTU converter with the same unit ID break no field rule but are
        // not a valid configuration: the same unit twice on one bus.
        let other = DeviceId::generate();
        let mut first = device(id, "meter", 1);
        first.transport = Transport::RtuOverTcp;
        let mut second = device(other, "meter", 1);
        second.transport = Transport::ModbusTcp;
        let invalid = build(
            vec![
                DeviceRow {
                    device_id: id,
                    model_id: meter,
                    device: first,
                },
                DeviceRow {
                    device_id: other,
                    model_id: meter,
                    device: second,
                },
            ],
            &known,
        )
        .expect_err("mixed transports on one endpoint");
        let GenerationError::Invalid(issues) = &invalid else {
            panic!("{invalid:?}");
        };
        assert!(
            issues
                .issues()
                .iter()
                .any(|issue| issue.code == IssueCode::MixedTransport)
        );
        assert_eq!(invalid.reason(), "invalid");
        // The summary names codes and paths, never the connection settings.
        assert!(!invalid.to_string().contains("192.0.2.10"), "{invalid}");
    }

    #[test]
    fn a_deletion_leaves_the_stored_version_without_the_device_and_its_unused_model() {
        let (meter, sensor) = (DeviceModelId::generate(), DeviceModelId::generate());
        let ids: Vec<DeviceId> = (0..3).map(|_| DeviceId::generate()).collect();
        let rows = vec![
            DeviceRow {
                device_id: ids[0],
                model_id: meter,
                device: device(ids[0], "meter", 1),
            },
            DeviceRow {
                device_id: ids[1],
                model_id: meter,
                device: device(ids[1], "meter", 2),
            },
            DeviceRow {
                device_id: ids[2],
                model_id: sensor,
                device: device(ids[2], "sensor", 3),
            },
        ];
        let known = models(&[(meter, "meter"), (sensor, "sensor")]);
        let version = ConfigVersion::new(4).expect("a version");
        let stored = build(rows, &known).expect("valid").at(version);

        // The sensor's only device goes, and its model with it; the meters stay as stored.
        let left = Content::without_device(&stored, ids[2]).expect("listed");
        let names: Vec<&str> = left.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["meter"]);
        assert_eq!(left.devices.len(), 2);
        assert!(
            left.devices
                .iter()
                .all(|d| d.id.as_str() != ids[2].to_string())
        );
        DesiredConfig::decode(&encode(&left, version).expect("encodes")).expect("valid");

        // One of two meters goes: the model stays.
        let left = Content::without_device(&stored, ids[0]).expect("listed");
        assert_eq!(left.models.len(), 2);
        assert_eq!(left.devices.len(), 2);

        // A device the version does not list leaves nothing to store.
        assert_eq!(Content::without_device(&stored, DeviceId::generate()), None);
    }

    #[test]
    fn more_devices_than_a_configuration_holds_are_refused_and_a_deletion_still_trims() {
        // MAX_DEVICES + 1 devices, each on its own converter port so no other rule breaks.
        let meter = DeviceModelId::generate();
        let known = models(&[(meter, "meter")]);
        let rows: Vec<DeviceRow> = (0..=protocol::config::MAX_DEVICES)
            .map(|index| {
                let id = DeviceId::generate();
                let mut polled = device(id, "meter", 1);
                polled.port = u16::try_from(1000 + index).expect("a port");
                DeviceRow {
                    device_id: id,
                    model_id: meter,
                    device: polled,
                }
            })
            .collect();
        let too_many = build(rows.clone(), &known).expect_err("one device too many");
        assert_eq!(too_many.reason(), "invalid");
        let GenerationError::Invalid(issues) = &too_many else {
            panic!("{too_many:?}");
        };
        assert!(
            issues
                .issues()
                .iter()
                .any(|issue| issue.code == IssueCode::TooMany)
        );

        // The version stored before the last device came: deleting one of its devices still
        // gives a configuration to store.
        let stored = build(rows[..protocol::config::MAX_DEVICES].to_vec(), &known)
            .expect("at the limit")
            .at(ConfigVersion::new(9).expect("a version"));
        let left = Content::without_device(&stored, rows[0].device_id).expect("listed");
        assert_eq!(left.devices.len(), protocol::config::MAX_DEVICES - 1);
        encode(&left, ConfigVersion::new(10).expect("a version")).expect("encodes");
    }
}
