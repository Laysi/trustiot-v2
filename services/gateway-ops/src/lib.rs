//! TrustIoT v2 gateway operations service: each gateway's desired configuration, versioned in
//! Postgres (decision 7, rebuild checklist E1).
//!
//! The service is built in steps under Laysi/trustiot-v2#93. This release
//! (Laysi/trustiot-v2#719) consumes the assets service's model, device and binding events into a
//! per-tenant projection and stores a new configuration version for a gateway whenever its
//! content changes. The MQTT delivery of the stored versions and the gateways' apply reports are
//! Laysi/trustiot-v2#720; the image and the chart Laysi/trustiot-v2#721.
//!
//! - [`projection`]: the consumer's handler, the projection and the versions it stores.
//! - [`generate`]: one gateway's configuration from the projection's rows.
//! - [`desired`]: the read interface of the stored configurations for the delivery.
//! - [`schema`]: the Postgres migration files this release expects, which readiness checks and
//!   the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` for the tables, the rules from events to configurations, the versions, the
//! errors and the tests.

pub mod config;
pub mod desired;
pub mod generate;
pub mod projection;
pub mod schema;

use std::time::Duration;

use axum::Router;
use contract::ServiceName;
use outbox::{Consumer, ConsumerConfig};
use service_kit::identity::ServiceIdentity;
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;

use crate::config::Config;
use crate::projection::AssetsEventsHandler;

/// Connections of the platform role's pool: the consumer's reads of the assets service's events
/// and its reads of gateways' bindings and versions, one at a time, one for readiness, and two
/// for the delivery's reads (Laysi/trustiot-v2#720, which may size it again).
pub const PLATFORM_CONNECTIONS: u32 = 4;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`) and starts the consumer
/// of the assets service's events. Readiness passes while both roles can open a checked session
/// and the database has every migration file this release expects ([`schema::SCHEMA`]). The
/// service has no API: it serves only service-kit's health endpoints.
///
/// # Errors
///
/// When the database settings would not verify the server or the consumer's settings are
/// invalid. Nothing is served then: the service exits with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let actor = ServiceIdentity::new(ServiceName::GatewayOps);
    let (db, background) = TenantDb::connect_lazy_with_background(
        config.service_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(config.database.max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT),
        actor,
    )?;
    let platform = PlatformDb::connect_lazy(
        config.platform_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(PLATFORM_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT),
    )?;
    let (ping_db, ping_platform) = (db.clone(), platform.clone());
    context.health().add_check("postgres", move || {
        let (db, platform) = (ping_db.clone(), ping_platform.clone());
        async move {
            db.ping().await?;
            platform.ping().await
        }
    });
    MigrationCheck::new(&schema::SCHEMA, db.pool().clone()).register(context.health());

    let consumer = Consumer::new(
        &platform,
        background,
        ConsumerConfig::new(projection::SUBSCRIPTION, projection::SOURCE_SCHEMA)?,
        AssetsEventsHandler::new(&platform),
    );
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        // `run` retries everything but a configuration mistake, which it logs; the service keeps
        // serving then and the consumer's lag shows the problem.
        let _ = consumer.run(stop).await;
    });
    Ok(Router::new())
}
