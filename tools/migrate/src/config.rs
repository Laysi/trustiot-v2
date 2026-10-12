//! The Job's settings, loaded like a service's (crates/service-kit, "設定"): the chart's platform
//! settings (`TRUSTIOT_<NAME>`) and the Job's own `TRUSTIOT__<SECTION>__<KEY>` variables. Error
//! messages name keys and variables, never values.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::secret::Password;

/// How long the Job waits by default for each database to accept it and for another run's lock.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(600);

/// The longest wait it accepts; the chart's Job deadline bounds the whole run anyway.
pub const MAX_WAIT: Duration = Duration::from_secs(3600);

/// Settings of `migrate run`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub platform: Platform,
    pub postgres: PostgresSettings,
    /// Absent: the run does not touch ClickHouse (for example a test of the Postgres part).
    #[serde(default)]
    pub clickhouse: Option<ClickHouseSettings>,
    #[serde(default)]
    pub wait: WaitSettings,
}

/// The platform settings the Job reads (the chart's `<release>-platform` ConfigMap and the
/// Postgres CA mount). No `deny_unknown_fields`: every platform setting reaches the Job.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_POSTGRES_HOST`
    pub postgres_host: String,
    /// `TRUSTIOT_POSTGRES_PORT`
    pub postgres_port: u16,
    /// `TRUSTIOT_POSTGRES_DATABASE`
    pub postgres_database: String,
    /// `TRUSTIOT_POSTGRES_SSLMODE`: only `verify-full`.
    pub postgres_sslmode: SslMode,
    /// `TRUSTIOT_POSTGRES_CA_FILE`: required, the only trust anchor.
    pub postgres_ca_file: PathBuf,
    /// `TRUSTIOT_CLICKHOUSE_URL`
    #[serde(default)]
    pub clickhouse_url: Option<String>,
    /// `TRUSTIOT_CLICKHOUSE_CA_FILE`: the CA of an `https` ClickHouse.
    #[serde(default)]
    pub clickhouse_ca_file: Option<PathBuf>,
}

/// `postgres.*`: the migration role and where the service roles' passwords are.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresSettings {
    /// `TRUSTIOT__POSTGRES__USERNAME`: the migration role (the database owner, with CREATEROLE).
    pub username: String,
    /// `TRUSTIOT__POSTGRES__PASSWORD`
    pub password: String,
    /// `TRUSTIOT__POSTGRES__ROLE_PASSWORDS_DIR`: a directory with one file per login role, named
    /// after the role (a mounted Secret). A role without a file keeps the password it has; a role
    /// the run creates without one has none and cannot log in.
    #[serde(default)]
    pub role_passwords_dir: Option<PathBuf>,
    /// `TRUSTIOT__POSTGRES__REQUIRED_PASSWORDS`: login roles, separated by commas, that a service
    /// of this deployment logs in with (the chart lists the enabled services that use Postgres).
    /// Each must be a login role of the registry and have a password file, or the run fails
    /// before it changes anything: a service must never start with a role it cannot log in as.
    #[serde(default)]
    pub required_passwords: Option<String>,
}

impl PostgresSettings {
    /// The roles of `required_passwords`, without blanks.
    #[must_use]
    pub fn required_passwords(&self) -> Vec<&str> {
        self.required_passwords
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|role| !role.is_empty())
            .collect()
    }
}

impl fmt::Debug for PostgresSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresSettings")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("role_passwords_dir", &self.role_passwords_dir)
            .field("required_passwords", &self.required_passwords)
            .finish()
    }
}

/// `clickhouse.*`: the account that applies the DDL and the accounts the run creates.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseSettings {
    /// `TRUSTIOT__CLICKHOUSE__USERNAME`: an account with access management (the bundled
    /// ClickHouse's bootstrap admin).
    pub username: String,
    /// `TRUSTIOT__CLICKHOUSE__PASSWORD`
    pub password: String,
    /// `TRUSTIOT__CLICKHOUSE__COLLECTOR_USERNAME`: the OpenTelemetry collector's account, created
    /// with the role `trustiot_otel_collector` only. Set together with the password, or neither.
    #[serde(default)]
    pub collector_username: Option<String>,
    /// `TRUSTIOT__CLICKHOUSE__COLLECTOR_PASSWORD`
    #[serde(default)]
    pub collector_password: Option<String>,
}

impl fmt::Debug for ClickHouseSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseSettings")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("collector_username", &self.collector_username)
            .field("collector_password", &"<redacted>")
            .finish()
    }
}

/// `wait.*`: how long the run waits before it gives up.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WaitSettings {
    /// `TRUSTIOT__WAIT__DATABASES`: for Postgres and ClickHouse to accept connections (a fresh
    /// install creates them next to the Job). Default 10 minutes.
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub databases: Duration,
    /// `TRUSTIOT__WAIT__LOCK`: for another migration run to finish. Default 10 minutes.
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub lock: Duration,
}

impl Default for WaitSettings {
    fn default() -> Self {
        Self {
            databases: DEFAULT_WAIT,
            lock: DEFAULT_WAIT,
        }
    }
}

/// A role, account or schema name the Job accepts: lower-case ASCII letters, digits and `_`,
/// starting with a letter or `_`, at most 63 bytes, not `pg_*` (Postgres reserves those).
#[must_use]
pub fn is_plain_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && !name.starts_with("pg_")
}

