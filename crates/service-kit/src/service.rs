//! The service lifecycle: configuration, telemetry, startup, serving, and graceful shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use axum::Router;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::{AbortOnDropHandle, TaskTracker};

use crate::config::{self, KitConfig, Settings, Validate};
use crate::health::Health;
use crate::http;
use crate::shutdown::{self, Shutdown};
use crate::telemetry::{self, Telemetry, duration_ms};

/// The error type service startup may return.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// How long buffered telemetry may take to export at exit. Together with [`HALT_GRACE`] and the
/// exporter shutdown margin, a service needs at most 3 s after its drain (README.md).
const TELEMETRY_FLUSH: Duration = Duration::from_secs(2);

/// How long work cut off at the drain timeout gets to unwind: requests in flight answer 504 and
/// background tasks are dropped at their next await point.
const HALT_GRACE: Duration = Duration::from_millis(500);

/// Identifies the service in logs and traces, and selects its environment-variable prefix.
#[derive(Clone, Copy, Debug)]
pub struct ServiceInfo {
    /// The service name, for example `assets`; becomes `service.name` in logs and traces.
    pub name: &'static str,
    /// The service version; becomes `service.version` in traces.
    pub version: &'static str,
    /// The prefix of configuration variables (see [`crate::config`]).
    pub env_prefix: &'static str,
}

impl ServiceInfo {
    /// A service configured through `TRUSTIOT__*` variables.
    pub const fn new(name: &'static str, version: &'static str) -> Self {
        Self {
            name,
            version,
            env_prefix: config::DEFAULT_ENV_PREFIX,
        }
    }

    /// Uses another configuration prefix, for a service that must not carry the product name.
    pub const fn with_env_prefix(self, env_prefix: &'static str) -> Self {
        Self { env_prefix, ..self }
    }
}

/// What a service's startup function receives.
#[derive(Debug)]
pub struct Context<T> {
    config: T,
    health: Health,
    shutdown: Shutdown,
    tasks: TaskTracker,
    /// Cancelled when the drain timeout runs out: everything still running is cut off.
    halt: CancellationToken,
    /// `http.request_timeout`, for the ports [`Context::serve_port`] adds.
    request_timeout: Duration,
}

impl<T> Context<T> {
    /// The service's own configuration, already validated.
    pub fn config(&self) -> &T {
        &self.config
    }

    /// Readiness, for registering dependency checks.
    pub fn health(&self) -> &Health {
        &self.health
    }

    /// Shutdown state, for background tasks and long-lived connections.
    pub fn shutdown(&self) -> &Shutdown {
        &self.shutdown
    }

    /// Runs a background task that the service waits for during shutdown. The task must end
    /// soon after [`Shutdown::stopping`] resolves; when `shutdown.drain_timeout` runs out first,
    /// the task is dropped at its next await point.
    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let halt = self.halt.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                () = task => {}
                () = halt.cancelled() => {}
            }
        });
    }

    /// Serves `routes` on a port of their own, `listen_addr`, with the API port's layers: the
    /// handler timeout (`http.request_timeout`), panic recovery, and request spans and access
    /// logs. For an internal API the mesh grants apart from the service's API, such as the
    /// identity service's token exchange (Laysi/trustiot-v2#106). The port stops accepting when
    /// [`Shutdown::stopping`] resolves, like the API port, and the drain waits for its requests;
    /// at the drain timeout they answer 504 like the API port's. Returns the bound address (port
    /// 0 picks a free one).
    ///
    /// # Errors
    ///
    /// When `listen_addr` cannot be bound; the service should not start then.
    pub async fn serve_port(
        &self,
        listen_addr: SocketAddr,
        routes: Router,
    ) -> Result<SocketAddr, BoxError> {
        let listener = TcpListener::bind(listen_addr)
            .await
            .map_err(|error| format!("cannot listen on {listen_addr}: {error}"))?;
        let address = listener.local_addr()?;
        let router = http::layer(routes, self.request_timeout, self.halt.clone());
        let stop = self.shutdown.clone();
        // Tracked but not cut off at the halt (unlike `spawn`), so requests still in flight then
        // get their 504 within the grace period, as on the API port.
        self.tasks.spawn(async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async move { stop.stopping().await })
                .await;
            if let Err(error) = served {
                tracing::error!(address = %address, error = %error, "a port stopped serving");
            }
        });
        Ok(address)
    }
}

