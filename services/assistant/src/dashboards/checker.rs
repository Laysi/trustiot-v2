//! The client of the dashboard check (services/dashboard-check; the API is
//! `contract::api::dashboard::check`): the `dashboard` tool sends a version's page, its public
//! spec and its datasets' results, and writes the version only when the verdict passes.
//!
//! - Called as the assistant: the request carries no identity, and the mesh lets only the
//!   assistant call the service (services/dashboard-check/README.md, 「威脅模型」).
//! - Every way of not getting a usable verdict is [`CheckOutcome::Unavailable`] with a closed
//!   reason ([`Unavailable`]): the service is busy (503), cannot be reached, does not answer in
//!   [`CHECK_CALL_TIMEOUT`], refuses the request (400), fails (500, any other status), or answers
//!   with something that is not a verdict or breaks the contract's rules
//!   ([`CheckVerdict::validate`], which also bounds what of the page a problem may carry). No
//!   text of the service's answer reaches the model or the logs, and the tool writes nothing on
//!   any of them.
//! - Plain HTTP inside the mesh, where ztunnel adds mutual TLS; no proxy from the environment, no
//!   redirects, like [`crate::callee`].
//! - At most [`MAX_CHECKS_IN_FLIGHT`] checks of this replica at a time, from building the request
//!   body to the validated verdict, taken in the order asked for: a check waits up to
//!   [`CHECK_QUEUE_WAIT`] for a place and is [`Unavailable::Busy`] after it, without calling the
//!   service. One tenant has at most one check among them: the `dashboard` tool takes the
//!   tenant's write slot first ([`crate::tools::dashboard`]).

use std::sync::Arc;
use std::time::Duration;

use contract::api::dashboard::check::{
    CHECK_ROUTE, CheckReport, CheckRequest, CheckVerdict, MAX_CHECK_REQUEST_BYTES, Problem,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::limits::{self, json_len};
use crate::callee::{read_limited, status_class};
use crate::tools::{ToolContext, millis};

/// How long one check may take, from sending to the verdict's last byte: the longest the service
/// may hold a request (`queue.max_wait`, at most 60 seconds, plus the 15-second check and
/// 5 seconds of clean-up; services/dashboard-check/README.md, 「上限」) and a margin.
pub const CHECK_CALL_TIMEOUT: Duration = Duration::from_secs(85);

/// The longest a connection to the service may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest verdict read: `MAX_MARKS` marks, each a reference and its value.
pub const MAX_VERDICT_BYTES: usize = 4 << 20;

/// The most checks of this replica at a time: the one the service runs and one waiting in its
/// queue. Each holds its request body (at most `MAX_CHECK_REQUEST_BYTES`, measured before it is
/// built and allocated at that size) and its verdict (at most [`MAX_VERDICT_BYTES`]).
pub const MAX_CHECKS_IN_FLIGHT: usize = 2;

/// How long a check waits for one of the [`MAX_CHECKS_IN_FLIGHT`] places before it is
/// [`Unavailable::Busy`]: a check takes a few seconds, at most the service's 15-second limit and
/// its clean-up.
pub const CHECK_QUEUE_WAIT: Duration = Duration::from_secs(30);

/// The check service's client. Cheap to clone; the clones share the places.
#[derive(Clone, Debug)]
pub struct Checker {
    http: reqwest::Client,
    url: reqwest::Url,
    timeout: Duration,
    places: Arc<Semaphore>,
    queue_wait: Duration,
}

/// The client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum CheckerConfigError {
    #[error("dashboard_check.url is invalid: {0}")]
    Url(&'static str),
    #[error("the HTTP client for the dashboard check could not be built")]
    Client(#[source] reqwest::Error),
}

/// What a check gave.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckOutcome {
    /// The page passed; every mark it showed with its value.
    Passed(CheckReport),
    /// The page failed: 1 to `MAX_PROBLEMS` problems of the contract's closed kinds.
    Failed(Vec<Problem>),
    /// No usable verdict.
    Unavailable(Unavailable),
}

/// Why a check gave no usable verdict: a closed set, for the model and the logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The service is running another check and its queue is full, or the wait was too long
    /// (503); or no place came free in this replica within [`CHECK_QUEUE_WAIT`], or the
    /// question's tenant has another write in its check (`dashboard` tool).
    Busy,
    /// The service could not be reached.
    Unreachable,
    /// No verdict within [`CHECK_CALL_TIMEOUT`].
    Timeout,
    /// The service refused the request as malformed (400), or the request broke the contract's
    /// rules before it was sent: a fault of the assistant, not of the page.
    Refused,
    /// The service failed (500, or any other status).
    Failed,
    /// The answer is not a verdict, is too large, or breaks [`CheckVerdict::validate`].
    InvalidVerdict,
}

impl Unavailable {
    /// The reason's name, for the logs and the model.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "check_busy",
            Self::Unreachable => "check_unreachable",
            Self::Timeout => "check_timeout",
            Self::Refused => "check_refused",
            Self::Failed => "check_failed",
            Self::InvalidVerdict => "check_invalid_verdict",
        }
    }
}

