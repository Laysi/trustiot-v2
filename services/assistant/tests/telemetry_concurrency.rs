//! Telemetry tool calls and the tenant account's queries at once (Laysi/trustiot-v2#1122), through
//! the tool registry on loopback, against a stand-in telemetry query service.
//!
//! The tenant's ClickHouse account runs at most 4 queries at once. Before this issue, the
//! telemetry query service answered a 5th with 429 `rate_limited` and no more, and a step of the
//! model with 6 or more telemetry calls ended with some of them `failed` (the nightly smoke of
//! 2026-11-05). The stand-in refuses as that service did: a request that arrives while 4 are in
//! flight gets a 429 without a limit. It holds every request it accepts until a given number are
//! in flight at once, or every call of the step has arrived, so the calls overlap as far as the
//! assistant lets them; no test waits for a fixed time.
//!
//! - A step of 8 telemetry calls (latest values and SQL) runs at most 3 at once (the tenant's
//!   slots, `TELEMETRY_CALLS_PER_TENANT`) and every call answers: none is refused or `failed`.
//! - The same step through a client without slots (the control) has 4 calls refused and `failed`:
//!   the stand-in refuses as the service did, so the first test would catch the slots missing.
//! - A 429 that names its limit reaches the model by its limit: a busy service asks to call again
//!   in the next step (outcome `busy`), a used-up quota not to call again in this question
//!   (`limit_used_up`); nothing of the body reaches the model or the logs.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use assistant::callee::Callee;
use assistant::callee::slots::TELEMETRY_CALLS_PER_TENANT;
use assistant::model::{BoxFuture, ToolCall};
use assistant::tools::telemetry::TelemetryLatest;
use assistant::tools::telemetry_sql::TelemetrySql;
use assistant::tools::{
    Delegation, DelegationError, Tool, ToolContent, ToolContext, ToolOutput, ToolRegistry,
};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use contract::api::telemetry_query::{QueryableFrom, RunSql, SqlColumn, SqlResult};
use contract::client::Endpoint as _;
use contract::{Actor, InternalToken, ServiceName, TenantGrant, TenantRole};
use serde_json::json;
use support::{Capture, WAIT, limits, principal};
use telemetry_query::history::{LATEST_PATH, Latest, LatestPoint, Reading};
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;
use tokio::time::Instant;
use uuid::Uuid;

/// The tenant account's queries at once (`max_concurrent_queries_for_user` of the settings
/// profile `trustiot_tenant_reader`).
const ACCOUNT_QUERIES: usize = 4;

/// The calls of one step: the most a step runs (`limits().max_calls_per_step`).
const STEP_CALLS: usize = 8;

/// The longest the stand-in holds a request for the others to arrive; reaching it fails the
/// test with a message, it is never part of a passing run.
const HOLD_BOUND: Duration = Duration::from_secs(120);

/// In every error body the stand-in sends: it must reach neither the model nor the logs.
const SECRET: &str = "t1122-service-secret";

/// The question's delegation: a token of the user's tenant, exchanged by the assistant.
struct Tokens {
    issuer: TestIssuer,
    tenant: TenantGrant,
}

impl Delegation for Tokens {
    fn token_for(
        &self,
        audience: ServiceName,
    ) -> BoxFuture<'_, Result<InternalToken, DelegationError>> {
        let mut claims = TestIssuer::claims(audience, Some(self.tenant));
        claims.act = Some(Actor {
            sub: ServiceName::Assistant,
        });
        let token = InternalToken::new(self.issuer.token(&claims))
            .map_err(|_| DelegationError::Unavailable);
        Box::pin(async move { token })
    }
}

fn context() -> ToolContext {
    let principal = principal();
    ToolContext {
        principal,
        conversation_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        delegation: Arc::new(Tokens {
            issuer: TestIssuer::new(),
            tenant: TenantGrant {
                id: principal.tenant_id,
                role: TenantRole::Viewer,
            },
        }),
    }
}

/// How the stand-in answers.
#[derive(Clone)]
enum Mode {
    /// As the telemetry query service answered before Laysi/trustiot-v2#1122: a request that
    /// arrives while [`ACCOUNT_QUERIES`] are in flight is refused with a 429 without a limit; an
    /// accepted one is held until `hold_until` are in flight at once or `total` have arrived.
    Account { hold_until: usize, total: usize },
    /// Every request gets this status and body.
    Reply(u16, String),
}

/// What the stand-in saw.
#[derive(Default)]
struct Seen {
    arrived: usize,
    in_flight: usize,
    most_in_flight: usize,
    refused: usize,
    /// A request was held longer than [`HOLD_BOUND`].
    stalled: bool,
}