/// Runs a service: loads and validates its configuration, starts telemetry, calls `init` to
/// build its routes, serves them with the health routes until SIGTERM or SIGINT, drains, and
/// returns the process exit code. The health routes are also served alone on the health port
/// (`http.health_listen_addr`) until this returns.
///
/// Startup fails (exit code 1, the reason logged) on invalid configuration, when `init` fails,
/// or when either listen address is taken. A signal during `init` exits at once with code 0.
pub async fn run<T, F, Fut>(info: ServiceInfo, init: F) -> ExitCode
where
    T: DeserializeOwned + Validate,
    F: FnOnce(Context<T>) -> Fut,
    Fut: Future<Output = Result<Router, BoxError>>,
{
    let settings = match config::load::<T>(info.env_prefix) {
        Ok(settings) => settings,
        Err(error) => {
            telemetry::init_fallback(&info);
            tracing::error!(error = %error, "invalid configuration; not starting");
            return ExitCode::FAILURE;
        }
    };
    let telemetry = match Telemetry::init(&info, &settings.kit.telemetry) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            telemetry::init_fallback(&info);
            tracing::error!(error = %error, "cannot start telemetry; not starting");
            return ExitCode::FAILURE;
        }
    };
    let code = serve(&info, settings, init).await;
    telemetry.shutdown(TELEMETRY_FLUSH).await;
    code
}

async fn serve<T, F, Fut>(info: &ServiceInfo, settings: Settings<T>, init: F) -> ExitCode
where
    F: FnOnce(Context<T>) -> Fut,
    Fut: Future<Output = Result<Router, BoxError>>,
{
    let Settings { kit, app } = settings;
    // Only settings that cannot carry secrets; the OTLP endpoint's path could, so only whether
    // export is on is logged.
    tracing::info!(
        service.version = info.version,
        process.pid = std::process::id(),
        http.listen_addr = %kit.http.listen_addr,
        http.health_listen_addr = %kit.http.health_listen_addr,
        http.request_timeout_ms = duration_ms(kit.http.request_timeout),
        shutdown.drain_delay_ms = duration_ms(kit.shutdown.drain_delay),
        shutdown.drain_timeout_ms = duration_ms(kit.shutdown.drain_timeout),
        telemetry.log_filter = %kit.telemetry.log_filter,
        telemetry.otlp_export = kit.telemetry.otlp_endpoint.is_some(),
        "starting"
    );

    let health = Health::new();
    let shutdown = Shutdown::new();
    if let Err(error) = shutdown::listen(&kit.shutdown, health.clone(), shutdown.clone()) {
        tracing::error!(error = %error, "cannot install signal handlers; not starting");
        return ExitCode::FAILURE;
    }
    let tasks = TaskTracker::new();
    let halt = CancellationToken::new();
    // Whichever way serving ends, nothing the service started keeps running while telemetry is
    // flushed at exit.
    let _halt_on_return = halt.clone().drop_guard();
    let context = Context {
        config: app,
        health: health.clone(),
        shutdown: shutdown.clone(),
        tasks: tasks.clone(),
        halt: halt.clone(),
        request_timeout: kit.http.request_timeout,
    };

    let routes = tokio::select! {
        result = init(context) => match result {
            Ok(routes) => routes,
            Err(error) => {
                tracing::error!(error = %error, "startup failed");
                return ExitCode::FAILURE;
            }
        },
        () = shutdown.requested() => {
            tracing::info!("shutdown requested during startup; exiting");
            return ExitCode::SUCCESS;
        }
    };

    let Some((listener, address)) = bind(kit.http.listen_addr).await else {
        return ExitCode::FAILURE;
    };
    let Some((health_listener, health_address)) = bind(kit.http.health_listen_addr).await else {
        return ExitCode::FAILURE;
    };
    let router = http::compose(routes, health.router(), &kit.http, halt.clone());
    let stop = shutdown.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { stop.stopping().await })
            .await
    });
    // The health port keeps answering through the drain, readiness with 503, so the kubelet
    // sees a draining pod rather than refused connections. It stops accepting before this
    // function returns (and the handle aborts it on any other way out).
    let health_router = health.router();
    let mut health_server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(health_listener, health_router).await
    }));
    health.set_ready();
    tracing::info!(
        address = %address,
        health_address = %health_address,
        "listening"
    );

    let code = drain(
        &kit,
        &shutdown,
        &tasks,
        &halt,
        &mut server,
        &mut health_server,
    )
    .await;
    // Close the health port before telemetry is flushed: readiness fails from now on, the
    // listener closes, and idle probe connections are shut down. A probe still in flight gets
    // its answer, at once after a signal, when readiness runs no checks.
    health.set_draining();
    health_server.abort();
    let _ = health_server.await;
    code
}

