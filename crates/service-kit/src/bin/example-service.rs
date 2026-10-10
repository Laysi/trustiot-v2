//! A minimal service built on the kit, for demonstrations and the kit's integration tests.
//! It is not a product service and is never deployed to production.
//!
//! Configuration: `TRUSTIOT__GREETING` (required) plus the kit's settings. With the default
//! `contract` feature, also the platform setting `TRUSTIOT_INTERNAL_IDENTITY_KEYS` (optional
//! here only; see `Platform`).
//!
//! Routes besides `/healthz` and `/readyz`:
//! - `GET /v1/hello` returns the configured greeting.
//! - `POST /v1/echo` with `{"text": "..."}` returns the text's length in characters.
//! - `GET /v1/slow?ms=N` answers after N milliseconds (at most 10 000), to exercise draining and
//!   the handler timeout.
//! - `GET /v1/panic` panics, to exercise the kit's panic handling.
//! - `GET /v1/whoami` (`contract` feature, when the identity keys are set) returns the user of the
//!   request's internal identity token, which must be addressed to `assets`; otherwise 401.

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
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        if self.greeting.trim().is_empty() {
            return Err(Invalid::new("greeting", "must not be blank"));
        }
        Ok(())
    }
}

struct AppState {
    greeting: String,
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

    let state = Arc::new(AppState {
        greeting: context.config().greeting.clone(),
    });
    let router = Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/echo", post(echo))
        .route("/v1/slow", get(slow))
        .route("/v1/panic", get(panic))
        .with_state(state);
    #[cfg(feature = "contract")]
    let router = match &context.config().platform.internal_identity_keys {
        Some(keys) => router.merge(identity::routes(keys.clone())),
        None => router,
    };
    Ok(router)
}

/// The route that needs the user's identity.
#[cfg(feature = "contract")]
mod identity {
    use axum::routing::get;
    use axum::{Json, Router};
    use contract::ServiceName;
    use service_kit::identity::{IdentityKeys, IdentityVerifier, UserIdentity};

    /// The example stands in for a product service, so it accepts tokens addressed to `assets`.
    /// A product service names itself here.
    pub(crate) fn routes(keys: IdentityKeys) -> Router {
        Router::new()
            .route("/v1/whoami", get(whoami))
            .with_state(IdentityVerifier::new(ServiceName::Assets, keys))
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

async fn echo(Json(request): Json<EchoRequest>) -> Json<serde_json::Value> {
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
