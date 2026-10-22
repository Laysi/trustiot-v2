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

use protocol::ModelName;
use protocol::config::DeviceModel;
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::DeviceModelId;
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
