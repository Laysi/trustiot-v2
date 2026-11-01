//! The identity service's configuration (services/identity/README.md, "設定").

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::audience::IngressRoutes;
use crate::entitlements::{BackfillPlan, Plan};
use crate::exchange::OnBehalfCalls;
use crate::session::WARM_CONNECTIONS;

/// Most database connections one replica may open.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// `TRUSTIOT__DATABASE__MAX_CONNECTIONS` when it is not set.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// Shortest `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`, the schema minimum of the chart's
/// `internalIdentity.keyPropagationSeconds`.
pub const MIN_KEY_PROPAGATION_SECONDS: u64 = 120;

/// Longest `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`: a day.
pub const MAX_KEY_PROPAGATION_SECONDS: u64 = 86_400;

/// The service's settings: the platform settings the chart sets, the database role, and the
/// token exchange's port.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The service role's credentials and pool size.
    pub database: Database,
    /// Where the token exchange listens.
    #[serde(default)]
    pub exchange: ExchangeListen,
}

/// `exchange.*`: the token exchange's own port (Laysi/trustiot-v2#106). It is not the API port,
/// so the mesh can let the services that call on behalf of users reach the exchange and nothing
/// else of this service, and the ingress gateway reach everything but the exchange (the chart's
/// Service and policies `<release>-identity-exchange` for this port, deploy/chart/README.md).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeListen {
    /// `TRUSTIOT__EXCHANGE__LISTEN_ADDR`: default `0.0.0.0:8082`, the chart's port
    /// `token-exchange`. Startup fails when it is taken.
    pub listen_addr: SocketAddr,
}

impl Default for ExchangeListen {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], DEFAULT_EXCHANGE_PORT)),
        }
    }
}

/// The token exchange's default port; the chart names it `token-exchange`.
pub const DEFAULT_EXCHANGE_PORT: u16 = 8082;

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
    /// `TRUSTIOT_IDENTITY_BACKFILL_PLAN`: the plan tenants that existed before entitlements get
    /// as their first snapshot (Laysi/trustiot-v2#111, [`crate::entitlements::BackfillPlan`]):
    /// `starter`, `business`, `enterprise` or `enterprise_on_prem`. Default `enterprise`
    /// ([`crate::entitlements::DEFAULT_BACKFILL_PLAN`]); anything else stops the startup. New
    /// tenants start on Starter whatever it is. The chart does not set it.
    #[serde(default)]
    pub identity_backfill_plan: Option<String>,
    /// `TRUSTIOT_INGRESS_ROUTES`: the ingress gateway's cookie routes, for the audience.
    pub ingress_routes: IngressRoutes,
    /// `TRUSTIOT_ON_BEHALF_CALLS`: which service may call which on behalf of users, from the
    /// chart's `mesh.calls`; the token exchange follows it (Laysi/trustiot-v2#106). Default
    /// empty: nothing is exchanged.
    #[serde(default)]
    pub on_behalf_calls: OnBehalfCalls,
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
    /// `TRUSTIOT_AI_GATEWAY_ADMIN_URL`: the AI gateway's admin API, `http://<host>:<port>`, which
    /// the chart sets to the Service `<release>-ai-gateway-admin` that only this service may
    /// reach (Laysi/trustiot-v2#607). Without it, or with an address the service cannot use
    /// (logged at ERROR at startup), the model role routes answer 503
    /// ([`crate::ai_model_roles`]); everything else runs as usual.
    #[serde(default)]
    pub ai_gateway_admin_url: Option<String>,
    /// `TRUSTIOT_SMTP_HOST`: the chart's `smtp.host`, the relay login codes are sent through.
    /// Without it the service runs, but cannot send login codes ([`crate::mail`]).
    #[serde(default)]
    pub smtp_host: Option<String>,
    /// `TRUSTIOT_SMTP_PORT`: `smtp.port`.
    #[serde(default)]
    pub smtp_port: Option<u16>,
    /// `TRUSTIOT_SMTP_SECURITY`: `smtp.security`, `starttls` or `tls`.
    #[serde(default)]
    pub smtp_security: Option<SmtpSecurity>,
    /// `TRUSTIOT_SMTP_FROM`: `smtp.from`, the sender, `Name <address>` or a bare address.
    #[serde(default)]
    pub smtp_from: Option<String>,
    /// `TRUSTIOT_SMTP_AUTH_REQUIRED`: `true` when the relay needs an account, which the chart
    /// sets when `smtp.credentialsSecret` names a Secret. The chart's reference to that Secret is
    /// optional, so the pod starts without it (the ext_authz check must not depend on mail); this
    /// setting turns a missing account into an unusable relay setting, logged at startup, instead
    /// of mail sent without authentication. Default `false`.
    #[serde(default)]
    pub smtp_auth_required: bool,
    /// `TRUSTIOT_SMTP_USERNAME`, from the Secret `smtp.credentialsSecret` (only for services
    /// whose `credentials` list `smtp`). Absent when the relay needs no authentication.
    #[serde(default)]
    pub smtp_username: Option<String>,
    /// `TRUSTIOT_SMTP_PASSWORD`, from the same Secret.
    #[serde(default)]
    pub smtp_password: Option<SmtpPassword>,
    /// `TRUSTIOT_SMTP_CA_FILE`: PEM certificates to trust for the relay instead of the image's
    /// CA bundle ([`crate::mail::SYSTEM_CA_FILE`]), for a relay with a private CA. The chart
    /// does not set it yet.
    #[serde(default)]
    pub smtp_ca_file: Option<PathBuf>,
}

