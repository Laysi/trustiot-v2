//! The endpoint without ClickHouse: who may call it, how answers and failures map, and that a
//! refused statement reaches neither Postgres nor ClickHouse (Laysi/trustiot-v2#143, acceptance
//! 4). `tests/sql_api.rs` runs it against the real servers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::to_bytes;
use contract::identity::Actor;
use contract::{InternalClaims, TenantGrant};
use service_kit::tenant::TenantDb;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testkit::identity::TestIssuer;

use super::*;
use crate::accounts::Accounts;
use crate::clickhouse::ClickHouse;

fn claims(act: Option<ServiceName>, role: Option<TenantRole>) -> InternalClaims {
    let grant = role.map(|role| TenantGrant {
        id: TenantId::generate(),
        role,
    });
    let mut claims = TestIssuer::claims(ServiceName::TelemetryQuery, grant);
    claims.act = act.map(|sub| Actor { sub });
    claims
}

#[test]
fn only_an_ai_service_in_a_tenant_may_run_sql() {
    let issuer = TestIssuer::new();
    let user = |act, role| issuer.user(ServiceName::TelemetryQuery, &claims(act, role));
    for role in [TenantRole::Viewer, TenantRole::Editor, TenantRole::Admin] {
        assert!(
            may_run_sql(&user(Some(ServiceName::Assistant), Some(role))),
            "{role:?}"
        );
    }
    // A user's own request (no exchange), another service, no tenant, a role this build does
    // not know.
    assert!(!may_run_sql(&user(None, Some(TenantRole::Admin))));
    assert!(!may_run_sql(&user(
        Some(ServiceName::Assets),
        Some(TenantRole::Admin)
    )));
    assert!(!may_run_sql(&user(Some(ServiceName::Assistant), None)));
    assert!(!may_run_sql(&user(
        Some(ServiceName::Assistant),
        Some(TenantRole::Unrecognized)
    )));
}

fn failed(code: u32) -> QueryError {
    QueryError::Failed {
        status: 500,
        code: Some(code),
        body: "Code: 0. DB::Exception: text that must not be returned".to_owned(),
    }
}

#[test]
fn the_answer_limit_is_the_contracts() {
    assert_eq!(u64::try_from(MAX_ANSWER_BYTES), Ok(MAX_RESULT_BYTES));
}

#[test]
fn clickhouse_failures_become_classes_or_statuses() {
    let kind = |code| match query_refusal(&failed(code)) {
        Some(SqlRefusal::QueryError { kind }) => Some(kind),
        _ => None,
    };
    for (codes, expected) in [
        (&[47, 10, 16][..], QueryErrorKind::UnknownColumn),
        (&[352, 207], QueryErrorKind::AmbiguousColumn),
        (&[60], QueryErrorKind::UnknownTable),
        (&[43, 44, 53, 59, 70, 386, 50], QueryErrorKind::TypeMismatch),
        (&[36, 42, 69, 127, 135, 190], QueryErrorKind::BadArguments),
        (&[6, 27, 38, 41, 72], QueryErrorKind::CannotParseValue),
        (&[215, 184], QueryErrorKind::AggregateMisuse),
        (&[153], QueryErrorKind::DivisionByZero),
        (&[62], QueryErrorKind::Syntax),
        (&[46, 48, 179], QueryErrorKind::Other),
    ] {
        for code in codes {
            assert_eq!(kind(*code), Some(expected), "code {code}");
        }
    }
    for code in [158, 160, 241, 290, 307, 396, 128, 131] {
        assert_eq!(
            query_refusal(&failed(code)),
            Some(SqlRefusal::QueryTooHeavy),
            "code {code}"
        );
    }
    assert_eq!(
        query_refusal(&QueryError::TooLarge {
            max_bytes: MAX_ANSWER_BYTES
        }),
        Some(SqlRefusal::ResultTooLarge {
            max_bytes: MAX_RESULT_BYTES
        })
    );
    // Not the query's fault: a status of its own.
    for failure in [
        failed(code::TIMEOUT_EXCEEDED),
        failed(code::QUOTA_EXCEEDED),
        failed(code::TOO_MANY_SIMULTANEOUS_QUERIES),
        failed(497),
        failed(164),
        failed(code::UNKNOWN_SETTING),
        failed(code::AUTHENTICATION_FAILED),
        failed(999_999),
        QueryError::Unreachable("connection refused".to_owned()),
        QueryError::Failed {
            status: 502,
            code: None,
            body: String::new(),
        },
    ] {
        assert_eq!(query_refusal(&failure), None, "{failure:?}");
    }
}

