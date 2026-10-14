//! Tenants (Laysi/trustiot-v2#65; services/identity/README.md, "租戶"): a platform administrator
//! creates one with `POST /api/identity/v1/tenants`. The tenant row and the
//! `identity.tenant_created` event are written in one transaction, in the new tenant's row-level
//! security context, so the event exists exactly when the tenant does, and every service that
//! keeps per-tenant state creates it from the event (decision 4).
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | A user who is not a platform administrator (A2) | 403 `permission_denied` |
//! | A body that is not a JSON object with a string `name` | 400 `invalid_request` |
//! | A `name` that is empty after trimming, longer than [`MAX_NAME_CHARS`] characters, or holds a control character | 422 `validation_failed`, field `/name`, key `tenant.name_invalid`, `{"max": 100}` |
//! | The database failed or refused the transaction | 503 `unavailable`; nothing was created |
//! | Created | 201 with [`Tenant`] |

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, Utc};
use contract::events::identity::{TENANT_AGGREGATE, TenantCreated};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, ServiceName, TenantId};
use outbox::Aggregate;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, ServiceIdentity, UserIdentity};
use service_kit::postgres::InsecureConnection;
use service_kit::tenant::{BackgroundTenantDb, TenantDb, TenantDbError};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use utoipa::ToSchema;

/// The route, as the ingress gateway forwards it (no prefix is stripped).
pub const TENANTS_PATH: &str = "/api/identity/v1/tenants";

/// The longest tenant name, in characters (Unicode scalar values, as Postgres' `char_length`
/// counts them).
pub const MAX_NAME_CHARS: usize = 100;

/// Connections of the pool that creates tenants, separate from the session store's, so a burst
/// of tenant creations never takes a connection the ext_authz check is waiting for.
pub const POOL_CONNECTIONS: u32 = 2;

/// How long creating a tenant may wait for a connection (opening one included).
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The message key of an invalid name; `details.max` is [`MAX_NAME_CHARS`].
pub const NAME_INVALID: MessageKey = MessageKey::new("tenant.name_invalid");

/// Names the work in the kit's DEBUG log of the tenant transaction.
const CREATE_PURPOSE: &str = "create-tenant";

/// Inserts the tenant in its own transaction; row-level security checks that the row is the
/// transaction's tenant. `$1` is the tenant ID as text, `$2` the name.
const INSERT_TENANT: &str = "INSERT INTO identity.tenants (tenant_id, name) VALUES ($1::uuid, $2)
RETURNING created_at";

/// The body of `POST /api/identity/v1/tenants`. Unknown fields are ignored.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTenant {
    /// What people call the tenant: 1 to 100 characters after trimming, no control characters.
    pub name: String,
}

/// A tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Tenant {
    /// The tenant's ID (UUIDv7).
    pub tenant_id: TenantId,
    /// The name, trimmed.
    pub name: String,
    /// When the tenant was created, by the database clock.
    pub created_at: DateTime<Utc>,
}

/// A tenant name that passed the rules: trimmed, 1 to [`MAX_NAME_CHARS`] characters, no
/// control characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantName(String);

impl TenantName {
    /// Checks a requested name; `None` when it breaks a rule.
    #[must_use]
    pub fn parse(requested: &str) -> Option<Self> {
        let name = requested.trim();
        let chars = name.chars().count();
        let valid = (1..=MAX_NAME_CHARS).contains(&chars) && !name.chars().any(char::is_control);
        valid.then(|| Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a tenant was not created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The user is not a platform administrator.
    #[error("only a platform administrator may create a tenant")]
    NotPlatformAdmin,
    /// The transaction could not start, or a statement in it failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// The event could not be written; the transaction was rolled back.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; the kind and a tenant ID always are, so this does
    /// not happen.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<sqlx::Error> for CreateError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl CreateError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        let sqlstate = |error: &sqlx::Error| match error {
            sqlx::Error::Database(database) => database
                .code()
                .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
            sqlx::Error::PoolTimedOut => "pool timed out".to_owned(),
            _ => "database client".to_owned(),
        };
        match self {
            Self::NotPlatformAdmin => "not a platform administrator".to_owned(),
            Self::Database(TenantDbError::Database(error)) => sqlstate(error),
            Self::Database(error) => error.to_string(),
            // `PublishError` prints only the SQLSTATE or kind, `InvalidAggregate` only the rule.
            Self::Publish(error) => error.to_string(),
            Self::Aggregate(error) => error.to_string(),
        }
    }
}

/// Creates tenants: the only code in the service that opens a transaction for a tenant that no
/// request names. It holds the background handle of its own pool privately, and its only
/// method makes the tenant ID itself, so a request can never choose the tenant a transaction
/// runs in (services/identity/README.md, "租戶"; crates/service-kit/README.md).
#[derive(Clone, Debug)]
pub struct TenantStore {
    db: BackgroundTenantDb,
}

