//! Binding a gateway of the device pool to a tenant (rebuild checklist D3; services/assets/
//! README.md, "閘道器設備池與綁定").
//!
//! [`GatewayBinder`] is the only code in the service that opens a transaction for a tenant that
//! the request's token does not name: a platform administrator binds a gateway to the tenant in
//! the request's body, and the binding and its `assets.gateway_bound` event have to be written
//! in that tenant (the binding is the tenant's data, and the event is the tenant's event). So it
//! holds the service role's background handle privately, and its only method checks the caller
//! itself before it opens the transaction: a platform administrator's own request, never a token
//! another service exchanged (`act`; Laysi/trustiot-v2#104). Keep it that way; the handler checks
//! the same thing first, so a refusal never reads anything. Every transaction it opens is logged
//! at INFO (`tenant transaction for a platform administrator`, with the administrator's user ID,
//! the tenant and the hardware ID), as `PlatformDb` logs its own.
//!
//! One binding is one transaction of the target tenant:
//!
//! 1. the tenant must be known to this service (`assets.tenants`, which row-level security
//!    shows for the transaction's tenant only);
//! 2. a current binding of the gateway in this tenant means the gateway is bound already: the
//!    answer is that binding, and nothing is written (a retry after a lost response);
//! 3. otherwise a new binding is inserted. The gateway must be in the pool (the foreign key, which
//!    Postgres checks with the table owner's rights; the service role cannot read the pool), and
//!    no other tenant may hold it (`gateway_bindings_one_current`, a unique index across tenants:
//!    `ON CONFLICT DO NOTHING` waits for a concurrent binding of the same gateway and then inserts
//!    nothing). When nothing was inserted, the gateway's current binding is read again in a new
//!    snapshot: in this tenant it is a concurrent retry that won (bound already), otherwise
//!    another tenant holds the gateway;
//! 4. `assets.gateway_bound` is written in the same transaction, with the administrator as the
//!    actor, and the transaction commits.
//!
//! Every failure rolls the whole transaction back, so a failed binding leaves nothing behind and
//! a retry starts from the same state: the legacy system's binding stayed stuck at 409 after a
//! failure (Laysi/trustiot#84).

use chrono::{DateTime, Utc};
use contract::event::Actor;
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{GatewayBindingId, TenantId};
use outbox::Aggregate;
use protocol::GatewayId;
use service_kit::identity::UserIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDbError, TenantTransaction};
use sqlx::Row;

/// The purpose the tenant transaction names in its log line.
const BIND_PURPOSE: &str = "bind-gateway";

/// Whether the transaction's tenant is known (row-level security shows only its row).
const TENANT_KNOWN: &str = "SELECT count(*) FROM assets.tenants";

/// The gateway's current binding in the transaction's tenant. `$1`: the hardware ID.
const CURRENT_BINDING: &str = "SELECT binding_id::text AS binding_id, bound_at
FROM assets.gateway_bindings
WHERE hardware_id = $1 AND unbound_at IS NULL";

/// A new binding, unless the gateway has a current one in any tenant. `$1` tenant, `$2` binding,
/// `$3` hardware ID.
const INSERT_BINDING: &str =
    "INSERT INTO assets.gateway_bindings (tenant_id, binding_id, hardware_id)
VALUES ($1::uuid, $2::uuid, $3)
ON CONFLICT DO NOTHING
RETURNING bound_at";

/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// The foreign key from a binding to the pool.
const POOL_FOREIGN_KEY: &str = "gateway_bindings_hardware_id_fkey";
/// The foreign key from a binding to the tenant.
const TENANT_FOREIGN_KEY: &str = "gateway_bindings_tenant_id_fkey";

/// A gateway's binding to a tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The binding.
    pub binding_id: GatewayBindingId,
    /// The gateway.
    pub hardware_id: GatewayId,
    /// The tenant it is bound to.
    pub tenant_id: TenantId,
    /// When it was bound.
    pub bound_at: DateTime<Utc>,
}

/// What a binding request did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The gateway is now bound to the tenant; `assets.gateway_bound` was written.
    Bound(Binding),
    /// The gateway was bound to the tenant already; nothing was written.
    AlreadyBound(Binding),
}

/// Why a gateway was not bound. Every error leaves the database as it was.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The caller is not a platform administrator, or the token was exchanged by another service.
    #[error("only a platform administrator's own request may bind a gateway")]
    NotPlatformAdmin,
    /// The assets service does not know the tenant (no such tenant, or its creation has not
    /// reached the service yet).
    #[error("the tenant is not known to the assets service")]
    TenantUnknown,
    /// The device pool does not have the gateway.
    #[error("the gateway is not in the device pool")]
    NotInPool,
    /// Another tenant holds the gateway; unbinding first is Laysi/trustiot-v2#115.
    #[error("the gateway is bound to another tenant")]
    BoundElsewhere,
    /// A stored row breaks the rules the tables enforce, which cannot happen.
    #[error("a stored binding breaks the rules")]
    Corrupt,
    /// The database failed or refused the transaction.
    #[error(transparent)]
    Database(TenantDbError),
    /// The event could not be written.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; a binding ID always is, so this does not happen.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<TenantDbError> for BindError {
    fn from(error: TenantDbError) -> Self {
        Self::Database(error)
    }
}

