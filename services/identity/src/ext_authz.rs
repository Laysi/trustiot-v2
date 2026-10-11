//! The ingress gateway's cookie check (decision 27): Istio's ext_authz provider
//! (`deploy/istio/istiod-values.yaml`) calls `<CHECK_PATH_PREFIX><original path>` with the
//! original method, Host and `Cookie` for every request that needs a session.
//!
//! | Outcome | Response | What the gateway does |
//! | --- | --- | --- |
//! | Active session, routed request | 200 with exactly one `x-internal-identity`: a token this service just signed for the route's service | Replaces any client copy of the header with this one and forwards the request |
//! | No, repeated, malformed, unknown, expired or revoked session cookie | 401 `unauthenticated` | Returns this response to the client |
//! | Active session, but no cookie route matches the host and path | 403 `permission_denied` | Returns this response to the client |
//! | The session store failed, was busy, or did not answer within [`CHECK_DEADLINE`] | 503 `unavailable` | Treats a 5xx as an error and answers its own 503 (`statusOnError`) |
//!
//! A 200 is only ever built from a freshly signed token ([`Decision::Allow`] cannot exist
//! without one), because the gateway forwards the client's own `x-internal-identity` when an
//! allow lacks it (docs/feasibility/2026-10-10-edge-poc.md). Nothing in the check request but the
//! cookie, Host and path is read: an `x-internal-identity` header the client sent, in any
//! spelling, has no effect. Denials carry no identity header, since the gateway returns them to
//! the client as they are.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use contract::{ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalToken};

use crate::audience::IngressRoutes;
use crate::issuer::Issuer;
use crate::session::{SessionStore, session_secret};

/// The path the ext_authz provider calls, followed by the original request's path. It is under
/// `/internal`, which the ingress gateway never routes, so clients cannot reach it.
pub const CHECK_PATH_PREFIX: &str = "/internal/ext-authz";

/// How long one check may take. The gateway gives up after 0.5 s (`timeout` of the provider), so
/// the check answers 503 before that. The lookup itself goes on in the background, bounded by
/// [`crate::session::WORK_LIMIT`], so a database connection it was opening is kept for the next
/// check instead of being cut off (services/identity/README.md, "連線池").
pub const CHECK_DEADLINE: Duration = Duration::from_millis(400);

/// The check's state: the session store, the signer, and the routes that decide the audience.
pub struct ExtAuthz<S> {
    store: S,
    issuer: Issuer,
    routes: IngressRoutes,
    clock: fn() -> i64,
}

impl<S: SessionStore> ExtAuthz<S> {
    /// A check that issues tokens at the current time.
    #[must_use]
    pub fn new(store: S, issuer: Issuer, routes: IngressRoutes) -> Self {
        Self::with_clock(store, issuer, routes, crate::issuer::unix_now)
    }

    /// A check with another clock (seconds since the Unix epoch), for tests.
    #[must_use]
    pub fn with_clock(store: S, issuer: Issuer, routes: IngressRoutes, clock: fn() -> i64) -> Self {
        Self {
            store,
            issuer,
            routes,
            clock,
        }
    }

    /// Decides one check request.
    async fn decide(&self, headers: &HeaderMap, uri: &Uri) -> Decision {
        let secret = match session_secret(headers) {
            Ok(secret) => secret,
            Err(error) => return Decision::Unauthenticated(error.reason()),
        };
        let grant =
            match tokio::time::timeout(CHECK_DEADLINE, self.store.lookup(secret.hash())).await {
                Ok(Ok(Some(grant))) => grant,
                Ok(Ok(None)) => return Decision::Unauthenticated("no_active_session"),
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "could not look up the session");
                    return Decision::Unavailable;
                }
                Err(_) => {
                    tracing::error!(
                        deadline_ms = CHECK_DEADLINE.as_millis(),
                        "the session lookup did not finish in time"
                    );
                    return Decision::Unavailable;
                }
            };
        let Some(audience) =
            original_host(headers).and_then(|host| self.routes.audience(host, original_path(uri)))
        else {
            return Decision::NoRoute;
        };
        match self.issuer.issue(&grant, audience, (self.clock)()) {
            Ok(token) => Decision::Allow(token),
            Err(error) => {
                tracing::error!(error = %error, "could not sign the internal identity");
                Decision::Unavailable
            }
        }
    }
}

/// The routes of the check endpoint: [`CHECK_PATH_PREFIX`] itself, with a trailing `/`, and with
/// any original path after it, for every method (the gateway keeps the original method).
pub fn routes<S: SessionStore>(ext_authz: ExtAuthz<S>) -> Router {
    Router::new()
        .route(CHECK_PATH_PREFIX, any(check::<S>))
        .route(&format!("{CHECK_PATH_PREFIX}/"), any(check::<S>))
        .route(
            &format!("{CHECK_PATH_PREFIX}/{{*original}}"),
            any(check::<S>),
        )
        .with_state(Arc::new(ext_authz))
}

async fn check<S: SessionStore>(
    State(ext_authz): State<Arc<ExtAuthz<S>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let decision = ext_authz.decide(&headers, &uri).await;
    if let Some(reason) = decision.denial_reason() {
        tracing::info!(reason, "denied the ext_authz check");
    }
    decision.into_response()
}

/// The `Host` of the original request: exactly one `Host` header (HTTP/1.1, which the gateway
/// uses towards this service), or the URI's authority.
fn original_host(headers: &HeaderMap) -> Option<&str> {
    let mut hosts = headers.get_all(HOST).into_iter();
    match (hosts.next(), hosts.next()) {
        (Some(host), None) => host.to_str().ok(),
        _ => None,
    }
}

/// The original request's path: what follows [`CHECK_PATH_PREFIX`], `/` when nothing does. The
/// query is not part of [`Uri::path`].
fn original_path(uri: &Uri) -> &str {
    match uri.path().strip_prefix(CHECK_PATH_PREFIX) {
        Some("") | None => "/",
        Some(path) => path,
    }
}

/// The outcome of one check.
enum Decision {
    /// The only way to a 200: carries the token the response must hold.
    Allow(InternalToken),
    /// 401; the reason goes to the log only.
    Unauthenticated(&'static str),
    /// 403: an active session, but no cookie route for the request.
    NoRoute,
    /// 503: the check could not be completed.
    Unavailable,
}

impl Decision {
    fn denial_reason(&self) -> Option<&'static str> {
        match self {
            Self::Allow(_) | Self::Unavailable => None,
            Self::Unauthenticated(reason) => Some(reason),
            Self::NoRoute => Some("no_route"),
        }
    }
}

impl IntoResponse for Decision {
    fn into_response(self) -> Response {
        let no_store = (CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let error = |code: ErrorCode| {
            let body = ErrorBody::new(code);
            (body.http_status(), [no_store.clone()], axum::Json(body)).into_response()
        };
        match self {
            Self::Allow(token) => match HeaderValue::from_str(token.expose()) {
                Ok(value) => (
                    StatusCode::OK,
                    [
                        (HeaderName::from_static(INTERNAL_IDENTITY_HEADER), value),
                        no_store,
                    ],
                )
                    .into_response(),
                // InternalToken only holds visible ASCII, so this cannot happen; never allow
                // without the header.
                Err(_) => error(ErrorCode::Unavailable),
            },
            Self::Unauthenticated(_) => error(ErrorCode::Unauthenticated),
            Self::NoRoute => error(ErrorCode::PermissionDenied),
            Self::Unavailable => error(ErrorCode::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests;
