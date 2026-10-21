//! The device model tables (migrations/postgres/0002_device_models.sql) and their events. Every
//! function works in one transaction of the caller's tenant (`TenantDb::begin`), so row-level
//! security limits it to that tenant's models; a write and its `assets.device_model_*` event
//! commit or roll back together (the outbox, decision 4).

use chrono::{DateTime, Utc};
use contract::events::assets::{
    DEVICE_MODEL_AGGREGATE, DeviceModelCreated, DeviceModelDeleted, DeviceModelUpdated,
};
use contract::{DeviceModelId, Event, TenantId};
use outbox::Aggregate;
use protocol::config::{ByteOrder, DataPoint, DataType, DeviceModel, FunctionCode};
use protocol::{ModelName, PointKey};
use service_kit::identity::UserIdentity;
use service_kit::tenant::{TenantDb, TenantDbError, TenantTransaction};
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::document::{NewModel, Point, Replacement, from_wire_name, wire_name};

/// The tenant's models with their number of points, by name. `$1`: none.
const LIST_MODELS: &str = "SELECT model.model_id::text AS model_id, model.name, model.revision,
    model.created_at, model.updated_at,
    (SELECT count(*) FROM assets.device_model_points AS point
        WHERE point.tenant_id = model.tenant_id AND point.model_id = model.model_id)
        AS point_count
FROM assets.device_models AS model
ORDER BY model.name COLLATE \"C\"";

/// One model with its points in order, in one statement so the model's revision and its points
/// come from the same snapshot; a model without points is one row with NULL point columns.
/// `$1`: the model ID as text.
const SELECT_MODEL: &str = "SELECT model.model_id::text AS model_id, model.name, model.revision,
    model.created_at, model.updated_at, point.key, point.function_code, point.address,
    point.data_type, point.byte_order, point.length, point.scale, point.value_offset,
    point.description
FROM assets.device_models AS model
LEFT JOIN assets.device_model_points AS point
    ON point.tenant_id = model.tenant_id AND point.model_id = model.model_id
WHERE model.model_id = $1::uuid
ORDER BY point.position";

/// A new model. `$1` tenant, `$2` model ID, `$3` name; row-level security checks the tenant.
const INSERT_MODEL: &str = "INSERT INTO assets.device_models (tenant_id, model_id, name)
VALUES ($1::uuid, $2::uuid, $3)
RETURNING revision, created_at, updated_at";

/// A model's points, one array per column, in order. `$1` tenant, `$2` model ID.
const INSERT_POINTS: &str = "INSERT INTO assets.device_model_points
    (tenant_id, model_id, position, key, function_code, address, data_type, byte_order, length,
     scale, value_offset, description)
SELECT $1::uuid, $2::uuid, point.position, point.key, point.function_code, point.address,
    point.data_type, point.byte_order, point.length, point.scale, point.value_offset,
    point.description
FROM UNNEST($3::integer[], $4::text[], $5::smallint[], $6::integer[], $7::text[], $8::text[],
    $9::integer[], $10::double precision[], $11::double precision[], $12::text[])
    AS point (position, key, function_code, address, data_type, byte_order, length, scale,
              value_offset, description)";

/// The model's row, locked until the transaction ends so concurrent updates and deletes of the
/// model queue up. `$1`: the model ID.
const LOCK_MODEL: &str = "SELECT name, revision FROM assets.device_models
WHERE model_id = $1::uuid FOR UPDATE";

/// Every point of a model. `$1`: the model ID.
const DELETE_POINTS: &str = "DELETE FROM assets.device_model_points WHERE model_id = $1::uuid";

/// The next revision of a model. `$1`: the model ID.
const BUMP_REVISION: &str = "UPDATE assets.device_models
SET revision = revision + 1, updated_at = now()
WHERE model_id = $1::uuid
RETURNING revision, created_at, updated_at";

/// A model and, by the foreign key's cascade, its points. A device that uses the model makes it
/// fail with SQLSTATE 23503. `$1`: the model ID.
const DELETE_MODEL: &str = "DELETE FROM assets.device_models WHERE model_id = $1::uuid
RETURNING name, revision";

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// The constraint that keeps names unique per tenant.
const NAME_UNIQUE: &str = "device_models_name_unique";