impl From<sqlx::Error> for BindError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl BindError {
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

/// Whether `user` may act as a platform administrator here: the administrator's own request,
/// whose token the ingress's cookie check issued. A token with `act` was exchanged by another
/// service calling on the administrator's behalf (decision 27), and binding a gateway into a
/// tenant is the administrator's own act (Laysi/trustiot-v2#104).
#[must_use]
pub fn is_own_platform_request(user: &UserIdentity) -> bool {
    user.is_platform_admin() && user.claims().act.is_none()
}

/// Binds gateways to tenants; see the module.
#[derive(Clone, Debug)]
pub struct GatewayBinder {
    db: BackgroundTenantDb,
}

impl GatewayBinder {
    /// A binder on the service role's pool. `db` must be the background handle of the service's
    /// own pool (`TenantDb::connect_lazy_with_background`), which only startup has.
    #[must_use]
    pub fn new(db: BackgroundTenantDb) -> Self {
        Self { db }
    }

    /// Binds `hardware_id` to `tenant` for the platform administrator `admin`, or finds it bound
    /// there already.
    ///
    /// # Errors
    ///
    /// [`BindError::NotPlatformAdmin`] before anything is opened; [`BindError::TenantUnknown`],
    /// [`BindError::NotInPool`], [`BindError::BoundElsewhere`] or a failure, and then nothing was
    /// written.
    pub async fn bind(
        &self,
        admin: &UserIdentity,
        hardware_id: &GatewayId,
        tenant: TenantId,
    ) -> Result<Outcome, BindError> {
        if !is_own_platform_request(admin) {
            return Err(BindError::NotPlatformAdmin);
        }
        // The audit line of every transaction opened here, like the actor line of
        // `PlatformDb::begin`: the background handle logs only at DEBUG and names no user.
        tracing::info!(
            actor = "platform_admin",
            user_id = %admin.user_id(),
            tenant_id = %tenant,
            hardware_id = hardware_id.as_str(),
            purpose = BIND_PURPOSE,
            "tenant transaction for a platform administrator"
        );
        let mut tx = self.db.begin(tenant, BIND_PURPOSE).await?;
        let known: i64 = sqlx::query_scalar(TENANT_KNOWN).fetch_one(&mut *tx).await?;
        if known != 1 {
            return Err(BindError::TenantUnknown);
        }
        if let Some(binding) = current(&mut tx, hardware_id).await? {
            return Ok(Outcome::AlreadyBound(binding));
        }
        let binding_id = GatewayBindingId::generate();
        // This INSERT must stay the transaction's first write: the transaction gets its ID here,
        // after `ON CONFLICT` has waited for a concurrent binding of the gateway, and the certs
        // service orders a gateway's bindings by the ID of the transaction that wrote their event
        // (`services/certs/README.md`, "目前的綁定"). An earlier write would take the ID before
        // the wait, so a later binding could sort before the one it waited for.
        let inserted = sqlx::query(INSERT_BINDING)
            .bind(tenant.to_string())
            .bind(binding_id.to_string())
            .bind(hardware_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| refused_insert(&error).unwrap_or_else(|| BindError::from(error)))?;
        let Some(row) = inserted else {
            // A current binding of the gateway committed while this one waited: a concurrent
            // retry for this tenant, or another tenant's binding.
            return match current(&mut tx, hardware_id).await? {
                Some(binding) => Ok(Outcome::AlreadyBound(binding)),
                None => Err(BindError::BoundElsewhere),
            };
        };
        let binding = Binding {
            binding_id,
            hardware_id: hardware_id.clone(),
            tenant_id: tenant,
            bound_at: row.try_get("bound_at")?,
        };
        let aggregate = Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding_id.to_string())?;
        // The event names the administrator, so the operation log shows who bound the gateway.
        outbox::publish_as(
            tx.transaction(),
            &aggregate,
            &GatewayBound {
                binding_id,
                hardware_id: hardware_id.clone(),
            },
            Some(tenant),
            Actor::User {
                user_id: admin.user_id(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Outcome::Bound(binding))
    }
}

/// The gateway's current binding in the transaction's tenant.
async fn current(
    tx: &mut TenantTransaction,
    hardware_id: &GatewayId,
) -> Result<Option<Binding>, BindError> {
    let tenant = tx.tenant_id();
    let row = sqlx::query(CURRENT_BINDING)
        .bind(hardware_id.as_str())
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| {
        Ok(Binding {
            binding_id: row
                .try_get::<String, _>("binding_id")?
                .parse()
                .map_err(|_| BindError::Corrupt)?,
            hardware_id: hardware_id.clone(),
            tenant_id: tenant,
            bound_at: row.try_get("bound_at")?,
        })
    })
    .transpose()
}

/// The refusal a foreign key gave the insert, if that is what failed.
fn refused_insert(error: &sqlx::Error) -> Option<BindError> {
    let database = error.as_database_error()?;
    if database.code().as_deref() != Some(FOREIGN_KEY_VIOLATION) {
        return None;
    }
    match database.constraint() {
        Some(POOL_FOREIGN_KEY) => Some(BindError::NotInPool),
        Some(TENANT_FOREIGN_KEY) => Some(BindError::TenantUnknown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use contract::identity::Actor as Exchanged;
    use testkit::identity::TestIssuer;

    use contract::ServiceName;

    use super::*;

    #[test]
    fn only_a_platform_administrators_own_request_may_bind() {
        let issuer = TestIssuer::new();
        let mut claims = TestIssuer::claims(ServiceName::Assets, None);
        assert!(!is_own_platform_request(
            &issuer.user(ServiceName::Assets, &claims)
        ));
        claims.platform_admin = true;
        assert!(is_own_platform_request(
            &issuer.user(ServiceName::Assets, &claims)
        ));
        claims.act = Some(Exchanged {
            sub: ServiceName::Identity,
        });
        assert!(
            !is_own_platform_request(&issuer.user(ServiceName::Assets, &claims)),
            "a token another service exchanged is refused"
        );
    }
}
