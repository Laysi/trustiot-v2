//! A minimal service built on the kit, for demonstrations and the kit's integration tests.
//! It is not a product service and is never deployed to production.
//!
//! Configuration: `TRUSTIOT__GREETING` (required) plus the kit's settings. With the default
//! `contract` feature, also the platform setting `TRUSTIOT_INTERNAL_IDENTITY_KEYS` (optional
//! here only; see `Platform`), and a tenant database when `TRUSTIOT__DATABASE__USERNAME` and
//! `TRUSTIOT__DATABASE__PASSWORD` are set: the platform settings
//! `TRUSTIOT_POSTGRES_{HOST,PORT,DATABASE,SSLMODE,CA_FILE}` then say where, and the service
//! refuses to start unless `TRUSTIOT_POSTGRES_SSLMODE` is `verify-full`.
//!
//! Routes besides `/healthz` and `/readyz`:
//! - `GET /v1/hello` returns the configured greeting.
//! - `POST /v1/echo` with `{"text": "..."}` returns the text's length in characters and counts
//!   the call in the `example_service.echo.requests` metric.
//! - `GET /v1/slow?ms=N` answers after N milliseconds (at most 10 000), to exercise draining and
//!   the handler timeout.
//! - `GET /v1/panic` panics, to exercise the kit's panic handling.
//! - `GET /v1/whoami` (`contract` feature, when the identity keys are set) returns the user of the
//!   request's internal identity token, which must be addressed to `assets`; otherwise 401.
//! - `GET /v1/items` (`contract` feature, when the identity keys and the database are set) lists
//!   the names in `example_items` (crates/service-kit/sql/tenant-table.sql) in a tenant
//!   transaction for the token's tenant. The query has no tenant condition: row-level security
//!   returns only that tenant's rows. A token without a tenant answers 403 `permission_denied`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::{BoxError, Context, ServiceInfo};

/// The longest delay `/v1/slow` accepts.
const MAX_SLOW_MS: u64 = 10_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// What `/v1/hello` says.
    greeting: String,
    /// The deployment's platform settings. The kit always supplies the `platform` table, even
    /// when no platform setting is set; the default only keeps the example independent of that,
    /// since everything in it is optional.
    #[cfg(feature = "contract")]
    #[serde(default)]
    platform: Platform,
    /// The example's database role (`database.username`, `database.password`). Product
    /// services get their roles' credentials from the deployment (Laysi/trustiot-v2#58,
    /// Laysi/trustiot-v2#60).
    #[cfg(feature = "contract")]
    database: Option<DatabaseCredentials>,
}

/// The platform settings the example reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[cfg(feature = "contract")]
#[derive(Debug, Default, Deserialize)]
struct Platform {
    /// The identity service's public keys (`TRUSTIOT_INTERNAL_IDENTITY_KEYS`). Optional only so
    /// the kit's other tests can run the example without keys; a product service declares it
    /// without `Option`, so it refuses to start without keys.
    internal_identity_keys: Option<service_kit::identity::IdentityKeys>,
    /// `TRUSTIOT_POSTGRES_*`: read only when `database` is set. A product service declares
    /// them without `Option` and `postgres_sslmode` as `service_kit::postgres::SslMode`, so any
    /// other `sslmode` stops it at startup. The example keeps them optional and checks the
    /// `sslmode` in `validate`, because the kit's own tests run it without a database.
    postgres_host: Option<String>,
    postgres_port: Option<u16>,
    postgres_database: Option<String>,
    postgres_sslmode: Option<String>,
    postgres_ca_file: Option<std::path::PathBuf>,
}

#[cfg(feature = "contract")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseCredentials {
    username: String,
    password: String,
}

/// Never prints the password.
#[cfg(feature = "contract")]
impl std::fmt::Debug for DatabaseCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseCredentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "contract")]
impl Config {
    /// The tenant database's connection settings, when the example has a database.
    fn postgres(&self) -> Result<Option<service_kit::postgres::PostgresConfig>, Invalid> {
        let Some(credentials) = &self.database else {
            return Ok(None);
        };
        let platform = &self.platform;
        let required = |key: &str| Invalid::new(key, "is required when `database` is set");
        let ssl_mode = platform
            .postgres_sslmode
            .as_deref()
            .ok_or_else(|| required("platform.postgres_sslmode"))?
            .parse::<service_kit::postgres::SslMode>()
            .map_err(|error| Invalid::new("platform.postgres_sslmode", error.to_string()))?;
        Ok(Some(service_kit::postgres::PostgresConfig {
            host: platform
                .postgres_host
                .clone()
                .ok_or_else(|| required("platform.postgres_host"))?,
            port: platform
                .postgres_port
                .ok_or_else(|| required("platform.postgres_port"))?,
            database: platform
                .postgres_database
                .clone()
                .ok_or_else(|| required("platform.postgres_database"))?,
            ssl_mode,
            ca_file: platform.postgres_ca_file.clone(),
            username: credentials.username.clone(),
            password: credentials.password.clone(),
        }))
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        if self.greeting.trim().is_empty() {
            return Err(Invalid::new("greeting", "must not be blank"));
        }
        #[cfg(feature = "contract")]
        self.postgres()?;
        Ok(())
    }
}

