//! The device table (migrations/postgres/0004_devices.sql) and its events. Every function works
//! in one transaction of the caller's tenant (`TenantDb::begin`), so row-level security limits
//! it to that tenant's devices, gateway bindings and models; a write and its `assets.device_*`
//! event commit or roll back together (the outbox, decision 4).
//!
//! Writes that place a device on a gateway (create, update) first take a transaction-scoped
//! advisory lock on the tenant's gateway, so the rules across the gateway's devices (one
//! transport per endpoint, one device per unit ID on it) are checked and written by one
//! transaction at a time. The unit ID rule is also a unique constraint, which the lock makes
//! unreachable; the transport rule spans rows and has only the lock.

use chrono::{DateTime, Utc};
use contract::events::assets::{DEVICE_AGGREGATE, DeviceCreated, DeviceDeleted, DeviceUpdated};
use contract::{DeviceId, DeviceModelId, Event, GatewayBindingId, TenantId};
use outbox::Aggregate;
use protocol::config::{Host, Transport};
use protocol::{DeviceId as GatewayDeviceId, GatewayId, ModelName};
use serde_json::Value;
use service_kit::identity::UserIdentity;
use service_kit::tenant::{TenantDb, TenantDbError, TenantTransaction};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::document::{Change, NewDevice, Settings, gateway_device};
use crate::device_models::document::{from_wire_name, wire_name};

/// Serializes the writes that place devices on one gateway of the tenant until the transaction
/// ends. `$1`: the lock's name, `assets.devices/<tenant>/<hardware ID>`. Creating a device reads
/// the gateway's binding under it; unbinding (Laysi/trustiot-v2#115) must take the same lock
/// before it ends a binding, since the service role cannot lock binding rows (no `UPDATE`).
const LOCK_GATEWAY: &str = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";

/// The tenant's current binding of a gateway, if any (row-level security shows only the
/// tenant's bindings). `$1`: the hardware ID.
const CURRENT_BINDING: &str = "SELECT binding_id::text AS binding_id FROM assets.gateway_bindings
WHERE hardware_id = $1 AND unbound_at IS NULL";

/// A model of the tenant, by ID. `$1`: the model ID.
const MODEL_NAME: &str = "SELECT name FROM assets.device_models WHERE model_id = $1::uuid";

/// The other devices of the gateway at the same endpoint, oldest first. `$1` hardware ID,
/// `$2` canonical host, `$3` port, `$4` the device's own ID (excluded).
const SAME_ENDPOINT: &str = "SELECT device_id::text AS device_id, transport, unit_id
FROM assets.devices
WHERE hardware_id = $1 AND host_key = $2 AND port = $3 AND device_id <> $4::uuid
ORDER BY created_at, device_id";

/// A new device. Row-level security checks the tenant.
const INSERT_DEVICE: &str = "INSERT INTO assets.devices
    (tenant_id, device_id, name, binding_id, hardware_id, model_id, transport, host, host_key,
     port, unit_id, poll_interval_ms)
VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5, $6::uuid, $7, $8, $9, $10, $11, $12)
RETURNING revision, created_at, updated_at";

/// The tenant's devices by name, each with its model's name.
const LIST_DEVICES: &str =
    "SELECT device.device_id::text AS device_id, device.name, device.hardware_id,
    device.binding_id::text AS binding_id, device.model_id::text AS model_id, model.name AS model_name, device.transport, device.host,
    device.port, device.unit_id, device.poll_interval_ms, device.revision, device.created_at,
    device.updated_at
FROM assets.devices AS device
JOIN assets.device_models AS model
    ON model.tenant_id = device.tenant_id AND model.model_id = device.model_id
ORDER BY device.name COLLATE \"C\"";

/// One device. `$1`: the device ID.
const SELECT_DEVICE: &str =
    "SELECT device.device_id::text AS device_id, device.name, device.hardware_id,
    device.binding_id::text AS binding_id, device.model_id::text AS model_id, model.name AS model_name, device.transport, device.host,
    device.port, device.unit_id, device.poll_interval_ms, device.revision, device.created_at,
    device.updated_at
