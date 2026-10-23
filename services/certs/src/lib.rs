//! The certs service (decisions 6 and 12; docs/architecture/deployment.md): the online issuing CA
//! under an offline root.
//!
//! Laysi/trustiot-v2#78: its own Postgres schema, the CA hierarchy and private-key custody
//! (`README.md`, "CA 階層與私鑰保管"), gateway certificates from CSRs with the profile the CA
//! decides, and each issuing CA's CRL regenerated on a schedule with an alert before it lapses.
//! The factory registration (Laysi/trustiot-v2#79), the renewal endpoint (Laysi/trustiot-v2#81)
//! and revocation (Laysi/trustiot-v2#82) build on [`issue`] and [`crl`].
//!
//! Laysi/trustiot-v2#534: production workstation certificates (`README.md`, "工作站憑證"), from
//! the same issuing CA with a profile the MQTT broker refuses, issued and revoked by platform
//! administrators through [`workstations`]. An issuing CA signs only once it has a CRL
//! ([`authority::Authority::signer`]).
//!
//! Laysi/trustiot-v2#557: the MQTT broker's TLS server certificate (`README.md`, "broker 伺服器
//! 憑證"), from the same issuing CA with a server-only profile, issued through [`broker`] on a
//! port of its own that the mesh opens to the broker alone.
//!
//! - [`authority`]: the root certificate, the issuing CA keys the pod's init container keeps in a
//!   Secret, and the issuing CA certificates the root signed; reloaded when the files change.
//! - [`issue`]: gateway and workstation certificates from CSRs, and the ledger of what was
//!   signed.
//! - [`crl`]: the CRL job and the CRLs it keeps; [`monitor`]: the gauges and the alert.
//! - [`http`]: the CRLs and the CA chain for the MQTT broker.
//! - [`workstations`]: the API platform administrators issue and revoke workstation
//!   certificates with.
//! - [`registrations`]: the factory registration (Laysi/trustiot-v2#79), which a production
//!   workstation calls with a platform administrator's session and its own signature, written
//!   as the platform role with the platform-wide `certs.gateway_registered` event.
//! - [`api`]: what the client API's handlers share.
//! - [`broker`]: the MQTT broker's server certificate, on its own port.
//! - [`ceremony`]: the offline root's certificates, for the `certs-ca` tool.
//! - [`schema`]: the migration files this release expects; [`config`]: the settings.
//!
//! No private key leaves the service: the issuing CA keys are read from the mounted Secret into
//! types that neither serialize nor print them, and no endpoint or log line carries one
//! (`tests/key_custody.rs`).

mod api;
pub mod authority;
pub mod broker;
pub mod ceremony;
pub mod config;
pub mod crl;
pub mod http;
pub mod issue;
pub mod monitor;
pub mod pem;
pub mod registrations;
pub mod schema;
pub mod workstations;

use std::time::Duration;

use axum::Router;
use contract::ServiceName;
use service_kit::identity::{IdentityVerifier, PublishedKeys, ServiceIdentity};
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;
use utoipa::OpenApi;

use crate::authority::CaState;
use crate::config::Config;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections of the platform role's pool per replica: factory registrations, one
/// transaction each, at the pace of a production line.
pub const REGISTRATION_CONNECTIONS: u32 = 4;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(
        workstations::issue_certificate,
        workstations::revoke_certificate,
        registrations::register_gateway
    ),
    tags((name = "certs", description = "Workstation certificates and factory registration from the issuing CA (services/certs)."))
)]
struct ApiDoc;

/// The service's OpenAPI document. The `/internal/certs/v1/` endpoints of [`http`] are not in
/// it: the ingress never routes them.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: opens the service role's pool lazily (with the
/// session check of `crates/service-kit`), loads the CA files and follows them, starts the CRL
/// job and the monitor, follows the identity service's public keys file, opens the platform
/// role's pool lazily for [`registrations`], serves the endpoints of [`http`],
/// [`workstations`] and [`registrations`], and serves [`broker`] on its own port
/// (`broker.listen_addr`). Readiness passes while the role can open a checked
/// session, the service has a usable set of public keys, and the database has every migration
/// file this release expects ([`schema::SCHEMA`]); whether an issuing CA can sign is reported by
/// the monitor, not by readiness ([`authority::CaState::watch`]).
///
/// # Errors
///
/// When the database settings would not verify the server, or the broker port is taken. Nothing
/// is served then: the service exits with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let (db, background) = TenantDb::connect_lazy_with_background(
        config.service_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(config.database.max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT),
        ServiceIdentity::new(ServiceName::Certs),
    )?;
    // Not part of readiness: the CRLs and the CA chain the MQTT broker reads must keep being
    // served while the platform role cannot log in; a registration answers 503 then.
    let platform = PlatformDb::connect_lazy(
        config.platform_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(REGISTRATION_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT),
    )?;
    let ping = db.clone();
    context.health().add_check("postgres", move || {
        let db = ping.clone();
        async move { db.ping().await }
    });
    MigrationCheck::new(&schema::SCHEMA, db.pool().clone()).register(context.health());

    let ca = CaState::watch(&context, config.ca.clone());
    crl::start(
        &context,
        db.pool().clone(),
        background.clone(),
        ca.clone(),
        config.crl.clone(),
    );
    monitor::start(&context, background, ca.clone(), config.crl.clone());

    let keys = PublishedKeys::watch(
        &context,
        config.platform.internal_identity_keys_file.clone(),
    )
    .await;
    let verifier = IdentityVerifier::new(ServiceName::Certs, keys);
    let api = workstations::routes(
        verifier.clone(),
        db.pool().clone(),
        ca.clone(),
        config.workstation.validity,
    );
    let registrations =
        registrations::routes(verifier, platform, ca.clone(), config.gateway.validity);
    serve_broker(&context, db.pool().clone(), ca.clone()).await?;
    Ok(http::routes(ca, db.pool().clone())
        .merge(api)
        .merge(registrations))
}

/// Serves the MQTT broker's certificate issuance on its own port (`broker.listen_addr`), apart
/// from the API port, so the mesh can let the broker alone reach it (deploy/chart/README.md,
/// "呼叫矩陣"). `Context::serve_port` gives it the API port's layers (handler timeout, panic
/// recovery, request logs and trace context) and drain.
///
/// # Errors
///
/// When the port is taken or the names are invalid (the configuration check refuses those
/// first): the service does not start.
async fn serve_broker(
    context: &Context<Config>,
    pool: sqlx::PgPool,
    ca: CaState,
) -> Result<(), BoxError> {
    let config = &context.config().broker;
    let names = config
        .names()
        .map_err(|problem| format!("broker.service_name: {problem}"))?;
    let bound = context
        .serve_port(
            config.listen_addr,
            broker::routes(pool, ca, names.clone(), config.validity),
        )
        .await
        .map_err(|error| format!("broker certificates, broker.listen_addr: {error}"))?;
    tracing::info!(
        address = %bound,
        names = %names.as_slice().join(","),
        "serving the MQTT broker's certificate issuance"
    );
    Ok(())
}
