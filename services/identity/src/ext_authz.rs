//! The ingress gateway's cookie check (decision 27): Istio's ext_authz provider
//! (`deploy/istio/istiod-values.yaml`) calls `<CHECK_PATH_PREFIX><original path>` with the
//! original method, Host, `Cookie`, `Origin` and `Sec-Fetch-Site` for every request that needs a
//! session.
//!
//! | Outcome | Response | What the gateway does |
//! | --- | --- | --- |
//! | A cross-site request ([`crate::csrf`]), decided before the session is looked up | 403 `permission_denied`, message key `security.cross_site_request` | Returns this response to the client |
//! | Active session, routed request, the session's tenant active (or none) | 200 with exactly one `x-internal-identity`: a token this service just signed for the route's service |
//! | Active session whose tenant is suspended or deleted, routed to this service | 200 as above, the token with the `tenant_suspended` claim: the user can still read the session, switch tenants, log out and change preferences, and a platform administrator can still resume the tenant; this service's tenant-scoped endpoints refuse the claim themselves |
//! | Active session whose tenant is suspended or deleted, routed to any other service | 403 `tenant_suspended`; no token is signed | Replaces any client copy of the header with this one and forwards the request |
//! | No, repeated, malformed, unknown, expired or revoked session cookie | 401 `unauthenticated` | Returns this response to the client |
//! | Active session, but no cookie route matches the host and path | 403 `permission_denied` | Returns this response to the client |
//! | The session store failed, was busy, or did not answer within [`CHECK_DEADLINE`]; or no signing key may sign now ([`crate::issuer::IssueError::NoSigningKey`]) | 503 `unavailable` | Treats a 5xx as an error and answers its own 503 (`statusOnError`) |
//!
//! A 200 is only ever built from a freshly signed token ([`Decision::Allow`] cannot exist
//! without one), because the gateway forwards the client's own `x-internal-identity` when an
//! allow lacks it (docs/feasibility/2026-10-10-edge-poc.md). Nothing in the check request but the
//! method, the cookie, `Origin`, `Sec-Fetch-Site`, Host and path is read: an
//! `x-internal-identity` header the client sent, in any spelling, has no effect. Denials carry no
//! identity header, since the gateway returns them to the client as they are.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use contract::{ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalToken, ServiceName};

use crate::audience::IngressRoutes;
use crate::csrf::{CROSS_SITE_REQUEST, cross_site};
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

    /// Decides one check request. A cross-site request is refused first, so it never costs a
    /// session lookup.
    async fn decide(&self, method: &Method, headers: &HeaderMap, uri: &Uri) -> Decision {
        if let Some(header) = cross_site(method, headers) {
            return Decision::CrossSite(header);
        }
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
        // A suspended tenant reaches this service only (checklist B1, Laysi/trustiot-v2#777): no
        // other service ever gets a token for it, so none has to know about suspensions.
        if grant.tenant_suspended && audience != ServiceName::Identity {
            return Decision::TenantSuspended;
        }
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
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let decision = ext_authz.decide(&method, &headers, &uri).await;
    if let Some(reason) = decision.denial_reason() {
        // The header's name only: Origin is the client's to choose.
        if let Decision::CrossSite(header) = &decision {
            tracing::info!(reason, header = *header, "denied the ext_authz check");
        } else {
            tracing::info!(reason, "denied the ext_authz check");
        }
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
    /// 403 before the session is looked up: a cross-site request, shown by this header.
    CrossSite(&'static str),
    /// 401; the reason goes to the log only.
    Unauthenticated(&'static str),
    /// 403: an active session, but no cookie route for the request.
    NoRoute,
    /// 403 `tenant_suspended`: the session's tenant is suspended or deleted, and the request
    /// goes to another service than this one.
    TenantSuspended,
    /// 503: the check could not be completed.
    Unavailable,
}

impl Decision {
    fn denial_reason(&self) -> Option<&'static str> {
        match self {
            Self::Allow(_) | Self::Unavailable => None,
            Self::CrossSite(_) => Some("cross_site_request"),
            Self::Unauthenticated(reason) => Some(reason),
            Self::NoRoute => Some("no_route"),
            Self::TenantSuspended => Some("tenant_suspended"),
        }
    }
}

impl IntoResponse for Decision {
    fn into_response(self) -> Response {
        let no_store = (CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let reply = |body: ErrorBody| {
            (body.http_status(), [no_store.clone()], axum::Json(body)).into_response()
        };
        let error = |code: ErrorCode| reply(ErrorBody::new(code));
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
            Self::CrossSite(_) => reply(
                ErrorBody::new(ErrorCode::PermissionDenied).with_message_key(CROSS_SITE_REQUEST),
            ),
            Self::Unauthenticated(_) => error(ErrorCode::Unauthenticated),
            Self::NoRoute => error(ErrorCode::PermissionDenied),
            Self::TenantSuspended => error(ErrorCode::TenantSuspended),
            Self::Unavailable => error(ErrorCode::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests;
