//! The endpoint without ClickHouse: who may call it, and how answers and failures map
//! (Laysi/trustiot-v2#143). That a refused statement reaches neither Postgres nor ClickHouse
//! (acceptance 4) is tested in `tests/sql_check.rs` through the service's router, because its
//! inputs include `SET ROLE`, which sql-check keeps out of `src`; `tests/sql_api.rs` runs the
//! endpoint against the real servers.

use axum::body::to_bytes;
use contract::identity::Actor;
use contract::{InternalClaims, TenantGrant};
use testkit::identity::TestIssuer;

use super::*;

fn claims(act: Option<ServiceName>, role: Option<TenantRole>) -> InternalClaims {
    let grant = role.map(|role| TenantGrant {
        id: TenantId::generate(),
        role,
    });
    let mut claims = TestIssuer::claims(ServiceName::TelemetryQuery, grant);
    claims.act = act.map(|sub| Actor { sub });
    claims
}

/// The logs' fingerprint of a SQL text: as long as before, the same for the same text in a
/// process, and keyed, so it is not the plain SHA-256 someone could compute for a guessed text
/// (the security review of 2026-10-26).
#[test]
fn the_logged_fingerprint_is_keyed_per_process() {
    use sha2::Digest as _;
    let sql = "SELECT point FROM tenant_telemetry WHERE value_string = 'a guess'";
    let logged = LoggedSql::new(sql);
    assert_eq!(logged.bytes, sql.len());
    if FINGERPRINT_KEY.is_none() {
        // A system without randomness: no fingerprint rather than a guessable one.
        assert_eq!(logged.hash, NO_FINGERPRINT);
        return;
    }
    assert_eq!(logged.hash.len(), 16, "{}", logged.hash);
    assert!(logged.hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(LoggedSql::new(sql).hash, logged.hash, "repeats tell apart");
    assert_ne!(LoggedSql::new("SELECT 1").hash, logged.hash);
    let plain: String = Sha256::digest(sql.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_ne!(logged.hash, plain, "not the plain hash");
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

/// A failure of the account check is the service's, never the AI's query's: the same 503 with
/// the fixed body for every code, where the history API's mapping would answer some as a query
/// too heavy, too slow or over its quota (the second security review of 2026-10-26, L3).
#[tokio::test]
async fn a_failed_account_check_is_unavailable_whatever_the_code() {
    let tenant = TenantId::generate();
    let fixed = serde_json::to_value(ErrorBody::new(ErrorCode::Unavailable)).expect("JSON");
    for failure in [
        failed(497),
        failed(164),
        failed(code::MEMORY_LIMIT_EXCEEDED),
        failed(code::TOO_MANY_ROWS),
        failed(code::TOO_MANY_ROWS_OR_BYTES),
        failed(code::TIMEOUT_EXCEEDED),
        failed(code::QUOTA_EXCEEDED),
        failed(code::TOO_MANY_SIMULTANEOUS_QUERIES),
        failed(62),
        failed(999_999),
        QueryError::Unreachable("refused".to_owned()),
    ] {
        let response = account_check_failed(tenant, &failure);
        assert_eq!(response.status().as_u16(), 503, "{failure:?}");
        assert_eq!(body_json(response).await, fixed, "{failure:?}");
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
