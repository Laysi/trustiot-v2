//! The certs service's configuration (services/certs/README.md, "設定").

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

/// Fewest connections of the service role's pool: one for the CRL job, one for the monitor and
/// requests.
pub const MIN_DATABASE_CONNECTIONS: u32 = 2;

/// Most connections of the service role's pool per replica.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// Shortest gateway certificate validity: the renewal endpoint (Laysi/trustiot-v2#81) needs room
/// to renew before expiry.
pub const MIN_GATEWAY_VALIDITY: Duration = Duration::from_secs(3600);

/// Longest gateway certificate validity: 398 days, the ceiling browsers accept for server
/// certificates, kept for client certificates too so a stolen certificate does not outlive a
/// year.
pub const MAX_GATEWAY_VALIDITY: Duration = Duration::from_secs(398 * 86_400);

/// Longest CRL validity: Mosquitto rejects every gateway of an issuing CA once its CRL expires,
/// so a CRL that lives long only delays revocation news, never helps.
pub const MAX_CRL_VALIDITY: Duration = Duration::from_secs(31 * 86_400);

/// The service's settings: the platform settings the chart sets, the database role, the CA
/// files, the gateway certificate profile and the CRL schedule.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's credentials and the pool size.
    pub database: Database,
    /// Where the issuing CA keys and the certificates are mounted.
    pub ca: CaFiles,
    /// The gateway client certificate profile.
    #[serde(default)]
    pub gateway: GatewayProfile,
    /// When CRLs are regenerated and when their expiry raises the alert.
    #[serde(default)]
    pub crl: CrlSchedule,
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

/// `database.*`: the service role (`certs_app`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `TRUSTIOT__DATABASE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections of the pool per replica, from
    /// [`MIN_DATABASE_CONNECTIONS`] to [`MAX_DATABASE_CONNECTIONS`]. Default 5.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    5
}

/// Never prints the password.
impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("username", &self.username)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

/// `ca.*`: the CA material, as the chart mounts it (services/certs/README.md, "CA 階層與私鑰保
/// 管").
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaFiles {
    /// `TRUSTIOT__CA__KEYS_FILE`: the issuing CA keys (`crates/ca-keys`, `issuing-ca-keys.json`)
    /// that the pod's init container keeps in a Secret; mounted into this container only.
    pub keys_file: PathBuf,
    /// `TRUSTIOT__CA__ROOT_CERTIFICATE_FILE`: the offline root's certificate, PEM.
    pub root_certificate_file: PathBuf,
    /// `TRUSTIOT__CA__CERTIFICATES_FILE`: the issuing CA certificates the root signed, PEM, one
    /// per key that may sign; certificates of keys the Secret no longer holds are ignored.
    pub certificates_file: PathBuf,
    /// `TRUSTIOT__CA__RELOAD_INTERVAL`: how often the files are read again. Default 30 s.
    #[serde(
        default = "default_reload_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub reload_interval: Duration,
}

const fn default_reload_interval() -> Duration {
    Duration::from_secs(30)
}

/// `gateway.*`: the profile of gateway client certificates. Everything else in the profile is
/// fixed: the subject is the gateway id, the extended key usage is client authentication only.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProfile {
    /// `TRUSTIOT__GATEWAY__VALIDITY`: how long a gateway certificate is valid, from
    /// [`MIN_GATEWAY_VALIDITY`] to [`MAX_GATEWAY_VALIDITY`], never past the issuing CA
    /// certificate. Default 90 days.
    #[serde(
        default = "default_gateway_validity",
        deserialize_with = "service_kit::config::duration"
    )]
    pub validity: Duration,
}

impl Default for GatewayProfile {
    fn default() -> Self {
        Self {
            validity: default_gateway_validity(),
        }
    }
}

const fn default_gateway_validity() -> Duration {
    Duration::from_secs(90 * 86_400)
}

/// `crl.*`: the CRL schedule. Every `check_interval` the replica that holds the job lock gives
/// every issuing CA whose CRL has less than `renew_before` left a new CRL valid for `validity`;
/// every replica reports each CRL's remaining time and raises the alert below `alert_before`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrlSchedule {
    /// `TRUSTIOT__CRL__CHECK_INTERVAL`: default 5 minutes.
    #[serde(
        default = "default_check_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub check_interval: Duration,
    /// `TRUSTIOT__CRL__VALIDITY`: how long after the round that makes it a new CRL's nextUpdate
    /// lies (its thisUpdate lies `issue::CLOCK_SKEW_SECS` before the round); default 7 days, at
    /// most [`MAX_CRL_VALIDITY`].
    #[serde(
        default = "default_crl_validity",
        deserialize_with = "service_kit::config::duration"
    )]
    pub validity: Duration,
    /// `TRUSTIOT__CRL__RENEW_BEFORE`: a CRL with less than this left is replaced; default 5 days.
    #[serde(
        default = "default_renew_before",
        deserialize_with = "service_kit::config::duration"
    )]
    pub renew_before: Duration,
    /// `TRUSTIOT__CRL__ALERT_BEFORE`: a CRL with less than this left raises the alert; default 3
    /// days.
    #[serde(
        default = "default_alert_before",
        deserialize_with = "service_kit::config::duration"
    )]
    pub alert_before: Duration,
}

impl Default for CrlSchedule {
    fn default() -> Self {
        Self {
            check_interval: default_check_interval(),
            validity: default_crl_validity(),
            renew_before: default_renew_before(),
            alert_before: default_alert_before(),
        }
    }
}

const fn default_check_interval() -> Duration {
    Duration::from_secs(300)
}

