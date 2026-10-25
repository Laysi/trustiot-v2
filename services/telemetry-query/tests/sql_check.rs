//! The AI SQL check (Laysi/trustiot-v2#143) against the inputs that may not be written in
//! `services/*/src`, so they live here rather than with the check's unit tests in `src/sql/`:
//!
//! - statements that change a schema, which repo-check rejects anywhere in `services/*/src`;
//! - statements that change the role (`SET ROLE`), and SQL the test builds with `format!` (long,
//!   deep or wide queries), which sql-check rejects in `src` (tools/sql-check, decision 29). This
//!   file is on sql-check's exception list for those two rules: the text is ClickHouse SQL that
//!   the check must refuse or size up, never Postgres SQL, and nothing here talks to Postgres
//!   but a counting port.
//!
//! No container: the check is pure, and a refused statement never reaches ClickHouse. The
//! endpoint's tests run the service's real router with Postgres and ClickHouse as loopback ports
//! that count connections.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use contract::api::telemetry_query::{MAX_SQL_BYTES, RunSql, SqlRefusal};
use contract::client::Endpoint as _;
use contract::identity::{Actor, INTERNAL_IDENTITY_HEADER};
use contract::{InternalClaims, ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use service_kit::tenant::TenantDb;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use telemetry_query::accounts::Accounts;
use telemetry_query::clickhouse::ClickHouse;
use telemetry_query::sql::lexer::{MAX_IDENTIFIER_BYTES, MAX_TOKENS, tokens};
use telemetry_query::sql::{CheckError, check_on_thread};
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;

async fn refusal(sql: &str) -> SqlRefusal {
    match check_on_thread(sql.to_owned(), 200).await {
        Err(CheckError::Refused(refusal)) => refusal,
        other => panic!("{sql}: expected a refusal, got {other:?}"),
    }
}

async fn passes(sql: &str) {
    if let Err(error) = check_on_thread(sql.to_owned(), 200).await {
        panic!("{sql}: {error:?}");
    }
}

/// What the lexer alone refuses `sql` with: steps 1 to 3 of the check.
fn lexer_refusal(sql: &str) -> SqlRefusal {
    tokens(sql).expect_err(sql)
}

#[tokio::test]
async fn a_schema_change_is_never_a_query() {
    for sql in [
        "ALTER TABLE trustiot.telemetry DELETE WHERE 1",
        "ALTER TABLE trustiot.telemetry_1m UPDATE value_sum = 0 WHERE 1",
        "TRUNCATE TABLE trustiot.telemetry",
        "DROP TABLE trustiot.telemetry",
        "DROP VIEW trustiot.tenant_telemetry",
        "DROP DATABASE trustiot",
        "DROP DICTIONARY trustiot.d",
        "CREATE TABLE t (x UInt8) ENGINE = Memory",
        "CREATE OR REPLACE VIEW trustiot.tenant_telemetry AS SELECT 1",
        "CREATE DATABASE other",
        "RENAME TABLE trustiot.telemetry TO trustiot.gone",
        "EXCHANGE TABLES trustiot.telemetry AND trustiot.telemetry_1m",
        "ATTACH TABLE trustiot.telemetry",
        "DETACH TABLE trustiot.telemetry",
        "UNDROP TABLE trustiot.telemetry",
        "  drop table t",
        "/* a comment */ ALTER TABLE t DROP COLUMN x",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::NotASelect, "{sql}");
    }
}

#[tokio::test]
async fn a_schema_change_never_follows_a_query() {
    for sql in [
        "(SELECT 1); DROP TABLE t",
        "SELECT 1 FROM tenant_telemetry; DROP TABLE trustiot.telemetry",
        "SELECT 1 FROM tenant_telemetry;\nTRUNCATE TABLE trustiot.telemetry",
        "SELECT 1 FROM tenant_telemetry; -- trailing\nALTER TABLE trustiot.telemetry DELETE WHERE 1",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::MultipleStatements, "{sql}");
    }
}

#[tokio::test]
async fn a_schema_change_in_a_comment_is_dropped_from_what_runs() {
    for sql in [
        "SELECT 1 FROM tenant_telemetry -- ; DROP TABLE trustiot.telemetry",
        "SELECT 1 /* ; TRUNCATE TABLE trustiot.telemetry */ FROM tenant_telemetry",
    ] {
        let checked = check_on_thread(sql.to_owned(), 200)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert_eq!(
            checked.canonical, "SELECT 1 FROM trustiot.tenant_telemetry",
            "{sql}"
        );
        for text in [&checked.canonical, &checked.statement] {
            assert!(!text.contains("DROP"), "{sql}: {text}");
            assert!(!text.contains("TRUNCATE"), "{sql}: {text}");
            assert!(!text.contains("--"), "{sql}: {text}");
            assert!(!text.contains("/*"), "{sql}: {text}");
        }
    }
}

