//! The Job's settings, loaded like a service's (crates/service-kit, "設定"): the chart's platform
//! settings (`TRUSTIOT_<NAME>`) and the Job's own `TRUSTIOT__<SECTION>__<KEY>` variables. Error
//! messages name keys and variables, never values.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::retention::RawRetention;

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

/// `clickhouse.*`: the account that applies the DDL and creates the collector's account, and
/// where its password comes from: exactly one of `password` and `password_secret`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseSettings {
    /// `TRUSTIOT__CLICKHOUSE__USERNAME`: an account with access management (the bundled
    /// ClickHouse's bootstrap admin).
    pub username: String,
    /// `TRUSTIOT__CLICKHOUSE__PASSWORD`: the password itself (an external ClickHouse's migration
    /// account, or a bundled one whose admin Secret a person created).
    #[serde(default)]
    pub password: Option<String>,
    /// `TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET`: the Secret, in the release namespace, whose key
    /// named after the account holds the password that the bundled ClickHouse's init container
    /// generated (src/clickhouse_admin.rs). The run reads it through the Kubernetes API of
    /// `passwords.*` once ClickHouse is about to be used, and reads it again while ClickHouse
    /// refuses it (a rotation restarts ClickHouse with a new password).
    #[serde(default)]
    pub password_secret: Option<String>,
    /// `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`: the raw data retention in days, which the run
    /// sets as the TTL of `trustiot.telemetry` and `trustiot.batch_commits` (src/clickhouse.rs).
    /// Unset: the platform value (`retention::RAW_RETENTION_DAYS`, 180 days). It must be longer
    /// than the gateway buffer's maximum age, or the run stops before it changes anything
    /// (Laysi/trustiot-v2#92, docs/architecture/data-retention.md). The chart does not set it;
    /// a values key for on-premises deployments comes with Laysi/trustiot-v2#446.
    #[serde(default)]
    pub raw_retention_days: Option<u32>,
}

impl ClickHouseSettings {
    /// The raw data retention the run applies: the setting, or the platform value.
    ///
    /// # Errors
    ///
    /// When the setting is not longer than the gateway buffer's maximum age, or implausibly long.
    pub fn raw_retention(&self) -> Result<RawRetention, Invalid> {
        self.raw_retention_days
            .map_or(Ok(RawRetention::PLATFORM), RawRetention::from_days)
            .map_err(|error| Invalid::new("clickhouse.raw_retention_days", error.to_string()))
    }
}

impl fmt::Debug for ClickHouseSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseSettings")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("password_secret", &self.password_secret)
            .field("raw_retention_days", &self.raw_retention_days)
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
    /// `TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY`: `account=secret`: the telemetry-query
    /// service's ClickHouse account ([`TELEMETRY_QUERY_ACCOUNT`], which
    /// `0008_telemetry_query.sql` defines without a password) and the Secret whose key of the
    /// same name holds its password. The run sets that password and lets the account log in;
    /// without this setting it keeps the account from logging in. Needs `clickhouse.*`.
    #[serde(default)]
    pub clickhouse_telemetry_query: Option<String>,
}

/// The telemetry-query service's ClickHouse account, as `0008_telemetry_query.sql` names it.
pub const TELEMETRY_QUERY_ACCOUNT: &str = "trustiot_telemetry_query";

