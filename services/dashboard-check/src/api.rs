//! The check endpoint (`contract::api::dashboard::check`, outcome table): one check at a time,
//! a small wait queue, the request's size and shape checked before any browser starts, and log
//! lines that carry only the verdict's kind, its problems' kinds and the timings.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use contract::api::dashboard::check::{
    CheckRequest, CheckVerdict, MAX_CHECK_REQUEST_BYTES, Problem, ProblemKind,
};
use contract::error::{ErrorBody, ErrorCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::check::{CheckError, Outcome, Timings};

/// Runs one check; the future ends early, with [`CheckError::Cancelled`], when `cancelled`
/// resolves.
pub trait Run: Send + Sync + 'static {
    fn run(
        &self,
        request: CheckRequest,
        cancelled: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = Result<Outcome, CheckError>> + Send + '_>>;
}

/// The endpoint's state.
pub struct Api {
    checks: Arc<dyn Run>,
    admission: Admission,
}

impl Api {
    #[must_use]
    pub fn new(checks: Arc<dyn Run>, max_waiting: usize, max_wait: Duration) -> Self {
        Self {
            checks,
            admission: Admission {
                running: Arc::new(Semaphore::new(1)),
                admitted: Arc::new(AtomicUsize::new(0)),
                limit: max_waiting + 1,
                max_wait,
            },
        }
    }
}

/// One check runs; up to `limit - 1` wait, each at most `max_wait`.
struct Admission {
    running: Arc<Semaphore>,
    admitted: Arc<AtomicUsize>,
    limit: usize,
    max_wait: Duration,
}

/// A place among the admitted requests, given back when dropped.
struct Ticket(Arc<AtomicUsize>);

impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Admission {
    fn enter(&self) -> Option<Ticket> {
        self.admitted
            .try_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
                (admitted < self.limit).then_some(admitted + 1)
            })
            .ok()
            .map(|_| Ticket(self.admitted.clone()))
    }

    async fn turn(&self) -> Option<OwnedSemaphorePermit> {
        tokio::time::timeout(self.max_wait, self.running.clone().acquire_owned())
            .await
            .ok()?
            .ok()
    }
}

/// `POST CHECK_ROUTE`.
pub async fn check(State(api): State<Arc<Api>>, body: Body) -> Response {
    let Some(ticket) = api.admission.enter() else {
        tracing::info!(outcome = "busy", "check refused");
        return error(ErrorCode::Unavailable);
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_CHECK_REQUEST_BYTES).await else {
        tracing::info!(
            outcome = "invalid_request",
            reason = "body",
            "check refused"
        );
        return error(ErrorCode::InvalidRequest);
    };
    let Ok(request) = serde_json::from_slice::<CheckRequest>(&bytes) else {
        tracing::info!(
            outcome = "invalid_request",
            reason = "shape",
            "check refused"
        );
        return error(ErrorCode::InvalidRequest);
    };
    drop(bytes);
    if let Err(invalid) = request.validate() {
        tracing::info!(outcome = "invalid_request", reason = %invalid, "check refused");
        return error(ErrorCode::InvalidRequest);
    }
    let Some(permit) = api.admission.turn().await else {
        tracing::info!(outcome = "busy", "check waited too long");
        return error(ErrorCode::Unavailable);
    };
    // The check runs in a task of its own that keeps the turn until the browser is gone; when
    // this request is dropped, `cancel` drops with it and the check stops early.
    let (_cancel, cancelled) = oneshot::channel::<()>();
    let checks = api.checks.clone();
    let task = tokio::spawn(async move {
        let outcome = checks.run(request, cancelled).await;
        drop(permit);
        drop(ticket);
        outcome
    });
    match task.await {
        Ok(Ok(outcome)) => {
            log_outcome(&outcome);
            ok(&outcome.verdict)
        }
        Ok(Err(CheckError::Cleanup(failure))) => crate::browser::cleanup_failed(&failure),
        Ok(Err(failure)) => {
            tracing::error!(outcome = "internal", error = %failure, "check failed");
            error(ErrorCode::Internal)
        }
        Err(_) => {
            tracing::error!(outcome = "internal", "check task failed");
            error(ErrorCode::Internal)
        }
    }
}

