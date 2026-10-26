//! The read-only SQL tool (Laysi/trustiot-v2#143) as the model uses it, through the tool
//! registry, on loopback:
//!
//! - against the telemetry query service's real router (`telemetry_query::history::routes`),
//!   whose check refuses before anything reaches Postgres or ClickHouse: both are counting ports
//!   that must see no connection, so no container is needed;
//! - against a stand-in of the endpoint for answers, refusals after execution and failures,
//!   including bodies a service should never send.
//!
//! The model sees fixed text per refusal class and nothing of a service's error text; the logs
//! carry neither the SQL nor an error body.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use assistant::callee::Callee;
use assistant::model::{BoxFuture, ToolCall};
use assistant::tools::telemetry_sql::TelemetrySql;
use assistant::tools::{
    CUT_MARKER, Delegation, DelegationError, ToolContent, ToolContext, ToolOutput, ToolRegistry,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::telemetry_query::{
    DEFAULT_MAX_ROWS, MAX_SQL_BYTES, QueryableFrom, RunSql, SqlColumn, SqlResult,
};
use contract::client::Endpoint as _;
use contract::{Actor, InternalToken, ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use service_kit::tenant::TenantDb;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use support::{Capture, WAIT, limits, principal};
use telemetry_query::accounts::Accounts;
use telemetry_query::clickhouse::ClickHouse;
use telemetry_query::sql::lexer::MAX_NESTING;
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;

/// In every SQL the tests send: it must reach neither the model's text nor the logs.
const MARKER: &str = "t143sqlmarker";
/// In every error body the stand-in sends: it must reach neither the model nor the logs.
const SECRET: &str = "t143-service-secret";

/// The question's delegation: a token of the user's tenant, exchanged by the assistant (`act`),
/// for whichever service the tool calls; counts the tokens it gave.
struct Tokens {
    issuer: Arc<TestIssuer>,
    tenant: TenantGrant,
    issued: AtomicUsize,
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
        self.issued.fetch_add(1, Ordering::SeqCst);
        let token = InternalToken::new(self.issuer.token(&claims))
            .map_err(|_| DelegationError::Unavailable);
        Box::pin(async move { token })
    }
}

fn context(tokens: Arc<Tokens>) -> ToolContext {
    ToolContext {
        principal: principal(),
        conversation_id: uuid::Uuid::now_v7(),
        run_id: uuid::Uuid::now_v7(),
        delegation: tokens,
    }
}

fn tokens(issuer: &Arc<TestIssuer>) -> Arc<Tokens> {
    Arc::new(Tokens {
        issuer: Arc::clone(issuer),
        tenant: TenantGrant {
            id: TenantId::generate(),
            role: TenantRole::Viewer,
        },
        issued: AtomicUsize::new(0),
    })
}

/// A loopback port that accepts and drops connections, counting them.
async fn counting_port() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (port, count)
}

async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let origin = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the server runs");
    });
    origin
}

fn registry(origin: &str) -> ToolRegistry {
    let telemetry = Callee::new(ServiceName::TelemetryQuery, origin, WAIT).expect("a client");
    ToolRegistry::new(vec![TelemetrySql::new(telemetry)], limits()).expect("the tool")
}

