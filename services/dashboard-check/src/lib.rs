//! The dashboard check service (Laysi/trustiot-v2#150, design record 「檢查服務」): the assistant
//! asks it whether a model-written dashboard page shows its data faithfully before the page is
//! saved. Each check runs the page in a fresh `chromium-headless-shell` driven over the DevTools
//! pipe, serves every file the browser loads from the sandbox build in the image, and compares
//! every value from an isolated world the page cannot reach (`frontend/sandbox/README.md`).
//!
//! The page's code is untrusted and Chromium runs without its own sandbox, so the service is
//! built to contain it: the pod has no egress, no ServiceAccount token and no secrets, only the
//! assistant can call it, one check runs at a time, and nothing of a check's browser survives
//! into the next (`cleanup`). README.md has the threat model, the browser's flags and the limits.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use contract::api::dashboard::check::{CHECK_ROUTE, CheckRequest};
use service_kit::{BoxError, Context};
use tokio::sync::oneshot;

mod api;
mod browser;
mod cdp;
mod check;
pub mod cleanup;
pub mod config;
mod observer;
mod site;

pub use browser::FLAGS;
pub use check::{CheckError, Checker, Outcome};
pub use config::Config;
pub use site::{ORIGIN, Site};

/// The internal API on top of `checker`.
pub fn routes(checker: Arc<Checker>, config: &Config) -> Router {
    let api = api::Api::new(checker, config.queue.max_waiting, config.queue.max_wait);
    Router::new()
        .route(CHECK_ROUTE, post(api::check))
        .with_state(Arc::new(api))
}

impl api::Run for Checker {
    fn run(
        &self,
        request: CheckRequest,
        cancelled: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = Result<Outcome, CheckError>> + Send + '_>> {
        Box::pin(async move {
            let cancelled = async {
                // The sender is never used: the request dropping it is the signal.
                let _ = cancelled.await;
            };
            self.check(&request, cancelled).await
        })
    }
}

/// Starts the service: refuses any setting that would send something out of the pod, protects
/// its own memory from the browser, refuses to run without the cleanup `cleanup` asks for (the
/// whole container's needs PID 1), cleans the container, loads the sandbox's files, and starts
/// one browser to prove it runs here.
///
/// # Errors
///
/// When any of that fails; the service does not start.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config().clone();
    let settings = service_kit::config::load::<Config>(service_kit::config::DEFAULT_ENV_PREFIX)?;
    config::refuse_egress(&settings.kit)
        .map_err(|invalid| format!("{}: {}", invalid.key, invalid.message))?;
    if context.request_timeout() < config.longest_request() {
        return Err(format!(
            "http.request_timeout must cover the longest check: at least {}s with \
             queue.max_wait {}s",
            config.longest_request().as_secs(),
            config.queue.max_wait.as_secs()
        )
        .into());
    }
    cleanup::protect_this_process()
        .map_err(|error| format!("cannot make the service's memory private: {error}"))?;
    if !config.work_dir.is_dir() {
        return Err("work_dir is not a directory".into());
    }
    // Fail closed: the service never falls back to a smaller cleanup than `cleanup` asks for.
    let cleanup = Arc::new(cleanup::Cleanup::for_this_process(
        &config.work_dir,
        config.cleanup,
    )?);
    // The pod's volumes and IPC namespace outlive a container restart.
    cleanup.container()?;
    let site = Arc::new(Site::load(&config.site_dir)?);
    let checker = Arc::new(Checker::new(config.chromium.clone(), site, cleanup.clone()));
    let version = checker.browser_version().await?;
    tracing::info!(
        browser = %version,
        whole_container_cleanup = cleanup.whole_container(),
        max_waiting = config.queue.max_waiting,
        max_wait_ms = u64::try_from(config.queue.max_wait.as_millis()).unwrap_or(u64::MAX),
        "dashboard check ready"
    );
    Ok(routes(checker, &config))
}
