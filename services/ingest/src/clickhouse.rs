//! ClickHouse's HTTP interface as this service uses it: one statement per request as the
//! service's own account, every value as a query parameter (`{name:Type}`, sent as
//! `param_<name>`), and `wait_end_of_query`, so ClickHouse answers only once the statement has
//! finished and a failure midway is an error status, never a cut-off 200.
//!
//! Readiness checks the connection ([`ClickHouse::ping`]); the telemetry path
//! ([`crate::telemetry`]) runs its statements through [`ClickHouse::execute`], each with the
//! settings of its stream ([`crate::rollup::stream_settings`]).

use std::path::Path;
use std::time::Duration;

use crate::rollup::CLIENT_TIMEOUT_SECONDS;
use crate::transport;

/// How long one request may take: [`CLIENT_TIMEOUT_SECONDS`], the shortest the I1 rewrite's
/// contract allows (src/rollup.rs), so the server has ended a statement of a stream (its wait for
/// an earlier one and its own `max_execution_time`) before the client gives up on it. Readiness
/// bounds its own check far below this (service-kit's 800 ms).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(CLIENT_TIMEOUT_SECONDS as u64);

/// One statement for [`ClickHouse::execute`].
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// The statement: static SQL, every value a query parameter (`{name:Type}`).
    pub statement: &'a str,
    /// The query parameters, as name (without `param_`) and value.
    pub params: &'a [(&'a str, &'a str)],
    /// Settings for this statement, as name and value, sent in the URL.
    pub settings: &'a [(&'a str, String)],
    /// For an `INSERT ... FORMAT <format>`: the rows in that format, sent as the body.
    pub data: Option<&'a str>,
}

/// The statement of [`ClickHouse::ping`]: it needs no table, so it checks the address, TLS and
/// the account's password, not the account's grants (the telemetry path reports a missing grant
/// as the statement's error).
const PING: &str = "SELECT 1";

/// The service's ClickHouse account and its password.
#[derive(Clone)]
pub struct Account {
    user: String,
    password: String,
}

impl Account {
    /// The account `user` with `password`.
    #[must_use]
    pub fn new(user: &str, password: &str) -> Self {
        Self {
            user: user.to_owned(),
            password: password.to_owned(),
        }
    }

    /// The account's name.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }
}

/// Never prints the password.
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Why a statement did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// No response: ClickHouse was unreachable or did not answer within [`REQUEST_TIMEOUT`].
    #[error("no response from ClickHouse: {0}")]
    Unreachable(String),
    /// ClickHouse refused or failed the statement. `code` is ClickHouse's error code, when the
    /// body names one. The body is kept for tests and never logged: it can quote the statement
    /// with its parameters.
    #[error("ClickHouse answered {status} with error code {code:?}")]
    Failed {
        status: u16,
        code: Option<u32>,
        body: String,
    },
}

impl QueryError {
    /// ClickHouse's error code, when it answered with one.
    #[must_use]
    pub fn code(&self) -> Option<u32> {
        match self {
            Self::Failed { code, .. } => *code,
            Self::Unreachable(_) => None,
        }
    }
}

/// The code at the start of a ClickHouse error body: `Code: 516. DB::Exception: ...`.
fn error_code(body: &str) -> Option<u32> {
    let rest = body.trim_start().strip_prefix("Code: ")?;
    let digits = rest.split('.').next()?;
    digits.parse().ok()
}

/// A client of one ClickHouse server as the service's account.
#[derive(Clone)]
pub struct ClickHouse {
    client: transport::Client,
    url: String,
    account: Account,
}

impl std::fmt::Debug for ClickHouse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouse")
            .field("url", &self.url)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl ClickHouse {
    /// A client for `url` (`http://` or `https://`) as `account`; `https` trusts only
    /// `ca_file` and checks the server's name against the URL's host.
    ///
    /// # Errors
    ///
    /// When `https` comes without a CA file, or the CA file is unusable.
    pub fn new(
        url: &str,
        ca_file: Option<&Path>,
        account: Account,
    ) -> Result<Self, transport::ClientError> {
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
            account,
        })
    }

    /// Runs `statement` with `params` as its query parameters. Returns the response body.
    ///
    /// # Errors
    ///
    /// [`QueryError::Unreachable`] without a response, [`QueryError::Failed`] for any status
    /// but 200.
    pub async fn query(
        &self,
        statement: &str,
        params: &[(&str, &str)],
    ) -> Result<String, QueryError> {
        self.execute(&Request {
            statement,
            params,
            settings: &[],
            data: None,
        })
        .await
    }

    /// Runs `request`: its statement with its query parameters and settings, and for an
    /// `INSERT ... FORMAT <format>` its rows as the body (the statement then goes in the URL's
    /// `query`). Returns the response body.
    ///
    /// # Errors
    ///
    /// As [`ClickHouse::query`].
    pub async fn execute(&self, request: &Request<'_>) -> Result<String, QueryError> {
        let names: Vec<String> = request
            .params
            .iter()
            .map(|(name, _)| format!("param_{name}"))
            .collect();
        let mut query: Vec<(&str, &str)> = names
            .iter()
            .map(String::as_str)
            .zip(request.params.iter().map(|(_, value)| *value))
            .collect();
        query.extend(
            request
                .settings
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        );
        query.push(("wait_end_of_query", "1"));
        let body = match &request.data {
            Some(data) => {
                query.push(("query", request.statement));
                (*data).to_owned()
            }
            None => request.statement.to_owned(),
        };
        let response = self
            .client
            .post(
                &self.url,
                &query,
                (&self.account.user, &self.account.password),
                body,
            )
            .await
            .map_err(|error| QueryError::Unreachable(error.to_string()))?;
        if response.status == http::StatusCode::OK {
            return Ok(response.body);
        }
        Err(QueryError::Failed {
            status: response.status.as_u16(),
            code: error_code(&response.body),
            body: response.body,
        })
    }

    /// Whether ClickHouse answers the service's account: the readiness check.
    ///
    /// # Errors
    ///
    /// A [`PingError`] that names the kind of failure and ClickHouse's error code, never the
    /// response body.
    pub async fn ping(&self) -> Result<(), PingError> {
        match self.query(PING, &[]).await {
            Ok(body) if body.trim() == "1" => Ok(()),
            Ok(_) => Err(PingError("unexpected answer".to_owned())),
            Err(QueryError::Unreachable(reason)) => Err(PingError(reason)),
            Err(QueryError::Failed { status, code, .. }) => Err(PingError(match code {
                Some(code) => format!("ClickHouse error {code} (HTTP {status})"),
                None => format!("HTTP {status}"),
            })),
        }
    }
}

/// Why [`ClickHouse::ping`] failed, safe to log: the transport's reason (which never carries
/// the credentials) or ClickHouse's status and error code.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PingError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_come_from_the_body() {
        assert_eq!(
            error_code(
                "Code: 516. DB::Exception: x: Authentication failed. (AUTHENTICATION_FAILED)"
            ),
            Some(516)
        );
        assert_eq!(error_code("Bad gateway"), None);
        assert_eq!(error_code("Code: x."), None);
    }

    #[test]
    fn the_request_timeout_is_the_rewrite_contracts() {
        // src/rollup.rs: at least CLIENT_TIMEOUT_SECONDS, longer than the server's wait and run.
        assert!(REQUEST_TIMEOUT >= Duration::from_secs(u64::from(CLIENT_TIMEOUT_SECONDS)));
    }

    #[test]
    fn the_account_never_prints_its_password() {
        let account = Account::new("trustiot_ingest_writer", "secret-value");
        assert!(!format!("{account:?}").contains("secret-value"));
    }
}