#[tokio::test]
async fn a_schema_change_as_a_string_stays_a_string() {
    let sql = "SELECT 'DROP TABLE trustiot.telemetry' AS text FROM tenant_telemetry";
    let checked = check_on_thread(sql.to_owned(), 200)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    assert_eq!(
        checked.canonical,
        "SELECT 'DROP TABLE trustiot.telemetry' AS text FROM trustiot.tenant_telemetry"
    );
}

// The lexer (steps 1 to 3), on its own.

#[test]
fn the_lexer_limits_the_size_and_the_tokens() {
    let long = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES as usize));
    assert_eq!(
        lexer_refusal(&long),
        SqlRefusal::TooLong {
            max_bytes: MAX_SQL_BYTES
        }
    );
    let many = format!("SELECT {}", vec!["1"; MAX_TOKENS].join(","));
    assert_eq!(lexer_refusal(&many), SqlRefusal::TooComplex);
    // Spaces and comments do not count as tokens.
    let spaced = format!("SELECT 1{}", " ".repeat(10_000));
    assert!(tokens(&spaced).is_ok());
}

#[test]
fn the_lexer_refuses_an_identifier_longer_than_allowed() {
    let long = format!("SELECT {}", "x".repeat(MAX_IDENTIFIER_BYTES + 1));
    assert_eq!(
        lexer_refusal(&long),
        SqlRefusal::LiteralNotAllowed { line: 1, column: 8 }
    );
    assert!(tokens(&format!("SELECT {}", "x".repeat(MAX_IDENTIFIER_BYTES))).is_ok());
}

#[test]
fn the_lexer_lets_only_a_select_start_and_only_one() {
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO t VALUES (1)",
        "ALTER USER x MODIFY SETTINGS y = 1",
        "GRANT SELECT ON *.* TO x",
        "SYSTEM DROP DNS CACHE",
        "KILL QUERY WHERE 1",
        "EXPLAIN SELECT 1",
        "SHOW TABLES",
        "DESCRIBE trustiot.telemetry",
        "VALUES (1)",
        "",
        "  -- only a comment",
        ";",
    ] {
        assert_eq!(lexer_refusal(sql), SqlRefusal::NotASelect, "{sql}");
    }
    for sql in [
        "SELECT 1; SELECT 2",
        "SELECT 1; SET DEFAULT ROLE NONE TO tenant",
        "SELECT 1;;",
        "(SELECT 1); SELECT 2",
    ] {
        assert_eq!(lexer_refusal(sql), SqlRefusal::MultipleStatements, "{sql}");
    }
    assert!(tokens("SELECT 1 ; -- done").is_ok());
}

// The whole check.

#[tokio::test]
async fn statements_other_than_one_select_are_refused_before_parsing() {
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry",
        "ALTER USER x MODIFY SETTINGS SQL_queryable_raw_days = 36500",
        "DROP ROW POLICY p ON trustiot.telemetry",
        "CREATE USER x",
        "GRANT SELECT ON trustiot.telemetry TO x",
        "SYSTEM RELOAD CONFIG",
        "KILL QUERY WHERE 1",
        "OPTIMIZE TABLE trustiot.telemetry FINAL",
        "EXPLAIN SELECT 1",
        "SHOW GRANTS",
        "DESCRIBE TABLE trustiot.telemetry",
        "USE system",
        "EXECUTE AS x SELECT 1",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::NotASelect, "{sql}");
    }
    for sql in [
        "SELECT 1 FROM tenant_telemetry; SET DEFAULT ROLE NONE TO x",
        "SELECT 1 FROM tenant_telemetry; SELECT 2 FROM tenant_telemetry",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::MultipleStatements, "{sql}");
    }
    // Inside a query the parser still finds only queries.
    assert_eq!(
        refusal("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x").await,
        SqlRefusal::NotASelect
    );
}