/// A stored model.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredModel {
    /// The model.
    pub model_id: DeviceModelId,
    /// Its name, never changed.
    pub name: ModelName,
    /// 1 at creation, one more with every update.
    pub revision: u64,
    /// When it was created, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    pub updated_at: DateTime<Utc>,
    /// Its points, in order.
    pub points: Vec<Point>,
}

/// A model in the tenant's list.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// The model.
    pub model_id: DeviceModelId,
    /// Its name.
    pub name: ModelName,
    /// Its revision.
    pub revision: u64,
    /// How many points it has.
    pub point_count: u64,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    pub updated_at: DateTime<Utc>,
}

/// Why a request was not carried out. Every error leaves the database as it was.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The caller has not picked or joined a tenant.
    #[error("the request's identity has no tenant")]
    NoTenant,
    /// No model of the caller's tenant has the ID.
    #[error("no such model")]
    NotFound,
    /// The tenant has a model of that name.
    #[error("the name is taken")]
    NameTaken,
    /// An update named the model differently.
    #[error("the name of a model never changes")]
    NameImmutable,
    /// An update expected another revision than the current one.
    #[error("the model is at revision {current}")]
    RevisionMismatch {
        /// The current revision.
        current: u64,
    },
    /// A device uses the model.
    #[error("a device uses the model")]
    InUse,
    /// The service has not processed the tenant's `identity.tenant_created` yet.
    #[error("the tenant is not known to the assets service yet")]
    TenantNotReady,
    /// A stored row breaks the rules the tables enforce, which cannot happen; nothing is
    /// returned from it.
    #[error("a stored device model breaks the rules")]
    Corrupt,
    /// The database failed or refused the transaction.
    #[error(transparent)]
    Database(TenantDbError),
    /// The event could not be written.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; a model ID always is, so this does not happen.
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

/// The SQLSTATE of a database error and the constraint it names, if any.
fn violation(error: &sqlx::Error) -> Option<(String, Option<String>)> {
    let database = error.as_database_error()?;
    let code = database.code()?.into_owned();
    Some((code, database.constraint().map(str::to_owned)))
}

