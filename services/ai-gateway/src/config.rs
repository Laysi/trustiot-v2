//! The gateway's settings: the providers, the model roles with their fallback lists, the time
//! and size limits, and the database that keeps the platform administrator's changes to the
//! roles' models (README.md, 「設定」 and 「資料庫」). They come from `AI_GATEWAY__*` variables or
//! the TOML file named by `AI_GATEWAY_CONFIG_FILE`, through service-kit, plus the platform
//! settings `AI_GATEWAY_POSTGRES_*`, and are checked at startup. Error messages name keys, never
//! values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ai_clients::Timeouts;
use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};
use url::{Host, Url};

/// The prefix of the gateway's configuration variables. Not `TRUSTIOT`: the gateway carries no
/// product name (decision 28).
pub const ENV_PREFIX: &str = "AI_GATEWAY";

/// The default request body limit: Anthropic's limit for a Messages request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 32 << 20;
const MIN_BYTES: usize = 1 << 10;
const MAX_BYTES: usize = 256 << 20;
/// Provider and role names: lower-case letters, digits and `_`, starting with a letter, so a
/// name is also an environment-variable segment and a path segment.
const MAX_NAME_CHARS: usize = 32;
/// Models a role may list (the first and its fallbacks).
pub const MAX_MODELS: usize = 8;
const MAX_MODEL_ID_CHARS: usize = 256;
const MAX_KEY_ENV_CHARS: usize = 128;
const MAX_CONCURRENT: usize = 10_000;
/// The upper bound of every time limit but `stream_total`: no single wait of the gateway may be
/// longer than a request may take (`http.request_timeout`, at most an hour).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// The upper bound of `stream_total`. Bounds keep every `Instant + Duration` far from overflow.
pub const MAX_STREAM_TOTAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The gateway's own settings (service-kit adds `http`, `shutdown` and `telemetry`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment: where Postgres is.
    pub platform: Platform,
    /// The service role's credentials and the limits of the settings check.
    pub database: Database,
    /// The AI providers by name: protocol, endpoint and where the key is.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// The model roles by name: the models in fallback order and the limits of a call.
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
    /// The largest request body accepted, in bytes; a larger one is refused with 413.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Where the admin API listens.
    #[serde(default)]
    pub admin: Admin,
}

fn default_max_request_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BYTES
}

/// The admin API's port (Laysi/trustiot-v2#606; README.md, 「管理 API」 and 「驗證方式」). It is
/// not the API port, so the mesh can let the identity service, and nobody else, reach it: the
/// gateway authenticates no caller itself (decisions 27 and 28).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    /// `AI_GATEWAY__ADMIN__LISTEN_ADDR`: default `127.0.0.1:8082`, which no other pod can reach.
    /// The chart sets `0.0.0.0:8082` only together with the Service and the mesh policies that
    /// admit the identity service alone to this port (Laysi/trustiot-v2#607): until then the
    /// AI gateway's AuthorizationPolicy admits its callers on every port, and a wildcard default
    /// would open the admin API to them. Startup fails when it is taken.
    pub listen_addr: SocketAddr,
}

/// The admin API's port.
pub const DEFAULT_ADMIN_PORT: u16 = 8082;

impl Default for Admin {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], DEFAULT_ADMIN_PORT)),
        }
    }
}

/// The platform settings the gateway reads, under its own prefix: `AI_GATEWAY_POSTGRES_*` (the
/// chart sets them for this service only). No `deny_unknown_fields`: every platform setting under
/// the prefix reaches this table.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `AI_GATEWAY_POSTGRES_HOST`.
    pub postgres_host: String,
    /// `AI_GATEWAY_POSTGRES_PORT`.
    pub postgres_port: u16,
    /// `AI_GATEWAY_POSTGRES_DATABASE`.
    pub postgres_database: String,
    /// `AI_GATEWAY_POSTGRES_SSLMODE`: only `verify-full` is accepted.
    pub postgres_sslmode: SslMode,
    /// `AI_GATEWAY_POSTGRES_CA_FILE`: the CA of the server certificate, the only trust anchor.
    pub postgres_ca_file: PathBuf,
}