FROM assets.devices AS device
JOIN assets.device_models AS model
    ON model.tenant_id = device.tenant_id AND model.model_id = device.model_id
WHERE device.device_id = $1::uuid";

/// The same, with the device's row locked until the transaction ends, so concurrent updates
/// and deletes of the device queue up, and whether the device's binding is still the gateway's
/// current one (`binding_current`). `$1`: the device ID.
const LOCK_DEVICE: &str =
    "SELECT device.device_id::text AS device_id, device.name, device.hardware_id,
    device.binding_id::text AS binding_id, device.model_id::text AS model_id, model.name AS model_name, device.transport, device.host,
    device.port, device.unit_id, device.poll_interval_ms, device.revision, device.created_at,
    device.updated_at, binding.unbound_at IS NULL AS binding_current
FROM assets.devices AS device
JOIN assets.device_models AS model
    ON model.tenant_id = device.tenant_id AND model.model_id = device.model_id
JOIN assets.gateway_bindings AS binding
    ON binding.tenant_id = device.tenant_id AND binding.binding_id = device.binding_id
WHERE device.device_id = $1::uuid
FOR UPDATE OF device";

/// The gateway of a device, which never changes, to take the gateway's lock before the device's.
/// `$1`: the device ID.
const DEVICE_GATEWAY: &str = "SELECT hardware_id FROM assets.devices WHERE device_id = $1::uuid";

/// New settings and the next revision. `$1`: the device ID.
const UPDATE_DEVICE: &str = "UPDATE assets.devices
SET transport = $2, host = $3, host_key = $4, port = $5, unit_id = $6, poll_interval_ms = $7,
    revision = revision + 1, updated_at = now()
WHERE device_id = $1::uuid
RETURNING revision, created_at, updated_at";

/// A device. `$1`: the device ID.
const DELETE_DEVICE: &str = "DELETE FROM assets.devices WHERE device_id = $1::uuid
RETURNING name, hardware_id, model_id::text AS model_id, revision";

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// The constraint that keeps names unique per tenant.
const NAME_UNIQUE: &str = "devices_name_unique";
/// The constraint that keeps one device per unit ID on an endpoint of a gateway.
const UNIT_UNIQUE: &str = "devices_unit_unique";
/// The foreign key to the gateway's binding.
const BINDING_KEY: &str = "devices_binding";
/// The foreign key to the model.
const MODEL_KEY: &str = "devices_model";

/// A stored device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDevice {
    /// The device.
    pub device_id: DeviceId,
    /// Its name, never changed.
    pub name: ModelName,
    /// The gateway that polls it, never changed.
    pub hardware_id: GatewayId,
    /// The gateway's binding the device belongs to: the current one when it was created; never
    /// changed.
    pub binding_id: GatewayBindingId,
    /// The model it uses, never changed.
    pub model_id: DeviceModelId,
    /// The model's name, which the gateway's configuration names the model by.
    pub model_name: ModelName,
    /// Its settings.
    pub settings: Settings,
    /// 1 at creation, one more with every update.
    pub revision: u64,
    /// When it was created, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    pub updated_at: DateTime<Utc>,
}

/// Which fields that never change an update sent with another value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    /// `name`.
    pub name: bool,
    /// `hardware_id`.
    pub hardware_id: bool,
    /// `model_id`.
    pub model_id: bool,
}

