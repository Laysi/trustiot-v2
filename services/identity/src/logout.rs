//! Logging out (Laysi/trustiot-v2#103; decision 13): revoking the request's session, or every
//! session of the user. Both come through the ingress's cookie check like any other API call,
//! so the request carries the user's internal identity token for `identity` and, forwarded by
//! the ingress, the session cookie itself.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | `DELETE /api/identity/v1/sessions/current` without a usable session cookie | 401 `unauthenticated` |
//! | `DELETE /api/identity/v1/sessions/current` | 204; the session of the cookie is revoked if it is the user's, and the cookie is cleared |
//! | `DELETE /api/identity/v1/sessions` | 204; every session of the user is revoked, this one included, and the cookie is cleared |
//! | The database failed | 503 `unavailable`; nothing was revoked |
//!
//! The ext_authz check reads the session from the database on every request, so a revoked
//! session is refused from its next request on, on every device. A page of another site cannot
//! make the browser send a `DELETE` with the cookie: that needs a CORS preflight, which nothing
//! allows.

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRef, State};
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::delete;
use contract::{ErrorBody, ErrorCode};
use service_kit::identity::{IdentityVerifier, UserIdentity};

use crate::login::LoginStore;
use crate::session::{clear_cookie, session_secret};
use crate::tenants::error;

/// Logs out the request's session.
pub const CURRENT_SESSION_PATH: &str = "/api/identity/v1/sessions/current";

/// Logs out every session of the user.
pub const SESSIONS_PATH: &str = "/api/identity/v1/sessions";

/// The state of the logout routes.
#[derive(Clone)]
pub struct LogoutRoutes {
    verifier: IdentityVerifier,
    store: Arc<LoginStore>,
}

impl FromRef<LogoutRoutes> for IdentityVerifier {
    fn from_ref(state: &LogoutRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The logout routes: [`CURRENT_SESSION_PATH`] and [`SESSIONS_PATH`].
pub fn routes(verifier: IdentityVerifier, store: LoginStore) -> Router {
    Router::new()
        .route(CURRENT_SESSION_PATH, delete(log_out))
        .route(SESSIONS_PATH, delete(log_out_everywhere))
        .with_state(LogoutRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// 204 with the session cookie cleared.
fn logged_out() -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (SET_COOKIE, clear_cookie()),
        ],
    )
        .into_response()
}

/// Logs out: revokes the session of the request's cookie.
#[utoipa::path(
    delete,
    path = "/api/identity/v1/sessions/current",
    tag = "identity",
    operation_id = "logOut",
    responses(
        (status = 204, description = "The session is revoked; `Set-Cookie` clears the session cookie. Its next request is refused."),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was revoked (`unavailable`).", body = ErrorBody),
    )
)]
async fn log_out(
    State(state): State<LogoutRoutes>,
    user: UserIdentity,
    headers: HeaderMap,
) -> Response {
    let secret = match session_secret(&headers) {
        Ok(secret) => secret,
        Err(failure) => {
            tracing::info!(
                user_id = %user.user_id(),
                reason = failure.reason(),
                "refused to log out: no usable session cookie"
            );
            return error(ErrorBody::new(ErrorCode::Unauthenticated));
        }
    };
    let user_id = user.user_id().to_string();
    match state.store.revoke_current(&user_id, secret.hash()).await {
        Ok(revoked) => {
            tracing::info!(user_id = %user_id, revoked, "logged out");
            logged_out()
        }
        Err(failure) => {
            tracing::error!(user_id = %user_id, error = %failure, "could not log out");
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// Logs out on every device: revokes every session of the user, this one included.
#[utoipa::path(
    delete,
    path = "/api/identity/v1/sessions",
    tag = "identity",
    operation_id = "logOutEverywhere",
    responses(
        (status = 204, description = "Every session of the user is revoked; `Set-Cookie` clears the session cookie. The next request of any of them is refused."),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was revoked (`unavailable`).", body = ErrorBody),
    )
)]
async fn log_out_everywhere(State(state): State<LogoutRoutes>, user: UserIdentity) -> Response {
    let user_id = user.user_id().to_string();
    match state.store.revoke_all(&user_id).await {
        Ok(revoked) => {
            tracing::info!(user_id = %user_id, revoked, "logged out everywhere");
            logged_out()
        }
        Err(failure) => {
            tracing::error!(
                user_id = %user_id,
                error = %failure,
                "could not log out everywhere"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}