/// `smtp.security`: how the connection to the relay is encrypted. There is no plain SMTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Plain TCP, then `STARTTLS` before anything else (usually port 587). A relay that does not
    /// offer it is not used.
    Starttls,
    /// TLS from the first byte (usually port 465).
    Tls,
}

/// The SMTP password. `Debug` never prints it, and it is wiped on drop.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SmtpPassword(String);

impl SmtpPassword {
    /// The password, for the `AUTH` command only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SmtpPassword {
    fn from(password: String) -> Self {
        Self(password)
    }
}

impl Drop for SmtpPassword {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl fmt::Debug for SmtpPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SmtpPassword(<redacted>)")
    }
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
    /// without both, the event log is off and the current session API answers 503
    /// ([`Config::platform_role`]).
    #[serde(default)]
    pub platform_username: Option<String>,
    /// `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`, from a Secret.
    #[serde(default)]
    pub platform_password: Option<String>,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections of the session store's pool per
    /// replica, from [`WARM_CONNECTIONS`] (the ones always kept open) to
    /// [`MAX_DATABASE_CONNECTIONS`]. Default [`DEFAULT_MAX_CONNECTIONS`]. The other work has
    /// small pools of its own besides; [`crate::connections_per_replica`] adds them all up.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    DEFAULT_MAX_CONNECTIONS
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
        self.platform.backfill_plan()?;
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
        // The SMTP settings are checked at startup by `Mailer::from_platform` instead: a broken
        // relay setting must not stop the ext_authz check that every request with a cookie
        // needs, so the service runs without a relay and logs the error.
        Ok(())
    }
}

impl Platform {
    /// `TRUSTIOT_IDENTITY_BACKFILL_PLAN`, or the default plan when it is not set.
    ///
    /// # Errors
    ///
    /// When it is set to anything but a plan's name: a typo must not give existing tenants
    /// another plan than the operator meant. (An empty variable is an unset one, as for every
    /// setting of the kit.)
    pub fn backfill_plan(&self) -> Result<BackfillPlan, Invalid> {
        match self.identity_backfill_plan.as_deref() {
            None => Ok(BackfillPlan::new(None)),
            Some(name) => Plan::parse(name)
                .map(|plan| BackfillPlan::new(Some(plan)))
                .ok_or_else(|| {
                    Invalid::new(
                        "platform.identity_backfill_plan",
                        "must be starter, business, enterprise or enterprise_on_prem \
                         (TRUSTIOT_IDENTITY_BACKFILL_PLAN, the plan tenants that existed before \
                         entitlements get); unset, it is enterprise",
                    )
                }),
        }
    }