/// Why a request was not carried out. Every error leaves the database as it was.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The caller has not picked or joined a tenant.
    #[error("the request's identity has no tenant")]
    NoTenant,
    /// No device of the caller's tenant has the ID.
    #[error("no such device")]
    NotFound,
    /// The gateway is not bound to the caller's tenant (bound to another, not in the pool, or
    /// unknown: the caller learns none of these apart).
    #[error("the gateway is not bound to the tenant")]
    GatewayNotBound,
    /// The tenant has no model of the ID.
    #[error("no such model")]
    ModelNotFound,
    /// The tenant has a device of that name.
    #[error("the name is taken")]
    NameTaken,
    /// Another device of the gateway is at the same endpoint with another transport.
    #[error("the endpoint is used with the other transport")]
    TransportMismatch {
        /// That device.
        device_id: Option<DeviceId>,
        /// Its transport.
        transport: Option<Transport>,
    },
    /// Another device of the gateway is at the same endpoint with the same unit ID.
    #[error("the unit ID is taken on the endpoint")]
    UnitTaken {
        /// That device, when known.
        device_id: Option<DeviceId>,
    },
    /// An update sent another value for a field that never changes.
    #[error("the name, gateway and model of a device never change")]
    Immutable(Changed),
    /// The device's binding is no longer its gateway's current one (the gateway was unbound, and
    /// perhaps bound again): the device can only be deleted.
    #[error("the device's gateway binding has ended")]
    BindingEnded,
    /// An update expected another revision than the current one.
    #[error("the device is at revision {current}")]
    RevisionMismatch {
        /// The current revision.
        current: u64,
    },
    /// A stored row breaks the rules the table enforces, which cannot happen; nothing is
    /// returned from it.
    #[error("a stored device breaks the rules")]
    Corrupt,
    /// The database failed or refused the transaction.
    #[error(transparent)]
    Database(TenantDbError),
    /// The event could not be written.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; a device ID always is, so this does not happen.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<TenantDbError> for StoreError {
    fn from(error: TenantDbError) -> Self {
        match error {
            TenantDbError::NoTenant => Self::NoTenant,
            other => Self::Database(other),
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl StoreError {
    /// The kind of the failure for the logs: a SQLSTATE or a category, never data.
    #[must_use]
    pub fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(sqlx::Error::Database(database))) => database
                .code()
                .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
            Self::Database(TenantDbError::Database(sqlx::Error::PoolTimedOut)) => {
                "pool timed out".to_owned()
            }
            Self::Database(TenantDbError::Database(_)) => "database client".to_owned(),
            Self::Publish(outbox::PublishError::Database(_)) => "outbox database".to_owned(),
            Self::Publish(_) => "outbox encode".to_owned(),
            other => other.to_string(),
        }
    }
}

/// A write's database error, with the constraints the API answers for mapped to their errors.
fn write_error(error: sqlx::Error) -> StoreError {
    let violation = error.as_database_error().and_then(|database| {
        Some((
            database.code()?.into_owned(),
            database.constraint().map(str::to_owned),
        ))
    });
    match violation {
        Some((code, Some(constraint))) if code == UNIQUE_VIOLATION && constraint == NAME_UNIQUE => {
            StoreError::NameTaken
        }
        Some((code, Some(constraint))) if code == UNIQUE_VIOLATION && constraint == UNIT_UNIQUE => {
            StoreError::UnitTaken { device_id: None }
        }
        Some((code, Some(constraint)))
            if code == FOREIGN_KEY_VIOLATION && constraint == BINDING_KEY =>
        {
            StoreError::GatewayNotBound
        }
        Some((code, Some(constraint)))
            if code == FOREIGN_KEY_VIOLATION && constraint == MODEL_KEY =>
        {
            StoreError::ModelNotFound
        }
        _ => StoreError::from(error),
    }
}

/// The caller's tenant's devices, by name.
///
/// # Errors
///
/// [`StoreError::NoTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn list(db: &TenantDb, user: &UserIdentity) -> Result<Vec<StoredDevice>, StoreError> {
    let mut tx = db.begin(user).await?;
    let rows = sqlx::query(LIST_DEVICES).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    rows.iter().map(stored_device).collect()
}