struct AppState {
    greeting: String,
    /// Exported over OTLP when the kit's endpoint is set; a no-op otherwise.
    echo_requests: opentelemetry::metrics::Counter<u64>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let info = ServiceInfo::new(env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
    service_kit::run(info, start).await
}

async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    // A background task the kit waits for at shutdown: it stops when the service starts
    // stopping.
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                () = shutdown.stopping() => break,
                _ = ticks.tick() => tracing::debug!("heartbeat"),
            }
        }
        tracing::debug!("heartbeat task stopped");
    });

    // Created in `start`, after the kit installed the meter provider.
    let echo_requests = opentelemetry::global::meter("example-service")
        .u64_counter("example_service.echo.requests")
        .with_description("POST /v1/echo calls")
        .build();
    let state = Arc::new(AppState {
        greeting: context.config().greeting.clone(),
        echo_requests,
    });
    let router = Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/echo", post(echo))
        .route("/v1/slow", get(slow))
        .route("/v1/panic", get(panic))
        .with_state(state);
    #[cfg(feature = "contract")]
    let router = {
        let config = context.config();
        // `validate` already accepted these settings.
        let postgres = config
            .postgres()
            .map_err(|invalid| format!("{}: {}", invalid.key, invalid.message))?;
        let database = match postgres {
            Some(postgres) => {
                let db = service_kit::tenant::TenantDb::connect_lazy(
                    postgres.connect_options(),
                    sqlx::postgres::PgPoolOptions::new().max_connections(4),
                )?;
                let ping = db.clone();
                context.health().add_check("postgres", move || {
                    let db = ping.clone();
                    async move { db.ping().await }
                });
                Some(db)
            }
            None => None,
        };
        match &config.platform.internal_identity_keys {
            Some(keys) => router.merge(identity::routes(keys.clone(), database)),
            None => router,
        }
    };
    Ok(router)
}

/// The routes that need the user's identity.
#[cfg(feature = "contract")]
mod identity {
    use axum::extract::{FromRef, State};
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use contract::{ErrorBody, ErrorCode, ServiceName};
    use service_kit::identity::{IdentityKeys, IdentityVerifier, UserIdentity};
    use service_kit::tenant::{TenantDb, TenantDbError};

    #[derive(Clone)]
    struct IdentityState {
        verifier: IdentityVerifier,
        db: TenantDb,
    }

    impl FromRef<IdentityState> for IdentityVerifier {
        fn from_ref(state: &IdentityState) -> Self {
            state.verifier.clone()
        }
    }

    /// The example stands in for a product service, so it accepts tokens addressed to `assets`.
    /// A product service names itself here.
    pub(crate) fn routes(keys: IdentityKeys, database: Option<TenantDb>) -> Router {
        let verifier = IdentityVerifier::new(ServiceName::Assets, keys);
        let whoami = Router::new()
            .route("/v1/whoami", get(whoami))
            .with_state(verifier.clone());
        match database {
            Some(db) => whoami.merge(
                Router::new()
                    .route("/v1/items", get(items))
                    .with_state(IdentityState { verifier, db }),
            ),
            None => whoami,
        }
    }

    /// The names of the user's tenant's items. The query has no tenant condition; row-level
    /// security limits it to the tenant the kit set from the token.
    async fn items(
        State(state): State<IdentityState>,
        user: UserIdentity,
    ) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
        let error = |code: ErrorCode| {
            let body = ErrorBody::new(code);
            (body.http_status(), Json(body))
        };
        let mut tx = state
            .db
            .begin(&user)
            .await
            .map_err(|failure| match failure {
                TenantDbError::NoTenant => error(ErrorCode::PermissionDenied),
                other => {
                    tracing::error!(error = %other, "could not start a tenant transaction");
                    error(ErrorCode::Unavailable)
                }
            })?;
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM example_items ORDER BY name")
            .fetch_all(&mut *tx)
            .await
            .map_err(|failure| {
                tracing::error!(error = %failure, "could not list the items");
                error(ErrorCode::Unavailable)
            })?;
        let tenant = tx.tenant_id();
        tx.commit().await.map_err(|failure| {
            tracing::error!(error = %failure, "could not commit");
            error(ErrorCode::Unavailable)
        })?;
        Ok(Json(
            serde_json::json!({ "tenant": tenant, "items": names }),
        ))
    }

    /// Answers only requests whose token verified; the extractor answers 401 for the rest.
    async fn whoami(user: UserIdentity) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "user_id": user.user_id(),
            "tenant": user.tenant(),
            "platform_admin": user.is_platform_admin(),
        }))
    }
}

async fn hello(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": state.greeting }))
}

#[derive(Deserialize)]
struct EchoRequest {
    text: String,
}

async fn echo(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EchoRequest>,
) -> Json<serde_json::Value> {
    state.echo_requests.add(1, &[]);
    let length = request.text.chars().count();
    // Log the size of the input, never the input.
    tracing::debug!(length, "echo request");
    Json(serde_json::json!({ "length": length }))
}

#[derive(Deserialize)]
struct SlowQuery {
    ms: u64,
}

async fn panic() -> Json<serde_json::Value> {
    panic!("example-service: /v1/panic was called");
}

async fn slow(Query(query): Query<SlowQuery>) -> Json<serde_json::Value> {
    let waited = query.ms.min(MAX_SLOW_MS);
    tokio::time::sleep(Duration::from_millis(waited)).await;
    Json(serde_json::json!({ "waited_ms": waited }))
}