const fn default_crl_validity() -> Duration {
    Duration::from_secs(7 * 86_400)
}

const fn default_renew_before() -> Duration {
    Duration::from_secs(5 * 86_400)
}

const fn default_alert_before() -> Duration {
    Duration::from_secs(3 * 86_400)
}

impl Config {
    /// Where and how the service role connects.
    #[must_use]
    pub fn service_role(&self) -> PostgresConfig {
        PostgresConfig {
            host: self.platform.postgres_host.clone(),
            port: self.platform.postgres_port,
            database: self.platform.postgres_database.clone(),
            ssl_mode: self.platform.postgres_sslmode,
            ca_file: self.platform.postgres_ca_file.clone(),
            username: self.database.username.clone(),
            password: self.database.password.clone(),
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
        if database.username.trim().is_empty() {
            return Err(Invalid::new("database.username", "must not be blank"));
        }
        if database.password.is_empty() {
            return Err(Invalid::new("database.password", "must not be empty"));
        }
        for (key, path) in [
            ("ca.keys_file", &self.ca.keys_file),
            ("ca.root_certificate_file", &self.ca.root_certificate_file),
            ("ca.certificates_file", &self.ca.certificates_file),
        ] {
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(Invalid::new(key, "must be an absolute path to a file"));
            }
        }
        if !(Duration::from_secs(1)..=Duration::from_secs(3600)).contains(&self.ca.reload_interval)
        {
            return Err(Invalid::new(
                "ca.reload_interval",
                "must be between 1 second and 1 hour",
            ));
        }
        if !(MIN_GATEWAY_VALIDITY..=MAX_GATEWAY_VALIDITY).contains(&self.gateway.validity) {
            return Err(Invalid::new(
                "gateway.validity",
                "must be between 1 hour and 398 days",
            ));
        }
        self.crl.validate()
    }
}

impl CrlSchedule {
    fn validate(&self) -> Result<(), Invalid> {
        if self.check_interval < Duration::from_secs(1) {
            return Err(Invalid::new(
                "crl.check_interval",
                "must be at least 1 second",
            ));
        }
        if self.validity > MAX_CRL_VALIDITY {
            return Err(Invalid::new("crl.validity", "must be at most 31 days"));
        }
        if self.alert_before.is_zero() {
            return Err(Invalid::new("crl.alert_before", "must be more than 0"));
        }
        if !(self.alert_before < self.renew_before && self.renew_before < self.validity) {
            return Err(Invalid::new(
                "crl.renew_before",
                "must be more than crl.alert_before and less than crl.validity, so a CRL is \
                 replaced before the alert and not on every check",
            ));
        }
        // Two checks fit between the start of renewal and the alert, so one failed or skipped
        // round does not raise it.
        if self.renew_before - self.alert_before < self.check_interval.saturating_mul(2) {
            return Err(Invalid::new(
                "crl.check_interval",
                "must be at most half of crl.renew_before minus crl.alert_before",
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
                postgres_ca_file: PathBuf::from("/ca.crt"),
            },
            database: Database {
                username: "certs_app".to_owned(),
                password: "secret-password".to_owned(),
                max_connections: 5,
            },
            ca: CaFiles {
                keys_file: PathBuf::from("/keys/issuing-ca-keys.json"),
                root_certificate_file: PathBuf::from("/ca/root.pem"),
                certificates_file: PathBuf::from("/ca/issuing.pem"),
                reload_interval: Duration::from_secs(30),
            },
            gateway: GatewayProfile::default(),
            crl: CrlSchedule::default(),
        }
    }

    type Breakage = (&'static str, fn(&mut Config));

    #[test]
    fn defaults_are_valid_and_every_bound_is_checked() {
        assert!(config().validate().is_ok());
        let seconds = Duration::from_secs;
        let mut short = config();
        short.crl = CrlSchedule {
            check_interval: seconds(1),
            validity: seconds(6),
            renew_before: seconds(4),
            alert_before: seconds(2),
        };
        assert!(short.validate().is_ok());

        let broken: [Breakage; 12] = [
            ("database.max_connections", |c| {
                c.database.max_connections = 1
            }),
            ("database.username", |c| {
                c.database.username = " ".to_owned()
            }),
            ("database.password", |c| c.database.password = String::new()),
            ("ca.keys_file", |c| {
                c.ca.keys_file = PathBuf::from("keys.json")
            }),
            ("ca.certificates_file", |c| {
                c.ca.certificates_file = PathBuf::from("/");
            }),
            ("ca.reload_interval", |c| {
                c.ca.reload_interval = Duration::ZERO
            }),
            ("gateway.validity", |c| {
                c.gateway.validity = Duration::from_secs(60);
            }),
            ("gateway.validity", |c| {
                c.gateway.validity = MAX_GATEWAY_VALIDITY + Duration::from_secs(1);
            }),
            ("crl.validity", |c| {
                c.crl.validity = MAX_CRL_VALIDITY + Duration::from_secs(1);
            }),
            ("crl.renew_before", |c| c.crl.renew_before = c.crl.validity),
            ("crl.renew_before", |c| {
                c.crl.alert_before = c.crl.renew_before
            }),
            ("crl.check_interval", |c| {
                c.crl.check_interval = Duration::from_secs(86_400 + 1);
            }),
        ];
        for (key, breakage) in broken {
            let mut config = config();
            breakage(&mut config);
            let error = config.validate().expect_err(key);
            assert_eq!(error.key, key, "{}", error.message);
        }
    }

    #[test]
    fn debug_hides_the_password() {
        assert!(!format!("{:?}", config()).contains("secret-password"));
    }
}