fn log_outcome(outcome: &Outcome) {
    let Timings {
        launch,
        ready,
        rendered,
        compared,
        total,
    } = outcome.timings;
    let ms = |duration: Option<Duration>| {
        duration.map_or(-1, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
    };
    let (verdict, problems) = match &outcome.verdict {
        CheckVerdict::Passed { .. } => ("passed", String::new()),
        CheckVerdict::Failed { problems } => ("failed", kinds(problems)),
    };
    tracing::info!(
        outcome = "checked",
        verdict,
        problems = %problems,
        launch_ms = ms(Some(launch)),
        ready_ms = ms(ready),
        rendered_ms = ms(rendered),
        compared_ms = ms(compared),
        total_ms = ms(Some(total)),
        "check finished"
    );
}

/// The problems' kinds, comma-separated: fixed names only, never a problem's content.
pub fn kinds(problems: &[Problem]) -> String {
    let names: Vec<&str> = problems
        .iter()
        .map(|problem| match problem.kind() {
            ProblemKind::NotRendered => "not_rendered",
            ProblemKind::Navigated => "navigated",
            ProblemKind::Requested => "requested",
            ProblemKind::ScriptError => "script_error",
            ProblemKind::Unmapped => "unmapped",
            ProblemKind::Mismatch => "mismatch",
            ProblemKind::LiteralNumber => "literal_number",
            ProblemKind::MarkGeometry => "mark_geometry",
            ProblemKind::Timeout => "timeout",
            ProblemKind::TooLarge => "too_large",
            ProblemKind::UnsupportedContent => "unsupported_content",
            ProblemKind::Unrecognized => "unrecognized",
        })
        .collect();
    names.join(",")
}

fn ok(verdict: &CheckVerdict) -> Response {
    ([(CACHE_CONTROL, "no-store")], axum::Json(verdict)).into_response()
}

fn error(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::Router;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use contract::api::dashboard::check::{CHECK_ROUTE, CheckReport};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    use super::*;

    /// A check that waits until released, and records what it was given.
    #[derive(Default)]
    struct Scripted {
        started: Notify,
        release: Notify,
        cancelled: Notify,
        runs: Mutex<Vec<CheckRequest>>,
        fail: bool,
    }

    impl Run for Scripted {
        fn run(
            &self,
            request: CheckRequest,
            cancelled: oneshot::Receiver<()>,
        ) -> Pin<Box<dyn Future<Output = Result<Outcome, CheckError>> + Send + '_>> {
            Box::pin(async move {
                self.runs.lock().expect("lock").push(request);
                self.started.notify_one();
                tokio::select! {
                    () = self.release.notified() => {}
                    _ = cancelled => {
                        self.cancelled.notify_one();
                        return Err(CheckError::Cancelled);
                    }
                }
                if self.fail {
                    return Err(CheckError::Browser("test"));
                }
                Ok(Outcome {
                    verdict: CheckVerdict::Passed {
                        report: CheckReport::default(),
                    },
                    timings: Timings::default(),
                })
            })
        }
    }

    fn router(checks: Arc<Scripted>, max_waiting: usize, max_wait: Duration) -> Router {
        router_of(Arc::new(Api::new(checks, max_waiting, max_wait)))
    }

    fn router_of(api: Arc<Api>) -> Router {
        Router::new()
            .route(CHECK_ROUTE, post(check))
            .with_state(api)
    }

    /// Waits until `count` requests hold a place (running or waiting).
    async fn admitted(api: &Api, count: usize) {
        let held = tokio::time::timeout(Duration::from_secs(60), async {
            while api.admission.admitted.load(Ordering::Acquire) != count {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(held.is_ok(), "waited for {count} admitted requests");
    }

    fn request_body() -> String {
        include_str!("../../../crates/contract/tests/fixtures/dashboard/check-request.json")
            .to_owned()
    }

    fn post_check(body: impl Into<Body>) -> Request<Body> {
        Request::post(CHECK_ROUTE)
            .header("content-type", "application/json")
            .body(body.into())
            .expect("request")
    }

    async fn answer(router: Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.oneshot(request).await.expect("responds");
        let status = response.status();
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(|value| value.as_bytes()),
            Some(&b"no-store"[..])
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&body).expect("JSON"))
    }

    const LONG: Duration = Duration::from_secs(600);

    #[tokio::test]
    async fn a_valid_request_gets_the_verdict() {
        let checks = Arc::new(Scripted::default());
        let router = router(checks.clone(), 2, LONG);
        let waiting = tokio::spawn(answer(router, post_check(request_body())));
        checks.started.notified().await;
        checks.release.notify_one();
        let (status, body) = waiting.await.expect("joins");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"verdict": "passed", "report": {"marks": []}}));
        let expected: CheckRequest = serde_json::from_str(&request_body()).expect("fixture");
        assert_eq!(*checks.runs.lock().expect("lock"), vec![expected]);
    }

    #[tokio::test]
    async fn malformed_requests_are_refused_before_any_check() {
        let checks = Arc::new(Scripted::default());
        let mut fixture: Value = serde_json::from_str(&request_body()).expect("fixture");
        let mut unknown_runtime = fixture.clone();
        unknown_runtime["runtime"] = json!(99);
        let mut empty_page = fixture.clone();
        empty_page["page"] = json!("  ");
        let mut no_datasets = fixture.clone();
        no_datasets["datasets"] = json!([]);
        // Unknown fields are allowed in internal bodies.
        fixture["later"] = json!(true);
        let oversized = vec![b' '; MAX_CHECK_REQUEST_BYTES + 1];
        for (name, body, status) in [
            ("not json", Body::from("{"), StatusCode::BAD_REQUEST),
            (
                "unknown runtime",
                Body::from(unknown_runtime.to_string()),
                StatusCode::BAD_REQUEST,
            ),
            (
                "empty page",
                Body::from(empty_page.to_string()),
                StatusCode::BAD_REQUEST,
            ),
            (
                "no datasets",
                Body::from(no_datasets.to_string()),
                StatusCode::BAD_REQUEST,
            ),
            ("too large", Body::from(oversized), StatusCode::BAD_REQUEST),
        ] {
            let (got, body) = answer(router(checks.clone(), 2, LONG), post_check(body)).await;
            assert_eq!(got, status, "{name}");
            assert_eq!(body["code"], "invalid_request", "{name}");
        }
        assert!(checks.runs.lock().expect("lock").is_empty());
        // The extra field passes.
        let waiting = tokio::spawn(answer(
            router(checks.clone(), 2, LONG),
            post_check(fixture.to_string()),
        ));
        checks.started.notified().await;
        checks.release.notify_one();
        assert_eq!(waiting.await.expect("joins").0, StatusCode::OK);
    }

    #[tokio::test]
    async fn one_check_runs_a_few_wait_and_the_rest_are_busy() {
        let checks = Arc::new(Scripted::default());
        let api = Arc::new(Api::new(checks.clone(), 1, LONG));
        let router = router_of(api.clone());
        let first = tokio::spawn(answer(router.clone(), post_check(request_body())));
        checks.started.notified().await;
        let second = tokio::spawn(answer(router.clone(), post_check(request_body())));
        admitted(&api, 2).await;
        let (status, third) = answer(router.clone(), post_check(request_body())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(third["code"], "unavailable");
        assert_eq!(checks.runs.lock().expect("lock").len(), 1);
        checks.release.notify_one();
        assert_eq!(first.await.expect("joins").0, StatusCode::OK);
        checks.started.notified().await;
        checks.release.notify_one();
        assert_eq!(second.await.expect("joins").0, StatusCode::OK);
        assert_eq!(checks.runs.lock().expect("lock").len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_waits_too_long_is_busy() {
        let checks = Arc::new(Scripted::default());
        let api = Arc::new(Api::new(checks.clone(), 1, Duration::from_secs(10)));
        let router = router_of(api.clone());
        let first = tokio::spawn(answer(router.clone(), post_check(request_body())));
        checks.started.notified().await;
        // With the clock paused, the wait's limit passes as soon as nothing else can run.
        let (status, body) = answer(router.clone(), post_check(request_body())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "unavailable");
        checks.release.notify_one();
        assert_eq!(first.await.expect("joins").0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_dropped_request_cancels_its_check() {
        let checks = Arc::new(Scripted::default());
        let router = router(checks.clone(), 0, LONG);
        let first = tokio::spawn(answer(router.clone(), post_check(request_body())));
        checks.started.notified().await;
        first.abort();
        checks.cancelled.notified().await;
        // Its turn came back: the next check runs.
        let next = tokio::spawn(answer(router, post_check(request_body())));
        checks.started.notified().await;
        checks.release.notify_one();
        assert_eq!(next.await.expect("joins").0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_failed_check_is_internal_without_details() {
        let checks = Arc::new(Scripted {
            fail: true,
            ..Scripted::default()
        });
        let waiting = tokio::spawn(answer(
            router(checks.clone(), 0, LONG),
            post_check(request_body()),
        ));
        checks.started.notified().await;
        checks.release.notify_one();
        let (status, body) = waiting.await.expect("joins");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            json!({"code": "internal", "message_key": "error.internal"})
        );
    }

    #[test]
    fn kinds_name_only_the_kinds() {
        let problems = vec![
            Problem::ScriptError {
                message: Some("secret value 42".to_owned()),
            },
            Problem::Navigated,
        ];
        assert_eq!(kinds(&problems), "script_error,navigated");
    }
}