struct StandIn {
    mode: Mutex<Mode>,
    seen: Mutex<Seen>,
    answered: AtomicUsize,
}

impl StandIn {
    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mode(&self) -> Mode {
        self.mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Leaves the in-flight count when the request ends, also when the client went away.
struct InFlight(Arc<StandIn>);

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut seen = self.0.seen();
        seen.in_flight = seen.in_flight.saturating_sub(1);
    }
}

/// Takes the request in and holds it, or refuses it as the service did.
async fn admit(stand_in: &Arc<StandIn>) -> Result<InFlight, Box<Response>> {
    let mode = stand_in.mode();
    let (hold_until, total) = match mode {
        Mode::Reply(status, body) => {
            return Err(Box::new(
                (
                    StatusCode::from_u16(status).expect("a status"),
                    [("content-type", "application/json")],
                    body,
                )
                    .into_response(),
            ));
        }
        Mode::Account { hold_until, total } => (hold_until, total),
    };
    let held = {
        let mut seen = stand_in.seen();
        seen.arrived += 1;
        seen.in_flight += 1;
        seen.most_in_flight = seen.most_in_flight.max(seen.in_flight);
        let held = InFlight(Arc::clone(stand_in));
        if seen.in_flight > ACCOUNT_QUERIES {
            seen.refused += 1;
            drop(seen);
            drop(held);
            return Err(Box::new(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("content-type", "application/json")],
                    json!({"code": "rate_limited", "message_key": "error.rate_limited"})
                        .to_string(),
                )
                    .into_response(),
            ));
        }
        held
    };
    let deadline = Instant::now() + HOLD_BOUND;
    loop {
        {
            let mut seen = stand_in.seen();
            if seen.in_flight >= hold_until || seen.arrived >= total {
                break;
            }
            if Instant::now() >= deadline {
                seen.stalled = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(held)
}

async fn latest(State(stand_in): State<Arc<StandIn>>, Path(device): Path<Uuid>) -> Response {
    let _held = match admit(&stand_in).await {
        Ok(held) => held,
        Err(refusal) => return *refusal,
    };
    stand_in.answered.fetch_add(1, Ordering::SeqCst);
    let at = |text: &str| text.parse().expect("a time");
    axum::Json(Latest {
        device_id: device,
        earliest: at("2026-10-31T00:00:00Z"),
        points: vec![LatestPoint {
            point: "temperature".to_owned(),
            time: at("2026-11-07T00:00:00Z"),
            value: Reading::Float(Some(21.5)),
        }],
    })
    .into_response()
}

async fn sql(State(stand_in): State<Arc<StandIn>>) -> Response {
    let _held = match admit(&stand_in).await {
        Ok(held) => held,
        Err(refusal) => return *refusal,
    };
    stand_in.answered.fetch_add(1, Ordering::SeqCst);
    let at = |text: &str| text.parse().expect("a time");
    axum::Json(SqlResult {
        sql: "SELECT count() FROM trustiot.tenant_telemetry".to_owned(),
        columns: vec![SqlColumn {
            name: "readings".to_owned(),
            data_type: "UInt64".to_owned(),
        }],
        rows: vec![vec![json!(42)]],
        queryable_from: QueryableFrom {
            tenant_telemetry: at("2026-10-31T00:05:00Z"),
            tenant_telemetry_1m: at("2026-08-07T00:06:00Z"),
            tenant_telemetry_1h: at("2026-08-07T01:00:00Z"),
        },
        queried_at: at("2026-11-07T00:00:00Z"),
    })
    .into_response()
}

async fn serve(mode: Mode) -> (String, Arc<StandIn>) {
    let stand_in = Arc::new(StandIn {
        mode: Mutex::new(mode),
        seen: Mutex::new(Seen::default()),
        answered: AtomicUsize::new(0),
    });
    let router = Router::new()
        .route(LATEST_PATH, get(latest))
        .route(RunSql::ROUTE, post(sql))
        .with_state(Arc::clone(&stand_in));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let origin = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the server runs");
    });
    (origin, stand_in)
}

/// The telemetry tools on one client, as the service builds them (`src/lib.rs`).
fn registry(telemetry: &Callee) -> ToolRegistry {
    let tools: Vec<Arc<dyn Tool>> = vec![
        TelemetryLatest::new(telemetry.clone()),
        TelemetrySql::new(telemetry.clone()),
    ];
    ToolRegistry::new(tools, limits()).expect("the tools")
}

