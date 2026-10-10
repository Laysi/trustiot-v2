//! Liveness and readiness endpoints for the kubelet.
//!
//! - [`LIVENESS_PATH`] answers 200 while the process can serve HTTP at all; it never consults
//!   dependencies, so an outage elsewhere does not get every pod restarted.
//! - [`READINESS_PATH`] answers 200 only after startup finished, while no shutdown signal has
//!   arrived, and while every registered check passes; otherwise 503.
//!
//! Both are served on the main port, outside the request logging and timeout layers. Under Istio
//! ambient the kubelet's probes arrive from 169.254.7.127 without passing through ztunnel, so
//! the chart's NetworkPolicy must allow that address (docs/feasibility/2026-10-10-edge-poc.md).

use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// The liveness probe path.
pub const LIVENESS_PATH: &str = "/healthz";
/// The readiness probe path.
pub const READINESS_PATH: &str = "/readyz";

/// How long one readiness check may take. Below the kubelet's default probe timeout of 1s.
const CHECK_TIMEOUT: Duration = Duration::from_millis(800);
/// How long a readiness result is reused, so frequent probes do not hammer dependencies.
const RESULT_TTL: Duration = Duration::from_secs(1);

const STARTING: u8 = 0;
const READY: u8 = 1;
const DRAINING: u8 = 2;

type CheckFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type CheckFn = dyn Fn() -> CheckFuture + Send + Sync;

#[derive(Clone)]
struct Check {
    name: String,
    run: Arc<CheckFn>,
}

/// What `/readyz` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Serving traffic.
    Ready,
    /// Startup has not finished.
    Starting,
    /// A shutdown signal arrived.
    Draining,
    /// A registered check failed.
    Unavailable,
}

impl Readiness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Starting => "starting",
            Self::Draining => "draining",
            Self::Unavailable => "unavailable",
        }
    }
}

struct Inner {
    phase: AtomicU8,
    checks: Mutex<Vec<Check>>,
    last_result: tokio::sync::Mutex<Option<(Instant, bool)>>,
    last_ok: AtomicBool,
}

