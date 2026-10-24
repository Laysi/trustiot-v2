//! Which tenant each gateway is bound to, and each tenant's status, as this service knows them
//! (Laysi/trustiot-v2#632; `README.md`, "閘道器的綁定與租戶狀態").
//!
//! A gateway certificate carries the hardware ID only. A gateway's tenant is its current binding
//! in the assets service, and this service keeps its own copy from `assets.gateway_bound`, so the
//! renewal (Laysi/trustiot-v2#81) and the broker's permissions (Laysi/trustiot-v2#633) never call
//! another service on their request path.
//!
//! - [`GatewayBoundHandler`] stores every binding in `certs.gateway_bindings`. The consumer runs
//!   it in the event's tenant transaction as the service role, so row-level security admits only
//!   that tenant's row; the insert does nothing when the binding is there already, so a replay or
//!   the same event delivered again never adds or changes a row. An event that fails the same way
//!   on every attempt (an invalid hardware ID, no tenant, values the table refuses) is
//!   dead-lettered, so it cannot hold up the bindings after it; anything else is retried.
//! - [`store_tenant_status`] stores a tenant's status only when its version is newer than the
//!   stored one (rebuild checklist B1). Nothing calls it yet but the tests:
//!   Laysi/trustiot-v2#107 and Laysi/trustiot-v2#108 connect the tenant's real status.
//! - [`Projection`] reads both across tenants as the platform role, read only, with an audit log
//!   line for every read: the renewal has no tenant identity, and the broker sync compares every
//!   gateway.

use chrono::{DateTime, Utc};
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{GatewayBindingId, ServiceName, TenantId};
use outbox::{Delivery, Handler, HandlerError};
use protocol::GatewayId;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformTransaction, TenantDbError};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, Row, Transaction};

/// The consumer's subscription to the assets service's events; never renamed (a new name starts
/// reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "certs.assets-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "assets";

/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// A binding; nothing when it is there already. `$1` tenant, `$2` binding ID, `$3` hardware ID,
/// `$4` when it was bound, `$5` and `$6` the event's position in the assets service's log. No
/// conflict target: the service role has no SELECT on the table, and the primary key is its only
/// unique constraint.
const INSERT_BINDING: &str = "INSERT INTO certs.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
ON CONFLICT DO NOTHING";

/// A tenant's status, replacing the stored one only when `$3` is a newer version; returns a row
/// when it was stored. `$1` tenant, `$2` status. Row-level security limits it to the
/// transaction's tenant.
const STORE_STATUS: &str =
    "INSERT INTO certs.tenant_statuses AS stored (tenant_id, status, version)
VALUES ($1::uuid, $2, $3)
ON CONFLICT (tenant_id) DO UPDATE
    SET status = EXCLUDED.status, version = EXCLUDED.version, updated_at = now()
    WHERE stored.version < EXCLUDED.version
RETURNING 1";

/// The binding of `$1` (a hardware ID) whose event is the latest in the log, and whether it
/// still holds.
const LATEST_BINDING: &str = "SELECT hardware_id, tenant_id::text AS tenant_id,
    binding_id::text AS binding_id, bound_at, unbound_at IS NULL AS current
FROM certs.gateway_bindings
WHERE hardware_id = $1
ORDER BY position_tx DESC, position_id DESC
LIMIT 1";

/// Every gateway whose latest binding still holds, by hardware ID (byte order).
const BOUND_GATEWAYS: &str = "SELECT hardware_id, tenant_id::text AS tenant_id,
    binding_id::text AS binding_id, bound_at, true AS current
FROM (
    SELECT DISTINCT ON (hardware_id) hardware_id, tenant_id, binding_id, bound_at, unbound_at
    FROM certs.gateway_bindings
    ORDER BY hardware_id, position_tx DESC, position_id DESC
) AS latest
WHERE unbound_at IS NULL
ORDER BY hardware_id COLLATE \"C\"";

/// The stored status of `$1` (a tenant ID as text).
const TENANT_STATUS: &str = "SELECT status FROM certs.tenant_statuses WHERE tenant_id = $1::uuid";

/// Makes the platform role's read transaction read only, as `PlatformDb::reader` does: Postgres
/// accepts it at any point of a transaction.
const READ_ONLY: &str = "SET TRANSACTION READ ONLY";

/// Stores every `assets.gateway_bound` in `certs.gateway_bindings`; ignores the assets service's
/// other events.
#[derive(Clone, Copy, Debug, Default)]
pub struct GatewayBoundHandler;