/// Connections of the settings pool per replica, by default: every forwarded request runs one
/// short query on it, and a reload one short read-only transaction.
pub const DEFAULT_DATABASE_CONNECTIONS: u32 = 4;
/// The range of `database.max_connections`, which the chart's connection budget mirrors
/// (deploy/chart/templates/_postgres.tpl, `trustiot.postgres.connectionBudgets`).
pub const MIN_DATABASE_CONNECTIONS: u32 = 2;
pub const MAX_DATABASE_CONNECTIONS: u32 = 32;
/// How long a forwarded request waits for the settings check by default.
pub const DEFAULT_DATABASE_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_DATABASE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_DATABASE_TIMEOUT: Duration = Duration::from_secs(60);

/// `database.*`: the service role (`ai_gateway_app`) and the limits of the settings check.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `AI_GATEWAY__DATABASE__USERNAME`.
    pub username: String,
    /// `AI_GATEWAY__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `AI_GATEWAY__DATABASE__MAX_CONNECTIONS`: connections of the pool per replica, 1 to 32.
    #[serde(default = "default_database_connections")]
    pub max_connections: u32,
    /// `AI_GATEWAY__DATABASE__TIMEOUT`: the longest a forwarded request waits for the settings
    /// check (a pooled connection, the generation query and, when the settings changed, their
    /// reload), within the call's time budget; 100 ms to 60 s. Past it the call is refused with
    /// 503.
    #[serde(
        default = "default_database_timeout",
        deserialize_with = "service_kit::config::duration"
    )]
    pub timeout: Duration,
}

fn default_database_connections() -> u32 {
    DEFAULT_DATABASE_CONNECTIONS
}

fn default_database_timeout() -> Duration {
    DEFAULT_DATABASE_TIMEOUT
}

/// Never prints the password.
impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("username", &self.username)
            .field("max_connections", &self.max_connections)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Where and as whom the gateway connects to Postgres.
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

/// A provider's native protocol (decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Protocol {
    /// Anthropic Messages, `POST /v1/messages`, with streaming.
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    /// OpenAI Decisions, `POST /v1/decisions`, without streaming.
    #[serde(rename = "openai-decisions")]
    OpenaiDecisions,
}

impl Protocol {
    /// The name in the configuration and in logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenaiDecisions => "openai-decisions",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Self::AnthropicMessages => ai_clients::anthropic::DEFAULT_BASE_URL,
            Self::OpenaiDecisions => ai_clients::decisions::DEFAULT_BASE_URL,
        }
    }
}

/// One AI provider account: a protocol, an endpoint and an API key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub protocol: Protocol,
    /// The API's origin, such as a Cloudflare AI Gateway URL in front of the provider; the
    /// provider's own API when unset.
    #[serde(default)]
    pub base_url: Option<String>,
    /// The environment variable holding the API key, such as `ANTHROPIC_API_KEY`. Read once at
    /// startup; the key itself is never part of the configuration.
    pub key_env: String,
}

impl Provider {
    /// The configured endpoint, or the provider's own API.
    pub fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or_else(|| self.protocol.default_base_url())
    }
}

/// A model role: its models in fallback order and the limits of each call to one of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// `<provider>/<model>`, first choice first; every model of a role uses the same protocol.
    pub models: Vec<String>,
    /// Calls in flight per model of this role; more wait up to `queue_wait`.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// How long a call waits for a free slot before it counts as overloaded and falls back.
    #[serde(default, deserialize_with = "service_kit::config::duration")]
    pub queue_wait: Duration,
    #[serde(default)]
    pub timeouts: RoleTimeouts,
    /// A non-streaming response, or a whole stream, larger than this fails.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    /// A single stream event larger than this fails the stream.
    #[serde(default = "default_max_event_bytes")]
    pub max_event_bytes: usize,
}