/// The service's health state. Cheap to clone.
#[derive(Clone)]
pub struct Health {
    inner: Arc<Inner>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Health")
            .field("phase", &self.inner.phase.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Health {
    /// A health state in the starting phase.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                phase: AtomicU8::new(STARTING),
                checks: Mutex::new(Vec::new()),
                last_result: tokio::sync::Mutex::new(None),
                last_ok: AtomicBool::new(true),
            }),
        }
    }

    /// Adds a dependency check to readiness, for example a database ping. The check runs at
    /// most once per second, must finish within 800 ms, and fails readiness while it fails.
    /// The error is logged when the check starts failing, so it must not contain secrets.
    pub fn add_check<F, Fut, E>(&self, name: impl Into<String>, check: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Display,
    {
        let run: Arc<CheckFn> = Arc::new(move || {
            let future = check();
            Box::pin(async move { future.await.map_err(|error| error.to_string()) })
        });
        if let Ok(mut checks) = self.inner.checks.lock() {
            checks.push(Check {
                name: name.into(),
                run,
            });
        }
    }

    /// Marks startup as finished, unless shutdown already began.
    pub(crate) fn set_ready(&self) {
        let _ =
            self.inner
                .phase
                .compare_exchange(STARTING, READY, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Fails readiness from now on.
    pub(crate) fn set_draining(&self) {
        self.inner.phase.store(DRAINING, Ordering::Release);
    }

    /// The current readiness, running the checks when the cached result is stale.
    pub async fn readiness(&self) -> Readiness {
        match self.inner.phase.load(Ordering::Acquire) {
            STARTING => return Readiness::Starting,
            DRAINING => return Readiness::Draining,
            _ => {}
        }
        if self.checks_pass().await {
            Readiness::Ready
        } else {
            Readiness::Unavailable
        }
    }

    async fn checks_pass(&self) -> bool {
        let checks = match self.inner.checks.lock() {
            Ok(checks) => checks.clone(),
            Err(_) => return false,
        };
        if checks.is_empty() {
            return true;
        }
        // Holding the lock while checking makes concurrent probes share one round of checks.
        let mut last = self.inner.last_result.lock().await;
        if let Some((at, ok)) = *last
            && at.elapsed() < RESULT_TTL
        {
            return ok;
        }
        let mut runs = tokio::task::JoinSet::new();
        for check in checks {
            runs.spawn(async move {
                let outcome = match tokio::time::timeout(CHECK_TIMEOUT, (check.run)()).await {
                    Ok(outcome) => outcome,
                    Err(_) => Err(format!("timed out after {} ms", CHECK_TIMEOUT.as_millis())),
                };
                (check.name, outcome)
            });
        }
        let mut failures = Vec::new();
        while let Some(joined) = runs.join_next().await {
            match joined {
                Ok((_, Ok(()))) => {}
                Ok((name, Err(error))) => failures.push((name, error)),
                Err(_) => failures.push(("(check panicked)".to_owned(), String::new())),
            }
        }
        let ok = failures.is_empty();
        let was_ok = self.inner.last_ok.swap(ok, Ordering::AcqRel);
        if was_ok && !ok {
            for (check, error) in &failures {
                tracing::warn!(check = %check, error = %error, "readiness check failed");
            }
        } else if !was_ok && ok {
            tracing::info!("readiness checks pass again");
        }
        *last = Some((Instant::now(), ok));
        ok
    }

    /// The `/healthz` and `/readyz` routes.
    pub(crate) fn router(&self) -> Router {
        Router::new()
            .route(LIVENESS_PATH, get(liveness))
            .route(READINESS_PATH, get(readiness))
            .with_state(self.clone())
    }
}

fn probe_response(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        axum::Json(serde_json::json!({ "status": body })),
    )
        .into_response()
}

async fn liveness() -> Response {
    probe_response(StatusCode::OK, "ok")
}

async fn readiness(State(health): State<Health>) -> Response {
    let readiness = health.readiness().await;
    let status = if readiness == Readiness::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    probe_response(status, readiness.as_str())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[tokio::test]
    async fn readiness_follows_the_phase() {
        let health = Health::new();
        assert_eq!(health.readiness().await, Readiness::Starting);
        health.set_ready();
        assert_eq!(health.readiness().await, Readiness::Ready);
        health.set_draining();
        assert_eq!(health.readiness().await, Readiness::Draining);
        // A late "ready" cannot undo draining.
        health.set_ready();
        assert_eq!(health.readiness().await, Readiness::Draining);
    }

    #[tokio::test]
    async fn failing_or_hanging_checks_fail_readiness() {
        let health = Health::new();
        health.set_ready();
        let healthy = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&healthy);
        health.add_check("database", move || {
            let ok = flag.load(Ordering::SeqCst);
            async move {
                if ok {
                    Ok(())
                } else {
                    Err("connection refused")
                }
            }
        });
        assert_eq!(health.readiness().await, Readiness::Unavailable);

        healthy.store(true, Ordering::SeqCst);
        // The cached result is reused for a second.
        assert_eq!(health.readiness().await, Readiness::Unavailable);
        tokio::time::sleep(RESULT_TTL).await;
        assert_eq!(health.readiness().await, Readiness::Ready);

        let hanging = Health::new();
        hanging.set_ready();
        hanging.add_check("stuck", || async {
            std::future::pending::<Result<(), String>>().await
        });
        let started = Instant::now();
        assert_eq!(hanging.readiness().await, Readiness::Unavailable);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn concurrent_probes_share_one_round_of_checks() {
        let health = Health::new();
        health.set_ready();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        health.add_check("counted", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<(), String>(())
            }
        });
        let probes: Vec<_> = (0..8)
            .map(|_| {
                let health = health.clone();
                tokio::spawn(async move { health.readiness().await })
            })
            .collect();
        for probe in probes {
            assert_eq!(probe.await.expect("probe task"), Readiness::Ready);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
