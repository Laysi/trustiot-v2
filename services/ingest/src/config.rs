//! The ingest service's configuration (services/ingest/README.md, "設定").

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

/// Fewest connections of the service role's pool: one for the binding consumer's delivery, one
/// for readiness (the `postgres` ping and the migration check), and one for the telemetry path's
/// stream locks (it handles as many batches at once as the pool has connections beyond those
/// two, [`crate::telemetry::concurrency`]).
pub const MIN_DATABASE_CONNECTIONS: u32 = 3;

/// Most connections of the service role's pool per replica.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// The service's settings: the platform settings the chart sets, the database roles, the
/// service's ClickHouse account, and its broker account.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's and the platform role's credentials, and the pool size.
    pub database: Database,
    /// The service's ClickHouse account.
    pub clickhouse: ClickHouse,
    /// `mqtt.*`: the broker's service port and the service's account `service:ingest`
    /// (crates/mqtt-client, "設定"; deploy/chart/README.md, "MQTT broker"). Required: the service
    /// has no use without its telemetry. The chart sets every `TRUSTIOT__MQTT__*` and mounts the
    /// password file while the broker is on (Laysi/trustiot-v2#642).
    pub mqtt: mqtt_client::Config,
    /// `telemetry_limits.*`: each gateway's limits on the telemetry path
    /// ([`crate::telemetry::limits`]). Optional.
    #[serde(default)]
    pub telemetry_limits: TelemetryLimits,
}

/// Most batches per minute a gateway's bucket may be set to refill with.
pub const MAX_GATEWAY_BATCHES_PER_MINUTE: u32 = 60_000;

/// Most batches a gateway's bucket may be set to hold.
pub const MAX_GATEWAY_BURST: u32 = 10_000;

/// `telemetry_limits.*`: how many batches one gateway may send to one replica. A replica's
/// share of a gateway's messages is the broker's to decide, so the gateway's whole allowance is
/// the replicas' sum.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryLimits {
    /// `TRUSTIOT__TELEMETRY_LIMITS__BATCHES_PER_MINUTE`: how fast a gateway's bucket refills,
    /// 1 to [`MAX_GATEWAY_BATCHES_PER_MINUTE`]. Default 600 (10 a second).
    #[serde(default = "default_batches_per_minute")]
    pub batches_per_minute: u32,
    /// `TRUSTIOT__TELEMETRY_LIMITS__BURST`: the bucket's capacity, 1 to [`MAX_GATEWAY_BURST`].
    /// Default 60.
    #[serde(default = "default_burst")]
    pub burst: u32,
}

const fn default_batches_per_minute() -> u32 {
    600
}

const fn default_burst() -> u32 {
    60
}

impl Default for TelemetryLimits {
    fn default() -> Self {
        Self {
            batches_per_minute: default_batches_per_minute(),
            burst: default_burst(),
        }
    }
}

/// The platform settings the service reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
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
    /// server, over the mesh's mTLS) or `https://host:port` (an external one, verified against
    /// [`Platform::clickhouse_ca_file`] and the host name).
    pub clickhouse_url: String,
    /// `TRUSTIOT_CLICKHOUSE_CA_FILE`: the CA of an `https` ClickHouse, the only trust anchor.
    #[serde(default)]
    pub clickhouse_ca_file: Option<PathBuf>,
}

/// `database.*`: the service role (`ingest_app`), which records gateway bindings under row-level
/// security in the event's tenant, and the platform role (`ingest_platform`), which reads the
/// assets service's events and looks a gateway's tenant up across tenants, read only (decision
/// 29, crates/outbox; services/ingest/README.md, "閘道器綁定對照表").
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
    /// The platform role's pool has [`crate::PLATFORM_CONNECTIONS`] more.
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

