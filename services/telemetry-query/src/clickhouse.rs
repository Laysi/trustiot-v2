//! ClickHouse's HTTP interface as this service uses it: one statement per request, every value
//! as a query parameter (`{name:Type}`, sent as `param_<name>`), and no setting but
//! `wait_end_of_query` (a tenant account is `readonly = 1`, which refuses every other setting).
//! With `wait_end_of_query`, ClickHouse answers only once the query has finished, so a limit
//! that stops a query midway is an error status, never a cut-off 200.

use std::path::Path;
use std::time::Duration;

use crate::transport;

/// How long one request may take. Above the tenant accounts' `max_execution_time` of 30 s
/// (services/ingest/migrations/clickhouse/0004_access.sql), so ClickHouse's own timeout, which
/// names itself, is what stops a heavy query.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// How many more times a statement is sent after ClickHouse refused it with
/// `TOO_MANY_SIMULTANEOUS_QUERIES` (Laysi/trustiot-v2#1122). The API's requests take one of the
/// tenant's slots first ([`crate::slots`], which also stay taken while a cancelled request's
/// query may still run), so this refusal comes only from a query the slots do not see: another
/// replica's, or the whole server's `max_concurrent_queries`.
/// Kept small because ClickHouse 26.8 counts every refusal as one of the account's quota errors
/// (60 a minute) and none of its queries (measured on 2026-11-07, Laysi/trustiot-v2#1122): a
/// statement costs at most `1 + CONCURRENCY_RETRIES` errors.
pub const CONCURRENCY_RETRIES: u32 = 2;

/// The wait before the first retry of [`CONCURRENCY_RETRIES`]; each later wait doubles. Each wait
/// is a random part of it, half to all, so statements refused together are not sent again
/// together. All waits together are at most 300 ms.
pub const CONCURRENCY_BACKOFF: Duration = Duration::from_millis(100);

/// ClickHouse error codes the service tells apart (ClickHouse's `ErrorCodes.cpp`).
pub mod code {
    /// `BAD_ARGUMENTS`, for example an unknown time zone.
    pub const BAD_ARGUMENTS: u32 = 36;
    /// `SYNTAX_ERROR`: for a checked AI query, ClickHouse reads the printed text otherwise than
    /// sqlparser.
    pub const SYNTAX_ERROR: u32 = 62;
    /// `UNKNOWN_SETTING`, for example `getSetting` of a setting the account does not have.
    pub const UNKNOWN_SETTING: u32 = 115;
    /// `TOO_MANY_ROWS`: `max_rows_to_read` or `max_result_rows`.
    pub const TOO_MANY_ROWS: u32 = 158;
    /// `TIMEOUT_EXCEEDED`: `max_execution_time`.
    pub const TIMEOUT_EXCEEDED: u32 = 159;
    /// `QUOTA_EXCEEDED`: the account's quota for the interval is used up.
    pub const QUOTA_EXCEEDED: u32 = 201;
    /// `TOO_MANY_SIMULTANEOUS_QUERIES`: `max_concurrent_queries_for_user`.
    pub const TOO_MANY_SIMULTANEOUS_QUERIES: u32 = 202;
    /// `MEMORY_LIMIT_EXCEEDED`: `max_memory_usage`, or the server's own limit.
    pub const MEMORY_LIMIT_EXCEEDED: u32 = 241;
    /// `TOO_MANY_ROWS_OR_BYTES`: a `max_*_rows` or `max_*_bytes` limit other than the above.
    pub const TOO_MANY_ROWS_OR_BYTES: u32 = 396;
    /// `AUTHENTICATION_FAILED`: no such user, or a wrong password.
    pub const AUTHENTICATION_FAILED: u32 = 516;
}

/// An account and its password.
#[derive(Clone, Copy)]
pub struct Credentials<'a> {
    pub user: &'a str,
    pub password: &'a str,
}

/// Why a statement did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// No response: ClickHouse was unreachable or did not answer within [`REQUEST_TIMEOUT`].
    #[error("no response from ClickHouse: {0}")]
    Unreachable(String),
    /// ClickHouse refused or failed the statement. `code` is ClickHouse's error code, when the
    /// body names one. The body is kept for tests and never logged or returned to a client: it
    /// can quote the statement with its parameters.
    #[error("ClickHouse answered {status} with error code {code:?}")]
    Failed {
        status: u16,
        code: Option<u32>,
        body: String,
    },
    /// ClickHouse's answer is longer than the call allowed ([`ClickHouse::query_limited`]);
    /// the rest was not read.
    #[error("ClickHouse's answer is longer than {max_bytes} bytes")]
    TooLarge { max_bytes: usize },
}

