//! The gateway operations service's configuration (services/gateway-ops/README.md, "服務與設定").

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

/// Fewest connections of the service role's pool, the most the service holds at once:
///
/// - 1 for the consumer's delivery transaction;
/// - 2 for readiness, whose `postgres` ping and migration check run at the same time;
/// - 1 for the republish job's lock (`service_kit::job_lock`), held for a whole republish;
/// - [`crate::delivery::CONCURRENCY`] (2) for the publish slots, each holding a gateway's publish
///   lock until the PUBACK and then writing its record, one connection at a time;
/// - 1 for the report task, which records one apply report at a time.
pub const MIN_DATABASE_CONNECTIONS: u32 = 7;

/// Most connections of the service role's pool per replica.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// Shortest coalescing window of the delivery ([`Delivery::throttle`]).
pub const MIN_THROTTLE: Duration = Duration::from_millis(100);

/// Longest coalescing window of the delivery ([`Delivery::throttle`]).
pub const MAX_THROTTLE: Duration = Duration::from_secs(60);

/// The service's settings: the platform settings the chart sets, the database roles, the broker
/// and the delivery.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's and the platform role's credentials, and the pool size.
    pub database: Database,
    /// `mqtt.*`: the broker's service port and the service's account `service:gateway-ops`
    /// (crates/mqtt-client, "設定"; deploy/chart/README.md, "MQTT broker"), the same keys as
    /// ingest's. Required: delivering configurations is what the service is for, and readiness
    /// needs the broker.
    pub mqtt: mqtt_client::Config,
    /// `delivery.*`: how configurations are delivered. Optional.
    #[serde(default)]
    pub delivery: Delivery,
}

/// `delivery.*`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    /// `TRUSTIOT__DELIVERY__THROTTLE`: the coalescing window of a gateway's changes. The first
    /// change after a quiet period opens it, and only the gateway's latest version is published
    /// when it ends, so a burst of changes is one publish. From [`MIN_THROTTLE`] to
    /// [`MAX_THROTTLE`]; default 1 s.
    #[serde(
        default = "default_throttle",
        deserialize_with = "service_kit::config::duration"
    )]
    pub throttle: Duration,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            throttle: default_throttle(),
        }
    }
}

const fn default_throttle() -> Duration {
    Duration::from_secs(1)
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
}

/// `database.*`: the service role (`gateway_ops_app`), which keeps the projection and writes the
/// configurations under row-level security in the event's tenant, and the platform role
/// (`gateway_ops_platform`), which reads the assets service's events and, read only, a gateway's
/// current binding and latest configuration across tenants (decision 29, crates/outbox;
/// services/gateway-ops/README.md, "資料庫角色").
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
        if let Err(error) = self.mqtt.validate() {
            let key = match error {
                mqtt_client::ConfigError::Host => "mqtt.host",
                mqtt_client::ConfigError::Port => "mqtt.port",
                mqtt_client::ConfigError::Username => "mqtt.username",
                mqtt_client::ConfigError::PasswordFile => "mqtt.password_file",
            };
            return Err(Invalid::new(key, error.to_string()));
        }
        if !(MIN_THROTTLE..=MAX_THROTTLE).contains(&self.delivery.throttle) {
            return Err(Invalid::new(
                "delivery.throttle",
                format!("must be between {MIN_THROTTLE:?} and {MAX_THROTTLE:?}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            platform: Platform {
                postgres_host: "postgres".to_owned(),
                postgres_port: 5432,
                postgres_database: "trustiot".to_owned(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: PathBuf::from("/etc/ca.pem"),
            },
            database: Database {
                username: "gateway_ops_app".to_owned(),
                password: "p".to_owned(),
                platform_username: "gateway_ops_platform".to_owned(),
                platform_password: "p".to_owned(),
                max_connections: 10,
            },
            mqtt: mqtt_client::Config {
                host: "trustiot-mqtt-broker.trustiot.svc".to_owned(),
                port: mqtt_client::DEFAULT_PORT,
                username: "service:gateway-ops".to_owned(),
                password_file: PathBuf::from("/etc/trustiot/mqtt/gateway-ops"),
            },
            delivery: Delivery::default(),
        }
    }

    #[test]
    fn the_roles_must_differ_and_have_passwords() {
        assert!(config().validate().is_ok());
        let mut same = config();
        same.database.platform_username = same.database.username.clone();
        assert_eq!(
            same.validate().expect_err("same role").key,
            "database.platform_username"
        );
        let mut empty = config();
        empty.database.platform_password.clear();
        assert_eq!(
            empty.validate().expect_err("no password").key,
            "database.platform_password"
        );
        let mut pool = config();
        pool.database.max_connections = MIN_DATABASE_CONNECTIONS - 1;
        assert_eq!(
            pool.validate().expect_err("too small a pool").key,
            "database.max_connections"
        );
    }

    #[test]
    fn the_broker_and_the_throttle_are_checked() {
        let mut gateway = config();
        "gw-0001".clone_into(&mut gateway.mqtt.username);
        assert_eq!(
            gateway.validate().expect_err("a gateway account").key,
            "mqtt.username"
        );
        for throttle in [Duration::from_millis(99), Duration::from_secs(61)] {
            let mut bad = config();
            bad.delivery.throttle = throttle;
            assert_eq!(
                bad.validate().expect_err("out of bounds").key,
                "delivery.throttle"
            );
        }
        for throttle in [MIN_THROTTLE, MAX_THROTTLE] {
            let mut good = config();
            good.delivery.throttle = throttle;
            assert!(good.validate().is_ok());
        }
    }

    #[test]
    fn debug_never_prints_passwords() {
        let mut config = config();
        "first-pw".clone_into(&mut config.database.password);
        "second-pw".clone_into(&mut config.database.platform_password);
        let printed = format!("{config:?}");
        assert!(!printed.contains("first-pw") && !printed.contains("second-pw"));
    }
}