fn default_max_concurrent() -> usize {
    32
}

fn default_max_response_bytes() -> usize {
    32 << 20
}

fn default_max_event_bytes() -> usize {
    4 << 20
}

/// The time limits of one call ([`ai_clients::TimeoutKind`] says what each one bounds).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleTimeouts {
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub connect: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub first_token: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub stream_idle: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub stream_total: Duration,
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub response: Duration,
}

impl Default for RoleTimeouts {
    fn default() -> Self {
        let Timeouts {
            connect,
            first_token,
            stream_idle,
            stream_total,
            response,
        } = Timeouts::default();
        Self {
            connect,
            first_token,
            stream_idle,
            stream_total,
            response,
        }
    }
}

impl RoleTimeouts {
    /// The limits as the provider clients take them.
    pub fn to_client(&self) -> Timeouts {
        Timeouts {
            connect: self.connect,
            first_token: self.first_token,
            stream_idle: self.stream_idle,
            stream_total: self.stream_total,
            response: self.response,
        }
    }

    /// The longest a call takes to reach its response (non-streaming) or its first output
    /// (streaming), once it has a slot.
    pub fn until_answer(&self) -> Duration {
        self.response.max(self.first_token)
    }
}

/// `<provider>/<model>` split at the first `/`.
pub fn split_model(reference: &str) -> Option<(&str, &str)> {
    reference.split_once('/')
}

/// Why a model list cannot be a role's. The same rules hold for the configured lists and for the
/// lists stored in the database (src/store.rs). `index` counts from 0; the text never repeats a
/// model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelsError {
    #[error("a role lists between 1 and {MAX_MODELS} models")]
    Count,
    #[error("model {index} must be <provider>/<model>")]
    Format { index: usize },
    #[error("model {index} names a provider that is not configured under `providers`")]
    UnknownProvider { index: usize },
    #[error(
        "model {index}: the model ID must be 1 to {MAX_MODEL_ID_CHARS} printable ASCII characters \
         without spaces or commas"
    )]
    ModelId { index: usize },
    #[error("model {index} is listed twice")]
    Duplicate { index: usize },
    #[error("every model of a role must use the same protocol")]
    MixedProtocols,
    #[error("the models use {found}, but the role speaks {expected}")]
    WrongProtocol {
        expected: &'static str,
        found: &'static str,
    },
}

/// The protocol of each configured provider, which is what a model list is checked against.
pub type ProviderProtocols = BTreeMap<String, Protocol>;

/// Checks `models` as a role's model list (README.md, 「設定」): 1 to [`MAX_MODELS`] distinct
/// `<provider>/<model>`, each provider configured, each model ID 1 to 256 printable ASCII
/// characters without spaces or commas (the variable is a comma-separated list), all of one
/// protocol, which is returned.
pub fn check_models(
    models: &[String],
    providers: &ProviderProtocols,
) -> Result<Protocol, ModelsError> {
    if models.is_empty() || models.len() > MAX_MODELS {
        return Err(ModelsError::Count);
    }
    let mut protocol = None;
    let mut seen = BTreeSet::new();
    for (index, reference) in models.iter().enumerate() {
        let Some((provider, model)) = split_model(reference) else {
            return Err(ModelsError::Format { index });
        };
        let Some(&found) = providers.get(provider) else {
            return Err(ModelsError::UnknownProvider { index });
        };
        if model.is_empty()
            || model.chars().count() > MAX_MODEL_ID_CHARS
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b',')
        {
            return Err(ModelsError::ModelId { index });
        }
        if !seen.insert(reference.as_str()) {
            return Err(ModelsError::Duplicate { index });
        }
        match protocol {
            Some(first) if first != found => return Err(ModelsError::MixedProtocols),
            _ => protocol = Some(found),
        }
    }
    protocol.ok_or(ModelsError::Count)
}

