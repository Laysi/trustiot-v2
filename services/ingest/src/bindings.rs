//! The gateway binding projection (services/ingest/README.md, "閘道器綁定對照表"; decisions 8
//! and 12): which tenant each gateway is bound to, the only source of a telemetry batch's tenant.
//! Nothing in an MQTT message names the tenant for ingest, whatever it carries.
//!
//! - [`GatewayBoundHandler`] records every `assets.gateway_bound` in `ingest.gateway_bindings`,
//!   in a transaction of the service role in the event's tenant (the outbox consumer's), so
//!   row-level security keeps each binding in the tenant the assets service wrote it in. A
//!   replayed or repeated event finds its row and changes nothing; an event that can never be
//!   recorded is dead-lettered instead of holding up the bindings after it.
//! - [`GatewayBindings::gateway_tenant`] answers the telemetry path's question, which tenant
//!   holds this hardware ID now. The question is across tenants (the topic names a gateway, not a
//!   tenant), so it is the platform role's, in a READ ONLY transaction of
//!   [`PlatformReader`], through the table's `platform_access` policy, which opens SELECT only.
//!   The service role never reads another tenant's binding.
//!
//! Why the platform role and not a table without tenant data (decision 29; the issue left both
//! open): a binding is the tenant's data, written by the tenant's event; a table without
//! row-level security would let every tenant transaction of the service role read every
//! gateway's tenant, and a projection keyed by hardware ID alone could not be written in the
//! event's tenant when the gateway moves to another tenant (Laysi/trustiot-v2#115). The assets
//! service keeps its own bindings the same way (`assets.gateway_bindings`, read across tenants by
//! `assets_platform` only).

use contract::events::assets::GatewayBound;
use contract::{GatewayBindingId, ServiceName, TenantId};
use outbox::{Delivery, Handler, HandlerError};
use protocol::GatewayId;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformReader, TenantDbError};
use sqlx::{Postgres, Row, Transaction};

/// The consumer's subscription to the assets service's events; never renamed (a new name starts
/// reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "ingest.assets-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "assets";

/// The purpose the platform role's lookups name in their log lines.
pub const LOOKUP_PURPOSE: &str = "gateway-tenant";

/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// A binding of the transaction's tenant; nothing when the tenant has it already. `$1` tenant,
/// `$2` binding, `$3` hardware ID, `$4` when it was bound (the event's `occurred_at`).
const INSERT_BINDING: &str =
    "INSERT INTO ingest.gateway_bindings (tenant_id, binding_id, hardware_id, bound_at)
VALUES ($1::uuid, $2::uuid, $3, $4)
ON CONFLICT (tenant_id, binding_id) DO NOTHING";

/// The gateway of a binding the transaction's tenant has. `$1` tenant, `$2` binding.
const RECORDED_GATEWAY: &str = "SELECT hardware_id FROM ingest.gateway_bindings
WHERE tenant_id = $1::uuid AND binding_id = $2::uuid";

/// A gateway's current binding across tenants: its newest binding by `bound_at` (the binding ID,
/// a UUIDv7, breaks a tie), and only while that binding has not ended. An older binding never
/// answers once a newer one is recorded, ended or not. `$1`: the hardware ID.
const CURRENT_BINDING: &str = "SELECT newest.tenant_id::text AS tenant_id,
    newest.binding_id::text AS binding_id
FROM (
    SELECT tenant_id, binding_id, unbound_at
    FROM ingest.gateway_bindings
    WHERE hardware_id = $1
    ORDER BY bound_at DESC, binding_id DESC
    LIMIT 1
) AS newest
WHERE newest.unbound_at IS NULL";