impl TenantStore {
    /// A lazily connecting store with its own pool of [`POOL_CONNECTIONS`] connections of the
    /// service role. Nothing connects until the first tenant is created.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(options: PgConnectOptions) -> Result<Self, InsecureConnection> {
        let pool = PgPoolOptions::new()
            .max_connections(POOL_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT);
        let actor = ServiceIdentity::new(ServiceName::Identity);
        // The request-scoped handle is dropped: tenant creation is the pool's only use.
        let (_requests, db): (TenantDb, BackgroundTenantDb) =
            TenantDb::connect_lazy_with_background(options, pool, actor)?;
        Ok(Self { db })
    }

    /// Creates a tenant named `name` for the platform administrator `admin`: a new tenant ID,
    /// the row and the `identity.tenant_created` event in one transaction of that tenant.
    ///
    /// # Errors
    ///
    /// [`CreateError::NotPlatformAdmin`] when `admin` is not one; otherwise when the database
    /// fails, and then nothing was created.
    pub async fn create(
        &self,
        admin: &UserIdentity,
        name: &TenantName,
    ) -> Result<Tenant, CreateError> {
        if !admin.is_platform_admin() {
            return Err(CreateError::NotPlatformAdmin);
        }
        let tenant_id = TenantId::generate();
        let mut tx = self.db.begin(tenant_id, CREATE_PURPOSE).await?;
        let created_at: DateTime<Utc> = sqlx::query_scalar(INSERT_TENANT)
            .bind(tenant_id.to_string())
            .bind(name.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let aggregate = Aggregate::new(TENANT_AGGREGATE, tenant_id.to_string())?;
        outbox::publish(
            tx.transaction(),
            &aggregate,
            &TenantCreated {},
            Some(tenant_id),
        )
        .await?;
        tx.commit().await?;
        tracing::info!(
            user_id = %admin.user_id(),
            tenant_id = %tenant_id,
            "created a tenant"
        );
        Ok(Tenant {
            tenant_id,
            name: name.as_str().to_owned(),
            created_at,
        })
    }
}

/// The state of the tenant routes.
#[derive(Clone)]
pub struct TenantRoutes {
    verifier: IdentityVerifier,
    store: Arc<TenantStore>,
}

impl FromRef<TenantRoutes> for IdentityVerifier {
    fn from_ref(state: &TenantRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The tenant routes: [`TENANTS_PATH`].
pub fn routes(verifier: IdentityVerifier, store: TenantStore) -> Router {
    Router::new()
        .route(TENANTS_PATH, post(create_tenant))
        .with_state(TenantRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// Creates a tenant. Platform administrators only.
#[utoipa::path(
    post,
    path = "/api/identity/v1/tenants",
    tag = "identity",
    operation_id = "createTenant",
    request_body = CreateTenant,
    responses(
        (status = 201, description = "The tenant was created, and `identity.tenant_created` written in the same transaction.", body = Tenant),
        (status = 400, description = "The body is not a JSON object with a string `name` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "The user is not a platform administrator (`permission_denied`).", body = ErrorBody),
        (status = 422, description = "The name is empty, longer than 100 characters, or holds a control character (`validation_failed`, field `/name`, `tenant.name_invalid`).", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was created (`unavailable`).", body = ErrorBody),
    )
)]
async fn create_tenant(
    State(state): State<TenantRoutes>,
    user: UserIdentity,
    body: Bytes,
) -> Response {
    if !user.is_platform_admin() {
        tracing::info!(
            user_id = %user.user_id(),
            "refused to create a tenant: not a platform administrator"
        );
        return error(ErrorBody::new(ErrorCode::PermissionDenied));
    }
    // Parsed by hand so a malformed body answers the M1 format, not axum's plain text.
    let Ok(request) = serde_json::from_slice::<CreateTenant>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Some(name) = TenantName::parse(&request.name) else {
        let max = u64::try_from(MAX_NAME_CHARS).unwrap_or(u64::MAX);
        return error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/name", NAME_INVALID).with_detail("max", max)),
        );
    };
    match state.store.create(&user, &name).await {
        Ok(tenant) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, "no-store")],
            axum::Json(tenant),
        )
            .into_response(),
        Err(CreateError::NotPlatformAdmin) => error(ErrorBody::new(ErrorCode::PermissionDenied)),
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not create a tenant"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(
            TenantName::parse("  Plant A  ").map(|name| name.as_str().to_owned()),
            Some("Plant A".to_owned())
        );
        assert!(TenantName::parse("").is_none());
        assert!(TenantName::parse("   ").is_none());
        assert!(TenantName::parse("a\u{0}b").is_none());
        assert!(TenantName::parse("line\nbreak").is_none());
        // Characters, not bytes: 100 three-byte characters fit, 101 do not.
        assert!(TenantName::parse(&"廠".repeat(MAX_NAME_CHARS)).is_some());
        assert!(TenantName::parse(&"廠".repeat(MAX_NAME_CHARS + 1)).is_none());
    }
}
