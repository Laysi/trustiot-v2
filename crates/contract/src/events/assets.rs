//! Events the assets service produces.
//!
//! Device models (Laysi/trustiot-v2#99, rebuild checklist C1): every change of a model writes
//! one event in the transaction of the change, in the model's tenant, so the envelope's
//! `tenant_id` is the model's tenant (consumers take the tenant from there, never from the
//! payload). The aggregate is [`DEVICE_MODEL_AGGREGATE`] with the model ID, so a model's events
//! arrive in the order they were written.
//!
//! The created and updated events carry the whole model as the gateway reads it,
//! [`protocol::config::DeviceModel`] (the data point format is defined once, rebuild checklist
//! M4): a consumer such as the gateway configuration (Laysi/trustiot-v2#93) keeps its own copy
//! from the events alone and never has to ask the assets service for the current state. The data
//! points' descriptions are not in the events; they are read through the assets service's API.
//!
//! Gateway binding (Laysi/trustiot-v2#101, rebuild checklist D3): binding a gateway of the
//! device pool to a tenant writes one [`GatewayBound`] in the transaction of the binding, in the
//! tenant the gateway is bound to, so the envelope's `tenant_id` is that tenant (consumers take
//! it from there). The aggregate is [`GATEWAY_BINDING_AGGREGATE`] with the binding ID, not the
//! hardware ID: an aggregate belongs to the tenant of its first event (crates/outbox), and a
//! gateway that is unbound and bound again (Laysi/trustiot-v2#115) gets a new binding, in
//! whatever tenant it goes to. The certs service (renewal and broker permissions), the gateway
//! configuration and ingest map the hardware ID to the tenant from these events.
//!
//! Devices (Laysi/trustiot-v2#100, rebuild checklist C2): every change of a device writes one
//! event in the transaction of the change, in the device's tenant (the envelope's `tenant_id`;
//! no payload names a tenant). The aggregate is [`DEVICE_AGGREGATE`] with the device ID. The
//! created and updated events carry the device as the gateway polls it,
//! [`protocol::config::Device`], with the gateway's hardware ID and the model's ID next to it,
//! so the gateway configuration (Laysi/trustiot-v2#93) keeps its own copy of every gateway's
//! devices from the events alone. [`DeviceDeleted`] names the device, its gateway and its model,
//! which is what the gateway configuration needs to drop it and the field context service
//! (MS4, docs/architecture/field-context.md) needs to unmap it.
//!
//! Every device event also names the device's gateway binding, `binding_id` (optional, added
//! on 2026-10-29 by Laysi/trustiot-v2#719): the binding that was the gateway's current one when
//! the device was created, which the assets service never changes. A gateway that is unbound and
//! bound to the same tenant again (Laysi/trustiot-v2#115) gets a new binding, so the gateway
//! configuration leaves out the devices of an earlier binding. Events written before the field
//! existed have no `binding_id`; a consumer then knows only the tenant (`services/gateway-ops`,
//! README.md, "綁定與租戶").

use protocol::config::{Device, DeviceModel};
use protocol::{GatewayId, ModelName};
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::{DeviceId, DeviceModelId, GatewayBindingId};
use crate::service::ServiceName;

/// The aggregate kind of device model events; the aggregate ID is the model ID.
pub const DEVICE_MODEL_AGGREGATE: &str = "device_model";

/// A user created a device model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceModelCreated {
    /// The model.
    pub model_id: DeviceModelId,
    /// The model's revision: 1 for a new model, one more with every update.
    pub revision: u64,
    /// The name and the data points, as the gateway's configuration carries them. The model
    /// passed [`DeviceModel::validate`].
    pub model: DeviceModel,
}

impl Event for DeviceModelCreated {
    const TYPE: &'static str = "assets.device_model_created";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// A user replaced the data points of a device model. The name never changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceModelUpdated {
    /// The model.
    pub model_id: DeviceModelId,
    /// The model's revision after the update; a consumer that already holds a higher one for
    /// the model (after a replay, for example) keeps it.
    pub revision: u64,
    /// The whole model after the update, as in [`DeviceModelCreated::model`].
    pub model: DeviceModel,
}

