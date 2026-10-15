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
/// under row-level security.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `TRUSTIOT__DATABASE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections per replica, from
    /// [`WARM_CONNECTIONS`] (the ones always kept open) to [`MAX_DATABASE_CONNECTIONS`].
    /// Default 10.
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
        if !(WARM_CONNECTIONS..=MAX_DATABASE_CONNECTIONS).contains(&self.database.max_connections) {
            return Err(Invalid::new(
                "database.max_connections",
                format!("must be between {WARM_CONNECTIONS} and {MAX_DATABASE_CONNECTIONS}"),
            ));
        }
        if self.database.username.trim().is_empty() {
            return Err(Invalid::new("database.username", "must not be blank"));
        }
        if self.database.password.is_empty() {
            return Err(Invalid::new("database.password", "must not be empty"));
        }
        Ok(())
    }
}
