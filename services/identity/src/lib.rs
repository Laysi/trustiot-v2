//! The identity service (decisions 13 and 27; docs/architecture/deployment.md, "身分與租戶").
//!
//! Server-side sessions in its own Postgres schema (Laysi/trustiot-v2#61), and the ingress
//! gateway's ext_authz check, which turns a session cookie into a short-lived internal identity
//! token signed for the service the request is routed to. Users log in with a code sent to their
//! email address, the only way to create a session (Laysi/trustiot-v2#103); they log out of one
//! session or of all. Platform administrators create tenants (Laysi/trustiot-v2#65), which other
//! services learn of from the `identity.tenant_created` event. The service also keeps the
//! platform's event layer and operation log (Laysi/trustiot-v2#130, decision 34): it records
//! every producer's events once and serves them to tenant administrators and the back office.
//!
//! - [`session`]: the cookie, its hash, its lifetime, and the Postgres session store.
//! - [`login`]: `POST /auth/login`, the login codes and the sessions they create.
//! - [`logout`]: revoking one session or all of a user's.
//! - [`mail`]: the SMTP client that sends the codes.
//! - [`issuer`]: the signing keys the cluster generates, which one signs when, and the token
//!   format `crates/service-kit` verifies.
//! - [`audience`]: which service a request goes to, from the chart's cookie routes.
//! - [`ext_authz`]: the check endpoint.
//! - [`csrf`]: the check's refusal of cross-site requests, before the session is looked up.
//! - [`exchange`]: the token exchange, for services that call another one on behalf of the
//!   request's user (Laysi/trustiot-v2#106).
//! - [`tenants`]: creating tenants, and the event that announces them.
//! - [`event_log`]: the event layer: recording every producer's events, and reading them.
//! - [`schema`]: the migration files this release expects, which readiness checks
//!   (Laysi/trustiot-v2#402) and the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` in this crate for the contract with the ingress gateway, the schema, key
//! rotation and the tests.

pub mod audience;
pub mod config;
pub mod csrf;
pub mod event_log;
pub mod exchange;
pub mod ext_authz;
pub mod issuer;
pub mod login;
pub mod logout;
pub mod mail;
pub mod schema;
pub mod session;
pub mod tenants;

use axum::Router;
use contract::ServiceName;
use outbox::{Consumer, ConsumerConfig};
use service_kit::identity::{ExchangeVerifier, IdentityVerifier, PublishedKeys, ServiceIdentity};
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;
use utoipa::OpenApi;

