//! The tenants the service knows (Laysi/trustiot-v2#65; services/assets/README.md, "租戶").
//!
//! - [`TenantCreatedHandler`] gives every tenant its row in `assets.tenants` when the identity
//!   service's `identity.tenant_created` arrives. The consumer runs it in a transaction of the
//!   event's tenant, so row-level security admits only that tenant's row, and the insert does
//!   nothing when the row exists: a replay, or the same event delivered again, never adds a
//!   second one.
//! - `GET /api/assets/v1/tenant` returns the caller's tenant as this service knows it:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | A user who has not picked or joined a tenant | 403 `permission_denied` |
//! | The tenant's event has not been processed yet | 404 `not_found` |
//! | The database failed | 503 `unavailable` |
//! | Otherwise | 200 with [`AssetsTenant`] |

use axum::Router;
use axum::extract::{FromRef, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::events::identity::TenantCreated;
use contract::{ErrorBody, ErrorCode, TenantId};
use outbox::{Delivery, Handler, HandlerError};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use sqlx::{Postgres, Transaction};
use utoipa::ToSchema;

/// The consumer's subscription to the identity service's events; never renamed (a new name
/// starts reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "assets.identity-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "identity";

/// The route, as the ingress gateway forwards it (no prefix is stripped).
pub const TENANT_PATH: &str = "/api/assets/v1/tenant";

/// The delivery's tenant's row; nothing when it exists. `$1` is the tenant ID as text.
const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)
ON CONFLICT (tenant_id) DO NOTHING";

/// The transaction's tenant's row. Row-level security already limits the rows to that tenant.
const SELECT_TENANT: &str = "SELECT tenant_id::text, created_at FROM assets.tenants";

/// Applies `identity.tenant_created`; ignores the identity service's other events.
#[derive(Clone, Copy, Debug, Default)]
pub struct TenantCreatedHandler;

impl Handler for TenantCreatedHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if !delivery.envelope.is::<TenantCreated>() {
            return Ok(());
        }
        let TenantCreated {} = delivery.envelope.decode()?;
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "identity.tenant_created without a tenant",
            ));
        };
        let inserted = sqlx::query(INSERT_TENANT)
            .bind(tenant.to_string())
            .execute(&mut **tx)
            .await?;
        tracing::info!(
            tenant_id = %tenant,
            event_id = %delivery.envelope.id,
            created = inserted.rows_affected() == 1,
            "applied identity.tenant_created"
        );
        Ok(())
    }
}

/// The caller's tenant as this service knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssetsTenant {
    /// The tenant's ID.
    pub tenant_id: TenantId,
    /// When this service learned of the tenant, by the database clock.
    pub created_at: DateTime<Utc>,
}

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The service's API: [`TENANT_PATH`].
pub fn routes(verifier: IdentityVerifier, db: TenantDb) -> Router {
    Router::new()
        .route(TENANT_PATH, get(get_tenant))
        .with_state(Routes { verifier, db })
}

/// The caller's tenant, once this service has processed its `identity.tenant_created`.
#[utoipa::path(
    get,
    path = "/api/assets/v1/tenant",
    tag = "assets",
    operation_id = "getAssetsTenant",
    responses(
        (status = 200, description = "The caller's tenant.", body = AssetsTenant),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "The user has not picked or joined a tenant (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "The service has not processed the tenant's creation yet (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_tenant(State(state): State<Routes>, user: UserIdentity) -> Response {
    match read_tenant(&state.db, &user).await {
        Ok(Some(tenant)) => ([(CACHE_CONTROL, "no-store")], axum::Json(tenant)).into_response(),
        Ok(None) => error(ErrorCode::NotFound),
        Err(TenantDbError::NoTenant) => error(ErrorCode::PermissionDenied),
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = error_kind(&failure),
                "could not read the tenant"
            );
            error(ErrorCode::Unavailable)
        }
    }
}

async fn read_tenant(
    db: &TenantDb,
    user: &UserIdentity,
) -> Result<Option<AssetsTenant>, TenantDbError> {
    let mut tx = db.begin(user).await?;
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(SELECT_TENANT)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let Some((tenant_id, created_at)) = row else {
        return Ok(None);
    };
    // Row-level security returned the transaction's tenant only; a row that is not a valid ID
    // or not that tenant is not trusted.
    match tenant_id.parse::<TenantId>() {
        Ok(tenant_id) if Some(tenant_id) == user.tenant_id() => Ok(Some(AssetsTenant {
            tenant_id,
            created_at,
        })),
        _ => Err(TenantDbError::Database(sqlx::Error::RowNotFound)),
    }
}

/// The SQLSTATE or the kind of a failure, never data.
fn error_kind(error: &TenantDbError) -> String {
    match error {
        TenantDbError::Database(sqlx::Error::Database(database)) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        TenantDbError::Database(sqlx::Error::PoolTimedOut) => "pool timed out".to_owned(),
        TenantDbError::Database(_) => "database client".to_owned(),
        other => other.to_string(),
    }
}

fn error(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
