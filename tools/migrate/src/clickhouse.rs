//! The ClickHouse part of a run: the DDL of `services/ingest/migrations/clickhouse/` and the
//! OpenTelemetry collector's account, over the HTTP interface with the access-management account.
//!
//! Every statement of those files is idempotent (services/ingest/README.md, "檔案格式"), but a
//! later `ALTER TABLE ... MODIFY TTL` would start a mutation on every run, so the run also
//! records each applied file in `trustiot_migrate.applied` in ClickHouse itself: a ClickHouse
//! restored or rebuilt separately from Postgres gets its files again. A file that failed halfway
//! is not recorded, and the next run repeats it from the start, which its idempotent statements
//! allow.
//!
//! Values (names, hashes) travel as query parameters (`{name:Type}`), never inside the SQL text.
//! ClickHouse 26.8 takes parameters in `CREATE USER` and `ALTER USER` but not in `GRANT` or
//! `DROP USER`, so the collector's role comes with `CREATE USER ... DEFAULT ROLE`, which grants
//! it, and an account with other grants is reported for an administrator to drop.

use std::time::{Duration, Instant};

use crate::config::{ClickHouseSettings, RunConfig};
use crate::registry::{Migration, Registry};
use crate::secret::Password;
use crate::{Error, Report, http, sha256_hex};

/// The role `0006_logs.sql` creates for the collector: INSERT and SHOW COLUMNS on the two log
/// tables, nothing else.
pub const COLLECTOR_ROLE: &str = "trustiot_otel_collector";

const RETRY_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);
/// Error text from ClickHouse is cut to this many characters in messages.
const ERROR_TEXT_LIMIT: usize = 500;

const BOOTSTRAP: [&str; 2] = [
    "CREATE DATABASE IF NOT EXISTS trustiot_migrate",
    "CREATE TABLE IF NOT EXISTS trustiot_migrate.applied (
    name String,
    sha256 FixedString(64),
    applied_at DateTime64(3, 'UTC') DEFAULT now64(3)
) ENGINE = MergeTree ORDER BY name",
];
const APPLIED: &str =
    "SELECT DISTINCT sha256 FROM trustiot_migrate.applied WHERE name = {name:String} FORMAT TSV";
const RECORD: &str =
    "INSERT INTO trustiot_migrate.applied (name, sha256) SELECT {name:String}, {sha256:String}";
const USER_EXISTS: &str = "SELECT count() FROM system.users WHERE name = {user:String} FORMAT TSV";
/// The roles granted to the user, whether each is a default role (active at login) and whether it
/// came with ADMIN OPTION, then its direct grants: the account must have the collector role, as a
/// default role without ADMIN OPTION, and nothing else.
const USER_ROLES: &str = "SELECT granted_role_name, granted_role_is_default, with_admin_option
FROM system.role_grants WHERE user_name = {user:String} ORDER BY granted_role_name FORMAT TSV";
const USER_GRANTS: &str =
    "SELECT count() FROM system.grants WHERE user_name = {user:String} FORMAT TSV";
const CREATE_COLLECTOR: &str = "CREATE USER {user:Identifier}
IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}
DEFAULT ROLE trustiot_otel_collector";
const SET_PASSWORD: &str =
    "ALTER USER {user:Identifier} IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}";
const PROBE: &str = "SELECT 1 FORMAT TSV";

/// The access-management account's client.
pub struct ClickHouse {
    client: reqwest::Client,
    url: String,
    username: String,
    password: String,
}

/// Splits a migration file into statements, by the file format of services/ingest/README.md:
/// lines whose first non-blank characters are `--` are comments; a statement ends with `;` at
/// the end of a line, and `;` appears nowhere else.
///
/// # Errors
///
/// When the file breaks that format.
pub fn split_statements(text: &str) -> Result<Vec<String>, String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("--") {
            continue;
        }
        let (body, ends) = match trimmed.strip_suffix(';') {
            Some(body) => (body, true),
            None => (trimmed, false),
        };
        if body.contains(';') {
            return Err(format!(
                "line {}: `;` may only end a statement, at the end of a line",
                index + 1
            ));
        }
        if !body.trim().is_empty() {
            current.push_str(body);
            current.push('\n');
        }
        if ends {
            let statement = current.trim().to_owned();
            if statement.is_empty() {
                return Err(format!("line {}: empty statement", index + 1));
            }
            statements.push(statement);
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        return Err("the last statement does not end with `;`".to_owned());
    }
    Ok(statements)
}