impl QueryError {
    /// ClickHouse's error code, when it answered with one.
    #[must_use]
    pub fn code(&self) -> Option<u32> {
        match self {
            Self::Failed { code, .. } => *code,
            Self::Unreachable(_) | Self::TooLarge { .. } => None,
        }
    }

    /// Whether ClickHouse refused the account or its password.
    #[must_use]
    pub fn is_authentication(&self) -> bool {
        self.code() == Some(code::AUTHENTICATION_FAILED)
    }

    /// What may be logged: the kind of failure and the code, never the body.
    #[must_use]
    pub fn kind(&self) -> String {
        match self {
            Self::Unreachable(_) => "unreachable".to_owned(),
            Self::TooLarge { max_bytes } => format!("answer longer than {max_bytes} bytes"),
            Self::Failed { status, code, .. } => match code {
                Some(code) => format!("ClickHouse error {code} (HTTP {status})"),
                None => format!("HTTP {status}"),
            },
        }
    }
}

/// The code at the start of a ClickHouse error body: `Code: 516. DB::Exception: ...`.
fn error_code(body: &str) -> Option<u32> {
    let rest = body.trim_start().strip_prefix("Code: ")?;
    let digits = rest.split('.').next()?;
    digits.parse().ok()
}

/// The wait before retry number `retry` (from 0) of [`CONCURRENCY_RETRIES`]: half to all of
/// [`CONCURRENCY_BACKOFF`] doubled `retry` times, at random. Without randomness from the system
/// the whole wait.
fn backoff(retry: u32) -> Duration {
    let full = CONCURRENCY_BACKOFF.saturating_mul(1_u32 << retry.min(16));
    let mut bytes = [0_u8; 2];
    let random = getrandom::fill(&mut bytes).map_or(u16::MAX, |()| u16::from_le_bytes(bytes));
    let half = full / 2;
    half + half * u32::from(random) / u32::from(u16::MAX)
}

/// A client of one ClickHouse server.
#[derive(Clone)]
pub struct ClickHouse {
    client: transport::Client,
    url: String,
}

impl ClickHouse {
    /// A client for `url` (`http://` or `https://`); `https` trusts only `ca_file`.
    ///
    /// # Errors
    ///
    /// When `https` comes without a CA file, or the CA file is unusable.
    pub fn new(url: &str, ca_file: Option<&Path>) -> Result<Self, transport::ClientError> {
        let client = if url.starts_with("https://") {
            let ca_file = ca_file.ok_or_else(|| {
                transport::ClientError::Build(
                    "an https ClickHouse needs TRUSTIOT_CLICKHOUSE_CA_FILE".to_owned(),
                )
            })?;
            transport::trusting(ca_file, REQUEST_TIMEOUT)?
        } else {
            transport::plain(REQUEST_TIMEOUT)
        };
        Ok(Self {
            client,
            url: format!("{}/", url.trim_end_matches('/')),
        })
    }

    /// Runs `statement` as `account` with `params` as its query parameters, and with the role
    /// `role` enabled for this statement only, when given (the HTTP interface's `role`
    /// parameter). Returns the response body, of at most [`transport::MAX_BODY_BYTES`].
    ///
    /// # Errors
    ///
    /// [`QueryError::Unreachable`] without a response, [`QueryError::Failed`] for any status
    /// but 200.
    pub async fn query(
        &self,
        account: Credentials<'_>,
        statement: &str,
        params: &[(&str, &str)],
        role: Option<&str>,
    ) -> Result<String, QueryError> {
        self.query_limited(account, statement, params, role, transport::MAX_BODY_BYTES)
            .await
    }

