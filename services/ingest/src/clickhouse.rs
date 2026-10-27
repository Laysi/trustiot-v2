//! ClickHouse's HTTP interface as this service uses it: one statement per request as the
//! service's own account, every value as a query parameter (`{name:Type}`, sent as
//! `param_<name>`), and `wait_end_of_query`, so ClickHouse answers only once the statement has
//! finished and a failure midway is an error status, never a cut-off 200.
//!
//! This release only checks the connection for readiness ([`ClickHouse::ping`]); the telemetry
//! path (Laysi/trustiot-v2#641) writes through [`ClickHouse::query`].

use std::path::Path;
use std::time::Duration;

use crate::transport;

/// How long one request may take. Readiness bounds its own check far below this (service-kit's
/// 800 ms), so this only stops a statement of the telemetry path that ClickHouse never answers.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
        let response = self
            .client
            .post(
                &self.url,
                &query,
                (&self.account.user, &self.account.password),
                statement.to_owned(),
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
    fn the_account_never_prints_its_password() {
        let account = Account::new("trustiot_ingest_writer", "secret-value");
        assert!(!format!("{account:?}").contains("secret-value"));
    }
}
