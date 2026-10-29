//! TrustIoT v2 gateway operations service: each gateway's desired configuration, versioned in
//! Postgres (decision 7, rebuild checklist E1).
//!
//! The service is built in steps under Laysi/trustiot-v2#93. It consumes the assets service's
//! model, device and binding events into a per-tenant projection and stores a new configuration
//! version for a gateway whenever its content changes (Laysi/trustiot-v2#719), publishes each
//! gateway's latest version retained over MQTT and records the gateways' apply reports
//! (Laysi/trustiot-v2#720). The image and the chart are Laysi/trustiot-v2#721. It follows whether
//! each gateway is connected and lists a tenant's gateways with it (Laysi/trustiot-v2#112).
//!
//! - [`projection`]: the consumer's handler, the projection and the versions it stores.
//! - [`delivery`]: the MQTT delivery, its coalescing, the republish and the apply reports.
//! - [`presence`]: the gateways' statuses, heartbeats and the heartbeat timeout.
//! - [`gateways`]: the API's list of a tenant's gateways.
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
pub mod gateways;
pub mod generate;
pub mod presence;
pub mod projection;
pub mod schema;

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
use tokio::sync::Semaphore;
use utoipa::OpenApi;

use crate::config::Config;
use crate::delivery::records::Records;
use crate::delivery::{Delivery, Parts};
use crate::desired::DesiredConfigs;
use crate::presence::store::PresenceStore;
use crate::presence::{Parts as PresenceParts, Presence};
use crate::projection::AssetsEventsHandler;

/// Connections of the platform role's pool: the consumer's reads of the assets service's events
/// and its reads of gateways' bindings and versions, one at a time, one for readiness, and
/// [`delivery::CONCURRENCY`] (two) for the delivery's and the presence task's reads together.
pub const PLATFORM_CONNECTIONS: u32 = 4;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(gateways::list_gateways),
    tags((name = "gateway-ops", description = "Gateways' presence and configurations (services/gateway-ops)."))
)]
struct ApiDoc;

/// The service's OpenAPI document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The service's startup for `service_kit::run`: opens the service role's and the platform
/// role's pools lazily (with the session check of `crates/service-kit`), starts the MQTT
/// delivery and the consumer of the assets service's events, which announces every version it
/// stores to the delivery, starts the presence task, follows the identity service's public keys
/// file and serves the API ([`gateways`]). Readiness passes while both roles can open a checked
/// session, the database has every migration file this release expects ([`schema::SCHEMA`]),
/// the broker connection is up with the apply report subscription granted (`mqtt`), the
/// presence task's connection is up with its subscriptions granted (`presence`), and the service
/// has a usable set of public keys.
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

    let reads = Arc::new(Semaphore::new(delivery::CONCURRENCY));
    let delivery = Delivery::start(Parts {
        mqtt: config.mqtt.clone(),
        configs: DesiredConfigs::new(&platform),
        records: Records::new(background.clone()),
        locks: db.pool().clone(),
        throttle: config.delivery.throttle,
        reads: Arc::clone(&reads),
    })?;
    let handle = delivery.handle();
    let connection = handle.connection();
    context.health().add_check("mqtt", move || {
        let result = connection.check();
        async move { result }
    });

    let presence = Presence::start(PresenceParts {
        mqtt: config.mqtt.clone(),
        store: PresenceStore::new(&platform, background.clone()),
        reads,
        settings: config.presence.settings(),
    })?;
    let presence_handle = presence.handle();
    context.health().add_check("presence", move || {
        let result = presence_handle.check();
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
    let stop = context.shutdown().stopping_token();
    context.spawn(presence.run(stop));

    let keys = PublishedKeys::watch(
        &context,
        config.platform.internal_identity_keys_file.clone(),
    )
    .await;
    let verifier = IdentityVerifier::new(ServiceName::GatewayOps, keys);
    Ok(gateways::routes(
        verifier,
        db,
        gateways::request_slots(config.database.max_connections),
    ))
}
