//! The tenant's gateways with their presence and configuration versions (Laysi/trustiot-v2#112,
//! rebuild checklist C3; services/gateway-ops/README.md, "API").
//!
//! | Request | Role | Success |
//! | --- | --- | --- |
//! | `GET /api/gateway-ops/v1/gateways` | viewer | 200 [`GatewayList`] |
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or no role in it | 403 `permission_denied` |
//! | The database failed, or all request slots are taken | 503 `unavailable` |
//!
//! The list is read as the service role in the caller's tenant (decision 29), so row-level
//! security shows only the tenant's own bindings, presence rows and configurations. It lists
//! every binding of the tenant that has not ended, one per gateway (the latest by the assets
//! service's event order), with the gateway's presence row of that binding if the gateway has
//! been heard from during it. Every response carries `cache-control: no-store`.
//!
//! The requests share the service role's pool with the background work, which counts on its
//! connections (`crate::config::MIN_DATABASE_CONNECTIONS`): at most [`request_slots`] requests
//! read at once, and one beyond that is answered 503 at once instead of waiting for a
//! connection the presence task or the delivery needs.

use axum::Router;
use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::{ErrorBody, ErrorCode, GatewayBindingId, TenantRole};
use serde::Serialize;
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use std::sync::Arc;

use sqlx::Row;
use sqlx::postgres::PgRow;
use tokio::sync::Semaphore;
use utoipa::ToSchema;

use crate::config::MIN_DATABASE_CONNECTIONS;

/// The list's path.
pub const GATEWAYS_PATH: &str = "/api/gateway-ops/v1/gateways";

/// The tenant's current bindings, one per gateway, with their presence rows and the latest
/// configuration version made for them. Row-level security limits every table to the
/// transaction's tenant.
const LIST: &str = "SELECT binding.hardware_id, binding.binding_id::text AS binding_id,
    binding.bound_at, presence.state, presence.offline_reason, presence.changed_at,
    presence.last_seen_at, presence.agent_version, presence.config_version,
    (SELECT max(config.version) FROM gateway_ops.desired_configs AS config
        WHERE config.hardware_id = binding.hardware_id
            AND config.binding_id = binding.binding_id) AS desired_version
FROM (
    SELECT DISTINCT ON (hardware_id) tenant_id, binding_id, hardware_id, bound_at
    FROM gateway_ops.gateway_bindings
    WHERE unbound_at IS NULL
    ORDER BY hardware_id, position_tx DESC, position_id DESC
) AS binding
LEFT JOIN gateway_ops.gateway_presence AS presence
    ON presence.tenant_id = binding.tenant_id AND presence.binding_id = binding.binding_id
ORDER BY binding.hardware_id";

/// Whether a gateway is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Connected: its latest status of the session was `online`, or it sent heartbeats.
    Online,
    /// Not connected; `offline_reason` says why.
    Offline,
    /// Not heard from since it was bound.
    Unknown,
}

/// Why a gateway is offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReason {
    /// The agent stopped on purpose (shutdown, restart, update).
    Shutdown,
    /// The broker lost the connection and published the gateway's last will: the connection
    /// dropped, or was silent for 1.5 times its keepalive.
    ConnectionLost,
    /// The gateway's heartbeats stopped for longer than their interval allows.
    HeartbeatTimeout,
}

/// A gateway bound to the caller's tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct Gateway {
    /// The hardware ID.
    #[schema(
        pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
        max_length = 64,
        example = "tg-0001"
    )]
    pub hardware_id: String,
    /// The binding (a new one each time the gateway is bound).
    pub binding_id: GatewayBindingId,
    /// When it was bound.
    pub bound_at: DateTime<Utc>,
    /// Whether it is connected.
    pub presence: Presence,
    /// Why it is offline; `null` unless `presence` is `offline`.
    pub offline_reason: Option<OfflineReason>,
    /// When `presence` last changed, by the service's database clock; `null` while `unknown`.
    pub presence_changed_at: Option<DateTime<Utc>>,
    /// When the service last received a status or heartbeat of the gateway's current or last
    /// session (its last report); `null` while `unknown`.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The agent's release the gateway reported on its current (or last) connection; `null`
    /// before that connection reported one.
    #[schema(max_length = 64, example = "1.4.0")]
    pub agent_version: Option<String>,
    /// The configuration version the gateway said it has applied, in the latest heartbeat of
    /// its current (or last) connection; `null` before that connection sent one.
    #[schema(minimum = 1)]
    pub applied_config_version: Option<u64>,
    /// The latest configuration version stored for the binding (what the gateway should have);
    /// `null` before the first one.
    #[schema(minimum = 1)]
    pub desired_config_version: Option<u64>,
}

/// The caller's tenant's gateways.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct GatewayList {
    /// By hardware ID (byte order).
    pub gateways: Vec<Gateway>,
}