/// One call of the tool through the registry, as the model makes it.
async fn ask(registry: &ToolRegistry, context: &ToolContext, input: Value) -> ToolOutput {
    let results = registry
        .run(
            context,
            &[ToolCall {
                id: "call-1".into(),
                name: "telemetry_sql".into(),
                input,
            }],
        )
        .await
        .expect("no access revoked");
    results.into_iter().next().expect("one result").output
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

/// At most 120 characters of `text`, for the printed evidence.
fn shortened(text: &str) -> String {
    match text.char_indices().nth(120) {
        Some((end, _)) => format!("{}…", text.get(..end).unwrap_or(text)),
        None => text.to_owned(),
    }
}

/// Every log event as one line of text.
fn logged(capture: &Capture) -> Vec<String> {
    capture
        .events()
        .iter()
        .map(|event| format!("{} {:?}", event.message, event.fields))
        .collect()
}

#[allow(
    clippy::print_stderr,
    reason = "prints what the model got, for the test report"
)]
#[tokio::test]
async fn refused_sql_reaches_the_model_as_its_class_and_never_the_databases() {
    let (capture, _guard) = Capture::install();
    let issuer = Arc::new(TestIssuer::new());
    let (postgres_port, postgres) = counting_port().await;
    let (clickhouse_port, clickhouse) = counting_port().await;
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(postgres_port)
        .username("telemetry_query_app")
        .database("trustiot")
        .ssl_mode(PgSslMode::VerifyFull);
    let db = TenantDb::connect_lazy(
        options,
        PgPoolOptions::new().acquire_timeout(Duration::from_secs(2)),
    )
    .expect("verify-full options");
    let accounts = Accounts::new(
        ClickHouse::new(&format!("http://127.0.0.1:{clickhouse_port}"), None)
            .expect("a ClickHouse client"),
        "trustiot_telemetry_query",
        &"p".repeat(32),
    );
    let origin = serve(telemetry_query::history::routes(
        issuer.verifier(ServiceName::TelemetryQuery),
        db,
        accounts,
    ))
    .await;
    let registry = registry(&origin);
    let tokens = tokens(&issuer);
    let context = context(Arc::clone(&tokens));

    let deep = format!(
        "SELECT {}1{} FROM tenant_telemetry",
        "(".repeat(300),
        ")".repeat(300)
    );
    // The model is told the limits the check applies.
    let levels = format!("at most {MAX_NESTING} levels of parentheses and brackets");
    let cases: Vec<(String, &str)> = vec![
        (
            "SELECT * FROM system.tables".to_owned(),
            "`system.tables` is not a table you may read",
        ),
        (
            "SELECT dictGet('d', 'a', toUInt64(1)) FROM tenant_telemetry".to_owned(),
            "The function `dictGet` is not allowed",
        ),
        (
            "SELECT * FROM url('http://example.com/x', 'CSV')".to_owned(),
            "`url` is not a table you may read",
        ),
        (
            "SET DEFAULT ROLE NONE TO CURRENT_USER".to_owned(),
            "Only a SELECT",
        ),
        ("SET ROLE NONE".to_owned(), "Only a SELECT"),
        (
            "SELECT 1 FROM tenant_telemetry; SELECT 2".to_owned(),
            "exactly one statement",
        ),
        (
            "SELECT point FROM tenant_telemetry SETTINGS readonly = 0".to_owned(),
            "`settings` is not allowed here",
        ),
        (
            "SELECT CAST(point AS Array(String)) FROM tenant_telemetry".to_owned(),
            "`cast_type` is not allowed here",
        ),
        (
            "SELECT toStartOfDay(ts) FROM tenant_telemetry".to_owned(),
            "`toStartOfDay` needs the time zone",
        ),
        ("SELECT 1".to_owned(), "reads none of tenant_telemetry"),
        (
            "SELECT `point` FROM tenant_telemetry".to_owned(),
            "has a token form that is not allowed",
        ),
        (
            "SELECT 'a\\b' FROM tenant_telemetry".to_owned(),
            "has a character that is not allowed",
        ),
        (
            "SELECT point FROM tenant_telemetry WHERE".to_owned(),
            "does not parse",
        ),
        (deep.clone(), "too many tokens or nests too deeply"),
        (deep, levels.as_str()),
        (
            "SELECT position(position(value_string, 'a'), 'b') FROM tenant_telemetry".to_owned(),
            "no position() inside another position()",
        ),
    ];
    let sent = cases.len();
    for (written, expected) in cases {
        let sql = format!("{written} -- {MARKER}");
        let output = ask(&registry, &context, json!({ "sql": sql })).await;
        let text = text_of(&output);
        assert!(output.is_error, "{sql}: {text}");
        assert!(text.contains(expected), "{sql}: {text}");
        assert!(!text.contains(MARKER), "{sql}: {text}");
        for service_text in ["validation_failed", "message_key", "details", "class"] {
            assert!(!text.contains(service_text), "{sql}: {text}");
        }
        eprintln!("refused {:?}\n  the model got: {text}", shortened(&written));
    }

    // Too long for the endpoint: refused by the tool, nothing sent.
    let long = format!(
        "SELECT point FROM tenant_telemetry WHERE point = '{}'",
        "x".repeat(usize::try_from(MAX_SQL_BYTES).expect("fits"))
    );
    let output = ask(&registry, &context, json!({ "sql": long })).await;
    assert!(output.is_error);
    assert!(
        text_of(&output).contains(&format!("longer than {MAX_SQL_BYTES} bytes")),
        "{}",
        text_of(&output)
    );
    assert_eq!(
        tokens.issued.load(Ordering::SeqCst),
        sent,
        "one fresh token per call sent, none for the SQL that is too long"
    );
    assert_eq!(postgres.load(Ordering::SeqCst), 0, "Postgres was reached");
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        0,
        "ClickHouse was reached"
    );

    // The control: a query the check passes goes on to the tenant's account in Postgres, which
    // fails here, and the model gets the fixed failure.
    let output = ask(
        &registry,
        &context,
        json!({ "sql": format!("SELECT count() FROM tenant_telemetry -- {MARKER}") }),
    )
    .await;
    assert!(output.is_error);
    assert!(
        text_of(&output).contains("failed for a reason outside your control"),
        "{}",
        text_of(&output)
    );
    assert!(
        postgres.load(Ordering::SeqCst) > 0,
        "Postgres was not reached"
    );
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        0,
        "ClickHouse was reached"
    );

    let lines = logged(&capture);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("refused an AI's SQL")),
        "{lines:#?}"
    );
    for line in &lines {
        assert!(!line.contains(MARKER), "the SQL was logged: {line}");
    }
}