async fn body_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body");
    serde_json::from_slice(&bytes).expect("a JSON body")
}

#[tokio::test]
async fn failures_answer_with_their_status_and_no_clickhouse_text() {
    let tenant = TenantId::generate();
    let logged = LoggedSql::new("SELECT 1");
    for (failure, status, code) in [
        (failed(code::TIMEOUT_EXCEEDED), 504, "timeout"),
        (failed(code::QUOTA_EXCEEDED), 429, "rate_limited"),
        (
            failed(code::TOO_MANY_SIMULTANEOUS_QUERIES),
            429,
            "rate_limited",
        ),
        (failed(497), 500, "internal"),
        (failed(164), 500, "internal"),
        (failed(999_999), 503, "unavailable"),
        (
            QueryError::Unreachable("refused".to_owned()),
            503,
            "unavailable",
        ),
    ] {
        let response = failure_response(tenant, &logged, &failure);
        assert_eq!(response.status().as_u16(), status, "{failure:?}");
        let body = body_json(response).await;
        assert_eq!(body["code"], code, "{failure:?}");
        assert!(!body.to_string().contains("DB::Exception"), "{body}");
    }
}

#[tokio::test]
async fn a_refusal_is_its_class_and_fixed_details() {
    for (refusal, expected) in [
        (
            SqlRefusal::Syntax {
                line: Some(2),
                column: Some(33),
            },
            serde_json::json!({"class": "syntax", "line": 2, "column": 33}),
        ),
        (
            SqlRefusal::FunctionNotAllowed {
                function: "dictGet".to_owned(),
            },
            serde_json::json!({"class": "function_not_allowed", "function": "dictGet"}),
        ),
        (
            SqlRefusal::TooManyRows { max: 200 },
            serde_json::json!({"class": "too_many_rows", "max": 200}),
        ),
        (
            SqlRefusal::NotASelect,
            serde_json::json!({"class": "not_a_select"}),
        ),
    ] {
        let response = refused(&refusal);
        assert_eq!(response.status().as_u16(), 422);
        let body = body_json(response).await;
        assert_eq!(body["code"], "validation_failed");
        assert_eq!(body["details"], expected);
        assert_eq!(
            serde_json::from_value::<SqlRefusal>(body["details"].clone()).expect("a refusal"),
            refusal
        );
    }
}

#[test]
fn answers_have_names_types_and_rows_of_their_width() {
    let body = "[\"point\",\"n\"]\n[\"String\",\"UInt64\"]\n[\"a\",\"1\"]\n\n[\"b\",\"2\"]\n";
    let (columns, rows) = parse_answer(body).expect("an answer");
    assert_eq!(
        columns,
        vec![
            SqlColumn {
                name: "point".to_owned(),
                data_type: "String".to_owned()
            },
            SqlColumn {
                name: "n".to_owned(),
                data_type: "UInt64".to_owned()
            },
        ]
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1], vec![Value::from("b"), Value::from("2")]);
    assert!(parse_answer("[\"a\"]\n[\"String\"]\n").is_some_and(|(_, rows)| rows.is_empty()));
    for broken in [
        "",
        "[\"a\"]",
        "[\"a\",\"b\"]\n[\"String\"]",
        "[]\n[]",
        "[\"a\"]\n[\"String\"]\n[\"x\",\"y\"]",
        "[\"a\"]\n[\"String\"]\nnot json",
        "{\"a\":1}\n[\"String\"]",
    ] {
        assert!(parse_answer(broken).is_none(), "{broken}");
    }
}

#[test]
fn logs_name_the_sql_by_length_and_a_short_hash() {
    let logged = LoggedSql::new("SELECT 'secret-marker' FROM tenant_telemetry");
    assert_eq!(logged.bytes, 44);
    assert_eq!(logged.hash.len(), 16);
    assert!(logged.hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(logged.hash, LoggedSql::new("SELECT 1").hash);
}

/// A loopback port that accepts connections, counts them and closes them at once: a Postgres or
/// ClickHouse that is never reached unless the count says so.
async fn counting_port() -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let port = listener.local_addr().expect("its address").port();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, count)
}