/// The names of the tenant accounts telemetry-query creates, which `0008_telemetry_query.sql`
/// lets its account create and change.
pub const TENANT_ACCOUNT_PREFIX: &str = "trustiot_tenant_account_";

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

    /// The telemetry-query service's account and its Secret.
    ///
    /// # Errors
    ///
    /// When the value is not one `account=secret` pair with valid names, or names another
    /// account than [`TELEMETRY_QUERY_ACCOUNT`] (the migration file defines that one only).
    pub fn clickhouse_telemetry_query(&self) -> Result<Option<Binding>, Invalid> {
        let key = "passwords.clickhouse_telemetry_query";
        let mut found = bindings(key, self.clickhouse_telemetry_query.as_deref())?;
        if found.len() > 1 {
            return Err(Invalid::new(key, "must be one account=secret pair"));
        }
        let binding = found.pop();
        if binding
            .as_ref()
            .is_some_and(|binding| binding.owner != TELEMETRY_QUERY_ACCOUNT)
        {
            return Err(Invalid::new(
                key,
                format!(
                    "the account must be {TELEMETRY_QUERY_ACCOUNT}, the one 0008_telemetry_query.sql defines"
                ),
            ));
        }
        Ok(binding)
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
        check_migration_role(&self.postgres)?;
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
            // The collector's account must be none telemetry-query manages: not its own account,
            // and no name telemetry-query may create or change (it could set its password).
            if let Some(collector) = &collector
                && (collector.owner == TELEMETRY_QUERY_ACCOUNT
                    || collector.owner.starts_with(TENANT_ACCOUNT_PREFIX))
            {
                return Err(Invalid::new(
                    "passwords.clickhouse_collector",
                    format!(
                        "must not be {TELEMETRY_QUERY_ACCOUNT} or start with \
                         {TENANT_ACCOUNT_PREFIX}: those accounts belong to telemetry-query"
                    ),
                ));
            }
            // Its account name is fixed and the collector's cannot be it (above).
            if let Some(telemetry_query) = passwords.clickhouse_telemetry_query()? {
                if self.clickhouse.is_none() {
                    return Err(Invalid::new(
                        "passwords.clickhouse_telemetry_query",
                        "needs clickhouse.*, the account that sets the telemetry-query account's \
                         password",
                    ));
                }
                if postgres.contains(&telemetry_query) {
                    return Err(Invalid::new(
                        "passwords.clickhouse_telemetry_query",
                        "uses the same key of the same Secret as a Postgres role",
                    ));
                }
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
            // Checked at start, before the run touches either database.
            clickhouse.raw_retention()?;
            match (&clickhouse.password, &clickhouse.password_secret) {
                (Some(_), None) => {}
                (None, Some(secret)) => {
                    if !is_object_name(secret) {
                        return Err(Invalid::new(
                            "clickhouse.password_secret",
                            "must be a Kubernetes object name (lower-case letters, digits, - and .)",
                        ));
                    }
                    if self.passwords.is_none() {
                        return Err(Invalid::new(
                            "clickhouse.password_secret",
                            "needs passwords.*, the Kubernetes API the Secret is read from",
                        ));
                    }
                }
                _ => {
                    return Err(Invalid::new(
                        "clickhouse.password",
                        "set exactly one of clickhouse.password and clickhouse.password_secret",
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
        migration_role(&self.platform, &self.postgres)
    }
}

fn migration_role(platform: &Platform, postgres: &PostgresSettings) -> PostgresConfig {
    PostgresConfig {
        host: platform.postgres_host.clone(),
        port: platform.postgres_port,
        database: platform.postgres_database.clone(),
        ssl_mode: platform.postgres_sslmode,
        ca_file: platform.postgres_ca_file.clone(),
        username: postgres.username.clone(),
        password: postgres.password.clone(),
    }
}

fn check_migration_role(postgres: &PostgresSettings) -> Result<(), Invalid> {
    if is_plain_identifier(&postgres.username) {
        Ok(())
    } else {
        Err(Invalid::new(
            "postgres.username",
            "must be a lower-case role name (letters, digits, _)",
        ))
    }
}

/// Settings of `migrate bootstrap-platform-admin` (Laysi/trustiot-v2#582): the migration role,
/// as for a run, and nothing else. The address comes from stdin, never from a setting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub platform: Platform,
    pub postgres: PostgresSettings,
    #[serde(default)]
    pub wait: BootstrapWait,
}

/// `wait.*` of `migrate bootstrap-platform-admin`.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BootstrapWait {
    /// `TRUSTIOT__WAIT__DATABASES`: for Postgres to accept the migration role. Default 10
    /// minutes.
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub databases: Duration,
}

impl Default for BootstrapWait {
    fn default() -> Self {
        Self {
            databases: DEFAULT_WAIT,
        }
    }
}

impl Validate for BootstrapConfig {
    fn validate(&self) -> Result<(), Invalid> {
        check_migration_role(&self.postgres)?;
        check_wait("wait.databases", self.wait.databases)
    }
}

impl BootstrapConfig {
    /// How the command connects to Postgres: as the migration role.
    #[must_use]
    pub fn postgres(&self) -> PostgresConfig {
        migration_role(&self.platform, &self.postgres)
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

/// Settings of `migrate clickhouse-admin-password`, the init container of the bundled ClickHouse
/// that makes sure its bootstrap admin password exists before the server starts
/// (src/clickhouse_admin.rs).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouseAdminConfig {
    pub admin: AdminSettings,
}

/// `admin.*`: the Secret that holds the bootstrap admin password, how to reach it, and where the
/// server reads the password.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminSettings {
    /// `TRUSTIOT__ADMIN__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__ADMIN__NAMESPACE`: the release namespace, where the Secret is.
    pub namespace: String,
    /// `TRUSTIOT__ADMIN__TOKEN_FILE`: the projected ServiceAccount token of the ClickHouse pod,
    /// which only the init container mounts; read on every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__ADMIN__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__ADMIN__SECRET`: the Secret the chart declares empty for it.
    pub secret: String,
    /// `TRUSTIOT__ADMIN__ACCOUNT`: the bootstrap admin account, also the key in the Secret.
    pub account: String,
    /// `TRUSTIOT__ADMIN__GENERATION`: the chart's `clickhouse.bundled.adminPasswordGeneration`;
    /// a higher one than the Secret records generates a new password (a rotation).
    pub generation: String,
    /// `TRUSTIOT__ADMIN__PASSWORD_FILE`: where the server's entrypoint reads the password
    /// (`CLICKHOUSE_PASSWORD_FILE`), on a volume in memory that only this pod mounts.
    pub password_file: PathBuf,
    /// `TRUSTIOT__ADMIN__WAIT`: how long it retries an API server that does not answer. Default
    /// 10 minutes.
    #[serde(
        default = "default_wait",
        deserialize_with = "service_kit::config::duration"
    )]
    pub wait: Duration,
}

fn default_wait() -> Duration {
    DEFAULT_WAIT
}

impl Validate for ClickHouseAdminConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let admin = &self.admin;
        check_api_url("admin.api_url", &admin.api_url)?;
        if !is_dns_label(&admin.namespace) {
            return Err(Invalid::new(
                "admin.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        if !is_object_name(&admin.secret) {
            return Err(Invalid::new(
                "admin.secret",
                "must be a Kubernetes object name (lower-case letters, digits, - and .)",
            ));
        }
        if !is_plain_identifier(&admin.account) {
            return Err(Invalid::new(
                "admin.account",
                "must be a lower-case account name (letters, digits, _)",
            ));
        }
        if !is_generation(&admin.generation) {
            return Err(Invalid::new(
                "admin.generation",
                "must be 1 to 9 decimal digits (generations are compared as numbers and only go up)",
            ));
        }
        if !admin.password_file.is_absolute() || admin.password_file.file_name().is_none() {
            return Err(Invalid::new(
                "admin.password_file",
                "must be an absolute path to a file",
            ));
        }
        check_wait("admin.wait", admin.wait)
    }
}

/// Settings of `migrate identity-signing-keys`, the identity service's init container that keeps
/// the internal identity signing keys and their public keys (src/signing_keys.rs).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningKeysConfig {
    pub keys: KeySettings,
}

/// Shortest `keys.propagation_seconds`: the chart's schema minimum for
/// `internalIdentity.keyPropagationSeconds`.
pub const MIN_PROPAGATION_SECONDS: u64 = 120;

/// Longest `keys.propagation_seconds`: a day.
pub const MAX_PROPAGATION_SECONDS: u64 = 86_400;

