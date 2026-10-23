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

use protocol::config::DeviceModel;
use protocol::{GatewayId, ModelName};
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::{DeviceModelId, GatewayBindingId};
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