    /// Without `smtp_host` the other SMTP settings are not used. With it, the port, security and
    /// sender must be set and valid, the username and password come together, and they are set
    /// when `smtp_auth_required` is.
    ///
    /// # Errors
    ///
    /// The first setting that is missing or invalid, without its value.
    pub(crate) fn validate_smtp(&self) -> Result<(), Invalid> {
        if self.smtp_host.is_none() {
            return Ok(());
        }
        if !self.smtp_port.is_some_and(|port| port > 0) {
            return Err(Invalid::new(
                "platform.smtp_port",
                "must be a port from 1 to 65535 when platform.smtp_host is set",
            ));
        }
        if self.smtp_security.is_none() {
            return Err(Invalid::new(
                "platform.smtp_security",
                "must be starttls or tls when platform.smtp_host is set",
            ));
        }
        match self.smtp_from.as_deref().map(crate::mail::Mailbox::parse) {
            None => {
                return Err(Invalid::new(
                    "platform.smtp_from",
                    "must be set when platform.smtp_host is set",
                ));
            }
            Some(None) => {
                return Err(Invalid::new(
                    "platform.smtp_from",
                    "must be `Name <address>` or an address: one line, an ASCII address",
                ));
            }
            Some(Some(_)) => {}
        }
        if self.smtp_username.is_some() != self.smtp_password.is_some() {
            return Err(Invalid::new(
                "platform.smtp_password",
                "platform.smtp_username and platform.smtp_password must be set together",
            ));
        }
        if self.smtp_auth_required && self.smtp_username.is_none() {
            return Err(Invalid::new(
                "platform.smtp_username",
                "platform.smtp_username and platform.smtp_password must be set when \
                 platform.smtp_auth_required is true; the chart reads them from the Secret in \
                 smtp.credentialsSecret, which may be missing",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use service_kit::config::{ConfigError, DEFAULT_ENV_PREFIX, Settings, load_from};

    use super::*;

    /// The settings the service needs, plus `extra`.
    fn load(extra: &[(&str, &str)]) -> Result<Settings<Config>, ConfigError> {
        let routes = r#"[{"host": "app.saas.trustiot.test", "path": "/api/identity", "match": "PathPrefix", "service": "identity"}]"#;
        let required = [
            ("TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE", "/keys/public.json"),
            ("TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE", "/keys/signing.json"),
            ("TRUSTIOT_INGRESS_ROUTES", routes),
            ("TRUSTIOT_POSTGRES_HOST", "postgres.test"),
            ("TRUSTIOT_POSTGRES_PORT", "5432"),
            ("TRUSTIOT_POSTGRES_DATABASE", "trustiot"),
            ("TRUSTIOT_POSTGRES_SSLMODE", "verify-full"),
            ("TRUSTIOT_POSTGRES_CA_FILE", "/tls/ca.crt"),
            ("TRUSTIOT__DATABASE__USERNAME", "identity_app"),
            ("TRUSTIOT__DATABASE__PASSWORD", "not-a-secret"),
        ];
        load_from(
            DEFAULT_ENV_PREFIX,
            required
                .iter()
                .chain(extra)
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        )
    }

    #[test]
    fn the_backfill_plan_defaults_to_enterprise() {
        // An empty variable is an unset one, as for every setting of the kit.
        for extra in [&[][..], &[("TRUSTIOT_IDENTITY_BACKFILL_PLAN", "")][..]] {
            let settings = load(extra).expect("the settings");
            let plan = settings.app.platform.backfill_plan().expect("the default");
            assert_eq!(
                (plan.plan(), plan.source().as_str()),
                (Plan::Enterprise, "default"),
                "{extra:?}"
            );
        }
    }

    #[test]
    fn the_backfill_plan_takes_every_plan_by_name() {
        for plan in Plan::ALL {
            let settings = load(&[("TRUSTIOT_IDENTITY_BACKFILL_PLAN", plan.as_str())])
                .unwrap_or_else(|error| panic!("{plan:?}: {error}"));
            let set = settings.app.platform.backfill_plan().expect("a plan");
            assert_eq!((set.plan(), set.source().as_str()), (plan, "setting"));
        }
    }

    #[test]
    fn an_invalid_backfill_plan_stops_the_startup() {
        for invalid in [
            " enterprise",
            "Enterprise",
            "free",
            "unlimited",
            "enterprise-on-prem",
        ] {
            let error = load(&[("TRUSTIOT_IDENTITY_BACKFILL_PLAN", invalid)])
                .err()
                .unwrap_or_else(|| panic!("{invalid:?} was accepted"));
            let text = error.to_string();
            assert!(
                text.contains("platform.identity_backfill_plan")
                    && text.contains("TRUSTIOT_IDENTITY_BACKFILL_PLAN")
                    && text.contains("must be starter, business, enterprise or enterprise_on_prem"),
                "{invalid:?}: {text}"
            );
        }
    }
}
