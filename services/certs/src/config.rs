//! The certs service's configuration (services/certs/README.md, "設定").

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::issue::BrokerNames;

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

/// Shortest workstation certificate validity.
pub const MIN_WORKSTATION_VALIDITY: Duration = Duration::from_secs(3600);

/// Longest workstation certificate validity: the same ceiling as gateway certificates.
pub const MAX_WORKSTATION_VALIDITY: Duration = MAX_GATEWAY_VALIDITY;

/// Shortest MQTT broker server certificate validity: whoever renews it needs room to do so
/// before expiry, and the CA refuses to sign anything valid for less ([`crate::issue`]'s
/// `MIN_REMAINING_SECS`).
pub const MIN_BROKER_VALIDITY: Duration = Duration::from_secs(3600);

/// Longest MQTT broker server certificate validity: the gateway certificates' default. The
/// broker renews its certificate in the cluster without a person, so a long validity buys
/// nothing and keeps a stolen key usable for longer.
pub const MAX_BROKER_VALIDITY: Duration = Duration::from_secs(90 * 86_400);

/// The default port of the MQTT broker's certificate issuance; the chart names it
/// `broker-certs`.
pub const DEFAULT_BROKER_PORT: u16 = 8082;

/// The broker account the gateway permission sync logs in with: the only one the chart gives the
/// dynamic-security admin role (docs/architecture/secrets.md, "MQTT dynsec 管理帳密").
pub const MQTT_USERNAME: &str = "service:certs";

/// Shortest `dynsec.interval`.
pub const MIN_DYNSEC_INTERVAL: Duration = Duration::from_secs(1);

/// Longest `dynsec.interval`.
pub const MAX_DYNSEC_INTERVAL: Duration = Duration::from_secs(3600);

/// Longest CRL validity: Mosquitto rejects every gateway of an issuing CA once its CRL expires,
/// so a CRL that lives long only delays revocation news, never helps.
pub const MAX_CRL_VALIDITY: Duration = Duration::from_secs(31 * 86_400);

/// The service's settings: the platform settings the chart sets, the database role, the CA
/// files, the gateway and workstation certificate profiles and the CRL schedule.
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
    /// The workstation certificate profile.
    #[serde(default)]
    pub workstation: WorkstationProfile,
    /// The MQTT broker's server certificate profile and the port it is issued on.
    #[serde(default)]
    pub broker: BrokerProfile,
    /// When CRLs are regenerated and when their expiry raises the alert.
    #[serde(default)]
    pub crl: CrlSchedule,
    /// The renewal endpoint's limits (Laysi/trustiot-v2#81).
    #[serde(default)]
    pub renewal: RenewalLimits,
    /// `mqtt.*`: the MQTT broker's service port and the certs service's broker account, for the
    /// gateway permission sync ([`crate::dynsec`], Laysi/trustiot-v2#633). Without it nothing is
    /// synced: the chart sets it only when the broker is deployed.
    #[serde(default)]
    pub mqtt: Option<mqtt_client::Config>,
    /// `dynsec.*`: how often the gateway permission sync compares every gateway.
    #[serde(default)]
    pub dynsec: DynsecSchedule,
}

/// The platform settings the service reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`: the file with the public keys internal identity
    /// tokens are verified with (the chart's ConfigMap volume), read again while the service
    /// runs. The workstation certificate API needs it (Laysi/trustiot-v2#534).
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
}

/// `database.*`: the service role (`certs_app`), and the platform role (`certs_platform`), which
/// the factory registration (Laysi/trustiot-v2#79) writes with, because only a producer's
/// platform role may write platform-wide outbox events (crates/outbox).
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
    /// replica, from [`MIN_DATABASE_CONNECTIONS`] to [`MAX_DATABASE_CONNECTIONS`]. Default 5.
    /// The platform role's pool has [`crate::PLATFORM_CONNECTIONS`].
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    5
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

/// `workstation.*`: the profile of production workstation certificates
/// (Laysi/trustiot-v2#534). Everything else in the profile is fixed: the subject is
/// `workstation:<name>`, the extended key usage is `issue::WORKSTATION_PURPOSE` only.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkstationProfile {
    /// `TRUSTIOT__WORKSTATION__VALIDITY`: how long a workstation certificate is valid, from
    /// [`MIN_WORKSTATION_VALIDITY`] to [`MAX_WORKSTATION_VALIDITY`], never past the issuing CA
    /// certificate. Default 180 days: a platform administrator renews it by issuing another,
    /// twice a year, and a key kept in a file instead of a TPM is exposed for at most that long
    /// (revocation ends it sooner).
    #[serde(
        default = "default_workstation_validity",
        deserialize_with = "service_kit::config::duration"
    )]
    pub validity: Duration,
}

