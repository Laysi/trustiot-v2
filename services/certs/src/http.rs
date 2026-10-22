//! The service's HTTP endpoints. Only public data, under `/internal`, which the ingress never
//! routes (decision 27); inside the cluster the mesh lets only the callers `mesh.calls` names
//! reach the service.
//!
//! - `GET /internal/certs/v1/crl.pem`: the current CRL of every issuing CA whose certificate
//!   has not expired, PEM, for the MQTT broker's `crlfile` (Laysi/trustiot-v2#84). 503 while one
//!   of them has no CRL yet, so a fetcher keeps the file it has.
//! - `GET /internal/certs/v1/ca.pem`: the root certificate, then every issuing CA certificate,
//!   PEM. 503 while no CA is loaded.
//!
//! Nothing here reads, returns or logs a private key: the handlers see the CRLs in the database
//! and the certificates of [`crate::authority::Authority`].

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sqlx::PgPool;

use crate::authority::{CaState, unix_now};
use crate::crl;

/// The path of the CRL bundle.
pub const CRL_PATH: &str = "/internal/certs/v1/crl.pem";

/// The path of the CA chain.
pub const CHAIN_PATH: &str = "/internal/certs/v1/ca.pem";

/// The media type of PEM files.
const PEM_TYPE: &str = "application/x-pem-file";

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