impl Handler for GatewayBoundHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if !delivery.envelope.is::<GatewayBound>() {
            return Ok(());
        }
        // An invalid hardware ID or binding ID fails here (`GatewayId` checks the rule when it is
        // read), which the consumer dead-letters.
        let bound: GatewayBound = delivery.envelope.decode()?;
        // A binding belongs to the tenant the gateway is bound to; the assets service writes it in
        // that tenant's transaction. One without a tenant is not what it writes.
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "assets.gateway_bound without a tenant",
            ));
        };
        if delivery.aggregate.kind() != GATEWAY_BINDING_AGGREGATE
            || delivery.aggregate.id() != bound.binding_id.to_string()
        {
            return Err(HandlerError::permanent(
                "assets.gateway_bound outside its binding's aggregate",
            ));
        }
        let inserted = sqlx::query(INSERT_BINDING)
            .bind(tenant.to_string())
            .bind(bound.binding_id.to_string())
            .bind(bound.hardware_id.as_str())
            .bind(delivery.envelope.occurred_at)
            .bind(delivery.position.tx())
            .bind(delivery.position.id())
            .execute(&mut **tx)
            .await
            .map_err(insert_error)?;
        tracing::info!(
            hardware_id = bound.hardware_id.as_str(),
            binding_id = %bound.binding_id,
            tenant_id = %tenant,
            event_id = %delivery.envelope.id,
            created = inserted.rows_affected() == 1,
            "applied assets.gateway_bound"
        );
        Ok(())
    }
}

/// The error for a failed insert. A value the table refuses (SQLSTATE class 22, data exception,
/// or 23514, a CHECK) fails the same way on every attempt, so the event is dead-lettered, where it
/// shows, and the subscription moves on; every other database error is retried (`outbox`'s
/// rule).
fn insert_error(error: sqlx::Error) -> HandlerError {
    let refused = error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| is_refused_value(&code));
    if refused {
        HandlerError::permanent("the gateway bindings refuse the event's values").with_source(error)
    } else {
        HandlerError::from(error)
    }
}

/// Whether `sqlstate` means the row's values are refused, whatever the attempt.
fn is_refused_value(sqlstate: &str) -> bool {
    sqlstate.starts_with("22") || sqlstate == CHECK_VIOLATION
}

/// A tenant's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TenantStatus {
    /// The tenant's gateways may renew their certificates and use the broker.
    Active,
    /// The tenant is suspended (Laysi/trustiot-v2#107): its gateways may not.
    Suspended,
}

impl TenantStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
}

/// Stores `tenant`'s status read at `version`, unless the stored version is the same or newer;
/// returns whether it was stored. A late or replayed older state therefore never overwrites a
/// newer one, and the row stays locked until the transaction ends, so a concurrent store waits and
/// then compares against this one.
///
/// `tx` is a transaction in `tenant` (an event's delivery transaction, or a
/// [`service_kit::tenant::BackgroundTenantDb`] transaction): row-level security refuses any other
/// tenant with SQLSTATE 42501.
///
/// # Errors
///
/// When the query fails, including a negative `version` (SQLSTATE 23514).
pub async fn store_tenant_status(
    tx: &mut PgConnection,
    tenant: TenantId,
    status: TenantStatus,
    version: i64,
) -> Result<bool, sqlx::Error> {
    let stored: Option<i32> = sqlx::query_scalar(STORE_STATUS)
        .bind(tenant.to_string())
        .bind(status.as_str())
        .bind(version)
        .fetch_optional(tx)
        .await?;
    Ok(stored.is_some())
}

/// A gateway's binding to a tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The gateway's hardware ID.
    pub hardware_id: GatewayId,
    /// The tenant it is bound to.
    pub tenant_id: TenantId,
    /// The binding, as the assets service named it.
    pub binding_id: GatewayBindingId,
    /// When the assets service bound it.
    pub bound_at: DateTime<Utc>,
}

/// Why the projection could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A stored row breaks the rules the tables enforce, which cannot happen; nothing is returned
    /// from it.
    #[error("a stored binding or tenant status breaks the rules")]
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

/// The platform role's reads of the bindings and the tenant statuses, across tenants. Every read
/// is its own read-only transaction of [`PlatformDb::begin`], which logs an INFO line
/// (`platform database access`) with the certs service as the actor and the caller's purpose, so
/// every read is in the audit log. Cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct Projection {
    db: PlatformDb,
}