impl Default for WorkstationProfile {
    fn default() -> Self {
        Self {
            validity: default_workstation_validity(),
        }
    }
}

const fn default_workstation_validity() -> Duration {
    Duration::from_secs(180 * 86_400)
}

/// `broker.*`: the MQTT broker's server certificate (Laysi/trustiot-v2#557; README.md, "broker
/// 伺服器憑證"). Everything else in the profile is fixed: the subject and the first DNS name are
/// `issue::BROKER_SERVER_NAME`, the extended key usage is server authentication only.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerProfile {
    /// `TRUSTIOT__BROKER__LISTEN_ADDR`: the port that issues it and serves nothing else, apart
    /// from the API port, so the mesh can let the MQTT broker alone reach it (the chart's port
    /// `broker-certs` and its own Service and policies `<release>-certs-broker`). Default
    /// `0.0.0.0:8082`. Startup fails when it is taken.
    #[serde(default = "default_broker_listen_addr")]
    pub listen_addr: SocketAddr,
    /// `TRUSTIOT__BROKER__VALIDITY`: how long a broker server certificate is valid, from
    /// [`MIN_BROKER_VALIDITY`] to [`MAX_BROKER_VALIDITY`], never past the issuing CA certificate.
    /// Default 30 days; renewal is due once half of it has passed (README.md).
    #[serde(
        default = "default_broker_validity",
        deserialize_with = "service_kit::config::duration"
    )]
    pub validity: Duration,
    /// `TRUSTIOT__BROKER__SERVICE_NAME`: the broker's Service name, a second DNS name in the
    /// certificate after `issue::BROKER_SERVER_NAME` (the chart's
    /// `<release>-mqtt-broker.<namespace>.svc`). Default none.
    #[serde(default)]
    pub service_name: Option<String>,
}

impl Default for BrokerProfile {
    fn default() -> Self {
        Self {
            listen_addr: default_broker_listen_addr(),
            validity: default_broker_validity(),
            service_name: None,
        }
    }
}

impl BrokerProfile {
    /// The DNS names of the certificate.
    ///
    /// # Errors
    ///
    /// When `service_name` is not a DNS name; [`Config::validate`] checks it at startup.
    pub fn names(&self) -> Result<BrokerNames, String> {
        BrokerNames::new(self.service_name.as_deref())
    }
}

const fn default_broker_listen_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_BROKER_PORT)
}

const fn default_broker_validity() -> Duration {
    Duration::from_secs(30 * 86_400)
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

/// `renewal.*`: the limits of the renewal endpoint (Laysi/trustiot-v2#81; README.md, "換發").
/// The rate limits count per replica: a hardware ID may send `gateway_burst` requests of
/// either step at once and one more every `gateway_interval`, and all hardware IDs together
/// `global_burst` requests of each step at once and one more of each every `global_interval`
/// (the challenge step and the renew step have a global limit each).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewalLimits {
    /// `TRUSTIOT__RENEWAL__GATEWAY_BURST`: default 4 (one renewal is two requests), 1 to 100.
    #[serde(default = "default_gateway_burst")]
    pub gateway_burst: u32,
    /// `TRUSTIOT__RENEWAL__GATEWAY_INTERVAL`: default 30 seconds, 1 second to 1 hour.
    #[serde(
        default = "default_gateway_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub gateway_interval: Duration,
    /// `TRUSTIOT__RENEWAL__GLOBAL_BURST`: for each step; default 100, 1 to 10000.
    #[serde(default = "default_global_burst")]
    pub global_burst: u32,
    /// `TRUSTIOT__RENEWAL__GLOBAL_INTERVAL`: for each step; default 50 ms (20 requests of each
    /// step a second), 1 ms to 60 seconds.
    #[serde(
        default = "default_global_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub global_interval: Duration,
    /// `TRUSTIOT__RENEWAL__MAX_CONCURRENT`: requests whose database work runs at once per
    /// replica; default 2, 1 to one less than the platform role's pool
    /// ([`crate::PLATFORM_CONNECTIONS`]), so the factory registration and the consumer of the
    /// assets service's events always find a connection.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// `TRUSTIOT__RENEWAL__MAX_CONSUMER_LAG`: the endpoint answers 503 while the oldest event the
    /// consumer of the assets service's events has not processed is older than this, or its lag
    /// is unknown; default 60 seconds, 1 second to 1 hour.
    #[serde(
        default = "default_max_consumer_lag",
        deserialize_with = "service_kit::config::duration"
    )]
    pub max_consumer_lag: Duration,
    /// `TRUSTIOT__RENEWAL__REFUSAL_WINDOW`: refused renewals of a registered hardware ID are
    /// counted per window of this length, one `certs.gateway_renewal_refused` each; default 1
    /// hour, 1 minute to 1 day.
    #[serde(
        default = "default_refusal_window",
        deserialize_with = "service_kit::config::duration"
    )]
    pub refusal_window: Duration,
    /// `TRUSTIOT__RENEWAL__REFUSAL_FLUSH_INTERVAL`: how often each replica looks for ended
    /// windows to report; default 60 seconds, 1 second to 1 hour.
    #[serde(
        default = "default_refusal_flush_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub refusal_flush_interval: Duration,
}

