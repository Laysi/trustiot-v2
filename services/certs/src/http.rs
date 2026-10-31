//! The service's plain HTTP endpoints, all public data.
//!
//! Under `/internal`, which the ingress never routes (decision 27); inside the cluster the mesh
//! lets only the callers `mesh.calls` names reach the service ([`routes`]):
//!
//! - `GET /internal/certs/v1/crl.pem`: the current CRL of every issuing CA whose certificate
//!   has not expired, PEM, for the MQTT broker's `crlfile` (Laysi/trustiot-v2#84). 503 while one
//!   of them has no CRL yet, so a fetcher keeps the file it has.
//! - `GET /internal/certs/v1/ca.pem`: the root certificate, then every issuing CA certificate,
//!   PEM. 503 while no CA is loaded.
//!
//! On the ingress's public path for gateways ([`gateway_routes`], Laysi/trustiot-v2#831):
//!
//! - `GET /certs/root-crl` (`protocol::root_crl::ROOT_CRL_PATH`): the root CRL the service
//!   adopted ([`CaState::root_crl`]), PEM; 404 while it has none. No cookie and no credential:
//!   the CRL is signed by the offline root, and the gateway checks it itself. The handler reads
//!   nothing of the request (no body, no header, no query), answers from memory without the
//!   database, and the answer is bounded by the adoption's own limit (`root_crl::MAX_BYTES`,
//!   [`crate::root_crl::RootCrl::check`]).
//!
//! Nothing here reads, returns or logs a private key: the handlers see the CRLs in the database
//! and the certificates of [`crate::authority::Authority`].

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use protocol::root_crl::{ROOT_CRL_CONTENT_TYPE, ROOT_CRL_PATH};
use sqlx::PgPool;

use crate::api;
use crate::authority::{CaState, unix_now};
use crate::crl;

/// The path of the CRL bundle.
pub const CRL_PATH: &str = "/internal/certs/v1/crl.pem";

/// The path of the CA chain.
pub const CHAIN_PATH: &str = "/internal/certs/v1/ca.pem";

/// The media type of PEM files.
const PEM_TYPE: &str = "application/x-pem-file";

/// The body of the 404 while there is no root CRL to hand out: fixed text, so it tells nothing
/// about why (no root CRL yet, or none checked against the loaded root).
pub const NO_ROOT_CRL: &str = "no root CRL is published\n";

#[derive(Clone)]
struct AppState {
    ca: CaState,
    pool: PgPool,
}

/// The routes, served by `service_kit::run`.
pub fn routes(ca: CaState, pool: PgPool) -> Router {
    Router::new()
        .route(CRL_PATH, get(crl_pem))
        .route(CHAIN_PATH, get(chain_pem))
        .with_state(AppState { ca, pool })
}

/// The gateways' route on the ingress's public path (`ingress.publicPaths.rootCrl`): `GET` (and
/// `HEAD`, which axum answers with the same status and headers and no body) of the root CRL;
/// every other method gets 405 with `Allow: GET,HEAD`. Every answer is `Cache-Control:
/// no-store`: the content changes only at a root ceremony, but a cache between the service and
/// a gateway must never hold back a root CRL that revokes an issuing CA.
pub fn gateway_routes(ca: CaState) -> Router {
    Router::new()
        .route(ROOT_CRL_PATH, get(root_crl_pem))
        .route_layer(axum::middleware::map_response(api::no_store))
        .with_state(ca)
}

fn unavailable(reason: &'static str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, reason).into_response()
}

fn pem(body: String) -> Response {
    ([(header::CONTENT_TYPE, PEM_TYPE)], body).into_response()
}

async fn crl_pem(State(state): State<AppState>) -> Response {
    let Some(authority) = state.ca.authority() else {
        return unavailable("no issuing CA is loaded");
    };
    match crl::current(&state.pool, &authority, unix_now()).await {
        Ok(Ok(crls)) => pem(crl::pem_bundle(&crls)),
        Ok(Err(missing)) => {
            tracing::warn!(
                missing = ?missing,
                "asked for the CRLs while an issuing CA has none yet"
            );
            unavailable("an issuing CA has no CRL yet")
        }
        Err(error) => {
            tracing::warn!(error = %error, "cannot read the CRLs");
            unavailable("the CRLs cannot be read now")
        }
    }
}

async fn chain_pem(State(state): State<AppState>) -> Response {
    match state.ca.authority() {
        Some(authority) => pem(authority.chain_pem()),
        None => unavailable("no issuing CA is loaded"),
    }
}

/// The adopted root CRL, or 404 while there is none. Not logged per request: gateways poll, and
/// the request log of `service_kit` already records each one.
async fn root_crl_pem(State(ca): State<CaState>) -> Response {
    match ca.root_crl() {
        Some(crl) => (
            [(header::CONTENT_TYPE, ROOT_CRL_CONTENT_TYPE)],
            crl.pem().to_owned(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            NO_ROOT_CRL,
        )
            .into_response(),
    }
}