/// Serves until a shutdown signal or a server failure, then drains: the second half of `serve`.
async fn drain(
    kit: &KitConfig,
    shutdown: &Shutdown,
    tasks: &TaskTracker,
    halt: &CancellationToken,
    server: &mut JoinHandle<std::io::Result<()>>,
    health_server: &mut AbortOnDropHandle<std::io::Result<()>>,
) -> ExitCode {
    tokio::select! {
        // `stopping` first: it also ends the server, which on another worker thread can finish
        // before this select is polled. Unbiased, the select would then pick either ready branch
        // at random and report half of those graceful stops as crashes.
        biased;
        () = shutdown.stopping() => {}
        ended = &mut *server => {
            // axum's server only returns after the shutdown signal, so this is a crash.
            tracing::error!(outcome = ?ended, "server stopped unexpectedly");
            return ExitCode::FAILURE;
        }
        ended = &mut *health_server => {
            // The health server has no shutdown signal: it only returns on failure. The API
            // server must not outlive this return either.
            tracing::error!(outcome = ?ended, "health server stopped unexpectedly");
            server.abort();
            let _ = server.await;
            return ExitCode::FAILURE;
        }
    }

    let drain_timeout = kit.shutdown.drain_timeout;
    tracing::info!(
        drain_timeout_ms = duration_ms(drain_timeout),
        "stopped accepting connections; draining requests and background tasks"
    );
    let started = tokio::time::Instant::now();
    let deadline = started + drain_timeout;
    tasks.close();
    // The server finishes once every connection has; background tasks are waited for after it.
    let served = tokio::time::timeout_at(deadline, &mut *server).await;
    let tasks_done = tokio::time::timeout_at(deadline, tasks.wait())
        .await
        .is_ok();
    match served {
        Ok(Ok(Ok(()))) if tasks_done => {
            tracing::info!(
                elapsed_ms = duration_ms(started.elapsed()),
                "shutdown complete"
            );
            ExitCode::SUCCESS
        }
        Ok(outcome) if tasks_done => {
            tracing::error!(outcome = ?outcome, "server failed while draining");
            ExitCode::FAILURE
        }
        _ => {
            // Requests still in flight answer 504 and background tasks are dropped at their
            // next await point; whatever has not stopped within the grace period is abandoned
            // (the server task is aborted) so no work continues while telemetry is flushed.
            halt.cancel();
            let grace = tokio::time::Instant::now() + HALT_GRACE;
            if !server.is_finished() && tokio::time::timeout_at(grace, &mut *server).await.is_err()
            {
                server.abort();
            }
            let _ = tokio::time::timeout_at(grace, tasks.wait()).await;
            tracing::error!(
                background_tasks = tasks.len(),
                "drain timed out; cut off the work still in flight"
            );
            ExitCode::FAILURE
        }
    }
}

/// Binds a listener and returns it with the address it got, or logs why it cannot.
async fn bind(address: SocketAddr) -> Option<(TcpListener, String)> {
    match TcpListener::bind(address).await {
        Ok(listener) => {
            let bound = listener
                .local_addr()
                .map_or_else(|_| address.to_string(), |bound| bound.to_string());
            Some((listener, bound))
        }
        Err(error) => {
            tracing::error!(
                address = %address,
                error = %error,
                "cannot listen; not starting"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn background_tasks_are_dropped_when_the_drain_times_out() {
        let tasks = TaskTracker::new();
        let halt = CancellationToken::new();
        let context = Context {
            config: (),
            health: Health::new(),
            shutdown: Shutdown::new(),
            tasks: tasks.clone(),
            halt: halt.clone(),
            request_timeout: Duration::from_secs(30),
        };
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        // A task that ignores `Shutdown::stopping`.
        context.spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        tasks.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), tasks.wait())
                .await
                .is_err(),
            "the task runs until halted"
        );
        halt.cancel();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .expect("halted tasks end");
        assert!(dropped.load(Ordering::SeqCst), "the task was dropped");
    }
}
