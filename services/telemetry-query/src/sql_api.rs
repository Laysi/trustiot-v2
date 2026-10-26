//! The internal endpoint that runs an AI's read-only SQL (Laysi/trustiot-v2#143):
//! `POST /internal/telemetry/v1/sql`, [`RunSql`] in `crates/contract`.
//!
//! 1. The caller: the request's identity token must come from a token exchange by a service in
//!    [`SQL_CALLERS`] (its `act`), and carry a tenant role. A user's own request through the
//!    ingress gateway has no `act`; the gateway does not route `/internal` anyway.
//! 2. The body: [`RunSql`], at most [`MAX_REQUEST_BYTES`]; `max_rows` 1 to [`MAX_ROWS`].
//! 3. The check ([`crate::sql`]): pure, on a thread of its own; a refusal answers 422 with the
//!    [`SqlRefusal`] as `details`, before anything reaches Postgres or ClickHouse.
//! 4. The tenant's account: it exists (Postgres) and is as the service creates it, with its
//!    queryable period set ([`crate::accounts::Accounts::verify`]).
//! 5. The query: only the statement printed from the checked tree, as the tenant's read-only
//!    account, reading at most [`MAX_RESULT_BYTES`] of the answer; more rows than `max_rows` is a
//!    refusal, never a cut-off answer.
//!
//! The SQL is tenant content: the logs carry its length and a short keyed hash, never its text,
//! and no answer carries ClickHouse's error text.

use std::sync::LazyLock;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::response::Response;
use chrono::Utc;
use contract::api::telemetry_query::{
    DEFAULT_MAX_ROWS, MAX_RESULT_BYTES, MAX_ROWS, MAX_SQL_BYTES, QueryErrorKind, QueryableFrom,
    RunSql, SqlColumn, SqlRefusal, SqlResult,
};
use contract::{ErrorBody, ErrorCode, ServiceName, TenantId, TenantRole};
use hmac::{Hmac, KeyInit, Mac as _};
use serde_json::Value;
use service_kit::identity::UserIdentity;
use sha2::Sha256;

use crate::clickhouse::{QueryError, code};
use crate::history::{Failure, Routes, error, ok};
use crate::period::View;
use crate::sql::{self, CheckError, Checked};

/// The services that may run SQL for a user: those an AI acts through. The dashboards of
/// Laysi/trustiot-v2#150 add theirs.
pub const SQL_CALLERS: [ServiceName; 1] = [ServiceName::Assistant];

/// The largest request body: the SQL's [`MAX_SQL_BYTES`] with room for JSON escapes. A larger
/// body is refused as SQL that is too long.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// [`MAX_RESULT_BYTES`] as the transport counts it; a unit test checks that they agree.
const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;

/// The key of the SQL's fingerprint in the logs, random and made once per process: the
/// fingerprint tells repeats apart within a process, and someone who reads the logs cannot test
/// a guessed SQL text against it, as against a plain hash. `None` when the system has no
/// randomness; the fingerprint is then left out.
static FINGERPRINT_KEY: LazyLock<Option<[u8; 32]>> = LazyLock::new(|| {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key).ok().map(|()| key)
});

/// What the logs carry for a fingerprint without a key.
const NO_FINGERPRINT: &str = "none";

/// The SQL as the logs may name it: its length and the first 8 bytes of its HMAC-SHA-256 under
/// [`FINGERPRINT_KEY`], enough to tell repeats apart.
struct LoggedSql {
    bytes: usize,
    hash: String,
}

impl LoggedSql {
    fn new(sql: &str) -> Self {
        let hash = FINGERPRINT_KEY
            .as_ref()
            .and_then(|key| {
                let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).ok()?;
                mac.update(sql.as_bytes());
                let digest = mac.finalize().into_bytes();
                Some(
                    digest[..8]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                )
            })
            .unwrap_or_else(|| NO_FINGERPRINT.to_owned());
        Self {
            bytes: sql.len(),
            hash,
        }
    }
}