/// Routes whose Postgres and ClickHouse are counting ports.
async fn counted_routes(issuer: &TestIssuer) -> (Routes, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (postgres_port, postgres) = counting_port().await;
    let (clickhouse_port, clickhouse) = counting_port().await;
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(postgres_port)
        .username("telemetry_query_app")
        .database("trustiot")
        .ssl_mode(PgSslMode::VerifyFull);
    // Short: the positive control below waits for the pool to give up on the closed port.
    let pool = PgPoolOptions::new().acquire_timeout(Duration::from_secs(2));
    let db = TenantDb::connect_lazy(options, pool).expect("verify-full options");
    let client = ClickHouse::new(&format!("http://127.0.0.1:{clickhouse_port}"), None)
        .expect("a ClickHouse client");
    let accounts = Accounts::new(client, "trustiot_telemetry_query", &"p".repeat(32));
    let routes = Routes::new(issuer.verifier(ServiceName::TelemetryQuery), db, accounts);
    (routes, postgres, clickhouse)
}

async fn call(routes: &Routes, user: UserIdentity, body: &str) -> Response {
    run_sql(
        State(routes.clone()),
        user,
        Ok(Bytes::from(body.to_owned())),
    )
    .await
}

#[tokio::test]
async fn a_refused_statement_reaches_neither_postgres_nor_clickhouse() {
    let issuer = TestIssuer::new();
    let (routes, postgres, clickhouse) = counted_routes(&issuer).await;
    let assistant = claims(Some(ServiceName::Assistant), Some(TenantRole::Viewer));
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry",
        "GRANT SELECT ON trustiot.telemetry TO x",
        "SYSTEM RELOAD CONFIG",
        "KILL QUERY WHERE 1",
        "SELECT 1 FROM tenant_telemetry; SET DEFAULT ROLE NONE TO x",
        "SELECT point FROM tenant_telemetry SETTINGS readonly = 0",
        "SELECT * FROM system.users",
        "SELECT getSetting('SQL_tenant_id') FROM tenant_telemetry",
    ] {
        let body = serde_json::json!({ "sql": sql }).to_string();
        let response = call(
            &routes,
            issuer.user(ServiceName::TelemetryQuery, &assistant),
            &body,
        )
        .await;
        assert_eq!(response.status().as_u16(), 422, "{sql}");
    }
    // Callers that may not run SQL, and bodies that are not a request.
    for (claims, body, status) in [
        (
            claims(None, Some(TenantRole::Admin)),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (
            claims(Some(ServiceName::Assets), Some(TenantRole::Admin)),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (
            claims(Some(ServiceName::Assistant), None),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (assistant.clone(), "not json", 400),
        (
            assistant.clone(),
            "{\"sql\":\"SELECT 1\",\"max_rows\":0}",
            400,
        ),
        (
            assistant.clone(),
            "{\"sql\":\"SELECT 1\",\"max_rows\":10001}",
            400,
        ),
        (assistant.clone(), "{\"max_rows\":5}", 400),
    ] {
        let response = call(
            &routes,
            issuer.user(ServiceName::TelemetryQuery, &claims),
            body,
        )
        .await;
        assert_eq!(response.status().as_u16(), status, "{body}");
    }
    assert_eq!(postgres.load(Ordering::SeqCst), 0, "Postgres was reached");
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        0,
        "ClickHouse was reached"
    );

    // The control: a statement that passes goes on to Postgres, which the counter sees.
    let body = serde_json::json!({ "sql": "SELECT point FROM tenant_telemetry" }).to_string();
    let response = call(
        &routes,
        issuer.user(ServiceName::TelemetryQuery, &assistant),
        &body,
    )
    .await;
    assert_eq!(response.status().as_u16(), 503);
    assert!(
        postgres.load(Ordering::SeqCst) > 0,
        "the control reached Postgres"
    );
}

#[tokio::test]
async fn sql_over_its_limit_is_too_long_and_never_sent() {
    let issuer = TestIssuer::new();
    let (routes, _, clickhouse) = counted_routes(&issuer).await;
    let assistant = claims(Some(ServiceName::Assistant), Some(TenantRole::Viewer));
    let sql = format!("SELECT '{}' FROM tenant_telemetry", "x".repeat(17 * 1024));
    let body = serde_json::json!({ "sql": sql }).to_string();
    let response = call(
        &routes,
        issuer.user(ServiceName::TelemetryQuery, &assistant),
        &body,
    )
    .await;
    assert_eq!(response.status().as_u16(), 422);
    assert_eq!(
        body_json(response).await["details"],
        serde_json::json!({"class": "too_long", "max_bytes": MAX_SQL_BYTES})
    );
    assert_eq!(clickhouse.load(Ordering::SeqCst), 0);
}
