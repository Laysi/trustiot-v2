//! The telemetry-query service's configuration (services/telemetry-query/README.md, "設定").

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

/// Fewest connections of the service role's pool: one for the event consumer's delivery, one
/// for requests.
pub const MIN_DATABASE_CONNECTIONS: u32 = 2;

/// Most connections of the service role's pool per replica.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// Shortest ClickHouse password accepted. It is also the key every tenant account's password is
/// derived from (`accounts`), so a short one would weaken all of them; the migration Job
/// generates 64 characters.
pub const MIN_CLICKHOUSE_PASSWORD_CHARS: usize = 32;

/// The service's settings: the platform settings the chart sets, the database roles, and the
/// service's ClickHouse account.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's and the platform role's credentials, and the pool size.
    pub database: Database,
    /// The service's ClickHouse account.
    pub clickhouse: ClickHouse,
}

/// The platform settings the service reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`: the file with the public keys internal identity
    /// tokens are verified with (the chart's ConfigMap volume), read again while the service
    /// runs.
    pub internal_identity_keys_file: PathBuf,
    /// `TRUSTIOT_POSTGRES_HOST`.
    pub postgres_host: String,
    /// `TRUSTIOT_POSTGRES_PORT`.
    pub postgres_port: u16,
    /// `TRUSTIOT_POSTGRES_DATABASE`.
    pub postgres_database: String,
    /// `TRUSTIOT_POSTGRES_SSLMODE`: only `verify-full` is accepted.
    pub postgres_sslmode: SslMode,
    /// `TRUSTIOT_POSTGRES_CA_FILE`: the CA of the server certificate, the only trust anchor.
    pub postgres_ca_file: PathBuf,
    /// `TRUSTIOT_CLICKHOUSE_URL`: ClickHouse's HTTP interface, `http://host:port` (the bundled
    /// server, over the mesh's mTLS) or `https://host:port` (an external one).
    pub clickhouse_url: String,
    /// `TRUSTIOT_CLICKHOUSE_CA_FILE`: the CA of an `https` ClickHouse, the only trust anchor.
    #[serde(default)]
    pub clickhouse_ca_file: Option<PathBuf>,
}

/// `database.*`: the service role (`telemetry_query_app`), which records the tenants whose
/// ClickHouse account exists under row-level security, and the platform role
/// (`telemetry_query_platform`), which only reads the identity service's events across tenants
/// (decision 29, crates/outbox).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `TRUSTIOT__DATABASE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `TRUSTIOT__DATABASE__PLATFORM_USERNAME`.
    pub platform_username: String,
    /// `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`, from a Secret.
    pub platform_password: String,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections of the service role's pool per
    /// replica, from [`MIN_DATABASE_CONNECTIONS`] to [`MAX_DATABASE_CONNECTIONS`]. Default 10.
    /// The platform role's pool has [`crate::EVENT_READER_CONNECTIONS`] more.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    10
}

/// Never prints the passwords.
impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("username", &self.username)
            .field("platform_username", &self.platform_username)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

/// `clickhouse.*`: the service's own ClickHouse account (`trustiot_telemetry_query`,
/// services/ingest/migrations/clickhouse/0008_telemetry_query.sql), whose password the migration
/// Job generates into the service's Secret. It creates the tenant accounts and resets their
/// passwords; every tenant account's password is derived from it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickHouse {
    /// `TRUSTIOT__CLICKHOUSE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__CLICKHOUSE__PASSWORD`, from a Secret.
    pub password: String,
}

/// Never prints the password.
impl fmt::Debug for ClickHouse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouse")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Where and how the service role connects.
    #[must_use]
    pub fn service_role(&self) -> PostgresConfig {
        self.postgres(&self.database.username, &self.database.password)
    }

    /// Where and how the platform role connects.
    #[must_use]
    pub fn platform_role(&self) -> PostgresConfig {
        self.postgres(
            &self.database.platform_username,
            &self.database.platform_password,
        )
    }

    fn postgres(&self, username: &str, password: &str) -> PostgresConfig {
        PostgresConfig {
            host: self.platform.postgres_host.clone(),
            port: self.platform.postgres_port,
            database: self.platform.postgres_database.clone(),
            ssl_mode: self.platform.postgres_sslmode,
            ca_file: self.platform.postgres_ca_file.clone(),
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }
}

/// Whether `url` is `http://` or `https://` with a host, and nothing a request could be steered
/// by: no credentials, query or fragment, and no path but `/`.
fn is_clickhouse_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<http::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri.authority().is_some_and(|authority| {
            !authority.as_str().contains('@') && !authority.host().is_empty()
        })
        && uri.query().is_none()
        && matches!(uri.path(), "" | "/")
        && !url.contains('#')
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        let database = &self.database;
        if !(MIN_DATABASE_CONNECTIONS..=MAX_DATABASE_CONNECTIONS)
            .contains(&database.max_connections)
        {
            return Err(Invalid::new(
                "database.max_connections",
                format!(
                    "must be between {MIN_DATABASE_CONNECTIONS} and {MAX_DATABASE_CONNECTIONS}"
                ),
            ));
        }
        for (key, username) in [
            ("database.username", &database.username),
            ("database.platform_username", &database.platform_username),
            ("clickhouse.username", &self.clickhouse.username),
        ] {
            if username.trim().is_empty() {
                return Err(Invalid::new(key, "must not be blank"));
            }
        }
        if database.username == database.platform_username {
            return Err(Invalid::new(
                "database.platform_username",
                "must be another role than database.username: only the platform role may read \
                 across tenants",
            ));
        }
        for (key, password) in [
            ("database.password", &database.password),
            ("database.platform_password", &database.platform_password),
        ] {
            if password.is_empty() {
                return Err(Invalid::new(key, "must not be empty"));
            }
        }
        if self.clickhouse.password.chars().count() < MIN_CLICKHOUSE_PASSWORD_CHARS {
            return Err(Invalid::new(
                "clickhouse.password",
                format!(
                    "must have at least {MIN_CLICKHOUSE_PASSWORD_CHARS} characters: every tenant \
                     account's password is derived from it"
                ),
            ));
        }
        let url = &self.platform.clickhouse_url;
        if !is_clickhouse_url(url) {
            return Err(Invalid::new(
                "platform.clickhouse_url",
                "must be http://host[:port] or https://host[:port], without credentials, path or \
                 query",
            ));
        }
        if url.starts_with("https://") && self.platform.clickhouse_ca_file.is_none() {
            return Err(Invalid::new(
                "platform.clickhouse_ca_file",
                "is required for an https ClickHouse: the service trusts only the CA the \
                 deployment names",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_urls() {
        for good in [
            "http://clickhouse:8123",
            "http://clickhouse.trustiot.svc:8123/",
            "https://clickhouse.example:8443",
            "http://127.0.0.1:8123",
        ] {
            assert!(is_clickhouse_url(good), "{good}");
        }
        for bad in [
            "",
            "clickhouse:8123",
            "ftp://clickhouse:8123",
            "http://user:pw@clickhouse:8123",
            "http://clickhouse:8123/?user=x",
            "http://clickhouse:8123/other",
            "http://clickhouse:8123/#x",
            "http://:8123",
        ] {
            assert!(!is_clickhouse_url(bad), "{bad}");
        }
    }
}
