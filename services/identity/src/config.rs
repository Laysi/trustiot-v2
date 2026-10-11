//! The identity service's configuration (services/identity/README.md, "設定").

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::identity::IdentityKeys;
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::audience::IngressRoutes;

/// Most database connections one replica may open.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

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
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS`: the public keys every service verifies tokens with. The
    /// signing key's public key must be one of them.
    pub internal_identity_keys: IdentityKeys,
    /// `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE`: the PKCS#8 PEM private key, mounted from a Secret.
    pub identity_signing_key_file: PathBuf,
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
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: pool size per replica, 1 to
    /// [`MAX_DATABASE_CONNECTIONS`]. Default 10.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    10
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
        if !(1..=MAX_DATABASE_CONNECTIONS).contains(&self.database.max_connections) {
            return Err(Invalid::new(
                "database.max_connections",
                format!("must be between 1 and {MAX_DATABASE_CONNECTIONS}"),
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