impl Projection {
    /// Reads through `db`, the platform role's pool.
    #[must_use]
    pub fn new(db: PlatformDb) -> Self {
        Self { db }
    }

    /// The tenant `hardware_id` is bound to now, with the binding: `None` when it was never bound
    /// or its latest binding has ended. `purpose` names the caller in the audit log, for example
    /// `"renew-gateway-certificate"`.
    ///
    /// # Errors
    ///
    /// A database failure, or [`ReadError::Corrupt`].
    pub async fn gateway_tenant(
        &self,
        hardware_id: &GatewayId,
        purpose: &'static str,
    ) -> Result<Option<Binding>, ReadError> {
        let mut tx = self.begin(purpose).await?;
        let row = sqlx::query(LATEST_BINDING)
            .bind(hardware_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        let binding = row.as_ref().map(binding).transpose()?.flatten();
        if binding
            .as_ref()
            .is_some_and(|binding| binding.hardware_id != *hardware_id)
        {
            return Err(ReadError::Corrupt);
        }
        Ok(binding)
    }

    /// Every gateway bound to a tenant now, by hardware ID (byte order), across all tenants.
    /// `purpose` names the caller in the audit log, for example `"sync-broker-permissions"`.
    ///
    /// # Errors
    ///
    /// A database failure, or [`ReadError::Corrupt`].
    pub async fn bound_gateways(&self, purpose: &'static str) -> Result<Vec<Binding>, ReadError> {
        let mut tx = self.begin(purpose).await?;
        let rows = sqlx::query(BOUND_GATEWAYS).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        rows.iter()
            .filter_map(|row| binding(row).transpose())
            .collect()
    }

    /// Whether `tenant` is suspended. A tenant this service has no status for is active.
    /// `purpose` names the caller in the audit log.
    ///
    /// # Errors
    ///
    /// A database failure, or [`ReadError::Corrupt`].
    pub async fn tenant_suspended(
        &self,
        tenant: TenantId,
        purpose: &'static str,
    ) -> Result<bool, ReadError> {
        let mut tx = self.begin(purpose).await?;
        let status: Option<String> = sqlx::query_scalar(TENANT_STATUS)
            .bind(tenant.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        match status.as_deref() {
            None | Some("active") => Ok(false),
            Some("suspended") => Ok(true),
            Some(_) => Err(ReadError::Corrupt),
        }
    }

    /// A read-only transaction of the platform role for the certs service, logged with `purpose`.
    async fn begin(&self, purpose: &'static str) -> Result<PlatformTransaction, ReadError> {
        let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Certs));
        let mut tx = self.db.begin(actor, purpose).await?;
        sqlx::query(READ_ONLY).execute(&mut *tx).await?;
        Ok(tx)
    }
}

/// The binding of a row of [`LATEST_BINDING`] or [`BOUND_GATEWAYS`]; `None` when it has ended.
fn binding(row: &PgRow) -> Result<Option<Binding>, ReadError> {
    if !row.try_get::<bool, _>("current")? {
        return Ok(None);
    }
    let hardware_id =
        GatewayId::new(row.try_get::<String, _>("hardware_id")?).map_err(|_| ReadError::Corrupt)?;
    let tenant_id = row
        .try_get::<String, _>("tenant_id")?
        .parse()
        .map_err(|_| ReadError::Corrupt)?;
    let binding_id = row
        .try_get::<String, _>("binding_id")?
        .parse()
        .map_err(|_| ReadError::Corrupt)?;
    Ok(Some(Binding {
        hardware_id,
        tenant_id,
        binding_id,
        bound_at: row.try_get("bound_at")?,
    }))
}

#[cfg(test)]
mod tests {
    use outbox::FailureKind;

    use super::*;

    #[test]
    fn values_the_table_refuses_are_permanent_and_other_failures_are_retried() {
        for refused in ["22001", "22021", "22P02", "23514"] {
            assert!(is_refused_value(refused), "{refused}");
        }
        for retried in [
            "23505", "23503", "40001", "40P01", "57P01", "08006", "42501",
        ] {
            assert!(!is_refused_value(retried), "{retried}");
        }
        assert_eq!(
            insert_error(sqlx::Error::PoolTimedOut).kind(),
            FailureKind::Retry
        );
    }

    #[test]
    fn statuses_are_stored_as_the_table_names_them() {
        assert_eq!(TenantStatus::Active.as_str(), "active");
        assert_eq!(TenantStatus::Suspended.as_str(), "suspended");
    }
}
