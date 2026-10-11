//! A producer and a consumer of the outbox in one service on the kit, for demonstrations and
//! this crate's integration tests. It is not a product service and is never deployed.
//!
//! The producer keeps widgets in `producer_schema` and writes an `example.widget_renamed`
//! event with every rename; the consumer copies the names into `consumer_schema`. Both schemas
//! must exist; the service creates its tables at startup through `database_url`, as the
//! migration role, and writes as the producer through it too. The consumer connects as a
//! service does (decision 29): it reads events as its platform role and processes them as its
//! service role, in the event's tenant, through service-kit pools that verify the server; the
//! producer's `outbox_event` needs service-kit's row-level security template.
//!
//! Configuration besides the kit's:
//! - `TRUSTIOT__DATABASE_URL`, `TRUSTIOT__PRODUCER_SCHEMA`, `TRUSTIOT__CONSUMER_SCHEMA`
//! - `TRUSTIOT_POSTGRES_HOST`, `_PORT`, `_DATABASE`, `_SSLMODE` (only `verify-full`), `_CA_FILE`:
//!   the platform settings the consumer's pools use.
//! - `TRUSTIOT__CONSUMER_DATABASE__SERVICE_ROLE`, `__SERVICE_PASSWORD`, `__PLATFORM_ROLE`,
//!   `__PLATFORM_PASSWORD`: the consumer's roles.
//! - `TRUSTIOT__CONSUMER__PROCESS_AFTER`: wait this long before processing (default `0s`); the
//!   lag is measured from the start.
//! - `TRUSTIOT__CONSUMER__HOLD`: keep each delivery's transaction open this long after the
//!   handler's write (default `0s`), to kill the process mid-delivery.
//! - `TRUSTIOT__CONSUMER__POLL_INTERVAL` (default `200ms`), `TRUSTIOT__CONSUMER__LAG_INTERVAL`
//!   (default `1s`).
//!
//! Routes besides `/healthz` and `/readyz`: `PUT /v1/widgets/{id}` with `{"name": "..."}` and
//! optionally `"tenant_id"` renames a widget, writes the event for that tenant (platform-wide
//! without one), and answers `{"event_id": "...", "aggregate_seq": N}` after the commit.

use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::put;
use axum::{Json, Router};
use contract::{Event, ServiceName, TenantId};
use outbox::{Aggregate, Consumer, ConsumerConfig, Delivery, Handler, HandlerError};
use serde::{Deserialize, Serialize};
use service_kit::config::{Invalid, Validate};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::{PostgresConfig, SslMode};
use service_kit::tenant::{PlatformDb, TenantDb};
use service_kit::{BoxError, Context, ServiceInfo};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};

/// The subscription the example's consumer uses.
const SUBSCRIPTION: &str = "outbox-example.widget-names";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    database_url: String,
    producer_schema: String,
    consumer_schema: String,
    platform: Platform,
    consumer_database: ConsumerDatabase,
    #[serde(default)]
    consumer: ConsumerSettings,
}

/// The platform settings the consumer's pools use. No `deny_unknown_fields`: every service
/// gets every platform setting.
#[derive(Deserialize)]
struct Platform {
    postgres_host: String,
    postgres_port: u16,
    postgres_database: String,
    postgres_sslmode: SslMode,
    postgres_ca_file: std::path::PathBuf,
}

/// The consumer's roles: the service role processes events, the platform role reads them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsumerDatabase {
    service_role: String,
    service_password: String,
    platform_role: String,
    platform_password: String,
}