    /// [`ClickHouse::query`] reading at most `max_body` bytes of the answer.
    ///
    /// # Errors
    ///
    /// As [`ClickHouse::query`], and [`QueryError::TooLarge`] for a longer answer.
    pub async fn query_limited(
        &self,
        account: Credentials<'_>,
        statement: &str,
        params: &[(&str, &str)],
        role: Option<&str>,
        max_body: usize,
    ) -> Result<String, QueryError> {
        let names: Vec<String> = params
            .iter()
            .map(|(name, _)| format!("param_{name}"))
            .collect();
        let mut query: Vec<(&str, &str)> = names
            .iter()
            .map(String::as_str)
            .zip(params.iter().map(|(_, value)| *value))
            .collect();
        query.push(("wait_end_of_query", "1"));
        if let Some(role) = role {
            query.push(("role", role));
        }
        let mut retry = 0;
        loop {
            let response = self
                .client
                .post_limited(
                    &self.url,
                    &query,
                    (account.user, account.password),
                    statement.to_owned(),
                    max_body,
                )
                .await
                .map_err(|error| match error {
                    transport::RequestError::TooLarge { max_bytes } => {
                        QueryError::TooLarge { max_bytes }
                    }
                    transport::RequestError::Failed(reason) => QueryError::Unreachable(reason),
                })?;
            if response.status == http::StatusCode::OK {
                return Ok(response.body);
            }
            let code = error_code(&response.body);
            if code == Some(code::TOO_MANY_SIMULTANEOUS_QUERIES) && retry < CONCURRENCY_RETRIES {
                let wait = backoff(retry);
                retry += 1;
                tracing::info!(
                    retry,
                    wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    "ClickHouse refused a statement for the account's concurrent queries; \
                     sending it again"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(QueryError::Failed {
                status: response.status.as_u16(),
                code,
                body: response.body,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const ACCOUNT: Credentials<'static> = Credentials {
        user: "trustiot_tenant_account_x",
        password: "p",
    };

    /// A stand-in ClickHouse on loopback that refuses the first `refusals` statements with the
    /// error `code`, as ClickHouse words one, and answers `1` afterwards; its client and the
    /// statements it received.
    async fn stand_in(refusals: usize, code: u32) -> (ClickHouse, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stand-in ClickHouse");
        let address = listener.local_addr().expect("its address");
        let received = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&received);
        let app = axum::Router::new().fallback(move || {
            let number = counted.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if number <= refusals {
                    (
                        http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Code: {code}. DB::Exception: refused by the stand-in."),
                    )
                } else {
                    (http::StatusCode::OK, "1\n".to_owned())
                }
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve the stand-in ClickHouse");
        });
        let client = ClickHouse::new(&format!("http://{address}"), None).expect("a client");
        (client, received)
    }

    /// A statement refused for the account's concurrent queries is sent again, and answers when
    /// a later attempt gets through (Laysi/trustiot-v2#1122).
    #[tokio::test]
    async fn a_concurrency_refusal_is_retried_until_the_statement_runs() {
        let refusals = usize::try_from(CONCURRENCY_RETRIES).expect("a small number");
        let (client, received) = stand_in(refusals, code::TOO_MANY_SIMULTANEOUS_QUERIES).await;
        let answer = client.query(ACCOUNT, "SELECT 1", &[], None).await;
        assert_eq!(answer.expect("the last retry runs"), "1\n");
        assert_eq!(received.load(Ordering::SeqCst), refusals + 1);
    }

    /// The retries are bounded: each refusal costs one of the account's quota errors.
    #[tokio::test]
    async fn concurrency_retries_stop_at_their_bound() {
        let (client, received) = stand_in(usize::MAX, code::TOO_MANY_SIMULTANEOUS_QUERIES).await;
        let failure = client
            .query(ACCOUNT, "SELECT 1", &[], None)
            .await
            .expect_err("every attempt is refused");
        assert_eq!(failure.code(), Some(code::TOO_MANY_SIMULTANEOUS_QUERIES));
        assert_eq!(
            received.load(Ordering::SeqCst),
            1 + usize::try_from(CONCURRENCY_RETRIES).expect("a small number")
        );
    }

    /// Only the concurrency refusal is retried: a used-up quota stays used up for its interval.
    #[tokio::test]
    async fn other_refusals_are_not_retried() {
        for code in [
            code::QUOTA_EXCEEDED,
            code::TIMEOUT_EXCEEDED,
            code::AUTHENTICATION_FAILED,
        ] {
            let (client, received) = stand_in(usize::MAX, code).await;
            let failure = client
                .query(ACCOUNT, "SELECT 1", &[], None)
                .await
                .expect_err("refused");
            assert_eq!(failure.code(), Some(code));
            assert_eq!(received.load(Ordering::SeqCst), 1, "{code}");
        }
    }

    #[test]
    fn retry_waits_are_jittered_halves_to_wholes_that_double() {
        let mut longest = Duration::ZERO;
        for retry in 0..CONCURRENCY_RETRIES {
            let full = CONCURRENCY_BACKOFF * (1 << retry);
            for _ in 0..100 {
                let wait = backoff(retry);
                assert!(full / 2 <= wait && wait <= full, "{retry}: {wait:?}");
            }
            longest += full;
        }
        assert!(longest <= Duration::from_millis(300), "{longest:?}");
    }

    #[test]
    fn error_codes_come_from_the_body() {
        assert_eq!(
            error_code(
                "Code: 516. DB::Exception: x: Authentication failed. (AUTHENTICATION_FAILED)"
            ),
            Some(code::AUTHENTICATION_FAILED)
        );
        assert_eq!(error_code("Code: 159. DB::Exception: Timeout"), Some(159));
        assert_eq!(error_code("Bad gateway"), None);
        assert_eq!(error_code("Code: x."), None);
    }
}