/// `keys.*`: the Secret and the ConfigMap, how to reach them, and what the chart asks for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySettings {
    /// `TRUSTIOT__KEYS__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__KEYS__NAMESPACE`: the release namespace.
    pub namespace: String,
    /// `TRUSTIOT__KEYS__TOKEN_FILE`: the projected token of the identity service's
    /// ServiceAccount, which only the init container mounts; read on every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__KEYS__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__KEYS__SECRET`: the Secret the chart declares empty for the key set.
    pub secret: String,
    /// `TRUSTIOT__KEYS__CONFIG_MAP`: the ConfigMap the chart declares empty for the public keys.
    pub config_map: String,
    /// `TRUSTIOT__KEYS__GENERATION`: the chart's `internalIdentity.keyGeneration`; a higher one
    /// than every key in the Secret adds a key (a rotation).
    pub generation: String,
    /// `TRUSTIOT__KEYS__REVOKE_BEFORE_GENERATION`: the chart's
    /// `internalIdentity.revokeBeforeGeneration`; keys of a lower generation are removed at once.
    #[serde(default = "no_revocation")]
    pub revoke_before_generation: String,
    /// `TRUSTIOT__KEYS__PROPAGATION_SECONDS`: the chart's
    /// `internalIdentity.keyPropagationSeconds`, how long a new key waits after its public key is
    /// published before it signs.
    pub propagation_seconds: u64,
    /// `TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS`: the chart's `internalIdentity.maxKeyAgeSeconds`,
    /// set only by the rotation CronJob (Laysi/trustiot-v2#429): once the newest signing key was
    /// published this long ago, the run adds a key of the same generation and rotates it out on
    /// the schedule of a manual rotation. 0 (the default, and the init container's) rotates
    /// nothing by age; otherwise from `identity_keys::min_max_key_age(propagation_seconds)` to
    /// `identity_keys::MAX_KEY_AGE_SECS`.
    #[serde(default)]
    pub max_key_age_seconds: u64,
    /// `TRUSTIOT__KEYS__LEGACY_KEY_FILE`: while the chart still names a signing key a person made
    /// before Laysi/trustiot-v2#404 (`internalIdentity.signingKeySecret`), that key's PKCS#8 PEM
    /// file, mounted into the init container only.
    #[serde(default)]
    pub legacy_key_file: Option<PathBuf>,
    /// `TRUSTIOT__KEYS__LEGACY_PUBLIC_KEYS`: with the file, the chart's
    /// `internalIdentity.publicKeys` as a JWK Set, where the key's key id is found.
    #[serde(default)]
    pub legacy_public_keys: Option<String>,
    /// `TRUSTIOT__KEYS__WAIT`: how long it retries an API server that does not answer. Default
    /// 10 minutes.
    #[serde(
        default = "default_wait",
        deserialize_with = "service_kit::config::duration"
    )]
    pub wait: Duration,
}

fn no_revocation() -> String {
    "0".to_owned()
}

impl Validate for SigningKeysConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let keys = &self.keys;
        check_api_url("keys.api_url", &keys.api_url)?;
        if !is_dns_label(&keys.namespace) {
            return Err(Invalid::new(
                "keys.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        for (key, name) in [
            ("keys.secret", &keys.secret),
            ("keys.config_map", &keys.config_map),
        ] {
            if !is_object_name(name) {
                return Err(Invalid::new(
                    key,
                    "must be a Kubernetes object name (lower-case letters, digits, - and .)",
                ));
            }
        }
        let generation = crate::passwords::parse_generation(&keys.generation)
            .filter(|generation| *generation >= 1)
            .ok_or_else(|| {
                Invalid::new(
                    "keys.generation",
                    "must be 1 to 9 decimal digits, at least 1 (generations are compared as \
                     numbers and only go up)",
                )
            })?;
        let revoke_before = crate::passwords::parse_generation(&keys.revoke_before_generation)
            .ok_or_else(|| {
                Invalid::new(
                    "keys.revoke_before_generation",
                    "must be 1 to 9 decimal digits",
                )
            })?;
        if generation < revoke_before {
            return Err(Invalid::new(
                "keys.generation",
                "must not be below keys.revoke_before_generation, or no key could sign",
            ));
        }
        if !(MIN_PROPAGATION_SECONDS..=MAX_PROPAGATION_SECONDS).contains(&keys.propagation_seconds)
        {
            return Err(Invalid::new(
                "keys.propagation_seconds",
                format!(
                    "must be between {MIN_PROPAGATION_SECONDS} and {MAX_PROPAGATION_SECONDS} \
                     seconds"
                ),
            ));
        }
        if keys.max_key_age_seconds != 0 {
            // Both fit in i64: the propagation time was checked above.
            let shortest = identity_keys::min_max_key_age(
                i64::try_from(keys.propagation_seconds).unwrap_or(i64::MAX),
            );
            let longest = identity_keys::MAX_KEY_AGE_SECS;
            if !i64::try_from(keys.max_key_age_seconds)
                .is_ok_and(|age| (shortest..=longest).contains(&age))
            {
                return Err(Invalid::new(
                    "keys.max_key_age_seconds",
                    format!(
                        "must be 0 (no rotation by age) or between {shortest} (twice \
                         keys.propagation_seconds plus {} s, so the key before is always \
                         retired by the next rotation) and {longest} seconds",
                        identity_keys::TOKEN_EXPIRY_SECS
                    ),
                ));
            }
            if keys.legacy_key_file.is_some() {
                return Err(Invalid::new(
                    "keys.max_key_age_seconds",
                    "must be 0 while a person's key is moved over (keys.legacy_key_file): \
                     nothing rotates until the move's second step",
                ));
            }
        }
        match (&keys.legacy_key_file, &keys.legacy_public_keys) {
            (None, None) => {}
            (Some(file), Some(_)) if file.is_absolute() && file.file_name().is_some() => {}
            (Some(_), Some(_)) => {
                return Err(Invalid::new(
                    "keys.legacy_key_file",
                    "must be an absolute path to a file",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(Invalid::new(
                    "keys.legacy_public_keys",
                    "keys.legacy_key_file and keys.legacy_public_keys go together",
                ));
            }
        }
        check_wait("keys.wait", keys.wait)
    }
}

/// Settings of `migrate certs-ca-keys`, the certs service's init container that keeps the
/// issuing CA keys and their certificate signing requests (src/ca_keys.rs;
/// Laysi/trustiot-v2#78).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaKeysConfig {
    pub keys: CaKeySettings,
}

/// `keys.*` of `migrate certs-ca-keys`: the Secret and the ConfigMap, how to reach them, and
/// what the chart asks for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaKeySettings {
    /// `TRUSTIOT__KEYS__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__KEYS__NAMESPACE`: the release namespace.
    pub namespace: String,
    /// `TRUSTIOT__KEYS__TOKEN_FILE`: the projected token of the certs service's ServiceAccount,
    /// which only the init container mounts; read on every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__KEYS__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__KEYS__SECRET`: the Secret the chart declares empty for the issuing CA keys.
    pub secret: String,
    /// `TRUSTIOT__KEYS__CONFIG_MAP`: the ConfigMap the chart declares empty for their
    /// certificate signing requests.
    pub config_map: String,
    /// `TRUSTIOT__KEYS__GENERATION`: the chart's `certificateAuthority.keyGeneration`; a higher
    /// one than every key in the Secret adds a key (a rotation).
    pub generation: String,
    /// `TRUSTIOT__KEYS__REMOVE_BEFORE_GENERATION`: the chart's
    /// `certificateAuthority.removeKeysBeforeGeneration`; keys of a lower generation are removed.
    #[serde(default = "no_revocation")]
    pub remove_before_generation: String,
    /// `TRUSTIOT__KEYS__WAIT`: how long it retries an API server that does not answer. Default
    /// 10 minutes.
    #[serde(
        default = "default_wait",
        deserialize_with = "service_kit::config::duration"
    )]
    pub wait: Duration,
}