impl Platform {
    fn postgres(&self, username: &str, password: &str) -> PostgresConfig {
        PostgresConfig {
            host: self.postgres_host.clone(),
            port: self.postgres_port,
            database: self.postgres_database.clone(),
            ssl_mode: self.postgres_sslmode,
            ca_file: self.postgres_ca_file.clone(),
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConsumerSettings {
    #[serde(deserialize_with = "service_kit::config::duration")]
    process_after: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    hold: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    poll_interval: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    lag_interval: Duration,
}

impl Default for ConsumerSettings {
    fn default() -> Self {
        Self {
            process_after: Duration::ZERO,
            hold: Duration::ZERO,
            poll_interval: Duration::from_millis(200),
            lag_interval: Duration::from_secs(1),
        }
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        for (key, schema) in [
            ("producer_schema", &self.producer_schema),
            ("consumer_schema", &self.consumer_schema),
        ] {
            if !outbox::schema::is_schema_name(schema) {
                return Err(Invalid::new(key, "is not a plain lower case schema name"));
            }
        }
        Ok(())
    }
}

/// The event the producer writes.
#[derive(Serialize, Deserialize)]
struct WidgetRenamed {
    widget_id: String,
    name: String,
}

impl Event for WidgetRenamed {
    const TYPE: &'static str = "example.widget_renamed";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

const PRODUCER_TABLES: &str =
    "CREATE TABLE IF NOT EXISTS widget (id text PRIMARY KEY, name text NOT NULL)";
const CONSUMER_TABLES: &str = "CREATE TABLE IF NOT EXISTS widget_name \
     (id text PRIMARY KEY, name text NOT NULL, applied integer NOT NULL, tenant text)";

#[tokio::main]
async fn main() -> ExitCode {
    let info = ServiceInfo::new(env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
    service_kit::run(info, start).await
}

/// A pool whose connections use `schema` as their `search_path`.
fn pool(url: &str, schema: &str) -> Result<PgPool, sqlx::Error> {
    let options = PgConnectOptions::from_str(url)?.options([("search_path", schema)]);
    Ok(PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy_with(options))
}

async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let producer = pool(&config.database_url, &config.producer_schema)?;
    let migration = pool(&config.database_url, &config.consumer_schema)?;

    let mut conn = producer.acquire().await?;
    outbox::schema::install_producer(&mut conn).await?;
    sqlx::raw_sql(PRODUCER_TABLES).execute(&mut *conn).await?;
    drop(conn);
    let mut conn = migration.acquire().await?;
    outbox::schema::install_consumer(&mut conn).await?;
    sqlx::raw_sql(CONSUMER_TABLES).execute(&mut *conn).await?;
    drop(conn);
    migration.close().await;

    let roles = &config.consumer_database;
    let pool_options = || PgPoolOptions::new().max_connections(4);
    let events = PlatformDb::connect_lazy(
        config
            .platform
            .postgres(&roles.platform_role, &roles.platform_password)
            .connect_options(),
        pool_options(),
    )?;
    let (work_db, work) = TenantDb::connect_lazy_with_background(
        config
            .platform
            .postgres(&roles.service_role, &roles.service_password)
            .connect_options(),
        pool_options(),
        ServiceIdentity::new(ServiceName::Assets),
    )?;

    let ping = producer.clone();
    context.health().add_check("postgres", move || {
        let pool = ping.clone();
        async move { sqlx::query("SELECT 1").execute(&pool).await.map(|_| ()) }
    });
    let events_ping = events.clone();
    context.health().add_check("consumer-postgres", move || {
        let (work_db, events) = (work_db.clone(), events_ping.clone());
        async move {
            work_db.ping().await?;
            events.ping().await
        }
    });

    let settings = &config.consumer;
    let consumer_config = ConsumerConfig::new(SUBSCRIPTION, config.producer_schema.clone())?
        .poll_interval(settings.poll_interval)
        .lag_interval(settings.lag_interval);
    let consumer = Consumer::new(
        &events,
        work,
        consumer_config,
        Projection {
            hold: settings.hold,
        },
    );
    consumer.ensure_subscription().await?;
    let process_after = settings.process_after;
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        let monitor = consumer.monitor_lag(stop.clone());
        let process = async {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(process_after) => {}
            }
            tracing::info!("outbox consumer processing");
            consumer.process(stop.clone()).await;
        };
        tokio::join!(monitor, process);
    });

    Ok(Router::new()
        .route("/v1/widgets/{id}", put(rename))
        .with_state(Arc::new(producer)))
}

#[derive(Deserialize)]
struct RenameRequest {
    name: String,
    /// The event's tenant; platform-wide when absent.
    tenant_id: Option<TenantId>,
}

async fn rename(
    State(pool): State<Arc<PgPool>>,
    Path(id): Path<String>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let aggregate = Aggregate::new("widget", id.clone()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let result = async {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO widget (id, name) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
        )
        .bind(&id)
        .bind(&request.name)
        .execute(&mut *tx)
        .await?;
        let event = WidgetRenamed {
            widget_id: id.clone(),
            name: request.name.clone(),
        };
        let published = outbox::publish(&mut tx, &aggregate, &event, request.tenant_id).await?;
        tx.commit().await?;
        Ok::<_, BoxError>(published)
    }
    .await;
    match result {
        Ok(published) => {
            tracing::info!(
                event_id = %published.event_id,
                aggregate_seq = published.aggregate_seq,
                "event committed"
            );
            Ok(Json(serde_json::json!({
                "event_id": published.event_id,
                "aggregate_seq": published.aggregate_seq,
            })))
        }
        Err(error) => {
            // Only the kind: error messages can quote the request data.
            tracing::error!(error_kind = error_kind(&*error), "rename failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Copies widget names into the consumer's schema, counting how often each was applied.
struct Projection {
    hold: Duration,
}

impl Handler for Projection {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event: WidgetRenamed = delivery.envelope.decode()?;
        // `tenant` records the tenant the kit set for the delivery, NULL for none.
        sqlx::query(
            "INSERT INTO widget_name (id, name, applied, tenant) \
             VALUES ($1, $2, 1, NULLIF(current_setting('trustiot.tenant_id', true), '')) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, \
             applied = widget_name.applied + 1, tenant = EXCLUDED.tenant",
        )
        .bind(&event.widget_id)
        .bind(&event.name)
        .execute(&mut **tx)
        .await?;
        if !self.hold.is_zero() {
            tracing::info!(
                event_id = %delivery.envelope.id,
                "holding the delivery transaction open"
            );
            tokio::time::sleep(self.hold).await;
        }
        Ok(())
    }
}

/// The SQLSTATE of a failed rename, or the kind of failure, never the message.
fn error_kind(error: &(dyn std::error::Error + 'static)) -> String {
    if let Some(error) = error.downcast_ref::<outbox::PublishError>() {
        // `PublishError` prints only the SQLSTATE or kind.
        return error.to_string();
    }
    match error.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::Database(database)) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        Some(_) => "database client".to_owned(),
        None => "other".to_owned(),
    }
}