/// What the stand-in answers next, and what it received.
#[derive(Default)]
struct Script {
    replies: Vec<(u16, String)>,
    bodies: Vec<Value>,
    /// The claims were for this service and exchanged by the assistant.
    refused_tokens: usize,
}

#[derive(Clone)]
struct StandIn {
    issuer: Arc<TestIssuer>,
    script: Arc<Mutex<Script>>,
}

impl StandIn {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn stand_in_sql(
    State(stand_in): State<StandIn>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let verified = stand_in
        .issuer
        .verifier(ServiceName::TelemetryQuery)
        .verify_headers(&headers)
        .ok()
        .filter(|user| {
            user.claims().act
                == Some(Actor {
                    sub: ServiceName::Assistant,
                })
        });
    let mut script = stand_in.script();
    if verified.is_none() {
        script.refused_tokens += 1;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    script
        .bodies
        .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    let (status, reply) = if script.replies.is_empty() {
        (500, String::new())
    } else {
        script.replies.remove(0)
    };
    (
        StatusCode::from_u16(status).expect("a status"),
        [(header::CONTENT_TYPE, "application/json")],
        reply,
    )
        .into_response()
}

fn answer(rows: Vec<Vec<Value>>) -> String {
    let at = |text: &str| text.parse().expect("a time");
    serde_json::to_string(&SqlResult {
        sql: format!("SELECT point FROM trustiot.tenant_telemetry -- {MARKER}"),
        columns: vec![
            SqlColumn {
                name: "point".to_owned(),
                data_type: "LowCardinality(String)".to_owned(),
            },
            SqlColumn {
                name: "readings".to_owned(),
                data_type: "UInt64".to_owned(),
            },
        ],
        rows,
        queryable_from: QueryableFrom {
            tenant_telemetry: at("2026-10-19T12:05:00Z"),
            tenant_telemetry_1m: at("2026-07-26T12:06:00Z"),
            tenant_telemetry_1h: at("2026-07-26T13:00:00Z"),
        },
        queried_at: at("2026-10-26T12:00:00Z"),
    })
    .expect("JSON")
}

fn refusal(details: &Value) -> String {
    json!({
        "code": "validation_failed",
        "message_key": "error.validation_failed",
        "details": details,
    })
    .to_string()
}

#[allow(
    clippy::print_stderr,
    reason = "prints what the model got, for the test report"
)]
#[tokio::test]
async fn answers_limits_and_failures_reach_the_model_as_fixed_text() {
    let (capture, _guard) = Capture::install();
    let issuer = Arc::new(TestIssuer::new());
    let stand_in = StandIn {
        issuer: Arc::clone(&issuer),
        script: Arc::new(Mutex::new(Script::default())),
    };
    let origin = serve(
        Router::new()
            .route(RunSql::ROUTE, post(stand_in_sql))
            .with_state(stand_in.clone()),
    )
    .await;
    let registry = registry(&origin);
    let tokens = tokens(&issuer);
    let context = context(Arc::clone(&tokens));
    let sql = format!(
        "SELECT point, count() AS readings FROM tenant_telemetry GROUP BY point -- {MARKER}"
    );

    // Input the tool refuses itself, before any call.
    for input in [
        json!({"sql": sql, "max_rows": 10_000}),
        json!({"sql": sql, "tenant_id": TenantId::generate().to_string()}),
        json!({"sql": "  "}),
        json!({"query": sql}),
    ] {
        let output = ask(&registry, &context, input.clone()).await;
        assert!(output.is_error, "{input}");
        assert!(
            text_of(&output).contains("The input has `sql` only"),
            "{input}: {}",
            text_of(&output)
        );
    }
    assert_eq!(tokens.issued.load(Ordering::SeqCst), 0);

    let long = "é".repeat(300);
    let too_many =
        vec![vec![json!("p"), json!(1)]; usize::try_from(DEFAULT_MAX_ROWS).expect("fits") + 1];
    let failed = "failed for a reason outside your control";
    let cases: Vec<(&str, u16, String, &str)> = vec![
        (
            "an answer",
            200,
            answer(vec![
                vec![json!("voltage"), json!(48)],
                vec![json!(long), json!(1)],
            ]),
            "\"row_count\":2",
        ),
        (
            "too many rows",
            422,
            refusal(&json!({"class": "too_many_rows", "max": 200})),
            "more than 200 rows",
        ),
        (
            "an unknown column",
            422,
            refusal(&json!({"class": "query_error", "kind": "unknown_column"})),
            "A column or alias does not exist",
        ),
        (
            "a heavy query",
            422,
            refusal(&json!({"class": "query_too_heavy"})),
            "too many rows or needed too much memory",
        ),
        (
            "a large answer",
            422,
            refusal(&json!({"class": "result_too_large", "max_bytes": 8_388_608})),
            "larger than 8388608 bytes",
        ),
        (
            "a refusal with a member the contract does not have",
            422,
            refusal(&json!({"class": "query_too_heavy", "message": SECRET})),
            failed,
        ),
        (
            "a class this version does not know",
            422,
            refusal(&json!({"class": "newer_class", "note": SECRET})),
            failed,
        ),
        (
            "a clause this version does not know",
            422,
            refusal(&json!({"class": "clause_not_allowed", "clause": "newer_clause"})),
            failed,
        ),
        (
            "a kind this version does not know",
            422,
            refusal(&json!({"class": "query_error", "kind": "newer_kind"})),
            failed,
        ),
        (
            "a name that is not an identifier",
            422,
            refusal(&json!({"class": "table_not_allowed", "table": format!("t\n{SECRET}")})),
            failed,
        ),
        (
            "a 422 that is not a refusal",
            422,
            json!({"code": "invalid_request", "details": {"class": "query_too_heavy"}}).to_string(),
            failed,
        ),
        ("a 422 that is not JSON", 422, SECRET.to_owned(), failed),
        (
            "the organisation's queries are used up",
            429,
            json!({"code": "rate_limited", "secret": SECRET}).to_string(),
            "query limit is used up",
        ),
        (
            "the query ran too long",
            504,
            json!({"code": "timeout", "secret": SECRET}).to_string(),
            "ran longer than the time limit",
        ),
        (
            "the organisation's account is not ready",
            404,
            json!({"code": "not_found", "secret": SECRET}).to_string(),
            "cannot be queried yet",
        ),
        (
            "the user may not",
            403,
            json!({"code": "permission_denied", "secret": SECRET}).to_string(),
            "refused this request for the user",
        ),
        (
            "a malformed request",
            400,
            json!({"code": "invalid_request", "secret": SECRET}).to_string(),
            "rejected the request",
        ),
        (
            "an internal error",
            500,
            json!({"code": "internal", "secret": SECRET}).to_string(),
            failed,
        ),
        (
            "an unavailable service",
            503,
            json!({"code": "unavailable", "secret": SECRET}).to_string(),
            failed,
        ),
        (
            "more rows than the tool asked for",
            200,
            answer(too_many),
            failed,
        ),
        ("an answer that is not JSON", 200, SECRET.to_owned(), failed),
    ];
    let calls = cases.len();
    stand_in.script().replies = cases
        .iter()
        .map(|(_, status, body, _)| (*status, body.clone()))
        .collect();
    for (name, status, _, expected) in &cases {
        let output = ask(&registry, &context, json!({ "sql": sql })).await;
        let text = text_of(&output);
        assert_eq!(
            output.is_error,
            *status != 200 || *expected == failed,
            "{name}: {text}"
        );
        assert!(text.contains(expected), "{name}: {text}");
        assert!(!text.contains(SECRET), "{name}: {text}");
        assert!(!text.contains(MARKER), "{name}: {text}");
        eprintln!("{name} ({status})\n  the model got: {}", shortened(&text));
    }

    // The answer, as the model got it: the columns, the rows with long text cut, the period.
    stand_in.script().replies = vec![(200, answer(vec![vec![json!("é".repeat(300)), json!(1)]]))];
    let output = ask(&registry, &context, json!({ "sql": sql })).await;
    let result: Value = serde_json::from_str(&text_of(&output)).expect("JSON for the model");
    assert_eq!(
        result["columns"],
        json!([
            {"name": "point", "type": "LowCardinality(String)"},
            {"name": "readings", "type": "UInt64"},
        ])
    );
    assert_eq!(
        result["rows"][0][0],
        format!("{}{CUT_MARKER}", "é".repeat(256))
    );
    assert_eq!(
        result["queryable_from"]["tenant_telemetry_1h"],
        "2026-07-26T13:00:00Z"
    );
    assert!(result.get("sql").is_none(), "{result}");

    // Every call: one fresh token of the user's tenant, exchanged by the assistant, and exactly
    // the SQL with the model's row limit.
    let script = stand_in.script();
    assert_eq!(script.refused_tokens, 0);
    assert_eq!(script.bodies.len(), calls + 1);
    for body in &script.bodies {
        assert_eq!(body, &json!({"sql": sql, "max_rows": DEFAULT_MAX_ROWS}));
    }
    drop(script);
    assert_eq!(tokens.issued.load(Ordering::SeqCst), calls + 1);

    let lines = logged(&capture);
    for line in &lines {
        assert!(
            !line.contains(SECRET),
            "a service's body was logged: {line}"
        );
        assert!(!line.contains(MARKER), "the SQL was logged: {line}");
    }
    assert!(
        lines.iter().any(|line| line.contains("invalid_refusal")),
        "{lines:#?}"
    );
}
