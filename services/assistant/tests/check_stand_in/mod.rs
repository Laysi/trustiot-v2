//! A stand-in for the dashboard check service (services/dashboard-check) on a loopback socket:
//! it keeps every request and answers as the test scripts it, by default with a passing verdict
//! whose report shows each dataset's row count, which `CheckVerdict::validate` accepts.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the stand-in"
)]

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::dashboard::check::{CHECK_ROUTE, CheckRequest};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How the stand-in answers a request: a status and a body, sent as JSON.
pub type CheckScript = Arc<dyn Fn(&CheckRequest) -> (u16, String) + Send + Sync>;

struct CheckState {
    /// Every request, as sent.
    requests: Mutex<Vec<Value>>,
    script: Mutex<Option<CheckScript>>,
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