/// Records every `assets.gateway_bound`; ignores the assets service's other events.
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
        let bound: GatewayBound = delivery.envelope.decode()?;
        // A binding is always the tenant's event (crates/contract/src/events/assets.rs). One
        // without a tenant is not what the assets service writes, and its transaction could not
        // write a tenant's row anyway.
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "assets.gateway_bound without a tenant",
            ));
        };
        let (tenant_text, binding_text) = (tenant.to_string(), bound.binding_id.to_string());
        let inserted = sqlx::query(INSERT_BINDING)
            .bind(&tenant_text)
            .bind(&binding_text)
            .bind(bound.hardware_id.as_str())
            .bind(delivery.envelope.occurred_at)
            .execute(&mut **tx)
            .await
            .map_err(insert_error)?;
        let created = inserted.rows_affected() == 1;
        if !created {
            // The tenant has this binding already: a replay, or the event delivered again. The
            // same binding ID with another gateway is not a replay of this event, and no retry
            // can make the two agree, so it is set aside where it shows.
            let recorded: String = sqlx::query_scalar(RECORDED_GATEWAY)
                .bind(&tenant_text)
                .bind(&binding_text)
                .fetch_one(&mut **tx)
                .await?;
            if recorded != bound.hardware_id.as_str() {
                return Err(HandlerError::permanent(
                    "assets.gateway_bound names a recorded binding with another gateway",
                ));
            }
        }
        tracing::info!(
            hardware_id = bound.hardware_id.as_str(),
            binding_id = %bound.binding_id,
            tenant_id = %tenant,
            event_id = %delivery.envelope.id,
            created,
            "applied assets.gateway_bound"
        );
        Ok(())
    }
}

/// The table's error for a failed insert. A value the table refuses (SQLSTATE class 22, data
/// exception, or 23514, a CHECK) fails the same way on every attempt, so the event is
/// dead-lettered and the subscription moves on; every other database error is retried
/// (`outbox`'s rule), since most pass (a lost connection, a failover).
fn insert_error(error: sqlx::Error) -> HandlerError {
    let refused = error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| is_refused_value(&code));
    if refused {
        HandlerError::permanent("the binding table refuses the event's values").with_source(error)
    } else {
        HandlerError::from(error)
    }
}

/// Whether `sqlstate` means the row's values are refused, whatever the attempt.
fn is_refused_value(sqlstate: &str) -> bool {
    sqlstate.starts_with("22") || sqlstate == CHECK_VIOLATION
}

/// A gateway's current binding: the tenant its telemetry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayTenant {
    /// The tenant the gateway is bound to.
    pub tenant_id: TenantId,
    /// The binding, which the telemetry path records with each batch for tracing
    /// (Laysi/trustiot-v2#639).
    pub binding_id: GatewayBindingId,
}

/// Why a gateway's tenant could not be looked up. The telemetry path answers `retry` then, as
/// for an unknown gateway (decision 8).
#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    /// A stored row breaks the rules the table enforces, which cannot happen.
    #[error("a stored binding breaks the rules")]
    Corrupt,
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
}

impl From<sqlx::Error> for LookupError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

/// The lookup of a gateway's tenant; see the module. Cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct GatewayBindings {
    reader: PlatformReader,
}

impl GatewayBindings {
    /// Lookups on the platform role's pool `platform`, as the ingest service on its own behalf.
    /// Logged once at INFO here (`platform database reader`, purpose [`LOOKUP_PURPOSE`]); each
    /// lookup at DEBUG, since the telemetry path looks up every batch.
    #[must_use]
    pub fn new(platform: &PlatformDb) -> Self {
        let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Ingest));
        Self {
            reader: platform.reader(actor, LOOKUP_PURPOSE),
        }
    }

    /// The tenant `hardware_id` is bound to now, and the binding; `None` when it is bound to no
    /// tenant, or this service has not recorded its binding yet. The answer comes from this
    /// service's own table only, never from the caller or the message.
    ///
    /// # Errors
    ///
    /// [`LookupError`] when the database fails or a stored row is unreadable.
    pub async fn gateway_tenant(
        &self,
        hardware_id: &GatewayId,
    ) -> Result<Option<GatewayTenant>, LookupError> {
        let mut tx = self.reader.begin().await?;
        let row = sqlx::query(CURRENT_BINDING)
            .bind(hardware_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.map(|row| {
            let tenant: String = row.try_get("tenant_id")?;
            let binding: String = row.try_get("binding_id")?;
            Ok(GatewayTenant {
                tenant_id: tenant.parse().map_err(|_| LookupError::Corrupt)?,
                binding_id: binding.parse().map_err(|_| LookupError::Corrupt)?,
            })
        })
        .transpose()
    }
}