/// `clickhouse.*`: the service's own ClickHouse account, which holds the role `trustiot_ingest`
/// (services/ingest/migrations/clickhouse/0004_access.sql). The migration Job creates it as
/// `trustiot_ingest_writer` with a generated password, which the chart hands the service
/// (tools/migrate/src/clickhouse.rs, Laysi/trustiot-v2#642).
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
        let limits = &self.telemetry_limits;
        if !(1..=MAX_GATEWAY_BATCHES_PER_MINUTE).contains(&limits.batches_per_minute) {
            return Err(Invalid::new(
                "telemetry_limits.batches_per_minute",
                format!("must be between 1 and {MAX_GATEWAY_BATCHES_PER_MINUTE}"),
            ));
        }
        if !(1..=MAX_GATEWAY_BURST).contains(&limits.burst) {
            return Err(Invalid::new(
                "telemetry_limits.burst",
                format!("must be between 1 and {MAX_GATEWAY_BURST}"),
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
            ("clickhouse.password", &self.clickhouse.password),
        ] {
            if password.is_empty() {
                return Err(Invalid::new(key, "must not be empty"));
            }
        }
        let url = &self.platform.clickhouse_url;
        if !is_clickhouse_url(url) {
            return Err(Invalid::new(
                "platform.clickhouse_url",
                "must be http://host[:port] or https://host[:port], without credentials, path or \
                 query",
            ));
        }
        if let Err(error) = self.mqtt.validate() {
            let key = match error {
                mqtt_client::ConfigError::Host => "mqtt.host",
                mqtt_client::ConfigError::Port => "mqtt.port",
                mqtt_client::ConfigError::Username => "mqtt.username",
                mqtt_client::ConfigError::PasswordFile => "mqtt.password_file",
            };
            return Err(Invalid::new(key, error.to_string()));
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

    fn config(url: &str, ca_file: Option<&str>) -> Config {
        Config {
            platform: Platform {
                postgres_host: "postgres".to_owned(),
                postgres_port: 5432,
                postgres_database: "trustiot".to_owned(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: PathBuf::from("/etc/ca.pem"),
                clickhouse_url: url.to_owned(),
                clickhouse_ca_file: ca_file.map(PathBuf::from),
            },
            database: Database {
                username: "ingest_app".to_owned(),
                password: "p".to_owned(),
                platform_username: "ingest_platform".to_owned(),
                platform_password: "p".to_owned(),
                max_connections: 10,
            },
            clickhouse: ClickHouse {
                username: "trustiot_ingest_writer".to_owned(),
                password: "p".to_owned(),
            },
            mqtt: mqtt_client::Config {
                host: "mqtt-broker".to_owned(),
                port: mqtt_client::DEFAULT_PORT,
                username: "service:ingest".to_owned(),
                password_file: PathBuf::from("/etc/trustiot/mqtt/ingest"),
            },
            telemetry_limits: TelemetryLimits::default(),
        }
    }

    #[test]
    fn the_pool_and_the_gateway_limits_are_bounded() {
        let mut small = config("http://clickhouse:8123", None);
        small.database.max_connections = MIN_DATABASE_CONNECTIONS - 1;
        let error = small
            .validate()
            .expect_err("no connection left for readiness");
        assert_eq!(error.key, "database.max_connections");
        for (rate, burst, key) in [
            (0, 60, Some("telemetry_limits.batches_per_minute")),
            (
                MAX_GATEWAY_BATCHES_PER_MINUTE + 1,
                60,
                Some("telemetry_limits.batches_per_minute"),
            ),
            (600, 0, Some("telemetry_limits.burst")),
            (600, MAX_GATEWAY_BURST + 1, Some("telemetry_limits.burst")),
            (1, 1, None),
        ] {
            let mut limited = config("http://clickhouse:8123", None);
            limited.telemetry_limits = TelemetryLimits {
                batches_per_minute: rate,
                burst,
            };
            assert_eq!(
                limited.validate().err().map(|error| error.key),
                key.map(str::to_owned),
                "{rate} a minute, burst {burst}"
            );
        }
    }

    #[test]
    fn an_https_clickhouse_needs_its_ca_and_the_roles_must_differ() {
        assert!(config("http://clickhouse:8123", None).validate().is_ok());
        assert!(
            config("https://clickhouse:8443", Some("/etc/ch.pem"))
                .validate()
                .is_ok()
        );
        let error = config("https://clickhouse:8443", None)
            .validate()
            .expect_err("no CA");
        assert_eq!(error.key, "platform.clickhouse_ca_file");
        let mut same = config("http://clickhouse:8123", None);
        same.database.platform_username = same.database.username.clone();
        let error = same.validate().expect_err("same role");
        assert_eq!(error.key, "database.platform_username");
        let mut empty = config("http://clickhouse:8123", None);
        empty.clickhouse.password.clear();
        let error = empty.validate().expect_err("no password");
        assert_eq!(error.key, "clickhouse.password");
        let mut gateway = config("http://clickhouse:8123", None);
        gateway.mqtt.username = "gw-0001".to_owned();
        let error = gateway.validate().expect_err("not a service account");
        assert_eq!(error.key, "mqtt.username");
    }
}