/// The class of a refusal, as its JSON names it, for the logs.
fn class(refusal: &SqlRefusal) -> String {
    serde_json::to_value(refusal)
        .ok()
        .and_then(|value| {
            value
                .get("class")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The 422 of a refusal: `validation_failed` with the refusal's members as `details`.
fn refused(refusal: &SqlRefusal) -> Response {
    let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
    if let Ok(Value::Object(members)) = serde_json::to_value(refusal) {
        for (name, value) in members {
            body = body.with_detail(name, value);
        }
    }
    error(body)
}

fn denied() -> Response {
    error(ErrorBody::new(ErrorCode::PermissionDenied))
}

fn invalid_request(parameter: &'static str) -> Response {
    error(ErrorBody::new(ErrorCode::InvalidRequest).with_detail("parameter", parameter))
}

/// Whether `user` may run SQL: an exchanged token of a service in [`SQL_CALLERS`], in a tenant,
/// with a role this build knows.
fn may_run_sql(user: &UserIdentity) -> bool {
    let caller = user.claims().act.map(|actor| actor.sub);
    let role = user.tenant().map(|grant| grant.role);
    caller.is_some_and(|caller| SQL_CALLERS.contains(&caller))
        && role.is_some_and(|role| role.includes(TenantRole::Viewer))
}

/// What a ClickHouse failure of a checked query becomes. `None`: not the query's fault, a 5xx
/// answered by [`failure_response`].
fn query_refusal(failure: &QueryError) -> Option<SqlRefusal> {
    if matches!(failure, QueryError::TooLarge { .. }) {
        return Some(SqlRefusal::ResultTooLarge {
            max_bytes: MAX_RESULT_BYTES,
        });
    }
    let kind = match failure.code()? {
        // UNKNOWN_IDENTIFIER, NOT_FOUND_COLUMN_IN_BLOCK, NO_SUCH_COLUMN_IN_TABLE.
        47 | 10 | 16 => QueryErrorKind::UnknownColumn,
        // AMBIGUOUS_COLUMN_NAME, AMBIGUOUS_IDENTIFIER.
        352 | 207 => QueryErrorKind::AmbiguousColumn,
        // UNKNOWN_TABLE: a common table expression out of ClickHouse's scope.
        60 => QueryErrorKind::UnknownTable,
        // ILLEGAL_TYPE_OF_ARGUMENT, ILLEGAL_COLUMN, TYPE_MISMATCH, ILLEGAL_TYPE_OF_COLUMN_FOR_FILTER,
        // CANNOT_CONVERT_TYPE, NO_COMMON_TYPE, UNKNOWN_TYPE.
        43 | 44 | 53 | 59 | 70 | 386 | 50 => QueryErrorKind::TypeMismatch,
        // BAD_ARGUMENTS, NUMBER_OF_ARGUMENTS_DOESNT_MATCH, ARGUMENT_OUT_OF_BOUND,
        // ILLEGAL_INDEX, ZERO_ARRAY_OR_TUPLE_INDEX, SIZES_OF_ARRAYS_DONT_MATCH.
        36 | 42 | 69 | 127 | 135 | 190 => QueryErrorKind::BadArguments,
        // CANNOT_PARSE_TEXT, CANNOT_PARSE_INPUT_ASSERTION_FAILED, CANNOT_PARSE_DATE,
        // CANNOT_PARSE_DATETIME, CANNOT_PARSE_NUMBER.
        6 | 27 | 38 | 41 | 72 => QueryErrorKind::CannotParseValue,
        // NOT_AN_AGGREGATE, ILLEGAL_AGGREGATION.
        215 | 184 => QueryErrorKind::AggregateMisuse,
        // ILLEGAL_DIVISION.
        153 => QueryErrorKind::DivisionByZero,
        // ClickHouse reads the printed text differently from sqlparser: logged as a warning
        // ([`log_query_refusal`]).
        code::SYNTAX_ERROR => QueryErrorKind::Syntax,
        // UNKNOWN_FUNCTION, NOT_IMPLEMENTED, MULTIPLE_EXPRESSIONS_FOR_ALIAS.
        46 | 48 | 179 => QueryErrorKind::Other,
        // TOO_MANY_ROWS, TOO_SLOW, MEMORY_LIMIT_EXCEEDED, LIMIT_EXCEEDED, TOO_MANY_BYTES,
        // TOO_MANY_ROWS_OR_BYTES, TOO_LARGE_ARRAY_SIZE, TOO_LARGE_STRING_SIZE.
        code::TOO_MANY_ROWS
        | 160
        | code::MEMORY_LIMIT_EXCEEDED
        | 290
        | 307
        | code::TOO_MANY_ROWS_OR_BYTES
        | 128
        | 131 => return Some(SqlRefusal::QueryTooHeavy),
        _ => return None,
    };
    Some(SqlRefusal::QueryError { kind })
}

/// Logs a refusal ClickHouse gave a checked query, by the SQL's length and hash. A syntax error
/// is a WARN: ClickHouse could not parse the text the check printed, so the check and ClickHouse
/// read it differently, a gap in the check to look into (the second security review of
/// 2026-10-26). The other refusals are the query's own errors, at INFO.
fn log_query_refusal(
    tenant: TenantId,
    logged: &LoggedSql,
    refusal: &SqlRefusal,
    failure: &QueryError,
) {
    if failure.code() == Some(code::SYNTAX_ERROR) {
        tracing::warn!(
            tenant_id = %tenant,
            class = class(refusal),
            error_kind = failure.kind(),
            sql_bytes = logged.bytes,
            sql_hash = %logged.hash,
            "ClickHouse could not parse a checked query: the check and ClickHouse read it differently"
        );
    } else {
        tracing::info!(
            tenant_id = %tenant,
            class = class(refusal),
            error_kind = failure.kind(),
            sql_bytes = logged.bytes,
            sql_hash = %logged.hash,
            "an AI's query did not run"
        );
    }
}

/// The answer when the account check before an AI's query fails in ClickHouse: 503 with the
/// fixed body, whatever the code. The check is the service's query, not the AI's, so a code
/// that would describe a query (too heavy, too slow, over the quota, refused) would tell the
/// model something false about the SQL it wrote; only the query itself can be a checked query
/// the account refused ([`failure_response`]).
fn account_check_failed(tenant: TenantId, failure: &QueryError) -> Response {
    tracing::warn!(
        tenant_id = %tenant,
        error_kind = failure.kind(),
        "the account check before an AI's query failed in ClickHouse"
    );
    error(ErrorBody::new(ErrorCode::Unavailable))
}

/// The answer to a ClickHouse failure of the checked query that is not the query's fault. A
/// failure of the account check before it is [`account_check_failed`]'s.
fn failure_response(tenant: TenantId, logged: &LoggedSql, failure: &QueryError) -> Response {
    let code = match failure.code() {
        Some(code::TIMEOUT_EXCEEDED) => ErrorCode::Timeout,
        Some(code::QUOTA_EXCEEDED | code::TOO_MANY_SIMULTANEOUS_QUERIES) => ErrorCode::RateLimited,
        // ACCESS_DENIED, READONLY: the account refused what the check let through. The check
        // and the account's grants disagree, which must be looked into.
        Some(497 | 164) => {
            tracing::error!(
                tenant_id = %tenant,
                error_kind = failure.kind(),
                sql_bytes = logged.bytes,
                sql_hash = %logged.hash,
                "the tenant account refused a checked query"
            );
            return error(ErrorBody::new(ErrorCode::Internal));
        }
        _ => ErrorCode::Unavailable,
    };
    tracing::warn!(
        tenant_id = %tenant,
        error_kind = failure.kind(),
        sql_bytes = logged.bytes,
        sql_hash = %logged.hash,
        "an AI's query failed"
    );
    error(ErrorBody::new(code))
}

/// The columns and rows of a `JSONCompactEachRowWithNamesAndTypes` answer: a line of names, a
/// line of types, then a line per row, each a JSON array as long as the names.
fn parse_answer(body: &str) -> Option<(Vec<SqlColumn>, Vec<Vec<Value>>)> {
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    let names: Vec<String> = serde_json::from_str(lines.next()?).ok()?;
    let types: Vec<String> = serde_json::from_str(lines.next()?).ok()?;
    if names.is_empty() || names.len() != types.len() {
        return None;
    }
    let rows = lines
        .map(|line| {
            serde_json::from_str::<Vec<Value>>(line)
                .ok()
                .filter(|row| row.len() == names.len())
        })
        .collect::<Option<Vec<_>>>()?;
    let columns = names
        .into_iter()
        .zip(types)
        .map(|(name, data_type)| SqlColumn { name, data_type })
        .collect();
    Some((columns, rows))
}

/// `POST /internal/telemetry/v1/sql`.
pub(crate) async fn run_sql(
    State(state): State<Routes>,
    user: UserIdentity,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if !may_run_sql(&user) {
        tracing::info!(
            caller = user.claims().act.map_or("none", |actor| actor.sub.as_str()),
            has_tenant = user.tenant_id().is_some(),
            "refused a SQL call from a caller or role that may not run SQL"
        );
        return denied();
    }
    let body = match body {
        Ok(body) => body,
        Err(BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))) => {
            return refused(&SqlRefusal::TooLong {
                max_bytes: MAX_SQL_BYTES,
            });
        }
        Err(_) => return invalid_request("body"),
    };
    let Ok(request) = serde_json::from_slice::<RunSql>(&body) else {
        return invalid_request("body");
    };
    let max_rows = request.max_rows.unwrap_or(DEFAULT_MAX_ROWS);
    if !(1..=MAX_ROWS).contains(&max_rows) {
        return invalid_request("max_rows");
    }
    let logged = LoggedSql::new(&request.sql);
    let checked = match sql::check_on_thread(request.sql, max_rows).await {
        Ok(checked) => checked,
        Err(CheckError::Refused(refusal)) => {
            tracing::info!(
                tenant_id = ?user.tenant_id(),
                class = class(&refusal),
                sql_bytes = logged.bytes,
                sql_hash = %logged.hash,
                "refused an AI's SQL"
            );
            return refused(&refusal);
        }
        Err(CheckError::Internal(reason)) => {
            tracing::error!(
                tenant_id = ?user.tenant_id(),
                reason,
                sql_bytes = logged.bytes,
                sql_hash = %logged.hash,
                "the SQL check failed"
            );
            return error(ErrorBody::new(ErrorCode::Internal));
        }
    };
    let tenant = match state.tenant(&user).await {
        Ok(tenant) => tenant,
        Err(Failure::Response(response)) => return *response,
        // `tenant` reads Postgres only; a ClickHouse failure here would be a change to it.
        Err(Failure::ClickHouse(failure)) => {
            tracing::error!(
                error_kind = failure.kind(),
                "finding the tenant failed in ClickHouse"
            );
            return error(ErrorBody::new(ErrorCode::Internal));
        }
    };
    let windows = match state.verified(tenant).await {
        Ok(windows) => windows,
        Err(Failure::Response(response)) => return *response,
        Err(Failure::ClickHouse(failure)) => return account_check_failed(tenant, &failure),
    };
    let queried_at = Utc::now();
    let queryable_from = QueryableFrom {
        tenant_telemetry: windows.earliest(View::Raw, queried_at),
        tenant_telemetry_1m: windows.earliest(View::Minute, queried_at),
        tenant_telemetry_1h: windows.earliest(View::Hour, queried_at),
    };
    let Checked {
        canonical,
        statement,
    } = checked;
    let account = state.accounts().account(tenant);
    let answer = state
        .accounts()
        .clickhouse()
        .query_limited(
            account.credentials(),
            &statement,
            &[],
            None,
            MAX_ANSWER_BYTES,
        )
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(failure) => {
            return match query_refusal(&failure) {
                Some(refusal) => {
                    log_query_refusal(tenant, &logged, &refusal, &failure);
                    refused(&refusal)
                }
                None => failure_response(tenant, &logged, &failure),
            };
        }
    };
    let Some((columns, rows)) = parse_answer(&answer) else {
        tracing::error!(
            tenant_id = %tenant,
            sql_bytes = logged.bytes,
            sql_hash = %logged.hash,
            "ClickHouse answered an AI's query in an unexpected form"
        );
        return error(ErrorBody::new(ErrorCode::Internal));
    };
    if rows.len() > usize::try_from(max_rows).unwrap_or(usize::MAX) {
        return refused(&SqlRefusal::TooManyRows { max: max_rows });
    }
    tracing::info!(
        tenant_id = %tenant,
        rows = rows.len(),
        sql_bytes = logged.bytes,
        sql_hash = %logged.hash,
        "ran an AI's query"
    );
    ok(&SqlResult {
        sql: canonical,
        columns,
        rows,
        queryable_from,
        queried_at,
    })
}

#[cfg(test)]
mod tests;
