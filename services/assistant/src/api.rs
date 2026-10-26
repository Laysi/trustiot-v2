//! The assistant's public API under `/api/assistant/v1`, which the ingress gateway forwards
//! after its cookie check with an internal identity token addressed to the assistant
//! (decision 27). Today it holds the dashboards ([`dashboards`]); the chat API
//! (Laysi/trustiot-v2#146) adds its routes here.
//!
//! - Every request is verified ([`UserIdentity`]): the user, the tenant and the role come from
//!   the token on every request, never from the body or a stored copy.
//! - Every response carries `cache-control: no-store`, the extractor's 401 included.
//! - Errors are M1 bodies ([`ErrorBody`]) without database or downstream text; failures that
//!   are not the request's fault are logged with their kind only.

pub mod dashboards;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::FromRef;
use axum::extract::rejection::PathRejection;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use contract::{ErrorBody, ErrorCode, TenantRole};
use serde::Serialize;
use service_kit::identity::{IdentityVerifier, OnBehalf, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError, TenantTransaction};
use utoipa::OpenApi;

use crate::dashboards::runner::DatasetRunner;
use crate::dashboards::store::Requester;
use crate::history::Principal;

/// The largest request body: the bodies are a few numbers and a name.
pub const MAX_BODY_BYTES: usize = 4 * 1024;

/// What the routes share.
#[derive(Clone)]
pub struct ApiState {
    /// Verifies the internal identity token of every request.
    pub verifier: IdentityVerifier,
    /// The service role's pool: one short transaction of the request's tenant per request.
    pub db: TenantDb,
    /// Exchanges the request's token for one addressed to telemetry-query when a dataset runs;
    /// `None` without `identity.exchange_url`, and dataset runs then fail (503).
    pub on_behalf: Option<OnBehalf>,
    /// The dataset runs, shared with the `dashboard` tool so the caps hold for both.
    pub runner: Arc<DatasetRunner>,
}

impl FromRef<ApiState> for IdentityVerifier {
    fn from_ref(state: &ApiState) -> Self {
        state.verifier.clone()
    }
}

/// Every route of the public API.
pub fn routes(state: ApiState) -> Router {
    dashboards::routes()
        .route_layer(axum::middleware::map_response(no_store))
        .with_state(state)
}

/// The public API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(
        dashboards::list_saved,
        dashboards::list_drafts,
        dashboards::get_dashboard,
        dashboards::list_versions,
        dashboards::publish,
        dashboards::restore,
        dashboards::archive,
        dashboards::unarchive,
        dashboards::run_dataset,
    ),
    tags((name = "assistant", description = "The AI assistant's dashboards (services/assistant)."))
)]
pub(crate) struct ApiDoc;

/// Marks every response as not to be stored, including the ones the handlers never build: a
/// 401 that the [`UserIdentity`] extractor answers carries no `cache-control` of its own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// An M1 error answer: [`ErrorBody`] with its status and `cache-control: no-store`.
#[derive(Debug)]
pub struct ApiError(pub ErrorBody);

impl From<ErrorBody> for ApiError {
    fn from(body: ErrorBody) -> Self {
        Self(body)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error(self.0)
    }
}

/// The requester of a request: its user, tenant and role. Every route needs at least viewer;
/// the 403 is answered when the token has no tenant or a role this build does not know.
fn requester(user: &UserIdentity, action: &'static str) -> Result<Requester, ApiError> {
    match user.tenant() {
        Some(grant) if grant.role.includes(TenantRole::Viewer) => Ok(Requester {
            principal: Principal {
                tenant_id: grant.id,
                user_id: user.user_id(),
            },
            role: grant.role,
        }),
        _ => {
            tracing::info!(
                user_id = %user.user_id(),
                action,
                "refused: no tenant or no known role in it"
            );
            Err(ErrorBody::new(ErrorCode::PermissionDenied).into())
        }
    }
}

/// A transaction of the request's tenant.
async fn begin(db: &TenantDb, user: &UserIdentity) -> Result<TenantTransaction, ApiError> {
    db.begin(user).await.map_err(|failure| match failure {
        TenantDbError::NoTenant => ErrorBody::new(ErrorCode::PermissionDenied).into(),
        other => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = db_error_kind(&other),
                "could not open a transaction for an API request"
            );
            ErrorBody::new(ErrorCode::Unavailable).into()
        }
    })
}

/// Commits `tx`; a failed commit answers 503 and nothing was written.
async fn commit(tx: TenantTransaction, user: &UserIdentity) -> Result<(), ApiError> {
    tx.commit().await.map_err(|failure| {
        tracing::error!(
            user_id = %user.user_id(),
            error_kind = sqlx_error_kind(&failure),
            "could not commit an API request's transaction"
        );
        ErrorBody::new(ErrorCode::Unavailable).into()
    })
}

fn db_error_kind(failure: &TenantDbError) -> String {
    match failure {
        TenantDbError::NoTenant => "no tenant".to_owned(),
        TenantDbError::UnsafeSession(_) => "unsafe session".to_owned(),
        TenantDbError::Database(error) => sqlx_error_kind(error),
        _ => "tenant database".to_owned(),
    }
}

/// The SQLSTATE of a database error, or the kind of a client error; never the database's text.
fn sqlx_error_kind(failure: &sqlx::Error) -> String {
    match failure {
        sqlx::Error::Database(database) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        sqlx::Error::PoolTimedOut => "pool timed out".to_owned(),
        _ => "database client".to_owned(),
    }
}

/// The path's single ID: 404 `not_found` when it is not a UUID.
fn parse_id<T: std::str::FromStr>(
    path: Result<axum::extract::Path<String>, PathRejection>,
) -> Result<T, ApiError> {
    path.ok()
        .and_then(|axum::extract::Path(text)| text.parse().ok())
        .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound).into())
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters, compared without regard to ASCII case. A page of another origin cannot send that
/// type with the user's cookie without a CORS preflight, which nothing answers.
fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|value| {
        let essence = value.split(';').next().unwrap_or_default();
        essence.trim().eq_ignore_ascii_case("application/json")
    })
}

/// The JSON body of a request as `T`, which refuses fields it does not know: 400
/// `invalid_request` when the content type is not JSON, the body is over [`MAX_BODY_BYTES`],
/// or it is not exactly a `T`.
async fn read_body<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: Body,
) -> Result<T, ApiError> {
    let invalid = || ApiError(ErrorBody::new(ErrorCode::InvalidRequest));
    if !is_json(headers) {
        return Err(invalid());
    }
    let bytes: Bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| invalid())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

fn ok<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