fn cut(text: &str) -> String {
    text.chars().take(ERROR_TEXT_LIMIT).collect()
}

/// A request that did not get a 200: the status and ClickHouse's error text.
struct Failure {
    status: Option<reqwest::StatusCode>,
    text: String,
}

impl Failure {
    fn is_authentication(&self) -> bool {
        self.text.contains("AUTHENTICATION_FAILED") || self.text.contains("Authentication failed")
    }
}

impl ClickHouse {
    /// A client for `TRUSTIOT_CLICKHOUSE_URL`; an `https://` URL needs
    /// `TRUSTIOT_CLICKHOUSE_CA_FILE`, the only CA it trusts.
    ///
    /// # Errors
    ///
    /// When the URL is https without a CA file, or the client cannot be built.
    pub fn new(config: &RunConfig, settings: &ClickHouseSettings) -> Result<Self, Error> {
        let url =
            config.platform.clickhouse_url.clone().ok_or_else(|| {
                Error::ClickHouse("TRUSTIOT_CLICKHOUSE_URL is not set".to_owned())
            })?;
        let client = if url.starts_with("https://") {
            let ca = config
                .platform
                .clickhouse_ca_file
                .as_deref()
                .ok_or_else(|| {
                    Error::ClickHouse(
                    "an https ClickHouse needs TRUSTIOT_CLICKHOUSE_CA_FILE: the Job trusts only \
                     the CA the deployment names (clickhouse.external.caSecret)"
                        .to_owned(),
                )
                })?;
            http::trusting(ca)
        } else {
            http::plain()
        }
        .map_err(|error| Error::ClickHouse(error.to_string()))?;
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_owned(),
            username: settings.username.clone(),
            password: settings.password.clone(),
        })
    }

    async fn send(
        &self,
        username: &str,
        password: &str,
        query: &str,
        params: &[(&str, &str)],
    ) -> Result<String, Failure> {
        let mut pairs: Vec<(String, &str)> = params
            .iter()
            .map(|(name, value)| (format!("param_{name}"), *value))
            .collect();
        pairs.push(("wait_end_of_query".to_owned(), "1"));
        let response = self
            .client
            .post(format!("{}/", self.url))
            .basic_auth(username, Some(password))
            .query(&pairs)
            .body(query.to_owned())
            .send()
            .await
            .map_err(|error| Failure {
                status: None,
                text: error.to_string(),
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| Failure {
            status: Some(status),
            text: error.to_string(),
        })?;
        if status.is_success() {
            Ok(text)
        } else {
            Err(Failure {
                status: Some(status),
                text: cut(&text),
            })
        }
    }

    /// Runs `query` as the access-management account.
    async fn query(
        &self,
        what: &str,
        query: &str,
        params: &[(&str, &str)],
    ) -> Result<String, Error> {
        self.send(&self.username, &self.password, query, params)
            .await
            .map_err(|failure| {
                Error::ClickHouse(format!(
                    "{what}: {}{}",
                    failure
                        .status
                        .map(|status| format!("HTTP {} ", status.as_u16()))
                        .unwrap_or_default(),
                    failure.text.trim()
                ))
            })
    }

    /// Waits until ClickHouse answers a query as the access-management account. On a fresh
    /// install its pod starts next to the Job.
    ///
    /// # Errors
    ///
    /// When it did not answer within `wait`.
    pub async fn wait_until_up(&self, wait: Duration) -> Result<(), Error> {
        let started = Instant::now();
        let mut reported = None::<Instant>;
        loop {
            match self.query("ping", PROBE, &[]).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    if started.elapsed() >= wait {
                        return Err(Error::ClickHouse(format!(
                            "not reachable within {wait:?}: {error}"
                        )));
                    }
                    if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                        tracing::warn!(error = %error, "ClickHouse does not answer yet; retrying");
                        reported = Some(Instant::now());
                    }
                    tokio::time::sleep(RETRY_EVERY).await;
                }
            }
        }
    }

    /// The DDL files, then the collector's account.
    ///
    /// # Errors
    ///
    /// The first statement that fails, or a file that changed after it was applied.
    pub async fn migrate(
        &self,
        registry: &Registry,
        collector: Option<&(String, Password)>,
        report: &mut Report,
    ) -> Result<(), Error> {
        for statement in BOOTSTRAP {
            self.query(
                "create the bookkeeping table trustiot_migrate.applied",
                statement,
                &[],
            )
            .await?;
        }
        for migration in registry.clickhouse {
            self.apply(migration, report).await?;
        }
        if let Some((user, password)) = collector {
            self.ensure_collector(user, password, report).await?;
        }
        Ok(())
    }

    async fn apply(&self, migration: &Migration, report: &mut Report) -> Result<(), Error> {
        let label = format!("clickhouse/{}", migration.name);
        let sha256 = sha256_hex(migration.sql);
        let recorded = self
            .query(
                &format!("read whether {label} was applied"),
                APPLIED,
                &[("name", migration.name)],
            )
            .await?;
        let recorded: Vec<&str> = recorded.lines().filter(|line| !line.is_empty()).collect();
        if recorded.iter().any(|hash| *hash != sha256) {
            return Err(Error::Changed {
                component: "clickhouse".to_owned(),
                name: migration.name.to_owned(),
            });
        }
        if !recorded.is_empty() {
            tracing::debug!(migration = %label, "already applied");
            return Ok(());
        }
        let statements = split_statements(migration.sql)
            .map_err(|problem| Error::ClickHouse(format!("{label}: {problem}")))?;
        for (index, statement) in statements.iter().enumerate() {
            self.query(&format!("{label}, statement {}", index + 1), statement, &[])
                .await?;
        }
        self.query(
            &format!("record {label}"),
            RECORD,
            &[("name", migration.name), ("sha256", &sha256)],
        )
        .await?;
        report.change(format!("applied {label}"));
        Ok(())
    }

    /// The collector's account: exactly the collector role, no other grants, and a password
    /// that logs in. An account that has anything else stops the run.
    async fn ensure_collector(
        &self,
        user: &str,
        password: &Password,
        report: &mut Report,
    ) -> Result<(), Error> {
        let params = [("user", user)];
        let exists = self
            .query("look for the collector account", USER_EXISTS, &params)
            .await?;
        let create = exists.trim() == "0";
        if !create {
            let roles = self
                .query("read the collector account's roles", USER_ROLES, &params)
                .await?;
            let grants = self
                .query("read the collector account's grants", USER_GRANTS, &params)
                .await?;
            let roles: Vec<&str> = roles.lines().filter(|line| !line.is_empty()).collect();
            let expected = format!("{COLLECTOR_ROLE}\t1\t0");
            if roles != [expected.as_str()] || grants.trim() != "0" {
                // Never repaired by the Job: someone changed the account by hand, and dropping
                // it is an administrator's decision (DROP USER takes no query parameters either).
                return Err(Error::ClickHouse(format!(
                    "the collector account {user} has other roles or grants than \
                     {COLLECTOR_ROLE} as its default role; an administrator must drop it \
                     (DROP USER {user}), and the next run creates it again \
                     (docs/runbooks/migrations.md)"
                )));
            }
        }
        let (hash, salt) = password
            .clickhouse_sha256()
            .map_err(|error| Error::Random(error.to_string()))?;
        let auth = [
            ("user", user),
            ("hash", hash.as_str()),
            ("salt", salt.as_str()),
        ];
        if create {
            self.query("create the collector account", CREATE_COLLECTOR, &auth)
                .await?;
            report.change(format!("created ClickHouse account {user}"));
            return Ok(());
        }
        match self.send(user, password.expose(), PROBE, &[]).await {
            Ok(_) => Ok(()),
            Err(failure) if failure.is_authentication() => {
                self.query("set the collector account's password", SET_PASSWORD, &auth)
                    .await?;
                report.change(format!("set the password of ClickHouse account {user}"));
                Ok(())
            }
            Err(failure) => Err(Error::ClickHouse(format!(
                "log in as {user} to check its password: {}",
                failure.text.trim()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_the_ingest_tests() {
        let text = "-- comment; with a semicolon\nCREATE TABLE a\n(x UInt8);\n\nGRANT x TO y;\n";
        assert_eq!(
            split_statements(text).expect("valid"),
            ["CREATE TABLE a\n(x UInt8)", "GRANT x TO y"]
        );
        assert!(split_statements("SELECT 1; SELECT 2;").is_err());
        assert!(split_statements("SELECT 1").is_err());
        assert!(split_statements(";\n").is_err());
    }

    #[test]
    fn every_product_file_splits() {
        for migration in crate::registry::PRODUCT.clickhouse {
            let statements = split_statements(migration.sql)
                .unwrap_or_else(|problem| panic!("{}: {problem}", migration.name));
            assert!(!statements.is_empty(), "{}", migration.name);
        }
    }
}
