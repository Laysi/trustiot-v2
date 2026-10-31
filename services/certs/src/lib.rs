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
//!   Secret, and the issuing CA certificates the root signed; reloaded when the files change,
//!   with the [`root_crl`] checked against that root (Laysi/trustiot-v2#830).
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
//! - [`bindings`]: which tenant each gateway is bound to and each tenant's status
//!   (Laysi/trustiot-v2#632), kept from the assets service's `assets.gateway_bound` by an outbox
//!   consumer and read across tenants as the platform role, for the renewal
//!   (Laysi/trustiot-v2#81) and the broker's permissions (Laysi/trustiot-v2#633).
//! - [`tenant_states`]: each tenant's state (Laysi/trustiot-v2#778), kept in the same table of
//!   [`bindings`] from the identity service's `identity.tenant_created` and
//!   `identity.tenant_state_changed` by a second outbox consumer; the renewal refuses every
//!   tenant that is not active, a tenant without a state included.
//! - [`renewal`]: the gateway certificate renewal (Laysi/trustiot-v2#81) on the public path
//!   `/certs/renew`: a challenge, the TPM signature over it, the gateway's binding and tenant
//!   status, rate limits, and the tenant event `certs.gateway_certificate_renewed`.
//! - [`dynsec`]: the MQTT broker's gateway permissions (Laysi/trustiot-v2#633), kept in line
//!   with the registrations, certificates and bindings by dynamic-security commands on the
//!   broker's service port.
//! - [`gateways`]: revoking a gateway's certificates (Laysi/trustiot-v2#636): the ledger, a new
//!   CRL of each issuing CA concerned, then the broker sync drops the gateway's permissions and
//!   its connection; the platform administrators' API and the function the suspension and
//!   unbinding call.
//! - [`ceremony`]: the offline root's certificates, for the `certs-ca` tool.
//! - [`schema`]: the migration files this release expects; [`config`]: the settings.
//!
//! No private key leaves the service: the issuing CA keys are read from the mounted Secret into
//! types that neither serialize nor print them, and no endpoint or log line carries one
//! (`tests/key_custody.rs`).

mod api;
pub mod authority;
pub mod bindings;
pub mod broker;
pub mod ceremony;
pub mod config;
pub mod crl;
pub mod dynsec;
pub mod gateways;
pub mod http;
pub mod issue;
pub mod monitor;
pub mod pem;
pub mod registrations;
pub mod renewal;
pub mod root_crl;
pub mod schema;
pub mod tenant_states;
pub mod workstations;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use contract::ServiceName;
use outbox::{Consumer, ConsumerConfig};
use service_kit::identity::{IdentityVerifier, PublishedKeys, ServiceIdentity};
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;
use utoipa::OpenApi;