impl Validate for CaKeysConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let keys = &self.keys;
        check_api_url("keys.api_url", &keys.api_url)?;
        if !is_dns_label(&keys.namespace) {
            return Err(Invalid::new(
                "keys.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        for (key, name) in [
            ("keys.secret", &keys.secret),
            ("keys.config_map", &keys.config_map),
        ] {
            if !is_object_name(name) {
                return Err(Invalid::new(
                    key,
                    "must be a Kubernetes object name (lower-case letters, digits, - and .)",
                ));
            }
        }
        let generation = crate::passwords::parse_generation(&keys.generation)
            .filter(|generation| *generation >= 1)
            .ok_or_else(|| {
                Invalid::new(
                    "keys.generation",
                    "must be 1 to 9 decimal digits, at least 1 (generations are compared as \
                     numbers and only go up)",
                )
            })?;
        let remove_before = crate::passwords::parse_generation(&keys.remove_before_generation)
            .ok_or_else(|| {
                Invalid::new(
                    "keys.remove_before_generation",
                    "must be 1 to 9 decimal digits",
                )
            })?;
        if generation < remove_before {
            return Err(Invalid::new(
                "keys.generation",
                "must not be below keys.remove_before_generation, or the requested key would be \
                 removed",
            ));
        }
        check_wait("keys.wait", keys.wait)
    }
}

/// Settings of `migrate mqtt-broker-accounts`, the MQTT broker's init container that fills the
/// services' broker passwords and writes the dynamic-security file (src/mqtt_accounts.rs;
/// Laysi/trustiot-v2#84).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttAccountsConfig {
    pub accounts: MqttAccountSettings,
}

/// `accounts.*` of `migrate mqtt-broker-accounts`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttAccountSettings {
    /// `TRUSTIOT__ACCOUNTS__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__ACCOUNTS__NAMESPACE`: the release namespace, where the Secret is.
    pub namespace: String,
    /// `TRUSTIOT__ACCOUNTS__TOKEN_FILE`: the projected token of the broker's ServiceAccount,
    /// which only this init container mounts; read on every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__ACCOUNTS__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__ACCOUNTS__SECRET`: the Secret the chart declares empty for the passwords, one
    /// key per service.
    pub secret: String,
    /// `TRUSTIOT__ACCOUNTS__SERVICES`: the services that get an account, comma-separated (the
    /// enabled services whose `mesh.calls` list the broker); may be empty.
    #[serde(default)]
    pub services: String,
    /// `TRUSTIOT__ACCOUNTS__GENERATION`: the chart's `mqttBroker.passwordGeneration`; a higher
    /// one than the Secret records generates new passwords (a rotation).
    pub generation: String,
    /// `TRUSTIOT__ACCOUNTS__DYNSEC_FILE`: the dynamic-security file on the broker's volume.
    pub dynsec_file: PathBuf,
    /// `TRUSTIOT__ACCOUNTS__WAIT`: how long it retries an API server that does not answer.
    /// Default 10 minutes.
    #[serde(
        default = "default_wait",
        deserialize_with = "service_kit::config::duration"
    )]
    pub wait: Duration,
}

impl Validate for MqttAccountsConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let accounts = &self.accounts;
        check_api_url("accounts.api_url", &accounts.api_url)?;
        if !is_dns_label(&accounts.namespace) {
            return Err(Invalid::new(
                "accounts.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        if !is_object_name(&accounts.secret) {
            return Err(Invalid::new(
                "accounts.secret",
                "must be a Kubernetes object name (lower-case letters, digits, - and .)",
            ));
        }
        if let Err(problem) = crate::mqtt_accounts::parse_services(&accounts.services) {
            return Err(Invalid::new("accounts.services", problem));
        }
        if !is_generation(&accounts.generation) {
            return Err(Invalid::new(
                "accounts.generation",
                "must be 1 to 9 decimal digits (generations are compared as numbers and only go up)",
            ));
        }
        if !accounts.dynsec_file.is_absolute() || accounts.dynsec_file.file_name().is_none() {
            return Err(Invalid::new(
                "accounts.dynsec_file",
                "must be an absolute path to a file",
            ));
        }
        check_wait("accounts.wait", accounts.wait)
    }
}

/// The trust sidecar's default refresh interval.
pub const DEFAULT_TRUST_INTERVAL: Duration = Duration::from_secs(30);

/// Settings of `migrate mqtt-broker-trust`, the MQTT broker's init container and sidecar that
/// keep its trust files and reload it (src/mqtt_trust.rs; Laysi/trustiot-v2#84).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttTrustConfig {
    pub trust: MqttTrustSettings,
}

/// `trust.*` of `migrate mqtt-broker-trust`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttTrustSettings {
    /// `TRUSTIOT__TRUST__CERTS_URL`: the certs service, `http://<host>:<port>` inside the mesh
    /// (ztunnel encrypts it); empty when the release has no certs service, and the broker then
    /// trusts no gateway.
    #[serde(default)]
    pub certs_url: String,
    /// `TRUSTIOT__TRUST__DIR`: where the trust files go, on the broker's volume; the broker reads
    /// `<dir>/current/ca.pem` and `<dir>/current/crl.pem`.
    pub dir: PathBuf,
    /// `TRUSTIOT__TRUST__TLS_DIR`: the mount of the broker's server certificate Secret
    /// (`tls.crt`, `tls.key`).
    pub tls_dir: PathBuf,
    /// `TRUSTIOT__TRUST__LISTENERS_DIR`: the directory the broker's configuration includes
    /// (`include_dir`), empty when the pod starts. The gateway listener's file goes there once
    /// the Secret holds a usable server certificate (Laysi/trustiot-v2#535).
    pub listeners_dir: PathBuf,
    /// `TRUSTIOT__TRUST__GATEWAY_LISTENER`: the gateway listener's configuration (the chart's
    /// ConfigMap), copied into `listeners_dir`. Its `listener <port>` line names the port the
    /// sidecar checks the broker listens on.
    pub gateway_listener: PathBuf,
    /// `TRUSTIOT__TRUST__INTERVAL`: how often the sidecar fetches the files and checks the
    /// Secret. Default 30 seconds.
    #[serde(
        default = "default_trust_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub interval: Duration,
}

fn default_trust_interval() -> Duration {
    DEFAULT_TRUST_INTERVAL
}

/// Whether `url` is an `http://` URL without credentials, query or fragment: the certs
/// service's addresses inside the mesh, which ztunnel encrypts.
fn is_mesh_http_url(url: &str) -> bool {
    url.starts_with("http://")
        && !url.contains('@')
        && !url.contains('?')
        && !url.contains('#')
        && url.len() > "http://".len()
}