/// A step of `STEP_CALLS` calls, latest values and SQL in turn.
fn step() -> Vec<ToolCall> {
    (0..STEP_CALLS)
        .map(|index| {
            if index % 2 == 0 {
                ToolCall {
                    id: format!("call-{index}"),
                    name: TelemetryLatest::NAME.into(),
                    input: json!({"device_id": Uuid::now_v7().to_string()}),
                }
            } else {
                ToolCall {
                    id: format!("call-{index}"),
                    name: TelemetrySql::NAME.into(),
                    input: json!({"sql": "SELECT count() FROM tenant_telemetry"}),
                }
            }
        })
        .collect()
}

fn text_of(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .map(|part| match part {
            ToolContent::Text(text) => text.clone(),
            ToolContent::Image { .. } => String::new(),
        })
        .collect()
}

/// The `outcome` of every `tool call completed` line, in the order they were logged.
fn outcomes(capture: &Capture) -> Vec<String> {
    capture
        .events()
        .iter()
        .filter(|event| event.message == "tool call completed")
        .filter_map(|event| event.fields.get("outcome").cloned())
        .collect()
}

#[allow(clippy::print_stderr, reason = "prints the counts for the test report")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_step_of_more_telemetry_calls_than_the_account_runs_at_once_has_no_failed_call() {
    let (capture, _guard) = Capture::install();
    let (origin, stand_in) = serve(Mode::Account {
        hold_until: TELEMETRY_CALLS_PER_TENANT,
        total: STEP_CALLS,
    })
    .await;
    let telemetry = Callee::new(ServiceName::TelemetryQuery, &origin, WAIT).expect("a client");
    assert_eq!(
        telemetry.call_slots().map(|slots| slots.per_tenant()),
        Some(TELEMETRY_CALLS_PER_TENANT)
    );
    const { assert!(TELEMETRY_CALLS_PER_TENANT < ACCOUNT_QUERIES) };
    let registry = registry(&telemetry);
    let context = context();
    let calls = step();
    let results = registry
        .run(&context, &calls)
        .await
        .expect("no access revoked");

    for (call, result) in calls.iter().zip(&results) {
        let text = text_of(&result.output);
        assert!(!result.output.is_error, "{}: {text}", call.name);
        let expected = if call.name == TelemetryLatest::NAME {
            "temperature"
        } else {
            "\"row_count\":1"
        };
        assert!(text.contains(expected), "{}: {text}", call.name);
    }
    let seen = stand_in.seen();
    assert!(
        !seen.stalled,
        "a request waited {HOLD_BOUND:?} for the others"
    );
    assert_eq!(seen.refused, 0, "no call was refused");
    assert_eq!(seen.arrived, STEP_CALLS);
    assert_eq!(
        seen.most_in_flight, TELEMETRY_CALLS_PER_TENANT,
        "the calls ran as many at once as the tenant's slots allow, and no more"
    );
    assert_eq!(stand_in.answered.load(Ordering::SeqCst), STEP_CALLS);
    let outcomes = outcomes(&capture);
    assert_eq!(outcomes, vec!["ok"; STEP_CALLS], "{outcomes:?}");
    eprintln!(
        "{STEP_CALLS} telemetry calls in one step: {} answered, at most {} at once, {} refused, \
         outcomes {outcomes:?}",
        stand_in.answered.load(Ordering::SeqCst),
        seen.most_in_flight,
        seen.refused
    );
}

/// The control: the same step through a client without slots (one built for another service,
/// which the stand-in serves all the same) runs every call at once, and the stand-in refuses the
/// calls beyond the account's queries as the service did. The model saw that as before
/// Laysi/trustiot-v2#1122: `telemetry_latest` as `failed`, and `telemetry_sql` as its message for
/// a 429, which told it not to query again in this question.
#[allow(clippy::print_stderr, reason = "prints the counts for the test report")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_slots_the_same_step_has_calls_refused_as_before() {
    let (capture, _guard) = Capture::install();
    let (origin, stand_in) = serve(Mode::Account {
        hold_until: STEP_CALLS,
        total: STEP_CALLS,
    })
    .await;
    let unlimited = Callee::new(ServiceName::Assets, &origin, WAIT).expect("a client");
    assert!(unlimited.call_slots().is_none());
    let registry = registry(&unlimited);
    let calls = step();
    let results = registry
        .run(&context(), &calls)
        .await
        .expect("no access revoked");
    let mut refused_as_before = 0;
    for (call, result) in calls.iter().zip(&results) {
        if !result.output.is_error {
            continue;
        }
        let text = text_of(&result.output);
        let before_1122 = if call.name == TelemetrySql::NAME {
            "Do not call telemetry_sql again"
        } else {
            "The tool failed"
        };
        assert!(text.contains(before_1122), "{}: {text}", call.name);
        refused_as_before += 1;
    }
    let seen = stand_in.seen();
    assert!(
        !seen.stalled,
        "a request waited {HOLD_BOUND:?} for the others"
    );
    assert_eq!(seen.refused, STEP_CALLS - ACCOUNT_QUERIES);
    assert_eq!(refused_as_before, STEP_CALLS - ACCOUNT_QUERIES);
    let outcomes = outcomes(&capture);
    assert_eq!(
        outcomes.iter().filter(|outcome| *outcome != "ok").count(),
        STEP_CALLS - ACCOUNT_QUERIES,
        "{outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|outcome| outcome == "failed"),
        "{outcomes:?}"
    );
    eprintln!(
        "control without slots: {STEP_CALLS} calls, at most {} at once, {} refused, outcomes \
         {outcomes:?}",
        seen.most_in_flight, seen.refused
    );
}

