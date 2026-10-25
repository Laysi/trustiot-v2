//! The telemetry-query service (decisions 4 and 14; docs/architecture/deployment.md): each
//! tenant's read-only ClickHouse account, and the history API of device charts.
//!
//! Laysi/trustiot-v2#119: the consumer of the identity service's `identity.tenant_created`
//! creates the tenant's ClickHouse account, and the API answers a device's series and latest
//! values as that account, so ClickHouse's row policy, settings profile and quota apply to every
//! query.
//!
//! - [`accounts`]: the tenant accounts and the consumer.
//! - [`history`]: the API.
//! - [`semantic`]: the semantic API (statistics and usage of several data points,
//!   Laysi/trustiot-v2#142), which the assistant's telemetry tools call.
//! - [`clickhouse`] and [`transport`]: the ClickHouse client.
//! - [`schema`]: the migration files this release expects, which readiness checks
//!   (Laysi/trustiot-v2#402) and the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` in this crate for the accounts, the API, the limits and the tests.

pub mod accounts;
pub mod clickhouse;
pub mod config;
pub mod history;
pub mod schema;
pub mod semantic;
pub mod transport;

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

use crate::accounts::{Accounts, TenantCreatedHandler};
use crate::clickhouse::ClickHouse;
use crate::config::Config;

/// Connections of the platform role's pool: the consumer's reads of the identity service's
/// events, one read-only transaction at a time, plus one for readiness.
pub const EVENT_READER_CONNECTIONS: u32 = 2;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The consumer's subscription to the identity service's events.
pub const SUBSCRIPTION: &str = "telemetry-query.identity-events";

/// The schema of the identity service's outbox.
pub const SOURCE_SCHEMA: &str = "identity";

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(
        history::get_series,
        history::get_latest,
        semantic::post_statistics,
        semantic::post_usage
    ),
    tags((name = "telemetry", description = "Device telemetry history (services/telemetry-query)."))
)]
struct ApiDoc;

/// The service's OpenAPI document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`), starts the consumer of
/// `identity.tenant_created`, which creates tenant accounts, follows the identity service's
/// public keys file, and serves the API. Readiness passes while both roles can open a checked
/// session, the service has a usable set of public keys, and the database has every migration
/// file this release expects ([`schema::SCHEMA`]). ClickHouse is not a readiness check: the API
/// answers 503 while it is down, and the consumer retries.
///
/// # Errors
///
/// When the database settings would not verify the server, the ClickHouse URL or CA file is
/// unusable, or the consumer's settings are invalid. Nothing is served then: the service exits
/// with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let actor = ServiceIdentity::new(ServiceName::TelemetryQuery);
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

    let clickhouse = ClickHouse::new(
        &config.platform.clickhouse_url,
        config.platform.clickhouse_ca_file.as_deref(),
    )?;
    let accounts = Accounts::new(
        clickhouse,
        &config.clickhouse.username,
        &config.clickhouse.password,
    );

    let consumer = Consumer::new(
        &events,
        background,
        ConsumerConfig::new(SUBSCRIPTION, SOURCE_SCHEMA)?,
        TenantCreatedHandler::new(accounts.clone()),
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
    let verifier = IdentityVerifier::new(ServiceName::TelemetryQuery, keys);
    let semantic = semantic::routes(history::Routes::new(
        verifier.clone(),
        db.clone(),
        accounts.clone(),
    ));
    Ok(history::routes(verifier, db, accounts).merge(semantic))
}