impl Validate for MqttTrustConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let trust = &self.trust;
        let url = &trust.certs_url;
        if !url.is_empty() && !is_mesh_http_url(url) {
            return Err(Invalid::new(
                "trust.certs_url",
                "must be empty or an http:// URL of the certs service without credentials, query \
                 or fragment (the mesh encrypts it)",
            ));
        }
        for (key, path) in [
            ("trust.dir", &trust.dir),
            ("trust.tls_dir", &trust.tls_dir),
            ("trust.listeners_dir", &trust.listeners_dir),
        ] {
            if !path.is_absolute() {
                return Err(Invalid::new(key, "must be an absolute path"));
            }
        }
        if !trust.gateway_listener.is_absolute() || trust.gateway_listener.file_name().is_none() {
            return Err(Invalid::new(
                "trust.gateway_listener",
                "must be an absolute path to a file",
            ));
        }
        if trust.interval < Duration::from_secs(1) || trust.interval > MAX_WAIT {
            return Err(Invalid::new(
                "trust.interval",
                "must be at least 1 second and at most 1 hour",
            ));
        }
        Ok(())
    }
}

/// How long `migrate mqtt-broker-certificate` retries an API server or a certs service that
/// does not answer, or answers that it cannot sign yet, by default. Short: the CronJob runs again
/// on its schedule.
pub const DEFAULT_CERTIFICATE_WAIT: Duration = Duration::from_secs(120);

/// Settings of `migrate mqtt-broker-certificate`, the CronJob that keeps the MQTT broker's server
/// certificate and key in the Secret the chart declares (src/mqtt_certificate.rs;
/// Laysi/trustiot-v2#535).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttCertificateConfig {
    pub certificate: MqttCertificateSettings,
}

/// `certificate.*` of `migrate mqtt-broker-certificate`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttCertificateSettings {
    /// `TRUSTIOT__CERTIFICATE__API_URL`: the API server, `https://` (the chart sets
    /// `https://kubernetes.default.svc`). `http://` is accepted for 127.0.0.1 only, for tests.
    pub api_url: String,
    /// `TRUSTIOT__CERTIFICATE__NAMESPACE`: the release namespace, where the Secret is.
    pub namespace: String,
    /// `TRUSTIOT__CERTIFICATE__TOKEN_FILE`: the projected token of the issuer's own
    /// ServiceAccount, which only its pod mounts; read on every request.
    pub token_file: PathBuf,
    /// `TRUSTIOT__CERTIFICATE__CA_FILE`: the cluster CA (`kube-root-ca.crt`), the only trust
    /// anchor.
    pub ca_file: PathBuf,
    /// `TRUSTIOT__CERTIFICATE__SECRET`: the Secret the chart declares empty for the server
    /// certificate and key (`tls.crt`, `tls.key`).
    pub secret: String,
    /// `TRUSTIOT__CERTIFICATE__CERTS_URL`: the certs service's broker certificate port,
    /// `http://<release>-certs-broker.<namespace>.svc:8082` inside the mesh, which admits this
    /// workload alone.
    pub certs_url: String,
    /// `TRUSTIOT__CERTIFICATE__SERVICE_NAME`: the broker Service's name, the second DNS name the
    /// certs service puts in the certificate (its `broker.service_name`); empty when it puts
    /// none. A certificate with other names is refused.
    #[serde(default)]
    pub service_name: String,
    /// `TRUSTIOT__CERTIFICATE__GENERATION`: the chart's `mqttBroker.certificateGeneration`; a
    /// higher one than the Secret records renews the certificate at once (with a new key).
    pub generation: String,
    /// `TRUSTIOT__CERTIFICATE__WAIT`: how long it retries the API server and the certs service.
    /// Default 2 minutes.
    #[serde(
        default = "default_certificate_wait",
        deserialize_with = "service_kit::config::duration"
    )]
    pub wait: Duration,
}

fn default_certificate_wait() -> Duration {
    DEFAULT_CERTIFICATE_WAIT
}