/// A 429 that names its limit (Laysi/trustiot-v2#1122) reaches the model by its limit, through
/// the GET of `telemetry_latest` and the POST of `telemetry_sql` alike: only a used-up quota
/// tells the model not to call again in this question.
#[allow(
    clippy::print_stderr,
    reason = "prints what the model got, for the test report"
)]
#[tokio::test]
async fn a_concurrency_refusal_asks_to_call_again_and_only_the_quota_says_stop() {
    let (capture, _guard) = Capture::install();
    let (origin, stand_in) = serve(Mode::Reply(200, String::new())).await;
    let telemetry = Callee::new(ServiceName::TelemetryQuery, &origin, WAIT).expect("a client");
    let registry = registry(&telemetry);
    let context = context();
    let named = |limit: &str| {
        json!({
            "code": "rate_limited",
            "message_key": "error.rate_limited",
            "details": {"limit": limit},
        })
        .to_string()
    };
    let stop = "Do not call";
    let cases: Vec<(&str, String, &str)> = vec![
        ("a busy service", named("concurrency"), "busy"),
        ("a used-up quota", named("quota"), "limit_used_up"),
        (
            "a limit this version does not know",
            named(SECRET),
            "unnamed",
        ),
    ];
    for (name, body, expected) in &cases {
        *stand_in.mode.lock().unwrap_or_else(PoisonError::into_inner) =
            Mode::Reply(429, body.clone());
        for call in step().into_iter().take(2) {
            let before = outcomes(&capture).len();
            let results = registry
                .run(&context, std::slice::from_ref(&call))
                .await
                .expect("no access revoked");
            let output = &results.first().expect("one result").output;
            let text = text_of(output);
            let outcome = outcomes(&capture)
                .get(before)
                .cloned()
                .expect("the call is logged");
            assert!(output.is_error, "{name} {}: {text}", call.name);
            assert!(!text.contains(SECRET), "{name} {}: {text}", call.name);
            match *expected {
                "busy" => {
                    assert_eq!(outcome, "busy", "{name} {}", call.name);
                    assert!(!text.contains(stop), "{name} {}: {text}", call.name);
                    assert!(text.contains("Call the tool again"), "{text}");
                }
                "limit_used_up" => {
                    assert_eq!(outcome, "limit_used_up", "{name} {}", call.name);
                    assert!(text.contains(stop), "{name} {}: {text}", call.name);
                }
                _ => {
                    // As before Laysi/trustiot-v2#1122: the SQL tool's message for a 429, a
                    // failure for the others.
                    let before_1122 = if call.name == TelemetrySql::NAME {
                        "input_error"
                    } else {
                        "failed"
                    };
                    assert_eq!(outcome, before_1122, "{name} {}", call.name);
                }
            }
            eprintln!(
                "{name} ({}): outcome {outcome}; the model got: {text}",
                call.name
            );
        }
    }
    let logged: String = capture
        .events()
        .iter()
        .map(|event| format!("{} {:?}\n", event.message, event.fields))
        .collect();
    assert!(!logged.contains(SECRET), "{logged}");
}

/// Every value an `outcome` takes here is one the README lists, so a dashboard of the logs can
/// count them.
#[test]
fn the_new_outcomes_are_documented() {
    // Read at run time: an included file is checked as SQL by tools/sql-check, and the README
    // quotes SQL that sets the role.
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("the README");
    for outcome in ["`busy`", "`limit_used_up`"] {
        assert!(readme.contains(outcome), "README.md lists {outcome}");
    }
}