impl Checker {
    /// A client of the service at `base_url` (`dashboard_check.url`), with `timeout` per call
    /// ([`CHECK_CALL_TIMEOUT`] in the service), [`MAX_CHECKS_IN_FLIGHT`] places and
    /// [`CHECK_QUEUE_WAIT`].
    ///
    /// # Errors
    ///
    /// When `base_url` is not a plain HTTP origin, or the HTTP client cannot be built.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, CheckerConfigError> {
        crate::config::check_service_url(base_url).map_err(CheckerConfigError::Url)?;
        let mut url = reqwest::Url::parse(base_url)
            .map_err(|_| CheckerConfigError::Url("is not an absolute URL"))?;
        url.set_path(CHECK_ROUTE);
        let http = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(CheckerConfigError::Client)?;
        Ok(Self {
            http,
            url,
            timeout,
            places: Arc::new(Semaphore::new(MAX_CHECKS_IN_FLIGHT)),
            queue_wait: CHECK_QUEUE_WAIT,
        })
    }

    /// This client with `places` checks at a time (at least one) and `queue_wait` for a place:
    /// tests use fewer and shorter.
    #[must_use]
    pub fn with_places(mut self, places: usize, queue_wait: Duration) -> Self {
        self.places = Arc::new(Semaphore::new(places.max(1)));
        self.queue_wait = queue_wait;
        self
    }

    /// The longest one check takes: the wait for a place, then the call's `timeout`.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.queue_wait.saturating_add(self.timeout)
    }

    /// Checks `request` for the question of `context`, and logs one line with the outcome.
    pub async fn check(&self, context: &ToolContext, request: &CheckRequest) -> CheckOutcome {
        let started = Instant::now();
        let (outcome, status) = match limits::take(Arc::clone(&self.places), self.queue_wait).await
        {
            // The place is held until the verdict is validated: the body and the verdict are
            // both in memory until then.
            Some(_place) => self.send(request).await,
            None => (CheckOutcome::Unavailable(Unavailable::Busy), None),
        };
        let (name, problems) = match &outcome {
            CheckOutcome::Passed(_) => ("passed", 0),
            CheckOutcome::Failed(problems) => ("failed", problems.len()),
            CheckOutcome::Unavailable(reason) => (reason.as_str(), 0),
        };
        if matches!(outcome, CheckOutcome::Unavailable(_)) {
            tracing::warn!(
                run_id = %context.run_id,
                conversation_id = %context.conversation_id,
                tenant_id = %context.principal.tenant_id,
                outcome = name,
                status_class = status.map(status_class),
                duration_ms = millis(started.elapsed()),
                "a dashboard check gave no verdict"
            );
        } else {
            tracing::info!(
                run_id = %context.run_id,
                conversation_id = %context.conversation_id,
                tenant_id = %context.principal.tenant_id,
                outcome = name,
                problems,
                duration_ms = millis(started.elapsed()),
                "dashboard check finished"
            );
        }
        outcome
    }

    async fn send(&self, request: &CheckRequest) -> (CheckOutcome, Option<u16>) {
        let unavailable = |reason, status| (CheckOutcome::Unavailable(reason), status);
        if request.validate().is_err() {
            return unavailable(Unavailable::Refused, None);
        }
        // Measured first, so a body too large is never built and the one sent takes no more
        // than its size.
        let size = json_len(request);
        if size > MAX_CHECK_REQUEST_BYTES {
            return unavailable(Unavailable::Refused, None);
        }
        let mut body = Vec::with_capacity(size);
        if serde_json::to_writer(&mut body, request).is_err() {
            return unavailable(Unavailable::Refused, None);
        }
        let response = match self
            .http
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return unavailable(Unavailable::Timeout, None),
            Err(_) => return unavailable(Unavailable::Unreachable, None),
        };
        let status = response.status().as_u16();
        match status {
            200 => {}
            503 => return unavailable(Unavailable::Busy, Some(status)),
            400 => return unavailable(Unavailable::Refused, Some(status)),
            _ => return unavailable(Unavailable::Failed, Some(status)),
        }
        let body = match read_limited(response, MAX_VERDICT_BYTES).await {
            Ok(body) => body,
            Err("timeout") => return unavailable(Unavailable::Timeout, Some(status)),
            Err("too_large") => return unavailable(Unavailable::InvalidVerdict, Some(status)),
            Err(_) => return unavailable(Unavailable::Unreachable, Some(status)),
        };
        let Ok(verdict) = serde_json::from_slice::<CheckVerdict>(&body) else {
            return unavailable(Unavailable::InvalidVerdict, Some(status));
        };
        if verdict.validate(request).is_err() {
            return unavailable(Unavailable::InvalidVerdict, Some(status));
        }
        let outcome = match verdict {
            CheckVerdict::Passed { report } => CheckOutcome::Passed(report),
            CheckVerdict::Failed { problems } => CheckOutcome::Failed(problems),
        };
        (outcome, Some(status))
    }
}
