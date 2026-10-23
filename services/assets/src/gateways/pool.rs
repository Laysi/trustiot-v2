//! The device pool (services/assets/README.md, "閘道器設備池與綁定"): every gateway the certs
//! service registers at the factory, and the platform administrators' view of it.
//!
//! - [`GatewayRegisteredHandler`] puts a gateway in `assets.gateway_pool` when
//!   `certs.gateway_registered` arrives. The event is platform-wide, so the consumer runs it in
//!   a transaction of the service role without a tenant; the insert does nothing when the row
//!   exists, so a replay or the same event delivered again never adds a second one.
//! - [`list`] and [`get`] read the pool with the current binding of each gateway, as the
//!   platform role for a platform administrator (`PlatformDb`, which logs the actor and the
//!   purpose of every transaction).

use chrono::{DateTime, Utc};
use contract::events::certs::GatewayRegistered;
use contract::{GatewayBindingId, TenantId};
use outbox::{Delivery, Handler, HandlerError};
use protocol::GatewayId;
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDbError};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

/// The consumer's subscription to the certs service's events; never renamed (a new name starts
/// reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "assets.certs-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "certs";

/// The purpose the platform role's transactions name in their log line.
const LIST_PURPOSE: &str = "list-gateway-pool";

/// A gateway of the pool; nothing when it is there already. `$1` hardware ID, `$2` when it was
/// registered, `$3` the workstation. No conflict target: the service role has no SELECT on the
/// table, and the primary key is its only unique constraint.
const INSERT_GATEWAY: &str =
    "INSERT INTO assets.gateway_pool (hardware_id, registered_at, workstation)
VALUES ($1, $2, $3)
ON CONFLICT DO NOTHING";

/// The pool with each gateway's current binding, by hardware ID (byte order), after `$1` (NULL:
/// from the start), `$2` NULL for every gateway, `true` for the bound ones only, `false` for the
/// unbound ones only, at most `$3` rows.
const LIST_POOL: &str = "SELECT pool.hardware_id, pool.registered_at, pool.workstation,
    pool.pooled_at, binding.binding_id::text AS binding_id, binding.tenant_id::text AS tenant_id,
    binding.bound_at
FROM assets.gateway_pool AS pool
LEFT JOIN assets.gateway_bindings AS binding
    ON binding.hardware_id = pool.hardware_id AND binding.unbound_at IS NULL
WHERE ($1::text IS NULL OR pool.hardware_id COLLATE \"C\" > $1::text COLLATE \"C\")
    AND ($2::boolean IS NULL OR (binding.binding_id IS NOT NULL) = $2::boolean)
ORDER BY pool.hardware_id COLLATE \"C\"
LIMIT $3";

/// One gateway of the pool with its current binding. `$1`: the hardware ID.
const SELECT_GATEWAY: &str = "SELECT pool.hardware_id, pool.registered_at, pool.workstation,
    pool.pooled_at, binding.binding_id::text AS binding_id, binding.tenant_id::text AS tenant_id,
    binding.bound_at
FROM assets.gateway_pool AS pool
LEFT JOIN assets.gateway_bindings AS binding
    ON binding.hardware_id = pool.hardware_id AND binding.unbound_at IS NULL
WHERE pool.hardware_id = $1";

/// Puts every gateway of `certs.gateway_registered` in the pool; ignores the certs service's
/// other events.
#[derive(Clone, Copy, Debug, Default)]
pub struct GatewayRegisteredHandler;

impl Handler for GatewayRegisteredHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if !delivery.envelope.is::<GatewayRegistered>() {
            return Ok(());
        }
        let registered: GatewayRegistered = delivery.envelope.decode()?;
        // A factory registration belongs to no tenant. One with a tenant is not what the certs
        // service writes, and a tenant's transaction could not write the pool's row anyway.
        if delivery.tenant().is_some() {
            return Err(HandlerError::permanent(
                "certs.gateway_registered with a tenant",
            ));
        }
        let inserted = sqlx::query(INSERT_GATEWAY)
            .bind(registered.hardware_id.as_str())
            .bind(delivery.envelope.occurred_at)
            .bind(&registered.workstation)
            .execute(&mut **tx)
            .await?;
        tracing::info!(
            hardware_id = registered.hardware_id.as_str(),
            event_id = %delivery.envelope.id,
            created = inserted.rows_affected() == 1,
            "applied certs.gateway_registered"
        );
        Ok(())
    }
}