/// One device of the caller's tenant.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the tenant has no device `device_id`; [`StoreError::NoTenant`],
/// [`StoreError::Corrupt`] or a database failure.
pub async fn get(
    db: &TenantDb,
    user: &UserIdentity,
    device_id: DeviceId,
) -> Result<StoredDevice, StoreError> {
    let mut tx = db.begin(user).await?;
    let row = sqlx::query(SELECT_DEVICE)
        .bind(device_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let device = stored_device(&row.ok_or(StoreError::NotFound)?)?;
    // Row-level security returned the caller's tenant's device only; a row that is not the one
    // asked for is not trusted.
    if device.device_id != device_id {
        return Err(StoreError::Corrupt);
    }
    Ok(device)
}

/// Creates a device in the caller's tenant and writes `assets.device_created`.
///
/// # Errors
///
/// [`StoreError::GatewayNotBound`], [`StoreError::ModelNotFound`], [`StoreError::NameTaken`],
/// [`StoreError::TransportMismatch`], [`StoreError::UnitTaken`], [`StoreError::NoTenant`] or a
/// failure; nothing is written then.
pub async fn create(
    db: &TenantDb,
    user: &UserIdentity,
    device: NewDevice,
) -> Result<StoredDevice, StoreError> {
    let mut tx = db.begin(user).await?;
    let tenant = tx.tenant_id();
    let device_id = DeviceId::generate();
    lock_gateway(&mut tx, tenant, &device.hardware_id).await?;
    let Some(binding) = sqlx::query(CURRENT_BINDING)
        .bind(device.hardware_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::GatewayNotBound);
    };
    let binding_id: GatewayBindingId = binding
        .try_get::<String, _>("binding_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt)?;
    let Some(model) = sqlx::query(MODEL_NAME)
        .bind(device.model_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::ModelNotFound);
    };
    let model_name =
        ModelName::new(model.try_get::<String, _>("name")?).map_err(|_| StoreError::Corrupt)?;
    check_endpoint(&mut tx, &device.hardware_id, device_id, &device.settings).await?;
    let settings = &device.settings;
    let row = sqlx::query(INSERT_DEVICE)
        .bind(tenant.to_string())
        .bind(device_id.to_string())
        .bind(device.name.as_str())
        .bind(binding_id.to_string())
        .bind(device.hardware_id.as_str())
        .bind(device.model_id.to_string())
        .bind(wire_name(settings.transport))
        .bind(settings.host.as_str())
        .bind(settings.host.canonical())
        .bind(i32::from(settings.port))
        .bind(i16::from(settings.unit_id))
        .bind(interval_column(settings.poll_interval_ms)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(write_error)?;
    let stored = StoredDevice {
        device_id,
        name: device.name,
        hardware_id: device.hardware_id,
        binding_id,
        model_id: device.model_id,
        model_name,
        settings: device.settings,
        revision: revision(&row)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    };
    publish(
        &mut tx,
        device_id,
        &DeviceCreated {
            device_id,
            revision: stored.revision,
            name: stored.name.clone(),
            hardware_id: stored.hardware_id.clone(),
            model_id: stored.model_id,
            device: protocol_device(&stored)?,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(stored)
}

/// Replaces the settings of a device of the caller's tenant and writes `assets.device_updated`.
/// The gateway's lock, then the device's row lock, are held from the read to the commit, so a
/// concurrent write waits and then sees this one.
///
/// # Errors
///
/// [`StoreError::NotFound`], [`StoreError::BindingEnded`] when the device's binding is no
/// longer the gateway's current one, [`StoreError::Immutable`] when the change sends another name,
/// gateway or model, [`StoreError::RevisionMismatch`] when it expects another revision,
/// [`StoreError::TransportMismatch`], [`StoreError::UnitTaken`], [`StoreError::NoTenant`] or a
/// failure; nothing is written then.
pub async fn replace(
    db: &TenantDb,
    user: &UserIdentity,
    device_id: DeviceId,
    change: Change,
) -> Result<StoredDevice, StoreError> {
    let mut tx = db.begin(user).await?;
    let tenant = tx.tenant_id();
    // The gateway never changes, so reading it before the lock is safe; the lock order (gateway,
    // then device) is the same as a create's, which takes the gateway's only.
    let Some(row) = sqlx::query(DEVICE_GATEWAY)
        .bind(device_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::NotFound);
    };
    let hardware_id = GatewayId::new(row.try_get::<String, _>("hardware_id")?)
        .map_err(|_| StoreError::Corrupt)?;
    lock_gateway(&mut tx, tenant, &hardware_id).await?;
    let Some(row) = sqlx::query(LOCK_DEVICE)
        .bind(device_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::NotFound);
    };
    let current = stored_device(&row)?;
    if current.device_id != device_id || current.hardware_id != hardware_id {
        return Err(StoreError::Corrupt);
    }
    // Read under the gateway's lock, which unbinding takes too (Laysi/trustiot-v2#115): a device
    // of an ended binding is never changed, so no event can put it back on the gateway, which
    // may serve another tenant now.
    if !row.try_get::<bool, _>("binding_current")? {
        return Err(StoreError::BindingEnded);
    }
    let changed = Changed {
        name: change
            .unchanged
            .name
            .as_ref()
            .is_some_and(|sent| sent.as_str() != Some(current.name.as_str())),
        hardware_id: change
            .unchanged
            .hardware_id
            .as_ref()
            .is_some_and(|sent| sent.as_str() != Some(current.hardware_id.as_str())),
        model_id: change
            .unchanged
            .model_id
            .as_ref()
            .is_some_and(|sent| !same_model(sent, current.model_id)),
    };
    if changed != Changed::default() {
        return Err(StoreError::Immutable(changed));
    }
    if change.revision != current.revision {
        return Err(StoreError::RevisionMismatch {
            current: current.revision,
        });
    }
    check_endpoint(&mut tx, &hardware_id, device_id, &change.settings).await?;
    let settings = &change.settings;
    let row = sqlx::query(UPDATE_DEVICE)
        .bind(device_id.to_string())
        .bind(wire_name(settings.transport))
        .bind(settings.host.as_str())
        .bind(settings.host.canonical())
        .bind(i32::from(settings.port))
        .bind(i16::from(settings.unit_id))
        .bind(interval_column(settings.poll_interval_ms)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(write_error)?;
    let stored = StoredDevice {
        settings: change.settings,
        revision: revision(&row)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        ..current
    };
    publish(
        &mut tx,
        device_id,
        &DeviceUpdated {
            device_id,
            revision: stored.revision,
            name: stored.name.clone(),
            hardware_id: stored.hardware_id.clone(),
            model_id: stored.model_id,
            device: protocol_device(&stored)?,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(stored)
}

/// Deletes a device of the caller's tenant and writes `assets.device_deleted`; returns the
/// deleted device's last revision.
///
/// # Errors
///
/// [`StoreError::NotFound`], [`StoreError::NoTenant`] or a failure; nothing is deleted then.
pub async fn delete(
    db: &TenantDb,
    user: &UserIdentity,
    device_id: DeviceId,
) -> Result<u64, StoreError> {
    let mut tx = db.begin(user).await?;
    let Some(row) = sqlx::query(DELETE_DEVICE)
        .bind(device_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::NotFound);
    };
    let revision = revision(&row)?;
    let deleted = DeviceDeleted {
        device_id,
        name: ModelName::new(row.try_get::<String, _>("name")?).map_err(|_| StoreError::Corrupt)?,
        hardware_id: GatewayId::new(row.try_get::<String, _>("hardware_id")?)
            .map_err(|_| StoreError::Corrupt)?,
        model_id: row
            .try_get::<String, _>("model_id")?
            .parse()
            .map_err(|_| StoreError::Corrupt)?,
        revision,
    };
    publish(&mut tx, device_id, &deleted).await?;
    tx.commit().await?;
    Ok(revision)
}

/// Whether an update's `model_id` names the stored model: the same UUID, in any letter case.
fn same_model(sent: &Value, stored: DeviceModelId) -> bool {
    sent.as_str()
        .and_then(|text| text.parse::<DeviceModelId>().ok())
        .is_some_and(|sent| sent == stored)
}

/// Takes the tenant's lock on the gateway's devices until the transaction ends.
async fn lock_gateway(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    hardware_id: &GatewayId,
) -> Result<(), StoreError> {
    let name = format!("assets.devices/{tenant}/{hardware_id}");
    sqlx::query(LOCK_GATEWAY)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The rules across the gateway's devices at the settings' endpoint (the same canonical host and
/// port, such as one RS485 converter): every device there uses one transport, and no two have
/// the same unit ID. Runs under the gateway's lock.
async fn check_endpoint(
    tx: &mut TenantTransaction,
    hardware_id: &GatewayId,
    device_id: DeviceId,
    settings: &Settings,
) -> Result<(), StoreError> {
    let rows = sqlx::query(SAME_ENDPOINT)
        .bind(hardware_id.as_str())
        .bind(settings.host.canonical())
        .bind(i32::from(settings.port))
        .bind(device_id.to_string())
        .fetch_all(&mut **tx)
        .await?;
    let mut others = Vec::with_capacity(rows.len());
    for row in &rows {
        let other: DeviceId = row
            .try_get::<String, _>("device_id")?
            .parse()
            .map_err(|_| StoreError::Corrupt)?;
        let transport = from_wire_name::<Transport>(&row.try_get::<String, _>("transport")?)
            .ok_or(StoreError::Corrupt)?;
        let unit_id =
            u8::try_from(row.try_get::<i16, _>("unit_id")?).map_err(|_| StoreError::Corrupt)?;
        others.push((other, transport, unit_id));
    }
    if let Some(&(other, transport, _)) = others
        .iter()
        .find(|(_, transport, _)| *transport != settings.transport)
    {
        return Err(StoreError::TransportMismatch {
            device_id: Some(other),
            transport: Some(transport),
        });
    }
    if let Some(&(other, _, _)) = others
        .iter()
        .find(|(_, _, unit_id)| *unit_id == settings.unit_id)
    {
        return Err(StoreError::UnitTaken {
            device_id: Some(other),
        });
    }
    Ok(())
}

/// The device as the gateway's configuration and the events carry it.
fn protocol_device(device: &StoredDevice) -> Result<protocol::config::Device, StoreError> {
    let id = GatewayDeviceId::new(device.device_id.to_string()).map_err(|_| StoreError::Corrupt)?;
    Ok(gateway_device(
        id,
        device.model_name.clone(),
        &device.settings,
    ))
}

async fn publish<E: Event>(
    tx: &mut TenantTransaction,
    device_id: DeviceId,
    event: &E,
) -> Result<(), StoreError> {
    let aggregate = Aggregate::new(DEVICE_AGGREGATE, device_id.to_string())?;
    let tenant = tx.tenant_id();
    outbox::publish(tx.transaction(), &aggregate, event, Some(tenant)).await?;
    Ok(())
}

/// The poll interval as the table's `integer`.
fn interval_column(poll_interval_ms: u32) -> Result<i32, StoreError> {
    i32::try_from(poll_interval_ms).map_err(|_| StoreError::Corrupt)
}

fn revision(row: &PgRow) -> Result<u64, StoreError> {
    u64::try_from(row.try_get::<i64, _>("revision")?).map_err(|_| StoreError::Corrupt)
}

/// A device read by [`LIST_DEVICES`], [`SELECT_DEVICE`] or [`LOCK_DEVICE`].
fn stored_device(row: &PgRow) -> Result<StoredDevice, StoreError> {
    let text = |column: &str| row.try_get::<String, _>(column);
    let settings = Settings {
        transport: from_wire_name::<Transport>(&text("transport")?).ok_or(StoreError::Corrupt)?,
        host: Host::new(text("host")?).map_err(|_| StoreError::Corrupt)?,
        port: u16::try_from(row.try_get::<i32, _>("port")?).map_err(|_| StoreError::Corrupt)?,
        unit_id: u8::try_from(row.try_get::<i16, _>("unit_id")?)
            .map_err(|_| StoreError::Corrupt)?,
        poll_interval_ms: u32::try_from(row.try_get::<i32, _>("poll_interval_ms")?)
            .map_err(|_| StoreError::Corrupt)?,
    };
    Ok(StoredDevice {
        device_id: text("device_id")?
            .parse()
            .map_err(|_| StoreError::Corrupt)?,
        name: ModelName::new(text("name")?).map_err(|_| StoreError::Corrupt)?,
        hardware_id: GatewayId::new(text("hardware_id")?).map_err(|_| StoreError::Corrupt)?,
        binding_id: text("binding_id")?
            .parse()
            .map_err(|_| StoreError::Corrupt)?,
        model_id: text("model_id")?.parse().map_err(|_| StoreError::Corrupt)?,
        model_name: ModelName::new(text("model_name")?).map_err(|_| StoreError::Corrupt)?,
        settings,
        revision: revision(row)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
