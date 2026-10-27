//! TrustIoT v2 ingest service: telemetry from gateways into ClickHouse, acknowledged only after
//! the batch is on disk (decision 8, rebuild checklist I1).
//!
//! The service is built in steps under Laysi/trustiot-v2#91. This release
//! (Laysi/trustiot-v2#640) is its skeleton: it starts, checks Postgres and ClickHouse for
//! readiness, and keeps the gateway binding projection the telemetry path takes every tenant
//! from. The telemetry path itself (MQTT, decoding, the writes and the acknowledgement) is
//! Laysi/trustiot-v2#641.
//!
//! - [`bindings`]: the consumer of `assets.gateway_bound` and the lookup of a gateway's tenant.
//! - [`clickhouse`] and [`transport`]: the ClickHouse client.
//! - [`schema`]: the Postgres migration files this release expects, which readiness checks and
//!   the migration Job applies.
//! - [`config`]: the settings.
//! - [`retention`]: the platform's raw and aggregate data retention, which the migration Job
//!   applies as the tables' TTL (rebuild checklist I2, Laysi/trustiot-v2#92,
//!   Laysi/trustiot-v2#141).
//! - [`rollup`]: the statements on the one-minute and one-hour aggregate tables: the settings
//!   every statement that writes a stream must carry ([`rollup::stream_settings`]) and the
//!   reconcile the ingest service must run after an I1 rewrite (no code writes or rewrites
//!   telemetry yet: the write path and the rewrite are Laysi/trustiot-v2#641, which takes over
//!   that contract), the backfill the migration Job runs, and the energy query of cumulative
//!   points.
//!
//! The crate also owns the ClickHouse schema in `migrations/clickhouse/`: the migration Job
//! (Laysi/trustiot-v2#60) applies it at deploy time, and runtime code never creates or alters
//! tables (decision 14; `tools/repo-check` rejects DDL in `services/*/src`). See `README.md` for
//! the tables, the row policy mechanism, the binding projection and the tests.

pub mod bindings;
pub mod clickhouse;
pub mod config;
pub mod retention;
pub mod rollup;
pub mod schema;
pub mod transport;

use std::time::Duration;

use axum::Router;
use contract::ServiceName;
use outbox::{Consumer, ConsumerConfig};
use service_kit::identity::ServiceIdentity;
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;

use crate::bindings::GatewayBoundHandler;
use crate::clickhouse::{Account, ClickHouse};
use crate::config::Config;

/// Connections of the platform role's pool: the consumer's reads of the assets service's events,
/// one read-only transaction at a time, one for readiness, and two for the lookups of the
/// telemetry path (Laysi/trustiot-v2#641, which may size it again with the chart's connection
/// budget in Laysi/trustiot-v2#642).
pub const PLATFORM_CONNECTIONS: u32 = 4;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`), builds the ClickHouse
/// client, and starts the consumer of `assets.gateway_bound`. Readiness passes while both roles
/// can open a checked session, the database has every migration file this release expects
/// ([`schema::SCHEMA`]), and ClickHouse answers the service's account. The service has no API:
/// it serves only service-kit's health endpoints.
///
/// # Errors
///
/// When the database settings would not verify the server, the ClickHouse CA file is unusable,
/// or the consumer's settings are invalid. Nothing is served then: the service exits with
/// code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let actor = ServiceIdentity::new(ServiceName::Ingest);
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

    let clickhouse = ClickHouse::new(
        &config.platform.clickhouse_url,
        config.platform.clickhouse_ca_file.as_deref(),
        Account::new(&config.clickhouse.username, &config.clickhouse.password),
    )?;
    context.health().add_check("clickhouse", move || {
        let clickhouse = clickhouse.clone();
        async move { clickhouse.ping().await }
    });

    let consumer = Consumer::new(
        &platform,
        background,
        ConsumerConfig::new(bindings::SUBSCRIPTION, bindings::SOURCE_SCHEMA)?,
        GatewayBoundHandler,
    );
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        // `run` retries everything but a configuration mistake, which it logs; the service keeps
        // serving then and the consumer's lag shows the problem.
        let _ = consumer.run(stop).await;
    });
    Ok(Router::new())
}