/// A gateway of the pool as the platform administrators see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolEntry {
    /// The hardware ID.
    pub hardware_id: GatewayId,
    /// When the certs service registered it.
    pub registered_at: DateTime<Utc>,
    /// The workstation that registered it.
    pub workstation: String,
    /// When this service put it in the pool.
    pub pooled_at: DateTime<Utc>,
    /// Its current binding, if it is bound.
    pub binding: Option<CurrentBinding>,
}

/// A gateway's current binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentBinding {
    /// The binding.
    pub binding_id: GatewayBindingId,
    /// The tenant the gateway is bound to.
    pub tenant_id: TenantId,
    /// When it was bound.
    pub bound_at: DateTime<Utc>,
}

/// Which part of the pool a listing shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Every gateway.
    #[default]
    All,
    /// The gateways bound to a tenant.
    Bound,
    /// The gateways bound to no tenant: the ones an operator can bind.
    Unbound,
}

/// Why the pool could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A stored row breaks the rules the table enforces, which cannot happen; nothing is
    /// returned from it.
    #[error("a stored gateway breaks the rules")]
    Corrupt,
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
}

impl From<sqlx::Error> for ReadError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl ReadError {
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
            other => other.to_string(),
        }
    }
}

/// At most `limit` gateways of the pool after `after`, by hardware ID, with their current
/// bindings, read as the platform role for `admin` (only a platform administrator has a
/// [`PlatformActor`]).
///
/// # Errors
///
/// A database failure, or [`ReadError::Corrupt`].
pub async fn list(
    db: &PlatformDb,
    admin: PlatformActor,
    filter: Filter,
    after: Option<&GatewayId>,
    limit: u32,
) -> Result<Vec<PoolEntry>, ReadError> {
    let bound = match filter {
        Filter::All => None,
        Filter::Bound => Some(true),
        Filter::Unbound => Some(false),
    };
    let mut tx = db.begin(admin, LIST_PURPOSE).await?;
    let rows = sqlx::query(LIST_POOL)
        .bind(after.map(GatewayId::as_str))
        .bind(bound)
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    rows.iter().map(entry).collect()
}

/// One gateway of the pool with its current binding, read as the platform role for `admin`;
/// `None` when the pool does not have it.
///
/// # Errors
///
/// A database failure, or [`ReadError::Corrupt`].
pub async fn get(
    db: &PlatformDb,
    admin: PlatformActor,
    hardware_id: &GatewayId,
) -> Result<Option<PoolEntry>, ReadError> {
    let mut tx = db.begin(admin, LIST_PURPOSE).await?;
    let row = sqlx::query(SELECT_GATEWAY)
        .bind(hardware_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let entry = row.as_ref().map(entry).transpose()?;
    if entry
        .as_ref()
        .is_some_and(|entry| entry.hardware_id != *hardware_id)
    {
        return Err(ReadError::Corrupt);
    }
    Ok(entry)
}

fn entry(row: &PgRow) -> Result<PoolEntry, ReadError> {
    let hardware_id =
        GatewayId::new(row.try_get::<String, _>("hardware_id")?).map_err(|_| ReadError::Corrupt)?;
    let binding_id: Option<String> = row.try_get("binding_id")?;
    let tenant_id: Option<String> = row.try_get("tenant_id")?;
    let bound_at: Option<DateTime<Utc>> = row.try_get("bound_at")?;
    let binding = match (binding_id, tenant_id, bound_at) {
        (None, None, None) => None,
        (Some(binding_id), Some(tenant_id), Some(bound_at)) => Some(CurrentBinding {
            binding_id: binding_id.parse().map_err(|_| ReadError::Corrupt)?,
            tenant_id: tenant_id.parse().map_err(|_| ReadError::Corrupt)?,
            bound_at,
        }),
        _ => return Err(ReadError::Corrupt),
    };
    Ok(PoolEntry {
        hardware_id,
        registered_at: row.try_get("registered_at")?,
        workstation: row.try_get("workstation")?,
        pooled_at: row.try_get("pooled_at")?,
        binding,
    })
}