impl Validate for MqttCertificateConfig {
    fn validate(&self) -> Result<(), Invalid> {
        let certificate = &self.certificate;
        check_api_url("certificate.api_url", &certificate.api_url)?;
        if !is_dns_label(&certificate.namespace) {
            return Err(Invalid::new(
                "certificate.namespace",
                "must be a Kubernetes namespace name (DNS label)",
            ));
        }
        if !is_object_name(&certificate.secret) {
            return Err(Invalid::new(
                "certificate.secret",
                "must be a Kubernetes object name (lower-case letters, digits, - and .)",
            ));
        }
        if !is_mesh_http_url(&certificate.certs_url) {
            return Err(Invalid::new(
                "certificate.certs_url",
                "must be an http:// URL of the certs service's broker certificate port without \
                 credentials, query or fragment (the mesh encrypts it)",
            ));
        }
        if !certificate.service_name.is_empty() && !is_object_name(&certificate.service_name) {
            return Err(Invalid::new(
                "certificate.service_name",
                "must be empty or a DNS name (lower-case letters, digits, - and .)",
            ));
        }
        crate::passwords::parse_generation(&certificate.generation)
            .filter(|generation| *generation >= 1)
            .ok_or_else(|| {
                Invalid::new(
                    "certificate.generation",
                    "must be 1 to 9 decimal digits, at least 1 (generations are compared as \
                     numbers and only go up)",
                )
            })?;
        check_wait("certificate.wait", certificate.wait)
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
            clickhouse_telemetry_query: None,
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

    fn run_config(
        password: Option<&str>,
        password_secret: Option<&str>,
        passwords: bool,
    ) -> RunConfig {
        RunConfig {
            platform: Platform {
                postgres_host: "postgres".to_owned(),
                postgres_port: 5432,
                postgres_database: "trustiot".to_owned(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: PathBuf::from("/ca.crt"),
                clickhouse_url: Some("http://clickhouse:8123".to_owned()),
                clickhouse_ca_file: None,
            },
            postgres: PostgresSettings {
                username: "trustiot_owner".to_owned(),
                password: "unused".to_owned(),
            },
            clickhouse: Some(ClickHouseSettings {
                username: "trustiot_admin".to_owned(),
                password: password.map(str::to_owned),
                password_secret: password_secret.map(str::to_owned),
                raw_retention_days: None,
            }),
            passwords: passwords.then(|| settings("", "")),
            wait: WaitSettings::default(),
        }
    }

    #[test]
    fn the_clickhouse_password_comes_from_exactly_one_place() {
        let key = |config: RunConfig| config.validate().err().map(|invalid| invalid.key);
        assert_eq!(key(run_config(Some("a-password"), None, false)), None);
        assert_eq!(
            key(run_config(
                None,
                Some("trustiot-clickhouse-admin-password"),
                true
            )),
            None
        );
        // The Secret is read through the API of passwords.*.
        assert_eq!(
            key(run_config(
                None,
                Some("trustiot-clickhouse-admin-password"),
                false
            ))
            .as_deref(),
            Some("clickhouse.password_secret")
        );
        assert_eq!(
            key(run_config(None, Some("Not/A-Name"), true)).as_deref(),
            Some("clickhouse.password_secret")
        );
        for (password, secret) in [(None, None), (Some("a-password"), Some("s"))] {
            assert_eq!(
                key(run_config(password, secret, true)).as_deref(),
                Some("clickhouse.password")
            );
        }
        // Debug never shows the password.
        let config = run_config(Some("a-secret-value"), None, false);
        assert!(!format!("{config:?}").contains("a-secret-value"));
    }

    #[test]
    fn telemetry_query_and_collector_accounts_stay_apart() {
        let with = |collector: &str, telemetry_query: Option<&str>| {
            let mut config = run_config(Some("a-password"), None, true);
            let passwords = config.passwords.as_mut().expect("password settings");
            passwords.clickhouse_collector = Some(collector.to_owned());
            passwords.clickhouse_telemetry_query = telemetry_query.map(str::to_owned);
            config
        };
        let key = |config: RunConfig| config.validate().err().map(|invalid| invalid.key);
        let bound = Some("trustiot_telemetry_query=trustiot-telemetry-query-password");
        assert_eq!(key(with("trustiot_collector=c", bound)), None);
        assert_eq!(
            with("trustiot_collector=c", bound)
                .passwords
                .expect("password settings")
                .clickhouse_telemetry_query()
                .expect("valid"),
            Some(Binding {
                owner: TELEMETRY_QUERY_ACCOUNT.to_owned(),
                secret: "trustiot-telemetry-query-password".to_owned()
            })
        );
        for bad in [
            "trustiot_other=trustiot-telemetry-query-password",
            "trustiot_telemetry_query=a,trustiot_telemetry_query=b",
            "trustiot_telemetry_query=Not/A-Secret",
        ] {
            assert_eq!(
                key(with("trustiot_collector=c", Some(bad))).as_deref(),
                Some("passwords.clickhouse_telemetry_query"),
                "{bad}"
            );
        }
        for collector in ["trustiot_telemetry_query=c", "trustiot_tenant_account_x=c"] {
            assert_eq!(
                key(with(collector, None)).as_deref(),
                Some("passwords.clickhouse_collector"),
                "{collector}"
            );
        }
        let mut without_clickhouse = with("trustiot_collector=c", bound);
        without_clickhouse.clickhouse = None;
        let passwords = without_clickhouse.passwords.as_mut().expect("settings");
        passwords.clickhouse_collector = None;
        assert_eq!(
            key(without_clickhouse).as_deref(),
            Some("passwords.clickhouse_telemetry_query")
        );
        let mut shared = with("trustiot_collector=c", bound);
        shared.passwords.as_mut().expect("settings").postgres =
            Some("trustiot_telemetry_query=trustiot-telemetry-query-password".to_owned());
        assert_eq!(
            key(shared).as_deref(),
            Some("passwords.clickhouse_telemetry_query")
        );
    }

    /// The raw data retention (Laysi/trustiot-v2#92): unset is the platform value; a setting that
    /// is not longer than the gateway buffer's maximum age, or implausibly long, is refused at
    /// start, as the environment variable the chart would set.
    #[test]
    fn the_raw_retention_must_outlast_the_gateway_buffer() {
        use protocol::telemetry::BUFFER_MAX_AGE_DAYS;

        use crate::retention::{MAX_RAW_RETENTION_DAYS, RAW_RETENTION_DAYS};

        let with = |days: Option<u32>| {
            let mut config = run_config(Some("a-password"), None, false);
            if let Some(clickhouse) = config.clickhouse.as_mut() {
                clickhouse.raw_retention_days = days;
            }
            config
        };
        let retention = |config: &RunConfig| {
            config
                .clickhouse
                .as_ref()
                .map(|clickhouse| clickhouse.raw_retention().map(RawRetention::days))
        };
        assert_eq!(retention(&with(None)), Some(Ok(RAW_RETENTION_DAYS)));
        assert_eq!(RAW_RETENTION_DAYS, 180);
        for days in [BUFFER_MAX_AGE_DAYS + 1, 365, MAX_RAW_RETENTION_DAYS] {
            let config = with(Some(days));
            assert!(config.validate().is_ok(), "{days} days");
            assert_eq!(retention(&config), Some(Ok(days)));
        }
        for days in [0, 1, BUFFER_MAX_AGE_DAYS, MAX_RAW_RETENTION_DAYS + 1] {
            let invalid = with(Some(days)).validate().expect_err("refused");
            assert_eq!(invalid.key, "clickhouse.raw_retention_days", "{days} days");
        }

        // As the environment variable, loaded the way main() loads it: a configuration error
        // (exit code 2) before anything runs, naming the key and the buffer's maximum age.
        let vars = |days: &str| {
            [
                ("TRUSTIOT_POSTGRES_HOST", "postgres"),
                ("TRUSTIOT_POSTGRES_PORT", "5432"),
                ("TRUSTIOT_POSTGRES_DATABASE", "trustiot"),
                ("TRUSTIOT_POSTGRES_SSLMODE", "verify-full"),
                ("TRUSTIOT_POSTGRES_CA_FILE", "/ca.crt"),
                ("TRUSTIOT_CLICKHOUSE_URL", "http://clickhouse:8123"),
                ("TRUSTIOT__POSTGRES__USERNAME", "trustiot_owner"),
                ("TRUSTIOT__POSTGRES__PASSWORD", "unused"),
                ("TRUSTIOT__CLICKHOUSE__USERNAME", "trustiot_admin"),
                ("TRUSTIOT__CLICKHOUSE__PASSWORD", "unused"),
                ("TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS", days),
            ]
            .map(|(name, value)| (name.into(), value.into()))
        };
        let loaded = service_kit::config::load_from::<RunConfig, _>("TRUSTIOT", vars("181"))
            .expect("181 days is accepted");
        assert_eq!(
            loaded
                .app
                .clickhouse
                .as_ref()
                .map(|clickhouse| clickhouse.raw_retention().map(RawRetention::days)),
            Some(Ok(181))
        );
        for days in ["30", "7", "0"] {
            let error = service_kit::config::load_from::<RunConfig, _>("TRUSTIOT", vars(days))
                .expect_err("not longer than the gateway buffer");
            let text = error.to_string();
            assert!(
                text.contains("clickhouse.raw_retention_days")
                    && text.contains(&format!(
                        "gateway buffer's maximum age of {BUFFER_MAX_AGE_DAYS} days"
                    )),
                "{days}: {text}"
            );
        }
        assert!(
            service_kit::config::load_from::<RunConfig, _>("TRUSTIOT", vars("-1")).is_err(),
            "a negative retention is refused"
        );
    }

    fn admin_config() -> ClickHouseAdminConfig {
        ClickHouseAdminConfig {
            admin: AdminSettings {
                api_url: "https://kubernetes.default.svc".to_owned(),
                namespace: "trustiot".to_owned(),
                token_file: PathBuf::from("/token"),
                ca_file: PathBuf::from("/ca.crt"),
                secret: "trustiot-clickhouse-admin-password".to_owned(),
                account: "trustiot_admin".to_owned(),
                generation: "1".to_owned(),
                password_file: PathBuf::from("/etc/trustiot/clickhouse-admin/password"),
                wait: DEFAULT_WAIT,
            },
        }
    }

    /// A key of the settings and a change that makes it invalid.
    type Breakage = (&'static str, fn(&mut AdminSettings));

    #[test]
    fn clickhouse_admin_settings() {
        assert!(admin_config().validate().is_ok());
        let broken: [Breakage; 7] = [
            ("admin.api_url", |a| {
                a.api_url = "http://kubernetes.default.svc".to_owned();
            }),
            ("admin.namespace", |a| a.namespace = "Trust_IoT".to_owned()),
            ("admin.secret", |a| a.secret = "a/b".to_owned()),
            ("admin.account", |a| a.account = "Admin".to_owned()),
            ("admin.generation", |a| a.generation = "v2".to_owned()),
            ("admin.password_file", |a| {
                a.password_file = PathBuf::from("relative/password");
            }),
            ("admin.wait", |a| a.wait = Duration::ZERO),
        ];
        for (key, change) in broken {
            let mut config = admin_config();
            change(&mut config.admin);
            assert_eq!(
                config
                    .validate()
                    .err()
                    .map(|invalid| invalid.key)
                    .as_deref(),
                Some(key)
            );
        }
    }

    fn keys_config() -> SigningKeysConfig {
        SigningKeysConfig {
            keys: KeySettings {
                api_url: "https://kubernetes.default.svc".to_owned(),
                namespace: "trustiot".to_owned(),
                token_file: PathBuf::from("/token"),
                ca_file: PathBuf::from("/ca.crt"),
                secret: "trustiot-identity-signing-keys".to_owned(),
                config_map: "trustiot-internal-identity-keys".to_owned(),
                generation: "1".to_owned(),
                revoke_before_generation: "0".to_owned(),
                propagation_seconds: 600,
                max_key_age_seconds: 0,
                legacy_key_file: None,
                legacy_public_keys: None,
                wait: DEFAULT_WAIT,
            },
        }
    }

    type KeysBreakage = (&'static str, fn(&mut KeySettings));

    #[test]
    fn signing_keys_settings() {
        assert!(keys_config().validate().is_ok());
        let mut legacy = keys_config();
        legacy.keys.legacy_key_file = Some(PathBuf::from("/etc/trustiot/legacy-key/key.pem"));
        legacy.keys.legacy_public_keys = Some(r#"{"keys":[]}"#.to_owned());
        assert!(legacy.validate().is_ok());
        let mut revoking = keys_config();
        revoking.keys.generation = "3".to_owned();
        revoking.keys.revoke_before_generation = "3".to_owned();
        assert!(revoking.validate().is_ok());
        // Rotation by age (Laysi/trustiot-v2#429): from twice the propagation time plus the
        // tokens' expiry up to 365 days.
        for age in [1505, 7_776_000, 31_536_000] {
            let mut aging = keys_config();
            aging.keys.max_key_age_seconds = age;
            assert!(aging.validate().is_ok(), "{age}");
        }

        let broken: [KeysBreakage; 16] = [
            ("keys.api_url", |k| {
                k.api_url = "http://kubernetes.default.svc".to_owned();
            }),
            ("keys.namespace", |k| k.namespace = "Trust_IoT".to_owned()),
            ("keys.secret", |k| k.secret = "a/b".to_owned()),
            ("keys.config_map", |k| k.config_map = String::new()),
            ("keys.generation", |k| k.generation = "0".to_owned()),
            ("keys.generation", |k| k.generation = "v2".to_owned()),
            ("keys.generation", |k| {
                k.revoke_before_generation = "2".to_owned();
            }),
            ("keys.revoke_before_generation", |k| {
                k.revoke_before_generation = String::new();
            }),
            ("keys.propagation_seconds", |k| k.propagation_seconds = 119),
            ("keys.propagation_seconds", |k| {
                k.propagation_seconds = 86_401
            }),
            ("keys.max_key_age_seconds", |k| k.max_key_age_seconds = 1504),
            ("keys.max_key_age_seconds", |k| {
                k.max_key_age_seconds = 31_536_001;
            }),
            ("keys.max_key_age_seconds", |k| {
                k.max_key_age_seconds = 7_776_000;
                k.legacy_key_file = Some(PathBuf::from("/key.pem"));
                k.legacy_public_keys = Some(String::new());
            }),
            ("keys.legacy_key_file", |k| {
                k.legacy_key_file = Some(PathBuf::from("relative.pem"));
                k.legacy_public_keys = Some(String::new());
            }),
            ("keys.legacy_public_keys", |k| {
                k.legacy_key_file = Some(PathBuf::from("/key.pem"));
            }),
            ("keys.wait", |k| k.wait = Duration::ZERO),
        ];
        for (key, change) in broken {
            let mut config = keys_config();
            change(&mut config.keys);
            assert_eq!(
                config
                    .validate()
                    .err()
                    .map(|invalid| invalid.key)
                    .as_deref(),
                Some(key)
            );
        }
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

    fn mqtt_accounts_config() -> MqttAccountsConfig {
        MqttAccountsConfig {
            accounts: MqttAccountSettings {
                api_url: "https://kubernetes.default.svc".to_owned(),
                namespace: "trustiot".to_owned(),
                token_file: PathBuf::from("/token"),
                ca_file: PathBuf::from("/ca.crt"),
                secret: "trustiot-mqtt-broker-accounts".to_owned(),
                services: "ingest,gateway-ops,certs".to_owned(),
                generation: "1".to_owned(),
                dynsec_file: PathBuf::from("/mosquitto/data/dynamic-security.json"),
                wait: DEFAULT_WAIT,
            },
        }
    }

    type MqttAccountsBreakage = (&'static str, fn(&mut MqttAccountSettings));

    #[test]
    fn mqtt_accounts_settings() {
        assert!(mqtt_accounts_config().validate().is_ok());
        let mut none = mqtt_accounts_config();
        none.accounts.services = String::new();
        assert!(none.validate().is_ok(), "a release without callers");
        let broken: [MqttAccountsBreakage; 8] = [
            ("accounts.api_url", |a| {
                a.api_url = "http://kubernetes.default.svc".to_owned();
            }),
            ("accounts.namespace", |a| {
                a.namespace = "Trust_IoT".to_owned()
            }),
            ("accounts.secret", |a| a.secret = "a/b".to_owned()),
            ("accounts.services", |a| {
                a.services = "ingest,assistant".to_owned()
            }),
            ("accounts.services", |a| {
                a.services = "certs,certs".to_owned()
            }),
            ("accounts.generation", |a| a.generation = "v2".to_owned()),
            ("accounts.dynsec_file", |a| {
                a.dynsec_file = PathBuf::from("dynamic-security.json");
            }),
            ("accounts.wait", |a| a.wait = Duration::ZERO),
        ];
        for (key, change) in broken {
            let mut config = mqtt_accounts_config();
            change(&mut config.accounts);
            assert_eq!(
                config
                    .validate()
                    .err()
                    .map(|invalid| invalid.key)
                    .as_deref(),
                Some(key)
            );
        }
    }

    fn mqtt_trust_config() -> MqttTrustConfig {
        MqttTrustConfig {
            trust: MqttTrustSettings {
                certs_url: "http://trustiot-certs.trustiot.svc:8080".to_owned(),
                dir: PathBuf::from("/mosquitto/data/trust"),
                tls_dir: PathBuf::from("/etc/trustiot/mqtt-broker-tls"),
                listeners_dir: PathBuf::from("/mosquitto/listeners"),
                gateway_listener: PathBuf::from(
                    "/etc/trustiot/mqtt-broker-config/gateway-listener.conf",
                ),
                interval: DEFAULT_TRUST_INTERVAL,
            },
        }
    }

    type MqttTrustBreakage = (&'static str, fn(&mut MqttTrustSettings));

    #[test]
    fn mqtt_trust_settings() {
        assert!(mqtt_trust_config().validate().is_ok());
        let mut without_certs = mqtt_trust_config();
        without_certs.trust.certs_url = String::new();
        assert!(without_certs.validate().is_ok(), "no certs service");
        let broken: [MqttTrustBreakage; 10] = [
            ("trust.certs_url", |t| {
                t.certs_url = "https://trustiot-certs:8080".to_owned();
            }),
            ("trust.certs_url", |t| {
                t.certs_url = "http://user:pw@trustiot-certs:8080".to_owned();
            }),
            ("trust.certs_url", |t| t.certs_url = "http://".to_owned()),
            ("trust.dir", |t| t.dir = PathBuf::from("trust")),
            ("trust.tls_dir", |t| t.tls_dir = PathBuf::from("tls")),
            ("trust.listeners_dir", |t| {
                t.listeners_dir = PathBuf::from("listeners");
            }),
            ("trust.gateway_listener", |t| {
                t.gateway_listener = PathBuf::from("gateway-listener.conf");
            }),
            ("trust.gateway_listener", |t| {
                t.gateway_listener = PathBuf::from("/");
            }),
            ("trust.interval", |t| t.interval = Duration::ZERO),
            ("trust.interval", |t| t.interval = Duration::from_secs(7200)),
        ];
        for (key, change) in broken {
            let mut config = mqtt_trust_config();
            change(&mut config.trust);
            assert_eq!(
                config
                    .validate()
                    .err()
                    .map(|invalid| invalid.key)
                    .as_deref(),
                Some(key)
            );
        }
    }

    fn mqtt_certificate_config() -> MqttCertificateConfig {
        MqttCertificateConfig {
            certificate: MqttCertificateSettings {
                api_url: "https://kubernetes.default.svc".to_owned(),
                namespace: "trustiot".to_owned(),
                token_file: PathBuf::from("/var/run/secrets/trustiot/kube/token"),
                ca_file: PathBuf::from("/var/run/secrets/trustiot/kube/ca.crt"),
                secret: "trustiot-mqtt-broker-tls".to_owned(),
                certs_url: "http://trustiot-certs-broker.trustiot.svc:8082".to_owned(),
                service_name: "trustiot-mqtt-broker.trustiot.svc".to_owned(),
                generation: "1".to_owned(),
                wait: DEFAULT_CERTIFICATE_WAIT,
            },
        }
    }

    type MqttCertificateBreakage = (&'static str, fn(&mut MqttCertificateSettings));

    #[test]
    fn mqtt_certificate_settings() {
        assert!(mqtt_certificate_config().validate().is_ok());
        let mut without_service = mqtt_certificate_config();
        without_service.certificate.service_name = String::new();
        assert!(without_service.validate().is_ok(), "no second name");
        let broken: [MqttCertificateBreakage; 12] = [
            ("certificate.api_url", |c| {
                c.api_url = "http://kubernetes.default.svc".to_owned();
            }),
            ("certificate.namespace", |c| {
                c.namespace = "Trust".to_owned()
            }),
            ("certificate.secret", |c| c.secret = String::new()),
            ("certificate.certs_url", |c| c.certs_url = String::new()),
            ("certificate.certs_url", |c| {
                c.certs_url = "https://trustiot-certs-broker:8082".to_owned();
            }),
            ("certificate.certs_url", |c| {
                c.certs_url = "http://user:pw@trustiot-certs-broker:8082".to_owned();
            }),
            ("certificate.service_name", |c| {
                c.service_name = "Broker.svc".to_owned();
            }),
            ("certificate.generation", |c| c.generation = "0".to_owned()),
            ("certificate.generation", |c| c.generation = "x".to_owned()),
            ("certificate.generation", |c| {
                c.generation = "1234567890".to_owned();
            }),
            ("certificate.wait", |c| c.wait = Duration::ZERO),
            ("certificate.wait", |c| c.wait = Duration::from_secs(7200)),
        ];
        for (key, change) in broken {
            let mut config = mqtt_certificate_config();
            change(&mut config.certificate);
            assert_eq!(
                config
                    .validate()
                    .err()
                    .map(|invalid| invalid.key)
                    .as_deref(),
                Some(key)
            );
        }
    }
}