/// How many requests may read at once with a service-role pool of `max_connections`: the
/// connections beyond what the background work holds at most (at least 1).
#[must_use]
pub fn request_slots(max_connections: u32) -> usize {
    let spare = max_connections
        .saturating_sub(MIN_DATABASE_CONNECTIONS - 1)
        .max(1);
    usize::try_from(spare).unwrap_or(1)
}

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
    slots: Arc<Semaphore>,
}

impl axum::extract::FromRef<Routes> for IdentityVerifier {
    fn from_ref(routes: &Routes) -> Self {
        routes.verifier.clone()
    }
}

/// The routes, each answered with `cache-control: no-store`, with `slots` requests reading at
/// once ([`request_slots`]); [`crate::start`] serves them, and tests serve them in process.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, slots: usize) -> Router {
    Router::new()
        .route(GATEWAYS_PATH, get(list_gateways))
        .route_layer(axum::middleware::map_response(no_store))
        .with_state(Routes {
            verifier,
            db,
            slots: Arc::new(Semaphore::new(slots)),
        })
}

/// Marks every response of these routes as not to be stored, including the 401 that the
/// [`UserIdentity`] extractor answers.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The tenant's gateways, with whether each is connected, its last report and its configuration
/// versions.
#[utoipa::path(
    get,
    path = "/api/gateway-ops/v1/gateways",
    tag = "gateway-ops",
    operation_id = "listGateways",
    responses(
        (status = 200, description = "The gateways bound to the caller's tenant, by hardware ID.", body = GatewayList),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed, or too many requests are being answered (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_gateways(State(state): State<Routes>, user: UserIdentity) -> Response {
    if let Err(refused) = user.require(Requirement::Tenant(TenantRole::Viewer), "list gateways") {
        return refused.into_response();
    }
    let Ok(_slot) = state.slots.try_acquire() else {
        tracing::debug!(
            user_id = %user.user_id(),
            "gateway list refused: every request slot is taken"
        );
        return error(ErrorBody::new(ErrorCode::Unavailable));
    };
    match list(&state.db, &user).await {
        Ok(gateways) => (
            StatusCode::OK,
            [(CACHE_CONTROL, "no-store")],
            axum::Json(GatewayList { gateways }),
        )
            .into_response(),
        Err(ListError::Database(TenantDbError::NoTenant)) => {
            error(ErrorBody::new(ErrorCode::PermissionDenied))
        }
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not list gateways"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

#[derive(Debug, thiserror::Error)]
enum ListError {
    #[error(transparent)]
    Database(#[from] TenantDbError),
    #[error("a stored row breaks the rules: {0}")]
    Corrupt(&'static str),
}

impl From<sqlx::Error> for ListError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl ListError {
    fn log_kind(&self) -> String {
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

async fn list(db: &TenantDb, user: &UserIdentity) -> Result<Vec<Gateway>, ListError> {
    let mut tx = db.begin(user).await?;
    let rows = sqlx::query(LIST).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    rows.iter().map(gateway).collect()
}

fn version(value: Option<i64>, column: &'static str) -> Result<Option<u64>, ListError> {
    value
        .map(|value| u64::try_from(value).map_err(|_| ListError::Corrupt(column)))
        .transpose()
}

fn gateway(row: &PgRow) -> Result<Gateway, ListError> {
    let state: Option<String> = row.try_get("state")?;
    let reason: Option<String> = row.try_get("offline_reason")?;
    let (presence, offline_reason) = match (state.as_deref(), reason.as_deref()) {
        (None, None) => (Presence::Unknown, None),
        (Some("online"), None) => (Presence::Online, None),
        (Some("offline"), Some(reason)) => (
            Presence::Offline,
            Some(match reason {
                "shutdown" => OfflineReason::Shutdown,
                "connection_lost" => OfflineReason::ConnectionLost,
                "heartbeat_timeout" => OfflineReason::HeartbeatTimeout,
                _ => return Err(ListError::Corrupt("offline_reason")),
            }),
        ),
        _ => return Err(ListError::Corrupt("state")),
    };
    Ok(Gateway {
        hardware_id: row.try_get("hardware_id")?,
        binding_id: row
            .try_get::<String, _>("binding_id")?
            .parse()
            .map_err(|_| ListError::Corrupt("binding_id"))?,
        bound_at: row.try_get("bound_at")?,
        presence,
        offline_reason,
        presence_changed_at: row.try_get("changed_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        agent_version: row.try_get("agent_version")?,
        applied_config_version: version(row.try_get("config_version")?, "config_version")?,
        desired_config_version: version(row.try_get("desired_version")?, "desired_version")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_get_the_connections_the_background_work_leaves() {
        // The minimum pool leaves exactly the one connection counted for the API.
        assert_eq!(request_slots(MIN_DATABASE_CONNECTIONS), 1);
        assert_eq!(request_slots(10), 2);
        assert_eq!(request_slots(100), 92);
        // Never none, whatever the pool (validation keeps it at the minimum or above).
        assert_eq!(request_slots(0), 1);
    }
}