/// Whether `name` is a valid provider or role name: 1 to 32 lower-case letters, digits or `_`,
/// starting with a letter.
pub fn is_name(name: &str) -> bool {
    name.len() <= MAX_NAME_CHARS
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// What a model list is checked against: the configured providers' protocols and the configured
/// roles' protocols. A role speaks the protocol of its configured models; only the chart (and the
/// callers' code) can change it, never a stored list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRules {
    pub providers: ProviderProtocols,
    pub roles: BTreeMap<String, Protocol>,
}

impl RoleRules {
    /// The rules of a configuration. A role whose configured models do not check is left out;
    /// validation refuses such a configuration before the gateway starts.
    pub fn of(config: &Config) -> Self {
        let providers: ProviderProtocols = config
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.protocol))
            .collect();
        let roles = config
            .roles
            .iter()
            .filter_map(|(name, role)| {
                check_models(&role.models, &providers)
                    .ok()
                    .map(|protocol| (name.clone(), protocol))
            })
            .collect();
        Self { providers, roles }
    }

    /// Checks `models` as the new model list of `role`: the configuration's rules, and the
    /// role's own protocol. `None` when no such role is configured.
    pub fn check(&self, role: &str, models: &[String]) -> Option<Result<(), ModelsError>> {
        let expected = *self.roles.get(role)?;
        Some(check_models(models, &self.providers).and_then(|found| {
            if found == expected {
                Ok(())
            } else {
                Err(ModelsError::WrongProtocol {
                    expected: expected.name(),
                    found: found.name(),
                })
            }
        }))
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        if self.roles.is_empty() {
            return Err(Invalid::new("roles", "at least one model role is required"));
        }
        for (name, provider) in &self.providers {
            validate_provider(name, provider)?;
        }
        let protocols: ProviderProtocols = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.protocol))
            .collect();
        for (name, role) in &self.roles {
            validate_role(name, role, &protocols)?;
        }
        check_bytes("max_request_bytes", self.max_request_bytes, MAX_BYTES)?;
        validate_database(&self.database)
    }
}

fn validate_database(database: &Database) -> Result<(), Invalid> {
    if database.username.is_empty() {
        return Err(Invalid::new("database.username", "must not be empty"));
    }
    if database.password.is_empty() {
        return Err(Invalid::new("database.password", "must not be empty"));
    }
    if !(MIN_DATABASE_CONNECTIONS..=MAX_DATABASE_CONNECTIONS).contains(&database.max_connections) {
        return Err(Invalid::new(
            "database.max_connections",
            format!("must be between {MIN_DATABASE_CONNECTIONS} and {MAX_DATABASE_CONNECTIONS}"),
        ));
    }
    if database.timeout < MIN_DATABASE_TIMEOUT || database.timeout > MAX_DATABASE_TIMEOUT {
        return Err(Invalid::new(
            "database.timeout",
            "must be between 100ms and 60s",
        ));
    }
    Ok(())
}

fn validate_provider(name: &str, provider: &Provider) -> Result<(), Invalid> {
    let key = format!("providers.{name}");
    check_name(&key, name)?;
    if let Some(base_url) = &provider.base_url {
        check_base_url(&format!("{key}.base_url"), base_url)?;
    }
    check_key_env(&format!("{key}.key_env"), &provider.key_env)
}