use crate::authority::CaState;
use crate::bindings::{GatewayBoundHandler, Projection};
use crate::config::Config;
use crate::dynsec::SyncAfterBinding;
use crate::tenant_states::TenantStateHandler;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections of the platform role's pool per replica, shared by short transactions: factory
/// registrations, one transaction each at the pace of a production line; the polls of the
/// consumers of the assets service's and the identity service's events and their lag
/// measurements, one read-only transaction at a time each; and the reads of
/// [`bindings::Projection`]. The renewal ([`renewal`]) takes at most
/// `renewal.max_concurrent` of them at a time (its reads and refusal counts, one transaction at a
/// time per request; the setting is at most one less than this), and its report of ended refusal
/// windows one now and then. A transaction that cannot get a connection within
/// [`ACQUIRE_TIMEOUT`] fails: a registration or a renewal answers 503, and the consumer retries.
pub const PLATFORM_CONNECTIONS: u32 = 4;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(
        workstations::issue_certificate,
        workstations::revoke_certificate,
        registrations::register_gateway,
        gateways::revoke_gateway
    ),
    tags((name = "certs", description = "Workstation certificates, factory registration and gateway revocation from the issuing CA (services/certs)."))
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
/// role's pool lazily for [`registrations`], the consumer of the assets service's events
/// ([`bindings`]) and the consumer of the identity service's events ([`tenant_states`]), starts
/// the broker's gateway permission sync when a broker is configured ([`dynsec`]; registrations,
/// renewals and applied bindings ask it to sync their gateway), starts both consumers, serves the endpoints of [`http`], [`workstations`], [`registrations`] and
/// [`renewal`] (which also reports its refusal counts), and serves [`broker`] on its own port
/// (`broker.listen_addr`). Readiness passes while the role can open a checked
/// session, the service has a usable set of public keys, and the database has every migration
/// file this release expects ([`schema::SCHEMA`]); whether an issuing CA can sign is reported by
/// the monitor, not by readiness ([`authority::CaState::watch`]).
///
/// # Errors
///
/// When the database settings would not verify the server, the consumer's settings are invalid,
/// the MQTT client cannot be started, or the broker port is taken. Nothing is served then: the service exits with code 1.
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
    // served while the platform role cannot log in; a registration answers 503 then, and the
    // consumers of the assets and identity services' events retry (their lag shows it, and the
    // renewal answers 503).
    let platform = PlatformDb::connect_lazy(
        config.platform_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(PLATFORM_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT),
    )?;
    let ping = db.clone();
    context.health().add_check("postgres", move || {
        let db = ping.clone();
        async move { db.ping().await }
    });
    MigrationCheck::new(&schema::SCHEMA, db.pool().clone()).register(context.health());

    // The broker's gateway permissions (Laysi/trustiot-v2#633): not part of readiness either,
    // since the broker may be down while the CRLs must be served; a broker that cannot be
    // reached only delays the sync.
    let broker_sync = dynsec::start(
        &context,
        config.mqtt.as_ref(),
        &config.dynsec,
        Projection::new(platform.clone()),
        db.pool().clone(),
    )
    .map_err(|error| format!("the broker permission sync: {error}"))?;

    // `run` retries everything but a configuration mistake, which it logs; the service keeps
    // serving then and the consumer's lag shows the problem.
    let bindings_consumer = Consumer::new(
        &platform,
        background.clone(),
        ConsumerConfig::new(bindings::SUBSCRIPTION, bindings::SOURCE_SCHEMA)?,
        SyncAfterBinding::new(GatewayBoundHandler, broker_sync.clone()),
    );
    // Each tenant's state from the identity service's events (Laysi/trustiot-v2#778), read from
    // the start of its log. It changes no broker permission (Laysi/trustiot-v2#108 decides
    // that), so it does not ask the broker sync for anything.
    let tenants_consumer = Consumer::new(
        &platform,
        background.clone(),
        ConsumerConfig::new(tenant_states::SUBSCRIPTION, tenant_states::SOURCE_SCHEMA)?,
        TenantStateHandler,
    );
    // The renewal refuses while either consumer's lag is unknown or too high.
    let bindings_lag = bindings_consumer.clone();
    let tenants_lag = tenants_consumer.clone();
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        let _ = bindings_consumer.run(stop).await;
    });
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        let _ = tenants_consumer.run(stop).await;
    });

    let ca = CaState::watch(&context, config.ca.clone());
    crl::start(
        &context,
        db.pool().clone(),
        background.clone(),
        ca.clone(),
        config.crl.clone(),
    );
    let renewal = renewal::routes(renewal::Parts {
        pool: db.pool().clone(),
        background: background.clone(),
        platform: platform.clone(),
        ca: ca.clone(),
        lag: Arc::new(move || renewal::slowest([bindings_lag.lag(), tenants_lag.lag()])),
        limits: config.renewal.clone(),
        validity: config.gateway.validity,
        broker_sync: broker_sync.clone(),
    });
    renewal::start_reporting(
        &context,
        platform.clone(),
        config.renewal.refusal_flush_interval,
    );
    monitor::start(&context, background.clone(), ca.clone(), config.crl.clone());

    let keys = PublishedKeys::watch(
        &context,
        config.platform.internal_identity_keys_file.clone(),
    )
    .await;
    let verifier = IdentityVerifier::new(ServiceName::Certs, keys);
    // A revocation gives its issuing CA a new CRL before it answers (Laysi/trustiot-v2#635),
    // within half the handler timeout so the response goes out even when that waits.
    let crls = crl::Reissuer::new(
        background,
        ca.clone(),
        config.crl.clone(),
        crl::REISSUE_TIMEOUT.min(context.request_timeout() / 2),
    );
    // Decision 6's three steps for a gateway (Laysi/trustiot-v2#636): the ledger, a new CRL
    // from the same reissuer (they share its turns), then the broker sync.
    let gateway_revocation =
        gateways::GatewayRevocation::new(db.pool().clone(), crls.clone(), broker_sync.clone());
    let api = workstations::routes(
        verifier.clone(),
        db.pool().clone(),
        ca.clone(),
        config.workstation.validity,
        crls,
    );
    let gateways = gateways::routes(verifier.clone(), gateway_revocation);
    let registrations = registrations::routes(
        verifier,
        platform,
        ca.clone(),
        config.gateway.validity,
        broker_sync,
    );
    serve_broker(&context, db.pool().clone(), ca.clone()).await?;
    Ok(http::routes(ca, db.pool().clone())
        .merge(api)
        .merge(registrations)
        .merge(gateways)
        .merge(renewal))
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