use crate::config::Config;
use crate::event_log::{EventLog, EventLogRecorder};
use crate::exchange::Exchange;
use crate::ext_authz::ExtAuthz;
use crate::issuer::{Issuer, IssuerSettings};
use crate::login::{Login, LoginStore};
use crate::mail::Mailer;
use crate::session::{PgSessionStore, SessionStore};
use crate::tenants::TenantStore;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`). The
/// ext_authz check and the token exchange under `/internal` are not part of it: clients never
/// reach them (the ingress gateway refuses `/internal*`), and the exchange's contract is
/// `contract::api::identity::ExchangeToken`.
#[derive(OpenApi)]
#[openapi(
    paths(
        tenants::create_tenant,
        login::log_in,
        logout::log_out,
        logout::log_out_everywhere,
        event_log::tenant_event_log,
        event_log::platform_tenant_event_log,
        event_log::platform_event_log
    ),
    tags((name = "identity", description = "Sessions, users and tenants (services/identity)."))
)]
struct ApiDoc;

/// The service's OpenAPI document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: starts following the public keys file and the
/// signing keys file ([`Issuer::watch`]), starts keeping the Postgres connections open (with the
/// session check of `crates/service-kit`), starts the worker that mails login codes, starts
/// recording the event log ([`event_log`]), serves the token exchange on its own port
/// ([`serve_exchange`]), and returns the ext_authz check, login, logout, the tenant API and the
/// event log API for the API port. Readiness passes once the connections are open
/// (`PgSessionStore::keep_warm`), the public keys have arrived, a key may sign, and the database
/// has every migration file this release expects ([`schema::SCHEMA`]).
///
/// # Errors
///
/// When the database settings would not verify the server, or an event log subscription name
/// is invalid. Nothing is served then: the service exits with code 1. Missing or unusable key
/// files do not stop it: it starts unready and becomes ready when they arrive, as on a fresh
/// install. Without `TRUSTIOT_SMTP_HOST`, or with SMTP settings it cannot use (logged at ERROR),
/// it runs, but cannot send login codes: the ext_authz check that every request with a cookie
/// needs must not depend on the relay.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let platform = &config.platform;
    let mailer = match Mailer::from_platform(platform) {
        Ok(Some(mailer)) => Some(mailer),
        Ok(None) => {
            tracing::warn!(
                "TRUSTIOT_SMTP_HOST is not set (the chart's smtp.host): nobody can get a login \
                 code"
            );
            None
        }
        Err(error) => {
            tracing::error!(
                %error,
                "the SMTP settings cannot be used: nobody can get a login code until they are \
                 fixed and the service restarts"
            );
            None
        }
    };
    let published =
        PublishedKeys::watch(&context, platform.internal_identity_keys_file.clone()).await;
    let settings = IssuerSettings {
        signing_keys_file: platform.identity_signing_keys_file.clone(),
        revoke_before: platform.identity_revoke_before_generation,
        propagation_seconds: platform.identity_key_propagation_seconds,
        max_key_age_seconds: platform.identity_max_key_age_seconds,
    };
    let issuer = Issuer::watch(&context, &settings, published.clone()).await;
    tracing::info!(
        routes = platform.ingress_routes.len(),
        revoke_before = settings.revoke_before,
        "serving the ext_authz check"
    );
    let store = PgSessionStore::connect_lazy(
        config.postgres().connect_options(),
        config.database.max_connections,
    )?;
    context.spawn(store.clone().keep_warm(context.shutdown().clone()));
    let ping = store.clone();
    context.health().add_check("postgres", move || {
        let store = ping.clone();
        async move { store.ping().await }
    });
    MigrationCheck::new(&schema::SCHEMA, store.pool().clone()).register(context.health());
    let tenants = TenantStore::connect_lazy(config.postgres().connect_options())?;
    let log = start_event_log(&context)?;
    // Requests to the service's own API come through the ingress gateway like any other, with a
    // token the ext_authz check signed for `identity`.
    let verifier = IdentityVerifier::new(ServiceName::Identity, published.clone());
    // Services that call another one on behalf of a user exchange the request's token here, along
    // the deployment's call list (Laysi/trustiot-v2#106), on a port of its own.
    let exchange = Exchange::new(
        ExchangeVerifier::new(published),
        issuer.clone(),
        platform.on_behalf_calls.clone(),
    );
    serve_exchange(&context, exchange).await?;
    // Two pools, so a flood of `send_code` never takes the connections `verify_code` and the
    // logouts need (README, "連線池").
    let logins =
        LoginStore::connect_lazy(config.postgres().connect_options(), login::POOL_CONNECTIONS)?;
    let sends = LoginStore::connect_lazy(
        config.postgres().connect_options(),
        login::SEND_POOL_CONNECTIONS,
    )?;
    context.spawn(sends.clone().clean_up(context.shutdown().clone()));
    let (login, worker) = Login::new(logins.clone(), sends, mailer, issuer.clone());
    if let Some(worker) = worker {
        context.spawn(worker.run(context.shutdown().clone()));
    }
    let ext_authz = ExtAuthz::new(store, issuer, platform.ingress_routes.clone());
    Ok(ext_authz::routes(ext_authz)
        .merge(login::routes(login))
        .merge(logout::routes(verifier.clone(), logins))
        .merge(tenants::routes(verifier.clone(), tenants))
        .merge(event_log::routes(verifier, log)))
}

/// Opens the event log's pools lazily and starts one consumer per producer in
/// [`schema::CONSUMES`], each recording that producer's events. `None`, with a WARN, when the
/// platform role has no credentials ([`Config::platform_role`]).
///
/// Neither pool is part of readiness: the ext_authz check every request passes through must not
/// fail because the platform role cannot log in, or the event log fell behind. A recorder that
/// cannot work retries and its consumer's lag grows (`outbox.consumer.lag.age`, crates/outbox),
/// which is what alerts; the event log's API answers 503 while its pools cannot connect.
fn start_event_log(context: &Context<Config>) -> Result<Option<EventLog>, BoxError> {
    let config = context.config();
    let Some(platform_role) = config.platform_role() else {
        tracing::warn!(
            "the event log is off: the platform role has no credentials \
             (TRUSTIOT__DATABASE__PLATFORM_USERNAME, TRUSTIOT__DATABASE__PLATFORM_PASSWORD)"
        );
        return Ok(None);
    };
    let pool = |connections| {
        PgPoolOptions::new()
            .max_connections(connections)
            .min_connections(0)
            .acquire_timeout(event_log::ACQUIRE_TIMEOUT)
    };
    let (db, work): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        config.postgres().connect_options(),
        pool(event_log::SERVICE_CONNECTIONS),
        ServiceIdentity::new(ServiceName::Identity),
    )?;
    let platform = PlatformDb::connect_lazy(
        platform_role.connect_options(),
        pool(event_log::platform_connections()),
    )?;
    for &source in schema::CONSUMES {
        let consumer = Consumer::new(
            &platform,
            work.clone(),
            ConsumerConfig::new(event_log::subscription(source), source)?,
            EventLogRecorder::new(source),
        );
        let stop = context.shutdown().stopping_token();
        context.spawn(async move {
            // `run` retries everything but a configuration mistake, which it logs; the service
            // keeps serving then and the consumer's lag shows the problem.
            let _ = consumer.run(stop).await;
        });
    }
    Ok(Some(EventLog::new(db, platform)))
}

/// Serves the token exchange on its own port (`exchange.listen_addr`), apart from the API port,
/// so the mesh can grant the services that call on behalf of users this port and nothing else
/// (deploy/chart/README.md, "呼叫矩陣"). `Context::serve_port` gives it the API port's layers
/// (handler timeout, panic recovery, request logs and trace context) and drain.
///
/// # Errors
///
/// When the port is taken: the service does not start.
async fn serve_exchange(context: &Context<Config>, exchange: Exchange) -> Result<(), BoxError> {
    let address = context.config().exchange.listen_addr;
    let bound = context
        .serve_port(address, exchange::routes(exchange))
        .await
        .map_err(|error| format!("token exchange, exchange.listen_addr: {error}"))?;
    tracing::info!(
        address = %bound,
        calls = context.config().platform.on_behalf_calls.len(),
        "serving the token exchange"
    );
    Ok(())
}