/// The caller's tenant's models, by name.
///
/// # Errors
///
/// [`StoreError::NoTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn list(db: &TenantDb, user: &UserIdentity) -> Result<Vec<Summary>, StoreError> {
    let mut tx = db.begin(user).await?;
    let rows = sqlx::query(LIST_MODELS).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    rows.iter()
        .map(|row| -> Result<Summary, StoreError> {
            Ok(Summary {
                model_id: row_model_id(row)?,
                name: model_name(row)?,
                revision: revision(row)?,
                point_count: u64::try_from(row.try_get::<i64, _>("point_count")?)
                    .map_err(|_| StoreError::Corrupt)?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect()
}

/// One model of the caller's tenant.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the tenant has no model `model_id`; [`StoreError::NoTenant`],
/// [`StoreError::Corrupt`] or a database failure.
pub async fn get(
    db: &TenantDb,
    user: &UserIdentity,
    model_id: DeviceModelId,
) -> Result<StoredModel, StoreError> {
    let mut tx = db.begin(user).await?;
    let rows = sqlx::query(SELECT_MODEL)
        .bind(model_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let Some(first) = rows.first() else {
        return Err(StoreError::NotFound);
    };
    let mut points = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(point) = stored_point(row)? {
            points.push(point);
        }
    }
    let stored = StoredModel {
        model_id: row_model_id(first)?,
        name: model_name(first)?,
        revision: revision(first)?,
        created_at: first.try_get("created_at")?,
        updated_at: first.try_get("updated_at")?,
        points,
    };
    // Row-level security returned the caller's tenant's model only; a row that is not the one
    // asked for is not trusted.
    if stored.model_id != model_id {
        return Err(StoreError::Corrupt);
    }
    Ok(stored)
}

/// Creates a model in the caller's tenant and writes `assets.device_model_created`.
///
/// # Errors
///
/// [`StoreError::NameTaken`], [`StoreError::TenantNotReady`], [`StoreError::NoTenant`] or a
/// failure; nothing is written then.
pub async fn create(
    db: &TenantDb,
    user: &UserIdentity,
    model: NewModel,
) -> Result<StoredModel, StoreError> {
    let mut tx = db.begin(user).await?;
    let tenant = tx.tenant_id();
    let model_id = DeviceModelId::generate();
    let row = sqlx::query(INSERT_MODEL)
        .bind(tenant.to_string())
        .bind(model_id.to_string())
        .bind(model.name.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| match violation(&error) {
            Some((code, Some(constraint)))
                if code == UNIQUE_VIOLATION && constraint == NAME_UNIQUE =>
            {
                StoreError::NameTaken
            }
            Some((code, _)) if code == FOREIGN_KEY_VIOLATION => StoreError::TenantNotReady,
            _ => StoreError::from(error),
        })?;
    insert_points(&mut tx, tenant, model_id, &model.points).await?;
    let stored = StoredModel {
        model_id,
        name: model.name,
        revision: revision(&row)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        points: model.points,
    };
    publish(
        &mut tx,
        model_id,
        &DeviceModelCreated {
            model_id,
            revision: stored.revision,
            model: gateway_model(&stored),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(stored)
}

/// Replaces the points of a model of the caller's tenant and writes
/// `assets.device_model_updated`. The model's row stays locked from the read to the commit, so
/// a concurrent update or delete waits and then sees this one.
///
/// # Errors
///
/// [`StoreError::NotFound`], [`StoreError::NameImmutable`] when `replacement` names the model
/// differently, [`StoreError::RevisionMismatch`] when it expects another revision,
/// [`StoreError::NoTenant`] or a failure; nothing is written then.
pub async fn replace(
    db: &TenantDb,
    user: &UserIdentity,
    model_id: DeviceModelId,
    replacement: Replacement,
) -> Result<StoredModel, StoreError> {
    let mut tx = db.begin(user).await?;
    let tenant = tx.tenant_id();
    let Some(row) = sqlx::query(LOCK_MODEL)
        .bind(model_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(StoreError::NotFound);
    };
    let name = model_name(&row)?;
    let current = revision(&row)?;
    if replacement
        .name
        .as_deref()
        .is_some_and(|requested| requested != name.as_str())
    {
        return Err(StoreError::NameImmutable);
    }
    if replacement
        .revision
        .is_some_and(|expected| expected != current)
    {
        return Err(StoreError::RevisionMismatch { current });
    }
    sqlx::query(DELETE_POINTS)
        .bind(model_id.to_string())
        .execute(&mut *tx)
        .await?;
    insert_points(&mut tx, tenant, model_id, &replacement.points).await?;
    let row = sqlx::query(BUMP_REVISION)
        .bind(model_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    let stored = StoredModel {
        model_id,
        name,
        revision: revision(&row)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        points: replacement.points,
    };
    publish(
        &mut tx,
        model_id,
        &DeviceModelUpdated {
            model_id,
            revision: stored.revision,
            model: gateway_model(&stored),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(stored)
}

/// Deletes a model of the caller's tenant with its points and writes
/// `assets.device_model_deleted`; returns the deleted model's name and last revision.
///
/// # Errors
///
/// [`StoreError::NotFound`], [`StoreError::InUse`] when a device uses the model,
/// [`StoreError::NoTenant`] or a failure; nothing is deleted then.
pub async fn delete(
    db: &TenantDb,
    user: &UserIdentity,
    model_id: DeviceModelId,
) -> Result<(ModelName, u64), StoreError> {
    let mut tx = db.begin(user).await?;
    let row = sqlx::query(DELETE_MODEL)
        .bind(model_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| match violation(&error) {
            Some((code, _)) if code == FOREIGN_KEY_VIOLATION => StoreError::InUse,
            _ => StoreError::from(error),
        })?;
    let Some(row) = row else {
        return Err(StoreError::NotFound);
    };
    let name = model_name(&row)?;
    let revision = revision(&row)?;
    publish(
        &mut tx,
        model_id,
        &DeviceModelDeleted {
            model_id,
            name: name.clone(),
            revision,
        },
    )
    .await?;
    tx.commit().await?;
    Ok((name, revision))
}

/// The model as the gateway's configuration and the events carry it: without descriptions.
#[must_use]
pub fn gateway_model(model: &StoredModel) -> DeviceModel {
    DeviceModel {
        name: model.name.clone(),
        points: model
            .points
            .iter()
            .map(|point| point.point.clone())
            .collect(),
    }
}

async fn publish<E: Event>(
    tx: &mut TenantTransaction,
    model_id: DeviceModelId,
    event: &E,
) -> Result<(), StoreError> {
    let aggregate = Aggregate::new(DEVICE_MODEL_AGGREGATE, model_id.to_string())?;
    let tenant = tx.tenant_id();
    outbox::publish(tx.transaction(), &aggregate, event, Some(tenant)).await?;
    Ok(())
}

async fn insert_points(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    model_id: DeviceModelId,
    points: &[Point],
) -> Result<(), StoreError> {
    if points.is_empty() {
        return Ok(());
    }
    let mut positions = Vec::with_capacity(points.len());
    let mut keys = Vec::with_capacity(points.len());
    let mut function_codes = Vec::with_capacity(points.len());
    let mut addresses = Vec::with_capacity(points.len());
    let mut data_types = Vec::with_capacity(points.len());
    let mut byte_orders = Vec::with_capacity(points.len());
    let mut lengths = Vec::with_capacity(points.len());
    let mut scales = Vec::with_capacity(points.len());
    let mut offsets = Vec::with_capacity(points.len());
    let mut descriptions = Vec::with_capacity(points.len());
    for (index, Point { point, description }) in points.iter().enumerate() {
        positions.push(i32::try_from(index).map_err(|_| StoreError::Corrupt)?);
        keys.push(point.key.as_str().to_owned());
        function_codes.push(i16::from(point.function_code.number()));
        addresses.push(i32::from(point.address));
        data_types.push(wire_name(point.data_type));
        byte_orders.push(wire_name(point.byte_order));
        lengths.push(point.length.map(i32::from));
        scales.push(point.scale);
        offsets.push(point.offset);
        descriptions.push(description.clone());
    }
    sqlx::query(INSERT_POINTS)
        .bind(tenant.to_string())
        .bind(model_id.to_string())
        .bind(positions)
        .bind(keys)
        .bind(function_codes)
        .bind(addresses)
        .bind(data_types)
        .bind(byte_orders)
        .bind(lengths)
        .bind(scales)
        .bind(offsets)
        .bind(descriptions)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn row_model_id(row: &PgRow) -> Result<DeviceModelId, StoreError> {
    row.try_get::<String, _>("model_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt)
}

fn model_name(row: &PgRow) -> Result<ModelName, StoreError> {
    ModelName::new(row.try_get::<String, _>("name")?).map_err(|_| StoreError::Corrupt)
}

fn revision(row: &PgRow) -> Result<u64, StoreError> {
    u64::try_from(row.try_get::<i64, _>("revision")?).map_err(|_| StoreError::Corrupt)
}

/// The point of one row of [`SELECT_MODEL`]; `None` for the row of a model without points.
fn stored_point(row: &PgRow) -> Result<Option<Point>, StoreError> {
    let Some(key) = row.try_get::<Option<String>, _>("key")? else {
        return Ok(None);
    };
    let function_code: i16 = row.try_get("function_code")?;
    let address: i32 = row.try_get("address")?;
    let data_type: String = row.try_get("data_type")?;
    let byte_order: String = row.try_get("byte_order")?;
    let length: Option<i32> = row.try_get("length")?;
    let point = DataPoint {
        key: PointKey::new(key).map_err(|_| StoreError::Corrupt)?,
        function_code: u8::try_from(function_code)
            .ok()
            .and_then(|number| FunctionCode::try_from(number).ok())
            .ok_or(StoreError::Corrupt)?,
        address: u16::try_from(address).map_err(|_| StoreError::Corrupt)?,
        data_type: from_wire_name::<DataType>(&data_type).ok_or(StoreError::Corrupt)?,
        byte_order: from_wire_name::<ByteOrder>(&byte_order).ok_or(StoreError::Corrupt)?,
        length: length
            .map(u16::try_from)
            .transpose()
            .map_err(|_| StoreError::Corrupt)?,
        scale: row.try_get("scale")?,
        offset: row.try_get("value_offset")?,
    };
    Ok(Some(Point {
        point,
        description: row.try_get("description")?,
    }))
}