fn validate_role(name: &str, role: &Role, providers: &ProviderProtocols) -> Result<(), Invalid> {
    let key = format!("roles.{name}");
    check_name(&key, name)?;
    let models_key = format!("{key}.models");
    match check_models(&role.models, providers) {
        Ok(_) => {}
        Err(
            error @ (ModelsError::Count
            | ModelsError::MixedProtocols
            | ModelsError::WrongProtocol { .. }),
        ) => return Err(Invalid::new(models_key, error.to_string())),
        Err(
            error @ (ModelsError::Format { index }
            | ModelsError::UnknownProvider { index }
            | ModelsError::ModelId { index }
            | ModelsError::Duplicate { index }),
        ) => {
            return Err(Invalid::new(
                format!("{models_key}[{index}]"),
                error.to_string(),
            ));
        }
    }
    if role.max_concurrent == 0 || role.max_concurrent > MAX_CONCURRENT {
        return Err(Invalid::new(
            format!("{key}.max_concurrent"),
            format!("must be between 1 and {MAX_CONCURRENT}"),
        ));
    }
    if role.queue_wait > MAX_TIMEOUT {
        return Err(Invalid::new(
            format!("{key}.queue_wait"),
            "must be at most 1 hour",
        ));
    }
    let timeouts = &role.timeouts;
    for (field, value, max, max_text) in [
        ("connect", timeouts.connect, MAX_TIMEOUT, "1 hour"),
        ("first_token", timeouts.first_token, MAX_TIMEOUT, "1 hour"),
        ("stream_idle", timeouts.stream_idle, MAX_TIMEOUT, "1 hour"),
        (
            "stream_total",
            timeouts.stream_total,
            MAX_STREAM_TOTAL,
            "24 hours",
        ),
        ("response", timeouts.response, MAX_TIMEOUT, "1 hour"),
    ] {
        if value.is_zero() || value > max {
            return Err(Invalid::new(
                format!("{key}.timeouts.{field}"),
                format!("must be more than 0 and at most {max_text}"),
            ));
        }
    }
    check_bytes(
        &format!("{key}.max_response_bytes"),
        role.max_response_bytes,
        MAX_BYTES,
    )?;
    check_bytes(
        &format!("{key}.max_event_bytes"),
        role.max_event_bytes,
        role.max_response_bytes,
    )
}

fn check_name(key: &str, name: &str) -> Result<(), Invalid> {
    if is_name(name) {
        Ok(())
    } else {
        Err(Invalid::new(
            key,
            format!(
                "the name must be 1 to {MAX_NAME_CHARS} lower-case letters, digits or `_`, \
                 starting with a letter"
            ),
        ))
    }
}

fn check_bytes(key: &str, value: usize, max: usize) -> Result<(), Invalid> {
    if (MIN_BYTES..=max).contains(&value) {
        Ok(())
    } else {
        Err(Invalid::new(
            key,
            format!("must be between {MIN_BYTES} and {max} bytes"),
        ))
    }
}