impl Default for RenewalLimits {
    fn default() -> Self {
        Self {
            gateway_burst: default_gateway_burst(),
            gateway_interval: default_gateway_interval(),
            global_burst: default_global_burst(),
            global_interval: default_global_interval(),
            max_concurrent: default_max_concurrent(),
            max_consumer_lag: default_max_consumer_lag(),
            refusal_window: default_refusal_window(),
            refusal_flush_interval: default_refusal_flush_interval(),
        }
    }
}

/// `dynsec.*`: the gateway permission sync's schedule ([`crate::dynsec`]).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynsecSchedule {
    /// `TRUSTIOT__DYNSEC__INTERVAL`: how often the replica that gets the job lock compares every
    /// gateway client of the broker with Postgres, from [`MIN_DYNSEC_INTERVAL`] to
    /// [`MAX_DYNSEC_INTERVAL`]. Default 5 minutes. It bounds how long a change that no immediate
    /// sync covers (a certificate that expired, a sync that failed) takes to reach the broker.
    #[serde(
        default = "default_dynsec_interval",
        deserialize_with = "service_kit::config::duration"
    )]
    pub interval: Duration,
}

impl Default for DynsecSchedule {
    fn default() -> Self {
        Self {
            interval: default_dynsec_interval(),
        }
    }
}

const fn default_gateway_burst() -> u32 {
    4
}

const fn default_gateway_interval() -> Duration {
    Duration::from_secs(30)
}

const fn default_global_burst() -> u32 {
    100
}

const fn default_global_interval() -> Duration {
    Duration::from_millis(50)
}

const fn default_max_concurrent() -> u32 {
    2
}

const fn default_max_consumer_lag() -> Duration {
    Duration::from_secs(60)
}

const fn default_refusal_window() -> Duration {
    Duration::from_secs(3600)
}

const fn default_refusal_flush_interval() -> Duration {
    Duration::from_secs(60)
}

impl RenewalLimits {
    fn validate(&self) -> Result<(), Invalid> {
        let seconds = Duration::from_secs;
        let counts = [
            ("renewal.gateway_burst", self.gateway_burst, 1, 100),
            ("renewal.global_burst", self.global_burst, 1, 10_000),
            (
                "renewal.max_concurrent",
                self.max_concurrent,
                1,
                crate::PLATFORM_CONNECTIONS - 1,
            ),
        ];
        for (key, value, min, max) in counts {
            if !(min..=max).contains(&value) {
                return Err(Invalid::new(
                    key,
                    format!("must be between {min} and {max}"),
                ));
            }
        }
        let durations = [
            (
                "renewal.gateway_interval",
                self.gateway_interval,
                seconds(1),
                seconds(3600),
                "1 second and 1 hour",
            ),
            (
                "renewal.global_interval",
                self.global_interval,
                Duration::from_millis(1),
                seconds(60),
                "1 ms and 60 seconds",
            ),
            (
                "renewal.max_consumer_lag",
                self.max_consumer_lag,
                seconds(1),
                seconds(3600),
                "1 second and 1 hour",
            ),
            (
                "renewal.refusal_window",
                self.refusal_window,
                seconds(60),
                seconds(86_400),
                "1 minute and 1 day",
            ),
            (
                "renewal.refusal_flush_interval",
                self.refusal_flush_interval,
                seconds(1),
                seconds(3600),
                "1 second and 1 hour",
            ),
        ];
        for (key, value, min, max, range) in durations {
            if !(min..=max).contains(&value) {
                return Err(Invalid::new(key, format!("must be between {range}")));
            }
        }
        Ok(())
    }
}

