//! The identity service's configuration (services/identity/README.md, "設定").

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::audience::IngressRoutes;
use crate::session::WARM_CONNECTIONS;

/// Most database connections one replica may open.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// Shortest `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`, the schema minimum of the chart's
/// `internalIdentity.keyPropagationSeconds`.
pub const MIN_KEY_PROPAGATION_SECONDS: u64 = 120;

/// Longest `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`: a day.
pub const MAX_KEY_PROPAGATION_SECONDS: u64 = 86_400;

/// The service's settings: the platform settings the chart sets and the database role.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's credentials and pool size.
    pub database: Database,
}

/// The platform settings the service reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`: the public keys every service verifies tokens
    /// with (the ConfigMap `<release>-internal-identity-keys`). Only a key listed there signs.
    pub internal_identity_keys_file: PathBuf,
    /// `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE`: the signing keys, mounted from the Secret
    /// `<release>-identity-signing-keys` that the init container fills.
    pub identity_signing_keys_file: PathBuf,
    /// `TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION`: the chart's
    /// `internalIdentity.revokeBeforeGeneration`; keys of a lower generation never sign. Default
    /// 0.
    #[serde(default)]
    pub identity_revoke_before_generation: u64,
    /// `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`: the chart's
    /// `internalIdentity.keyPropagationSeconds`; how long the signing keys and the public keys
    /// may differ before the service logs an error. Default 600.
    #[serde(default = "default_propagation_seconds")]
    pub identity_key_propagation_seconds: u64,
    /// `TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS`: the chart's `internalIdentity.maxKeyAgeSeconds`
    /// (Laysi/trustiot-v2#429); the service logs an error when its signing key is two days past
    /// its rotation by age. Default 0: rotation by age is off and nothing is checked.
    #[serde(default)]
    pub identity_max_key_age_seconds: u64,
    /// `TRUSTIOT_INGRESS_ROUTES`: the ingress gateway's cookie routes, for the audience.
    pub ingress_routes: IngressRoutes,
    /// `TRUSTIOT_POSTGRES_HOST`.
    pub postgres_host: String,
    /// `TRUSTIOT_POSTGRES_PORT`.
    pub postgres_port: u16,
    /// `TRUSTIOT_POSTGRES_DATABASE`.
    pub postgres_database: String,
    /// `TRUSTIOT_POSTGRES_SSLMODE`: only `verify-full` is accepted.
    pub postgres_sslmode: SslMode,
    /// `TRUSTIOT_POSTGRES_CA_FILE`: the CA of the server certificate, the only trust anchor
    /// (crates/service-kit, "連線"); required.
    pub postgres_ca_file: PathBuf,
}

/// `database.*`: the service role (`identity_app`), which owns nothing and reads the sessions
/// under row-level security, and the platform role (`identity_platform`), which reads every
/// producer's events across tenants for the event log and serves the back office's view of the
/// log (decisions 29 and 34; [`crate::event_log`]).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `TRUSTIOT__DATABASE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `TRUSTIOT__DATABASE__PLATFORM_USERNAME`. The chart always sets it with the password;
    /// without both, the event log is off ([`Config::platform_role`]).
    #[serde(default)]
    pub platform_username: Option<String>,
    /// `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`, from a Secret.
    #[serde(default)]
    pub platform_password: Option<String>,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections of the session store's pool per
    /// replica, from [`WARM_CONNECTIONS`] (the ones always kept open) to
    /// [`MAX_DATABASE_CONNECTIONS`]. Default 10. The tenant API and the event log have small
    /// pools of their own besides ([`crate::tenants::POOL_CONNECTIONS`],
    /// [`crate::event_log::SERVICE_CONNECTIONS`], [`crate::event_log::platform_connections`]).
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    10
}

const fn default_propagation_seconds() -> u64 {
    600
}

/// Never prints the password.
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
    /// Where and how to connect to Postgres.
    #[must_use]
    pub fn postgres(&self) -> PostgresConfig {
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

    /// Where and how the platform role connects; `None` without its credentials, and then the
    /// event log is off: nothing is recorded (the producers' outboxes keep the events, and the
    /// log catches up once it runs) and its API answers 503. Every deployment of the chart sets
    /// them; tests of the ext_authz check alone leave them out.
    #[must_use]
    pub fn platform_role(&self) -> Option<PostgresConfig> {
        let database = &self.database;
        match (&database.platform_username, &database.platform_password) {
            (Some(username), Some(password)) => Some(PostgresConfig {
                username: username.clone(),
                password: password.clone(),
                ..self.postgres()
            }),
            _ => None,
        }
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        if self.platform.identity_revoke_before_generation > identity_keys::MAX_GENERATION {
            return Err(Invalid::new(
                "platform.identity_revoke_before_generation",
                format!("must be at most {}", identity_keys::MAX_GENERATION),
            ));
        }
        if !(MIN_KEY_PROPAGATION_SECONDS..=MAX_KEY_PROPAGATION_SECONDS)
            .contains(&self.platform.identity_key_propagation_seconds)
        {
            return Err(Invalid::new(
                "platform.identity_key_propagation_seconds",
                format!(
                    "must be between {MIN_KEY_PROPAGATION_SECONDS} and \
                     {MAX_KEY_PROPAGATION_SECONDS} seconds"
                ),
            ));
        }
        if !i64::try_from(self.platform.identity_max_key_age_seconds)
            .is_ok_and(|age| age <= identity_keys::MAX_KEY_AGE_SECS)
        {
            return Err(Invalid::new(
                "platform.identity_max_key_age_seconds",
                format!(
                    "must be at most {} seconds (365 days); 0 turns the check off",
                    identity_keys::MAX_KEY_AGE_SECS
                ),
            ));
        }
        if !(WARM_CONNECTIONS..=MAX_DATABASE_CONNECTIONS).contains(&self.database.max_connections) {
            return Err(Invalid::new(
                "database.max_connections",
                format!("must be between {WARM_CONNECTIONS} and {MAX_DATABASE_CONNECTIONS}"),
            ));
        }
        let database = &self.database;
        if database.username.trim().is_empty() {
            return Err(Invalid::new("database.username", "must not be blank"));
        }
        if database.password.is_empty() {
            return Err(Invalid::new("database.password", "must not be empty"));
        }
        match (&database.platform_username, &database.platform_password) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(Invalid::new(
                    "database.platform_password",
                    "must be set with database.platform_username",
                ));
            }
            (None, Some(_)) => {
                return Err(Invalid::new(
                    "database.platform_username",
                    "must be set with database.platform_password",
                ));
            }
            (Some(username), Some(password)) => {
                if username.trim().is_empty() {
                    return Err(Invalid::new(
                        "database.platform_username",
                        "must not be blank",
                    ));
                }
                if *username == database.username {
                    return Err(Invalid::new(
                        "database.platform_username",
                        "must be another role than database.username: only the platform role \
                         may read across tenants",
                    ));
                }
                if password.is_empty() {
                    return Err(Invalid::new(
                        "database.platform_password",
                        "must not be empty",
                    ));
                }
            }
        }
        Ok(())
    }
}