#[tokio::test]
async fn deep_and_long_queries_are_too_complex_and_never_overflow_the_stack() {
    let nested = format!(
        "SELECT {}1{} FROM tenant_telemetry",
        "(".repeat(200),
        ")".repeat(200)
    );
    // The parser's recursion limit stops it; inside a speculative parse the limit shows as a
    // syntax error.
    assert!(
        matches!(
            refusal(&nested).await,
            SqlRefusal::TooComplex | SqlRefusal::Syntax { .. }
        ),
        "{nested}"
    );
    let subqueries = format!(
        "SELECT * FROM {}tenant_telemetry{}",
        "(SELECT * FROM ".repeat(100),
        ")".repeat(100)
    );
    assert!(
        matches!(
            refusal(&subqueries).await,
            SqlRefusal::TooComplex | SqlRefusal::Syntax { .. }
        ),
        "{subqueries}"
    );
    // A left-deep chain the parser builds in a loop: the walk's depth limit stops it.
    let chain = format!(
        "SELECT {} FROM tenant_telemetry",
        vec!["x"; 2_000].join("+")
    );
    assert_eq!(refusal(&chain).await, SqlRefusal::TooComplex);
    let not = format!("SELECT {}1 FROM tenant_telemetry", "NOT ".repeat(1_000));
    assert!(
        matches!(
            refusal(&not).await,
            SqlRefusal::TooComplex | SqlRefusal::Syntax { .. }
        ),
        "{not}"
    );
    // Within the limits, a long query passes.
    let wide = format!(
        "SELECT {} FROM tenant_telemetry",
        vec!["value_int + 1"; 500].join(", ")
    );
    passes(&wide).await;
}

#[tokio::test]
async fn the_check_runs_on_its_own_thread() {
    let checked = check_on_thread("SELECT point FROM tenant_telemetry".to_owned(), 5)
        .await
        .expect("passes");
    assert_eq!(
        checked.statement,
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 6 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    let deep = format!("SELECT {}1{}", "(".repeat(5_000), ")".repeat(5_000));
    assert!(matches!(
        check_on_thread(deep, 5).await,
        Err(CheckError::Refused(SqlRefusal::TooComplex))
    ));
}

// The endpoint, through the service's router.

/// A loopback port that accepts connections, counts them and closes them at once: a Postgres or
/// ClickHouse that is never reached unless the count says so.
async fn counting_port() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
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

/// Serves `router` on a loopback port and returns its origin.
async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let origin = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the server runs");
    });
    origin
}

/// The service's router, whose Postgres and ClickHouse are counting ports; its origin and the
/// two counts.
async fn counted_service(issuer: &TestIssuer) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
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
    let origin = serve(telemetry_query::history::routes(
        issuer.verifier(ServiceName::TelemetryQuery),
        db,
        accounts,
    ))
    .await;
    (origin, postgres, clickhouse)
}

fn claims(act: Option<ServiceName>, role: Option<TenantRole>) -> InternalClaims {
    let grant = role.map(|role| TenantGrant {
        id: TenantId::generate(),
        role,
    });
    let mut claims = TestIssuer::claims(ServiceName::TelemetryQuery, grant);
    claims.act = act.map(|sub| Actor { sub });
    claims
}

/// Posts `body` to the SQL endpoint with a token for `claims`; the status and the body.
async fn call(
    origin: &str,
    issuer: &TestIssuer,
    claims: &InternalClaims,
    body: &str,
) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{origin}{}", RunSql::ROUTE))
        .header(INTERNAL_IDENTITY_HEADER, issuer.token(claims))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_owned())
        // Above the 2 s the positive control's pool waits for Postgres.
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("the service answers");
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

#[tokio::test]
async fn a_refused_statement_reaches_neither_postgres_nor_clickhouse() {
    let issuer = TestIssuer::new();
    let (origin, postgres, clickhouse) = counted_service(&issuer).await;
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
        let body = json!({ "sql": sql }).to_string();
        let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
        assert_eq!(status, 422, "{sql}: {answer}");
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
        let (answered, answer) = call(&origin, &issuer, &claims, body).await;
        assert_eq!(answered, status, "{body}: {answer}");
    }
    assert_eq!(postgres.load(Ordering::SeqCst), 0, "Postgres was reached");
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        0,
        "ClickHouse was reached"
    );

    // The control: a statement that passes goes on to Postgres, which the counter sees.
    let body = json!({ "sql": "SELECT point FROM tenant_telemetry" }).to_string();
    let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
    assert_eq!(status, 503, "{answer}");
    assert!(
        postgres.load(Ordering::SeqCst) > 0,
        "the control reached Postgres"
    );
}

#[tokio::test]
async fn sql_over_its_limit_is_too_long_and_never_sent() {
    let issuer = TestIssuer::new();
    let (origin, _, clickhouse) = counted_service(&issuer).await;
    let assistant = claims(Some(ServiceName::Assistant), Some(TenantRole::Viewer));
    let sql = format!("SELECT '{}' FROM tenant_telemetry", "x".repeat(17 * 1024));
    let body = json!({ "sql": sql }).to_string();
    let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
    assert_eq!(status, 422, "{answer}");
    let answer: Value = serde_json::from_str(&answer).expect("a JSON body");
    assert_eq!(
        answer["details"],
        json!({"class": "too_long", "max_bytes": MAX_SQL_BYTES})
    );
    assert_eq!(clickhouse.load(Ordering::SeqCst), 0);
}