const fn default_dynsec_interval() -> Duration {
    Duration::from_secs(300)
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
                "must be another role than database.username: only the platform role may write \
                 platform-wide events",
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
        if !(MIN_WORKSTATION_VALIDITY..=MAX_WORKSTATION_VALIDITY)
            .contains(&self.workstation.validity)
        {
            return Err(Invalid::new(
                "workstation.validity",
                "must be between 1 hour and 398 days",
            ));
        }
        if !(MIN_BROKER_VALIDITY..=MAX_BROKER_VALIDITY).contains(&self.broker.validity) {
            return Err(Invalid::new(
                "broker.validity",
                "must be between 1 hour and 90 days",
            ));
        }
        if let Err(problem) = self.broker.names() {
            return Err(Invalid::new("broker.service_name", problem));
        }
        self.crl.validate()?;
        self.renewal.validate()?;
        self.validate_dynsec()
    }
}

impl Config {
    /// The settings of the gateway permission sync. Names keys, never values.
    fn validate_dynsec(&self) -> Result<(), Invalid> {
        if !(MIN_DYNSEC_INTERVAL..=MAX_DYNSEC_INTERVAL).contains(&self.dynsec.interval) {
            return Err(Invalid::new(
                "dynsec.interval",
                "must be between 1 second and 1 hour",
            ));
        }
        let Some(mqtt) = &self.mqtt else {
            return Ok(());
        };
        if let Err(problem) = mqtt.validate() {
            let key = match problem {
                mqtt_client::ConfigError::Host => "mqtt.host",
                mqtt_client::ConfigError::Port => "mqtt.port",
                mqtt_client::ConfigError::Username => "mqtt.username",
                mqtt_client::ConfigError::PasswordFile => "mqtt.password_file",
            };
            return Err(Invalid::new(key, problem.to_string()));
        }
        if mqtt.username != MQTT_USERNAME {
            return Err(Invalid::new(
                "mqtt.username",
                format!(
                    "must be {MQTT_USERNAME}: only that account holds the dynamic-security admin \
                     role"
                ),
            ));
        }
        if !mqtt.password_file.is_absolute() || mqtt.password_file.file_name().is_none() {
            return Err(Invalid::new(
                "mqtt.password_file",
                "must be an absolute path to a file",
            ));
        }
        Ok(())
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
                internal_identity_keys_file: PathBuf::from(
                    "/etc/trustiot/internal-identity/jwks.json",
                ),
                postgres_host: "postgres".to_owned(),
                postgres_port: 5432,
                postgres_database: "trustiot".to_owned(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: PathBuf::from("/ca.crt"),
            },
            database: Database {
                username: "certs_app".to_owned(),
                password: "secret-password".to_owned(),
                platform_username: "certs_platform".to_owned(),
                platform_password: "platform-secret".to_owned(),
                max_connections: 5,
            },
            ca: CaFiles {
                keys_file: PathBuf::from("/keys/issuing-ca-keys.json"),
                root_certificate_file: PathBuf::from("/ca/root.pem"),
                certificates_file: PathBuf::from("/ca/issuing.pem"),
                reload_interval: Duration::from_secs(30),
            },
            gateway: GatewayProfile::default(),
            workstation: WorkstationProfile::default(),
            broker: BrokerProfile::default(),
            crl: CrlSchedule::default(),
            renewal: RenewalLimits::default(),
            mqtt: None,
            dynsec: DynsecSchedule::default(),
        }
    }

    fn mqtt() -> mqtt_client::Config {
        mqtt_client::Config {
            host: "trustiot-mqtt-broker.trustiot.svc".to_owned(),
            port: 1883,
            username: MQTT_USERNAME.to_owned(),
            password_file: PathBuf::from("/etc/trustiot/mqtt-broker-account/certs"),
        }
    }

    #[test]
    fn the_broker_settings_are_optional_and_checked_when_set() {
        let mut with_broker = config();
        with_broker.mqtt = Some(mqtt());
        assert!(with_broker.validate().is_ok());
        assert_eq!(config().dynsec.interval, Duration::from_secs(300));
        let broken: [Breakage; 7] = [
            ("dynsec.interval", |c| c.dynsec.interval = Duration::ZERO),
            ("dynsec.interval", |c| {
                c.dynsec.interval = MAX_DYNSEC_INTERVAL + Duration::from_secs(1);
            }),
            ("mqtt.host", |c| {
                c.mqtt = Some(mqtt_client::Config {
                    host: String::new(),
                    ..mqtt()
                });
            }),
            ("mqtt.port", |c| {
                c.mqtt = Some(mqtt_client::Config { port: 0, ..mqtt() });
            }),
            ("mqtt.username", |c| {
                c.mqtt = Some(mqtt_client::Config {
                    username: "service:ingest".to_owned(),
                    ..mqtt()
                });
            }),
            ("mqtt.username", |c| {
                c.mqtt = Some(mqtt_client::Config {
                    username: "gw-1".to_owned(),
                    ..mqtt()
                });
            }),
            ("mqtt.password_file", |c| {
                c.mqtt = Some(mqtt_client::Config {
                    password_file: PathBuf::from("certs"),
                    ..mqtt()
                });
            }),
        ];
        for (key, breakage) in broken {
            let mut config = config();
            breakage(&mut config);
            let error = config.validate().expect_err(key);
            assert_eq!(error.key, key, "{}", error.message);
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

        let mut named = config();
        named.broker.service_name = Some("trustiot-mqtt-broker.trustiot.svc".to_owned());
        assert!(named.validate().is_ok());
        assert_eq!(
            named.broker.names().expect("names").as_slice(),
            [
                crate::issue::BROKER_SERVER_NAME,
                "trustiot-mqtt-broker.trustiot.svc"
            ]
        );
        assert_eq!(
            config().broker.listen_addr,
            "0.0.0.0:8082".parse().expect("an address")
        );

        let broken: [Breakage; 21] = [
            ("database.max_connections", |c| {
                c.database.max_connections = 1
            }),
            ("database.username", |c| {
                c.database.username = " ".to_owned()
            }),
            ("database.password", |c| c.database.password = String::new()),
            ("database.platform_username", |c| {
                c.database.platform_username = String::new()
            }),
            ("database.platform_username", |c| {
                c.database.platform_username = "certs_app".to_owned()
            }),
            ("database.platform_password", |c| {
                c.database.platform_password = String::new()
            }),
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
            ("workstation.validity", |c| {
                c.workstation.validity = Duration::from_secs(60);
            }),
            ("workstation.validity", |c| {
                c.workstation.validity = MAX_WORKSTATION_VALIDITY + Duration::from_secs(1);
            }),
            ("broker.validity", |c| {
                c.broker.validity = MIN_BROKER_VALIDITY - Duration::from_secs(1);
            }),
            ("broker.validity", |c| {
                c.broker.validity = MAX_BROKER_VALIDITY + Duration::from_secs(1);
            }),
            ("broker.service_name", |c| {
                c.broker.service_name = Some("*.trustiot.svc".to_owned());
            }),
            ("broker.service_name", |c| {
                c.broker.service_name = Some(String::new());
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
    fn debug_hides_the_passwords() {
        let printed = format!("{:?}", config());
        assert!(!printed.contains("secret-password"));
        assert!(!printed.contains("platform-secret"));
    }

    #[test]
    fn every_renewal_limit_is_checked() {
        let mut edges = config();
        edges.renewal = RenewalLimits {
            gateway_burst: 100,
            gateway_interval: Duration::from_secs(1),
            global_burst: 1,
            global_interval: Duration::from_millis(1),
            max_concurrent: crate::PLATFORM_CONNECTIONS - 1,
            max_consumer_lag: Duration::from_secs(3600),
            refusal_window: Duration::from_secs(60),
            refusal_flush_interval: Duration::from_secs(1),
        };
        assert!(edges.validate().is_ok());

        let broken: [Breakage; 12] = [
            ("renewal.gateway_burst", |c| c.renewal.gateway_burst = 0),
            ("renewal.gateway_burst", |c| c.renewal.gateway_burst = 101),
            ("renewal.global_burst", |c| c.renewal.global_burst = 10_001),
            ("renewal.max_concurrent", |c| c.renewal.max_concurrent = 0),
            // The platform role's pool keeps a connection for the registration and the consumer.
            ("renewal.max_concurrent", |c| {
                c.renewal.max_concurrent = crate::PLATFORM_CONNECTIONS;
            }),
            ("renewal.gateway_interval", |c| {
                c.renewal.gateway_interval = Duration::from_millis(999);
            }),
            ("renewal.global_interval", |c| {
                c.renewal.global_interval = Duration::ZERO;
            }),
            ("renewal.global_interval", |c| {
                c.renewal.global_interval = Duration::from_secs(61);
            }),
            ("renewal.max_consumer_lag", |c| {
                c.renewal.max_consumer_lag = Duration::ZERO;
            }),
            ("renewal.refusal_window", |c| {
                c.renewal.refusal_window = Duration::from_secs(59);
            }),
            ("renewal.refusal_window", |c| {
                c.renewal.refusal_window = Duration::from_secs(86_401);
            }),
            ("renewal.refusal_flush_interval", |c| {
                c.renewal.refusal_flush_interval = Duration::from_secs(3601);
            }),
        ];
        for (key, breakage) in broken {
            let mut config = config();
            breakage(&mut config);
            let error = config.validate().expect_err(key);
            assert_eq!(error.key, key, "{}", error.message);
        }
    }
}
