//! TrustIoT v2 gateway operations service: each gateway's desired configuration, versioned in
//! Postgres (decision 7, rebuild checklist E1).
//!
//! The service is built in steps under Laysi/trustiot-v2#93. It consumes the assets service's
//! model, device and binding events into a per-tenant projection and stores a new configuration
//! version for a gateway whenever its content changes (Laysi/trustiot-v2#719), publishes each
//! gateway's latest version retained over MQTT and records the gateways' apply reports
//! (Laysi/trustiot-v2#720). The image and the chart are Laysi/trustiot-v2#721.
//!
//! - [`projection`]: the consumer's handler, the projection and the versions it stores.
//! - [`delivery`]: the MQTT delivery, its coalescing, the republish and the apply reports.
//! - [`generate`]: one gateway's configuration from the projection's rows.
//! - [`desired`]: the read interface of the stored configurations for the delivery.
//! - [`schema`]: the Postgres migration files this release expects, which readiness checks and
//!   the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` for the tables, the rules from events to configurations, the versions, the
//! errors and the tests.

pub mod config;
pub mod delivery;
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
use crate::delivery::records::Records;
use crate::delivery::{Delivery, Parts};
use crate::desired::DesiredConfigs;
use crate::projection::AssetsEventsHandler;

/// Connections of the platform role's pool: the consumer's reads of the assets service's events
/// and its reads of gateways' bindings and versions, one at a time, one for readiness, and
/// [`delivery::CONCURRENCY`] (two) for the delivery's reads.
pub const PLATFORM_CONNECTIONS: u32 = 4;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`), starts the MQTT
/// delivery and the consumer of the assets service's events, which announces every version it
/// stores to the delivery. Readiness passes while both roles can open a checked session, the
/// database has every migration file this release expects ([`schema::SCHEMA`]), and the broker
/// connection is up with the apply report subscription granted (`mqtt`). The service has no
/// API: it serves only service-kit's health endpoints.
///
/// # Errors
///
/// When the database settings would not verify the server, the broker settings or the password
/// file are unusable, or the consumer's settings are invalid. Nothing is served then: the service
/// exits with code 1.
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

    let delivery = Delivery::start(Parts {
        mqtt: config.mqtt.clone(),
        configs: DesiredConfigs::new(&platform),
        records: Records::new(background.clone()),
        locks: db.pool().clone(),
        throttle: config.delivery.throttle,
    })?;
    let handle = delivery.handle();
    let connection = handle.connection();
    context.health().add_check("mqtt", move || {
        let result = connection.check();
        async move { result }
    });

    let consumer = Consumer::new(
        &platform,
        background,
        ConsumerConfig::new(projection::SUBSCRIPTION, projection::SOURCE_SCHEMA)?,
        AssetsEventsHandler::new(&platform).with_delivery(handle),
    );
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        // `run` retries everything but a configuration mistake, which it logs; the service keeps
        // serving then and the consumer's lag shows the problem.
        let _ = consumer.run(stop).await;
    });
    let stop = context.shutdown().stopping_token();
    context.spawn(delivery.run(stop));
    Ok(Router::new())
}
