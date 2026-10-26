//! The AI gateway (decisions 15 and 28): it holds the AI provider keys and forwards requests by
//! model role in each provider's native protocol, Anthropic Messages (with streaming) and
//! OpenAI Decisions, without converting formats. It sets the role's model, bounds every call
//! in time, and falls back along the role's models on 429, overload or timeout. Providers,
//! roles and limits come from its configuration at startup; a role's models can also be
//! replaced at run time through the admin API on its own port (src/admin.rs, README.md, 「管理
//! API」), which stores them in its Postgres schema, which every forwarded request checks
//! (README.md, 「資料庫」 and 「多副本」).
//!
//! The gateway knows nothing of the product: a tenant is an opaque ID, its settings carry its
//! own prefix (`AI_GATEWAY`), and it never depends on crates/contract (tests/no_contract.rs).

use std::sync::Arc;

use ai_clients::Tls;
use axum::Router;
use axum::routing::post;
use service_kit::migrations::MigrationCheck;
use service_kit::{BoxError, Context};

pub mod admin;
mod body;
pub mod config;
mod gateway;
mod reject;
mod relay;
pub mod schema;
pub mod store;

pub use config::{Config, ENV_PREFIX};
pub use gateway::{
    BuildError, CONFIGURED_ONLY, Gateway, Ignored, RoleRoute, SettingsError, Snapshot, Target,
};
pub use relay::{DECISIONS_PATH, MESSAGES_PATH, MODEL_HEADER, OUTCOME_HEADER, TENANT_HEADER};
pub use store::Store;

/// The internal API on top of `gateway`, for the API port only: the admin routes are on the admin
/// port ([`admin_routes`]).
pub fn routes(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route(MESSAGES_PATH, post(relay::messages))
        .route(DECISIONS_PATH, post(relay::decisions))
        .with_state(gateway)
}

/// The admin API on top of `gateway`, for the admin port only (src/admin.rs).
pub fn admin_routes(gateway: Arc<Gateway>) -> Router {
    admin::routes(gateway)
}

/// Builds the gateway from its configuration: reads each provider's key from the variable its
/// `key_env` names, trusts the system's root certificates for the providers' TLS, checks that
/// every role fits in `http.request_timeout`, and opens the service role's pool lazily, so the
/// gateway starts while the database is down and refuses calls with 503 until it answers.
/// Readiness passes while the database answers the settings query and has every migration file
/// this release expects ([`schema::SCHEMA`]). Fails, and the service does not start, when any
/// of the rest is not possible.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let tls = Tls::system_roots()?;
    let store = Store::connect_lazy(config)?;
    MigrationCheck::new(&schema::SCHEMA, store.pool().clone()).register(context.health());
    let gateway = Gateway::build(
        config,
        &|name| std::env::var(name).ok(),
        &tls,
        context.request_timeout(),
    )?
    .with_store(store, config.database.timeout);
    let gateway = Arc::new(gateway);
    let ping = Arc::clone(&gateway);
    context.health().add_check("postgres", move || {
        let gateway = Arc::clone(&ping);
        async move { gateway.ping().await }
    });
    for (role, route) in gateway.current().routes() {
        let models: Vec<&str> = route
            .targets
            .iter()
            .map(|target| target.label.as_str())
            .collect();
        tracing::info!(
            role,
            protocol = route.protocol.name(),
            models = %models.join(","),
            "model role ready"
        );
    }
    serve_admin(&context, Arc::clone(&gateway)).await?;
    Ok(routes(gateway))
}

/// Serves the admin API on its own port (`admin.listen_addr`), apart from the API port, so the
/// mesh can let the identity service, and nobody else, reach it (README.md, 「驗證方式」).
/// `Context::serve_port` gives it the API port's layers (handler timeout, panic recovery,
/// request logs and trace context) and drain.
///
/// # Errors
///
/// When the port is taken: the service does not start.
async fn serve_admin(context: &Context<Config>, gateway: Arc<Gateway>) -> Result<(), BoxError> {
    let bound = context
        .serve_port(context.config().admin.listen_addr, admin_routes(gateway))
        .await
        .map_err(|error| format!("admin API, admin.listen_addr: {error}"))?;
    tracing::info!(address = %bound, "serving the admin API");
    Ok(())
}