fn check_wait(key: &str, value: Duration) -> Result<(), Invalid> {
    if value.is_zero() || value > MAX_WAIT {
        return Err(Invalid::new(key, "must be more than 0 and at most 1 hour"));
    }
    Ok(())
}

impl Validate for RunConfig {
    fn validate(&self) -> Result<(), Invalid> {
        if !is_plain_identifier(&self.postgres.username) {
            return Err(Invalid::new(
                "postgres.username",
                "must be a lower-case role name (letters, digits, _)",
            ));
        }
        let required = self.postgres.required_passwords();
        if required.iter().any(|role| !is_plain_identifier(role)) {
            return Err(Invalid::new(
                "postgres.required_passwords",
                "must list lower-case role names (letters, digits, _), separated by commas",
            ));
        }
        if !required.is_empty() && self.postgres.role_passwords_dir.is_none() {
            return Err(Invalid::new(
                "postgres.required_passwords",
                "needs postgres.role_passwords_dir, where the passwords are",
            ));
        }
        if let Some(clickhouse) = &self.clickhouse {
            match self.platform.clickhouse_url.as_deref() {
                Some(url) if url.starts_with("http://") || url.starts_with("https://") => {}
                _ => {
                    return Err(Invalid::new(
                        "platform.clickhouse_url",
                        "TRUSTIOT_CLICKHOUSE_URL must be an http:// or https:// URL when clickhouse.* is set",
                    ));
                }
            }
            if !is_plain_identifier(&clickhouse.username) {
                return Err(Invalid::new(
                    "clickhouse.username",
                    "must be a lower-case account name (letters, digits, _)",
                ));
            }
            match (
                &clickhouse.collector_username,
                &clickhouse.collector_password,
            ) {
                (None, None) => {}
                (Some(name), Some(_)) if is_plain_identifier(name) => {}
                (Some(_), Some(_)) => {
                    return Err(Invalid::new(
                        "clickhouse.collector_username",
                        "must be a lower-case account name (letters, digits, _)",
                    ));
                }
                _ => {
                    return Err(Invalid::new(
                        "clickhouse.collector_password",
                        "set the collector's user name and password together, or neither",
                    ));
                }
            }
        }
        check_wait("wait.databases", self.wait.databases)?;
        check_wait("wait.lock", self.wait.lock)
    }
}

impl RunConfig {
    /// How the run connects to Postgres as the migration role.
    #[must_use]
    pub fn postgres(&self) -> PostgresConfig {
        PostgresConfig {
            host: self.platform.postgres_host.clone(),
            port: self.platform.postgres_port,
            database: self.platform.postgres_database.clone(),
            ssl_mode: self.platform.postgres_sslmode,
            ca_file: self.platform.postgres_ca_file.clone(),
            username: self.postgres.username.clone(),
            password: self.postgres.password.clone(),
        }
    }

    /// The collector account and its password, when configured.
    ///
    /// # Errors
    ///
    /// When the password is not acceptable.
    pub fn collector(&self) -> Result<Option<(String, Password)>, crate::secret::PasswordError> {
        let Some(clickhouse) = &self.clickhouse else {
            return Ok(None);
        };
        match (
            &clickhouse.collector_username,
            &clickhouse.collector_password,
        ) {
            (Some(name), Some(password)) => {
                Ok(Some((name.clone(), Password::new(name, password.clone())?)))
            }
            _ => Ok(None),
        }
    }
}

/// Settings of `migrate wait-for-job`, the init container that holds a service's pod until the
/// release's migration Job has completed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitForJobConfig {
    pub kube: KubeSettings,
}

/// `kube.*`: which Job, and how to reach the Kubernetes API.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubeSettings {
    /// `TRUSTIOT__KUBE__JOB`: the migration Job the pod waits for, named after a hash of its spec.
    pub job: String,
    /// `TRUSTIOT__KUBE__NAMESPACE`: the release namespace (the chart sets it with the downward
    /// API).
    pub namespace: String,
    /// `TRUSTIOT__KUBE__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`, a name the API server's certificate carries).
    /// `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__KUBE__TOKEN_FILE`: the projected ServiceAccount token, read on every request
    /// because the kubelet rotates it.
    pub token_file: PathBuf,
    /// `TRUSTIOT__KUBE__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
}

impl Validate for WaitForJobConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let kube = &self.kube;
        let dns_label = |text: &str| {
            !text.is_empty()
                && text.len() <= 63
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !text.starts_with('-')
                && !text.ends_with('-')
        };
        if !dns_label(&kube.job) {
            return Err(Invalid::new(
                "kube.job",
                "must be a Kubernetes object name (DNS label)",
            ));
        }
        if !dns_label(&kube.namespace) {
            return Err(Invalid::new(
                "kube.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        let loopback = kube.api_url.starts_with("http://127.0.0.1:");
        if !(kube.api_url.starts_with("https://") || loopback) || kube.api_url.contains('@') {
            return Err(Invalid::new(
                "kube.api_url",
                "must be an https:// URL without credentials (http:// only on 127.0.0.1)",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_identifiers() {
        for good in ["identity_app", "_x", "trustiot_owner", "a1"] {
            assert!(is_plain_identifier(good), "{good}");
        }
        for bad in [
            "",
            "Identity",
            "1a",
            "a-b",
            "pg_monitor",
            "a b",
            "a\"b",
            &"a".repeat(64),
        ] {
            assert!(!is_plain_identifier(bad), "{bad}");
        }
    }
}
