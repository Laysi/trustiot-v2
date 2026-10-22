//! The AI gateway (decisions 15 and 28): it holds the AI provider keys and forwards requests by
//! model role in each provider's native protocol, Anthropic Messages (with streaming) and
//! OpenAI Decisions, without converting formats. It sets the role's model, bounds every call
//! in time, and falls back along the role's models on 429, overload or timeout. Providers,
//! roles and limits come from its configuration at startup (README.md).
//!
//! The gateway knows nothing of the product: a tenant is an opaque ID, its settings carry its
//! own prefix (`AI_GATEWAY`), and it never depends on crates/contract (tests/no_contract.rs).

use std::sync::Arc;

use ai_clients::Tls;
use axum::Router;
use axum::routing::post;
use service_kit::{BoxError, Context};

mod body;
pub mod config;
mod gateway;
mod reject;
mod relay;

pub use config::{Config, ENV_PREFIX};
pub use gateway::{BuildError, Gateway};
pub use relay::{DECISIONS_PATH, MESSAGES_PATH, MODEL_HEADER, OUTCOME_HEADER, TENANT_HEADER};

/// The internal API on top of `gateway`.
pub fn routes(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route(MESSAGES_PATH, post(relay::messages))
        .route(DECISIONS_PATH, post(relay::decisions))
        .with_state(gateway)
}

/// Builds the gateway from its configuration: reads each provider's key from the variable its
/// `key_env` names, trusts the system's root certificates for the providers' TLS, and checks
/// that every role fits in `http.request_timeout`. Fails, and the service does not start,
/// when any of that is not possible.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let tls = Tls::system_roots()?;
    let gateway = Gateway::build(
        context.config(),
        &|name| std::env::var(name).ok(),
        &tls,
        context.request_timeout(),
    )?;
    for (role, route) in gateway.roles() {
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
    Ok(routes(Arc::new(gateway)))
}
