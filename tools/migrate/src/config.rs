//! The Job's settings, loaded like a service's (crates/service-kit, "設定"): the chart's platform
//! settings (`TRUSTIOT_<NAME>`) and the Job's own `TRUSTIOT__<SECTION>__<KEY>` variables. Error
//! messages name keys and variables, never values.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

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
    /// Absent: the run sets no password (every login role keeps the one it has, and no
    /// collector account is managed).
    #[serde(default)]
    pub passwords: Option<PasswordSettings>,
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

/// `postgres.*`: the migration role.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresSettings {
    /// `TRUSTIOT__POSTGRES__USERNAME`: the migration role (the database owner, with CREATEROLE).
    pub username: String,
    /// `TRUSTIOT__POSTGRES__PASSWORD`
    pub password: String,
}

impl fmt::Debug for PostgresSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresSettings")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// `clickhouse.*`: the account that applies the DDL and creates the collector's account.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseSettings {
    /// `TRUSTIOT__CLICKHOUSE__USERNAME`: an account with access management (the bundled
    /// ClickHouse's bootstrap admin).
    pub username: String,
    /// `TRUSTIOT__CLICKHOUSE__PASSWORD`
    pub password: String,
}

impl fmt::Debug for ClickHouseSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseSettings")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// `passwords.*`: the Kubernetes Secrets that hold the passwords of the login roles and the
/// collector account the run manages. The chart declares them empty; the run generates every
/// missing password, writes it into its Secret, and sets each role or account to the password
/// in its Secret (src/passwords.rs). Nobody else handles them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordSettings {
    /// `TRUSTIOT__PASSWORDS__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__PASSWORDS__NAMESPACE`: the release namespace, where the Secrets are.
    pub namespace: String,
    /// `TRUSTIOT__PASSWORDS__TOKEN_FILE`: the Job's projected ServiceAccount token, read on
    /// every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__PASSWORDS__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__PASSWORDS__GENERATION`: the chart's `migrate.passwordGeneration`. A Secret
    /// whose annotation records another generation gets new passwords for every key the run
    /// manages in it (a rotation).
    pub generation: String,
    /// `TRUSTIOT__PASSWORDS__POSTGRES`: `role=secret` pairs separated by commas: the login role
    /// and the Secret whose key of the same name holds its password.
    #[serde(default)]
    pub postgres: Option<String>,
    /// `TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR`: `account=secret`: the OpenTelemetry
    /// collector's ClickHouse account, created with the role `trustiot_otel_collector` only, and
    /// the Secret whose key of the same name holds its password. Needs `clickhouse.*`.
    #[serde(default)]
    pub clickhouse_collector: Option<String>,
}

/// One password the run manages: whose it is and where it is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The login role or account, also the key in the Secret.
    pub owner: String,
    /// The Secret in the release namespace.
    pub secret: String,
}

impl PasswordSettings {
    /// The Postgres login roles and their Secrets.
    ///
    /// # Errors
    ///
    /// When a pair is not `role=secret` with valid names.
    pub fn postgres(&self) -> Result<Vec<Binding>, Invalid> {
        bindings("passwords.postgres", self.postgres.as_deref())
    }

    /// The collector's account and its Secret.
    ///
    /// # Errors
    ///
    /// When the value is not one `account=secret` pair with valid names.
    pub fn clickhouse_collector(&self) -> Result<Option<Binding>, Invalid> {
        let key = "passwords.clickhouse_collector";
        let mut found = bindings(key, self.clickhouse_collector.as_deref())?;
        if found.len() > 1 {
            return Err(Invalid::new(key, "must be one account=secret pair"));
        }
        Ok(found.pop())
    }
}

fn bindings(key: &str, value: Option<&str>) -> Result<Vec<Binding>, Invalid> {
    let mut found: Vec<Binding> = Vec::new();
    for pair in value.unwrap_or_default().split(',').map(str::trim) {
        if pair.is_empty() {
            continue;
        }
        let Some((owner, secret)) = pair.split_once('=') else {
            return Err(Invalid::new(
                key,
                "must list name=secret pairs, separated by commas",
            ));
        };
        let (owner, secret) = (owner.trim(), secret.trim());
        if !is_plain_identifier(owner) {
            return Err(Invalid::new(
                key,
                "names must be lower-case role or account names (letters, digits, _)",
            ));
        }
        if !is_object_name(secret) {
            return Err(Invalid::new(
                key,
                "Secrets must be Kubernetes object names (lower-case letters, digits, - and .)",
            ));
        }
        if found.iter().any(|binding| binding.owner == owner) {
            return Err(Invalid::new(key, "lists a name twice"));
        }
        found.push(Binding {
            owner: owner.to_owned(),
            secret: secret.to_owned(),
        });
    }
    Ok(found)
}

