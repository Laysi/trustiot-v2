//! The assets service (decision 4; docs/architecture/deployment.md): tenants' sites, devices and
//! data points.
//!
//! MS0 skeleton (Laysi/trustiot-v2#65): its own Postgres schema under row-level security, the
//! consumer of the identity service's `identity.tenant_created`, which gives every new tenant its
//! row here, and one tenant-scoped example API that needs an internal identity token.
//!
//! - [`tenants`]: the consumer and the API.
//! - [`schema`]: the migration files this release expects, which readiness checks
//!   (Laysi/trustiot-v2#402) and the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` in this crate for the schema, the event, the API and the tests.

pub mod config;
pub mod schema;
pub mod tenants;

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

use crate::config::Config;
use crate::tenants::TenantCreatedHandler;

/// Connections of the platform role's pool: the consumer's reads of the identity service's
/// events, one read-only transaction at a time, plus one for readiness.
pub const EVENT_READER_CONNECTIONS: u32 = 2;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(tenants::get_tenant),
    tags((name = "assets", description = "Sites, devices and data points (services/assets)."))
)]
struct ApiDoc;

/// The service's OpenAPI document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`), starts the consumer of
/// `identity.tenant_created`, follows the identity service's public keys file, and serves the
/// API. Readiness passes while both roles can open a checked session, the service has a usable set
/// of public keys, and the database has every migration file this release expects
/// ([`schema::SCHEMA`]).
///
/// # Errors
///
/// When the database settings would not verify the server, or the consumer's settings are
/// invalid. Nothing is served then: the service exits with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let actor = ServiceIdentity::new(ServiceName::Assets);
    let (db, background) = TenantDb::connect_lazy_with_background(
        config.service_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(config.database.max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT),
        actor,
    )?;
    let events = PlatformDb::connect_lazy(
        config.platform_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(EVENT_READER_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT),
    )?;
    let (ping_db, ping_events) = (db.clone(), events.clone());
    context.health().add_check("postgres", move || {
        let (db, events) = (ping_db.clone(), ping_events.clone());
        async move {
            db.ping().await?;
            events.ping().await
        }
    });
    MigrationCheck::new(&schema::SCHEMA, db.pool().clone()).register(context.health());

    let consumer = Consumer::new(
        &events,
        background,
        ConsumerConfig::new(tenants::SUBSCRIPTION, tenants::SOURCE_SCHEMA)?,
        TenantCreatedHandler,
    );
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        // `run` retries everything but a configuration mistake, which it logs; the service keeps
        // serving then and the consumer's lag shows the problem.
        let _ = consumer.run(stop).await;
    });

    let keys = PublishedKeys::watch(
        &context,
        config.platform.internal_identity_keys_file.clone(),
    )
    .await;
    let verifier = IdentityVerifier::new(ServiceName::Assets, keys);
    Ok(tenants::routes(verifier, db))
}