impl Event for DeviceModelUpdated {
    const TYPE: &'static str = "assets.device_model_updated";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// A user deleted a device model. No device used it: the assets service refuses to delete a
/// model a device uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceModelDeleted {
    /// The model.
    pub model_id: DeviceModelId,
    /// Its name, free for a new model of the tenant from now on.
    pub name: ModelName,
    /// The model's last revision.
    pub revision: u64,
}

impl Event for DeviceModelDeleted {
    const TYPE: &'static str = "assets.device_model_deleted";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// The aggregate kind of gateway binding events; the aggregate ID is the binding ID.
pub const GATEWAY_BINDING_AGGREGATE: &str = "gateway_binding";

/// A platform administrator bound a gateway of the device pool to the event's tenant. The
/// gateway was bound to no tenant before. Binding it again to the same tenant writes no event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayBound {
    /// The binding: new for every binding of the gateway, so it is unique across tenants.
    pub binding_id: GatewayBindingId,
    /// The gateway's hardware ID: the common name of its certificates and its MQTT identity
    /// (`certs.gateway_registered`).
    pub hardware_id: GatewayId,
}

impl Event for GatewayBound {
    const TYPE: &'static str = "assets.gateway_bound";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// The aggregate kind of device events; the aggregate ID is the device ID.
pub const DEVICE_AGGREGATE: &str = "device";

/// A device's name: unique in its tenant, never changed after creation, under the naming rule of
/// model names (rebuild checklist C1 and C2): 1-40 lowercase letters or digits in groups joined
/// by single hyphens, starting with a letter.
pub type DeviceName = ModelName;

/// A user created a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCreated {
    /// The device.
    pub device_id: DeviceId,
    /// The device's revision: 1 for a new device, one more with every update.
    pub revision: u64,
    /// The device's name.
    pub name: DeviceName,
    /// The gateway that polls it: bound to the event's tenant when the device was created.
    pub hardware_id: GatewayId,
    /// The model it uses (`assets.device_model_*`); `device.model` is that model's name.
    pub model_id: DeviceModelId,
    /// The gateway binding the device belongs to: the gateway's current binding when the device
    /// was created, never changed afterwards (a binding of the event's tenant). `None` in events written before
    /// 2026-10-29 (Laysi/trustiot-v2#719), which the consumer must accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<GatewayBindingId>,
    /// The device as the gateway's configuration carries it: `id` is `device_id` as text,
    /// `model` the model's name, and the connection settings and poll interval passed the
    /// assets service's rules, which include crates/protocol's.
    pub device: Device,
}

impl Event for DeviceCreated {
    const TYPE: &'static str = "assets.device_created";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// A user changed a device's connection settings or poll interval. Its name, gateway and model
/// never change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceUpdated {
    /// The device.
    pub device_id: DeviceId,
    /// The device's revision after the update; a consumer that already holds a higher one for
    /// the device (after a replay, for example) keeps it.
    pub revision: u64,
    /// The device's name, as created.
    pub name: DeviceName,
    /// The gateway that polls it, as created.
    pub hardware_id: GatewayId,
    /// The model it uses, as created.
    pub model_id: DeviceModelId,
    /// The gateway binding the device belongs to: the gateway's current binding when the device
    /// was created, never changed afterwards (an update is refused once the binding has ended). `None` in events written before
    /// 2026-10-29 (Laysi/trustiot-v2#719), which the consumer must accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<GatewayBindingId>,
    /// The whole device after the update, as in [`DeviceCreated::device`].
    pub device: Device,
}

impl Event for DeviceUpdated {
    const TYPE: &'static str = "assets.device_updated";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// A user deleted a device: its gateway no longer polls it, and whatever refers to it (the field
/// context's mappings, MS4) lets go of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDeleted {
    /// The device.
    pub device_id: DeviceId,
    /// Its name, free for a new device of the tenant from now on.
    pub name: DeviceName,
    /// The gateway that polled it.
    pub hardware_id: GatewayId,
    /// The model it used, which may be deleted now if no other device uses it.
    pub model_id: DeviceModelId,
    /// The gateway binding the device belongs to: the gateway's current binding when the device
    /// was created, never changed afterwards (as created). `None` in events written before
    /// 2026-10-29 (Laysi/trustiot-v2#719), which the consumer must accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<GatewayBindingId>,
    /// The device's last revision.
    pub revision: u64,
}

impl Event for DeviceDeleted {
    const TYPE: &'static str = "assets.device_deleted";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}