/// `https://` anywhere, `http://` only on loopback (a proxy on the same host, tests); no
/// credentials, query or fragment. The same rule as crates/ai-clients, checked here so that the
/// message names the key and never shows the URL.
fn check_base_url(key: &str, text: &str) -> Result<(), Invalid> {
    let invalid = |why: &str| Err(Invalid::new(key, why));
    let Ok(url) = Url::parse(text) else {
        return invalid("must be an absolute URL");
    };
    let loopback = match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => return invalid("must name a host"),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return invalid("must be https (plain http only on loopback)"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return invalid("must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return invalid("must not have a query or fragment");
    }
    Ok(())
}

/// An environment-variable name outside the gateway's own `AI_GATEWAY_` prefix, which
/// service-kit reads as configuration.
fn check_key_env(key: &str, name: &str) -> Result<(), Invalid> {
    let valid = name.len() <= MAX_KEY_ENV_CHARS
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if !valid {
        return Err(Invalid::new(
            key,
            format!(
                "must be an environment-variable name of 1 to {MAX_KEY_ENV_CHARS} upper-case \
                 letters, digits or `_`"
            ),
        ));
    }
    if name.starts_with(&format!("{ENV_PREFIX}_")) {
        return Err(Invalid::new(
            key,
            format!("must not start with {ENV_PREFIX}_, which holds the gateway's configuration"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use service_kit::config::{ConfigError, load_from};

    use super::*;

    /// The platform and database settings the chart gives the gateway; nothing connects here.
    const DATABASE: [(&str, &str); 7] = [
        ("AI_GATEWAY_POSTGRES_HOST", "postgres.example.test"),
        ("AI_GATEWAY_POSTGRES_PORT", "5432"),
        ("AI_GATEWAY_POSTGRES_DATABASE", "trustiot"),
        ("AI_GATEWAY_POSTGRES_SSLMODE", "verify-full"),
        (
            "AI_GATEWAY_POSTGRES_CA_FILE",
            "/etc/trustiot/postgres-ca/ca.crt",
        ),
        ("AI_GATEWAY__DATABASE__USERNAME", "ai_gateway_app"),
        ("AI_GATEWAY__DATABASE__PASSWORD", "not-a-real-password"),
    ];

    fn database_vars() -> impl Iterator<Item = (OsString, OsString)> {
        DATABASE
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    }

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        load_from::<Config, _>(
            ENV_PREFIX,
            database_vars().chain(
                vars.iter()
                    .map(|(name, value)| (OsString::from(name), OsString::from(value))),
            ),
        )
        .map(|settings| settings.app)
    }

    const PROVIDER: [(&str, &str); 2] = [
        (
            "AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL",
            "anthropic-messages",
        ),
        (
            "AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV",
            "ANTHROPIC_API_KEY",
        ),
    ];

    fn with_provider(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut vars = PROVIDER.to_vec();
        vars.extend_from_slice(extra);
        vars
    }

    fn invalid_key(error: ConfigError) -> String {
        match error {
            ConfigError::Invalid { key, .. } => key,
            other => panic!("expected an invalid key, got {other}"),
        }
    }

    #[test]
    fn roles_and_providers_load_from_environment_variables() {
        let config = load(&with_provider(&[
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5, anthropic/claude-sonnet-5-5",
            ),
            ("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE", "10m"),
            ("AI_GATEWAY__ROLES__MAIN__QUEUE_WAIT", "2s"),
        ]))
        .unwrap();
        let main = &config.roles["main"];
        assert_eq!(
            main.models,
            ["anthropic/claude-opus-5-5", "anthropic/claude-sonnet-5-5"]
        );
        assert_eq!(main.timeouts.response, Duration::from_secs(600));
        assert_eq!(main.timeouts.first_token, Timeouts::default().first_token);
        assert_eq!(main.queue_wait, Duration::from_secs(2));
        assert_eq!(main.max_concurrent, 32);
        assert_eq!(config.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
        let anthropic = &config.providers["anthropic"];
        assert_eq!(anthropic.protocol, Protocol::AnthropicMessages);
        assert_eq!(anthropic.base_url(), "https://api.anthropic.com");
        assert_eq!(anthropic.key_env, "ANTHROPIC_API_KEY");
        // The admin port listens on loopback until the chart opens it (Laysi/trustiot-v2#607).
        assert_eq!(
            config.admin.listen_addr,
            "127.0.0.1:8082".parse::<SocketAddr>().unwrap()
        );
        let config = load(&with_provider(&[
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5",
            ),
            ("AI_GATEWAY__ADMIN__LISTEN_ADDR", "0.0.0.0:9082"),
        ]))
        .unwrap();
        assert_eq!(
            config.admin.listen_addr,
            "0.0.0.0:9082".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn a_role_must_name_configured_providers_of_one_protocol() {
        let error = load(&with_provider(&[(
            "AI_GATEWAY__ROLES__MAIN__MODELS",
            "anthropic/claude-opus-5-5,openai/gpt-6-luna",
        )]))
        .unwrap_err();
        assert_eq!(invalid_key(error), "roles.main.models[1]");

        let error = load(&with_provider(&[
            (
                "AI_GATEWAY__PROVIDERS__OPENAI__PROTOCOL",
                "openai-decisions",
            ),
            ("AI_GATEWAY__PROVIDERS__OPENAI__KEY_ENV", "OPENAI_API_KEY"),
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5,openai/gpt-6-luna",
            ),
        ]))
        .unwrap_err();
        assert_eq!(invalid_key(error), "roles.main.models");

        for models in [
            "claude-opus-5-5",
            "anthropic/",
            "anthropic/a b",
            "anthropic/x,anthropic/x",
        ] {
            let error = load(&with_provider(&[(
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                models,
            )]))
            .unwrap_err();
            assert!(
                invalid_key(error).starts_with("roles.main.models"),
                "{models} should be refused"
            );
        }
        let error = load(&PROVIDER).unwrap_err();
        assert_eq!(invalid_key(error), "roles");
    }

    #[test]
    fn time_limits_are_bounded() {
        for (variable, key) in [
            (
                "AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE",
                "roles.main.timeouts.response",
            ),
            (
                "AI_GATEWAY__ROLES__MAIN__TIMEOUTS__CONNECT",
                "roles.main.timeouts.connect",
            ),
            (
                "AI_GATEWAY__ROLES__MAIN__QUEUE_WAIT",
                "roles.main.queue_wait",
            ),
        ] {
            for value in ["500000000000y", "2h"] {
                let error = load(&with_provider(&[
                    (
                        "AI_GATEWAY__ROLES__MAIN__MODELS",
                        "anthropic/claude-opus-5-5",
                    ),
                    (variable, value),
                ]))
                .unwrap_err();
                assert_eq!(invalid_key(error), key, "{variable}={value}");
            }
        }
        let error = load(&with_provider(&[
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5",
            ),
            ("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__FIRST_TOKEN", "0s"),
        ]))
        .unwrap_err();
        assert_eq!(invalid_key(error), "roles.main.timeouts.first_token");
    }

    #[test]
    fn endpoint_and_key_settings_are_checked_without_echoing_them() {
        let secret_url = ["https://user", "hunter2@proxy.example"].join(":");
        let cases: [(&str, String, &str); 5] = [
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
                secret_url.clone(),
                "providers.anthropic.base_url",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
                "http://api.anthropic.com".into(),
                "providers.anthropic.base_url",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
                "https://api.anthropic.com/?k=v".into(),
                "providers.anthropic.base_url",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV",
                "anthropic_api_key".into(),
                "providers.anthropic.key_env",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV",
                "AI_GATEWAY_KEY".into(),
                "providers.anthropic.key_env",
            ),
        ];
        for (variable, value, key) in cases {
            let mut vars: Vec<(OsString, OsString)> = PROVIDER
                .iter()
                .filter(|(name, _)| *name != variable)
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .chain(database_vars())
                .collect();
            vars.push((
                OsString::from("AI_GATEWAY__ROLES__MAIN__MODELS"),
                OsString::from("anthropic/claude-opus-5-5"),
            ));
            vars.push((OsString::from(variable), OsString::from(&value)));
            let error = load_from::<Config, _>(ENV_PREFIX, vars).unwrap_err();
            let text = error.to_string();
            assert!(
                !text.contains(&value),
                "the message shows the value: {text}"
            );
            assert!(
                !text.contains("hunter2"),
                "the message shows the password: {text}"
            );
            assert_eq!(invalid_key(error), key, "{variable}");
        }
        let loopback = load(&with_provider(&[
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
                "http://127.0.0.1:8080",
            ),
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5",
            ),
        ]))
        .unwrap();
        assert_eq!(
            loopback.providers["anthropic"].base_url(),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn names_must_be_lower_case_words() {
        let error = load(&[
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL",
                "anthropic-messages",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV",
                "ANTHROPIC_API_KEY",
            ),
            (
                "AI_GATEWAY__ROLES__9MAIN__MODELS",
                "anthropic/claude-opus-5-5",
            ),
        ])
        .unwrap_err();
        assert_eq!(invalid_key(error), "roles.9main");
    }
}