/// A Kubernetes object name (DNS-1123 subdomain): lower-case letters, digits, `-` and `.`,
/// starting and ending with a letter or digit, at most 253 characters.
#[must_use]
pub fn is_object_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && text.starts_with(|c: char| c.is_ascii_alphanumeric())
        && text.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// A Kubernetes namespace or Job name (DNS-1123 label).
fn is_dns_label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
}

/// The API server URL rule of `kube.api_url` and `passwords.api_url`.
fn check_api_url(key: &str, url: &str) -> Result<(), Invalid> {
    let loopback = url.starts_with("http://127.0.0.1:");
    if !(url.starts_with("https://") || loopback) || url.contains('@') {
        return Err(Invalid::new(
            key,
            "must be an https:// URL without credentials (http:// only on 127.0.0.1)",
        ));
    }
    Ok(())
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
        if let Some(passwords) = &self.passwords {
            check_api_url("passwords.api_url", &passwords.api_url)?;
            if !is_dns_label(&passwords.namespace) {
                return Err(Invalid::new(
                    "passwords.namespace",
                    "must be a Kubernetes namespace name (DNS label)",
                ));
            }
            if !is_generation(&passwords.generation) {
                return Err(Invalid::new(
                    "passwords.generation",
                    "must be 1 to 9 decimal digits (generations are compared as numbers and only go up)",
                ));
            }
            let postgres = passwords.postgres()?;
            let collector = passwords.clickhouse_collector()?;
            if collector.is_some() && self.clickhouse.is_none() {
                return Err(Invalid::new(
                    "passwords.clickhouse_collector",
                    "needs clickhouse.*, the account that creates the collector's account",
                ));
            }
            // One key holds one password: a Postgres role and the collector account in the same
            // Secret must not share a name.
            if let Some(collector) = &collector
                && postgres.contains(collector)
            {
                return Err(Invalid::new(
                    "passwords.clickhouse_collector",
                    "uses the same key of the same Secret as a Postgres role",
                ));
            }
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
}

/// A password generation: 1 to 9 decimal digits, as the chart's schema allows. Generations are
/// compared as numbers: only a higher one rotates (`passwords::plan`).
#[must_use]
pub fn is_generation(text: &str) -> bool {
    crate::passwords::parse_generation(text).is_some()
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
        if !is_dns_label(&kube.job) {
            return Err(Invalid::new(
                "kube.job",
                "must be a Kubernetes object name (DNS label)",
            ));
        }
        if !is_dns_label(&kube.namespace) {
            return Err(Invalid::new(
                "kube.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        check_api_url("kube.api_url", &kube.api_url)
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

    fn settings(postgres: &str, collector: &str) -> PasswordSettings {
        PasswordSettings {
            api_url: "https://kubernetes.default.svc".to_owned(),
            namespace: "trustiot".to_owned(),
            token_file: PathBuf::from("/token"),
            ca_file: PathBuf::from("/ca.crt"),
            generation: "1".to_owned(),
            postgres: Some(postgres.to_owned()),
            clickhouse_collector: Some(collector.to_owned()),
        }
    }

    #[test]
    fn password_bindings() {
        let ok = settings(
            " identity_app=trustiot-identity-password , assets_app=trustiot-assets-password,",
            "trustiot_collector=trustiot-otel-collector-password",
        );
        assert_eq!(
            ok.postgres().expect("valid"),
            [
                Binding {
                    owner: "identity_app".to_owned(),
                    secret: "trustiot-identity-password".to_owned()
                },
                Binding {
                    owner: "assets_app".to_owned(),
                    secret: "trustiot-assets-password".to_owned()
                },
            ]
        );
        assert_eq!(
            ok.clickhouse_collector().expect("valid"),
            Some(Binding {
                owner: "trustiot_collector".to_owned(),
                secret: "trustiot-otel-collector-password".to_owned()
            })
        );
        assert_eq!(settings("", "").postgres().expect("empty"), []);
        for bad in [
            "identity_app",
            "Identity=s",
            "identity_app=Secret",
            "identity_app=-s",
            "identity_app=s,identity_app=t",
            "identity_app=s/../t",
        ] {
            assert!(settings(bad, "").postgres().is_err(), "{bad}");
        }
        assert!(
            settings("", "a=s,b=t").clickhouse_collector().is_err(),
            "one collector account"
        );
    }

    #[test]
    fn generations() {
        for good in ["1", "2", "20261013", "999999999"] {
            assert!(is_generation(good), "{good}");
        }
        for bad in [
            "",
            "a b",
            "x/y",
            "ü",
            "2026-10-13",
            "v2",
            "1234567890",
            &"1".repeat(64),
        ] {
            assert!(!is_generation(bad), "{bad}");
        }
    }
}
