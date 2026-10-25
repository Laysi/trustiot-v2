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

/// ClickHouse error codes the service tells apart (ClickHouse's `ErrorCodes.cpp`).
pub mod code {
    /// `BAD_ARGUMENTS`, for example an unknown time zone.
    pub const BAD_ARGUMENTS: u32 = 36;
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
        Err(QueryError::Failed {
            status: response.status.as_u16(),
            code: error_code(&response.body),
            body: response.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
