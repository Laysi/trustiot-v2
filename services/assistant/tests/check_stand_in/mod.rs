//! A stand-in for the dashboard check service (services/dashboard-check) on a loopback socket:
//! it keeps every request and answers as the test scripts it, by default with a passing verdict
//! whose report shows each dataset's row count, which `CheckVerdict::validate` accepts.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the stand-in"
)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::dashboard::check::{CHECK_ROUTE, CheckRequest};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// The longest a test waits for the stand-in to receive a request: generous, for a loaded
/// machine.
const WAIT: Duration = Duration::from_secs(60);

/// How the stand-in answers a request: a status and a body, sent as JSON.
pub type CheckScript = Arc<dyn Fn(&CheckRequest) -> (u16, String) + Send + Sync>;

struct CheckState {
    /// Every request, as sent.
    requests: Mutex<Vec<Value>>,
    script: Mutex<Option<CheckScript>>,
    /// Requests whose page contains the text wait for the gate before they are answered.
    hold: Mutex<Option<(String, Arc<Semaphore>)>>,
}

/// Holds the requests [`CheckStandIn::hold`] names until it is released or dropped.
pub struct Held(Arc<Semaphore>);

impl Held {
    /// Answers the held requests and every later one.
    pub fn release(&self) {
        self.0.close();
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.release();
    }
}

/// The stand-in check service.
pub struct CheckStandIn {
    /// Its origin, as `dashboard_check.url` names it.
    pub url: String,
    state: Arc<CheckState>,
    task: JoinHandle<()>,
}

impl Drop for CheckStandIn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CheckStandIn {
    pub async fn start() -> Self {
        let state = Arc::new(CheckState {
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(None),
            hold: Mutex::new(None),
        });
        let router = Router::new()
            .route(CHECK_ROUTE, post(check))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the check stand-in");
        });
        Self {
            url: format!("http://{address}"),
            state,
            task,
        }
    }

    /// Answers with `script` from now on; `None` goes back to [`passed`].
    pub fn script(&self, script: Option<CheckScript>) {
        *lock(&self.state.script) = script;
    }

    /// Answers every request with `status` and `body` from now on.
    pub fn answer(&self, status: u16, body: &str) {
        let body = body.to_owned();
        self.script(Some(Arc::new(move |_| (status, body.clone()))));
    }

    pub fn requests(&self) -> Vec<Value> {
        lock(&self.state.requests).clone()
    }

    /// Waits until the stand-in received `count` requests.
    pub async fn wait_requests(&self, count: usize) {
        let deadline = Instant::now() + WAIT;
        while self.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "the check stand-in did not receive {count} requests within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// From now on, a request whose page contains `marker` is recorded and then waits until the
    /// returned [`Held`] is released or dropped.
    pub fn hold(&self, marker: &str) -> Held {
        let gate = Arc::new(Semaphore::new(0));
        *lock(&self.state.hold) = Some((marker.to_owned(), Arc::clone(&gate)));
        Held(gate)
    }
}

/// A passing verdict that reports every dataset's row count.
pub fn passed(request: &CheckRequest) -> (u16, String) {
    let marks: Vec<Value> = request
        .datasets
        .iter()
        .map(|result| {
            json!({
                "reference": format!("m|{}|row_count", result.dataset),
                "value": result.row_count,
            })
        })
        .collect();
    (
        200,
        json!({"verdict": "passed", "report": {"marks": marks}}).to_string(),
    )
}

async fn check(State(state): State<Arc<CheckState>>, body: Bytes) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "not JSON").into_response();
    };
    lock(&state.requests).push(value.clone());
    let Ok(request) = serde_json::from_value::<CheckRequest>(value) else {
        return (StatusCode::BAD_REQUEST, "not a check request").into_response();
    };
    let hold = lock(&state.hold).clone();
    if let Some((marker, gate)) = hold
        && request.page.contains(&marker)
    {
        // Closed when released: every waiter goes on.
        let _ = gate.acquire().await;
    }
    let script = lock(&state.script).clone();
    let (status, body) = match script {
        Some(script) => script(&request),
        None => passed(&request),
    };
    (
        StatusCode::from_u16(status).expect("a status"),
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}
