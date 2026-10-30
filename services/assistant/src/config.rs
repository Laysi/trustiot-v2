//! The assistant's settings (README.md, 「設定」): where the AI gateway is, the model call, the
//! token budget of a question, the limits of tool calls and the service role's database login.
//! They come from `TRUSTIOT__*` variables (the platform settings from `TRUSTIOT_*`) or the TOML
//! file named by `TRUSTIOT_CONFIG_FILE`, through service-kit, and are checked at startup. Error
//! messages name keys, never values.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};
use service_kit::postgres::{PostgresConfig, SslMode};

use crate::model::{CacheTtl, Effort};

/// The longest a single model or tool call may be allowed to take.
pub const MAX_CALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// The most output tokens Claude Opus 5.5 produces in one response.
pub const MAX_OUTPUT_TOKENS: u32 = 128_000;
/// The largest token budget a question may have.
pub const MAX_BUDGET_TOKENS: u64 = 100_000_000;
const MIN_RESPONSE_BYTES: usize = 1 << 10;
const MAX_RESPONSE_BYTES: usize = 256 << 20;
/// A model role name, as the AI gateway accepts it (services/ai-gateway/README.md, 「設定」).
const MAX_ROLE_CHARS: usize = 32;
/// The most tool result images one result may carry.
pub const MAX_IMAGES: usize = 20;
/// The highest setting for how many tool calls of one model response run.
pub const MAX_CALLS_PER_STEP: usize = 32;
/// The API's limit on one image: 5 MB (<https://platform.claude.com/docs/en/build-with-claude/vision>).
pub const MAX_IMAGE_BYTES: usize = 5 << 20;

/// Fewest connections of the service role's pool: the two readiness checks (a ping and the
/// migration check) may run at the same time.
pub const MIN_DATABASE_CONNECTIONS: u32 = 2;

/// Most connections of the service role's pool per replica.
pub const MAX_DATABASE_CONNECTIONS: u32 = 100;

/// The assistant's own settings (service-kit adds `http`, `shutdown` and `telemetry`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment: where Postgres is.
    pub platform: Platform,
    /// The service role's login and the pool size.
    pub database: Database,
    pub ai_gateway: AiGateway,
    #[serde(default)]
    pub model: Model,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub tools: Tools,
    /// The identity service's token exchange, where a question's run delegation starts and is
    /// exchanged for a token before every tool call (Laysi/trustiot-v2#551).
    #[serde(default)]
    pub identity: Identity,
    /// The telemetry query service: the time series and latest-value tools.
    #[serde(default)]
    pub telemetry_query: Peer,
    /// The assets service: the device model tool.
    #[serde(default)]
    pub assets: Peer,
    /// The dashboard check (services/dashboard-check): the `dashboard` tool has every page
    /// checked there before it writes a version. Without it (or without `telemetry_query.url`)
    /// there is no `dashboard` tool.
    #[serde(default)]
    pub dashboard_check: DashboardCheck,
}

/// The platform settings the service reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`: the file with the public keys internal identity
    /// tokens are verified with (the chart's ConfigMap volume), read again while the service
    /// runs. The public API (`crate::api`) verifies every request's token with them.
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

/// `database.*`: the service role (`assistant_app`), which reads and writes the conversations
/// under row-level security. No platform role: nothing reads conversations across tenants.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `TRUSTIOT__DATABASE__USERNAME`.
    pub username: String,
    /// `TRUSTIOT__DATABASE__PASSWORD`, from a Secret.
    pub password: String,
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS`: most connections of the service role's pool per
    /// replica, from [`MIN_DATABASE_CONNECTIONS`] to [`MAX_DATABASE_CONNECTIONS`]. Default 4:
    /// readiness and the dashboard API's short transactions share it (a dataset run holds no
    /// connection while it calls telemetry-query); the chat API (Laysi/trustiot-v2#146) sizes it
    /// again when it stores conversations.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    4
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
    /// Where and how the service role connects: TLS with verify-full against the deployment's CA.
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

/// The shortest run delegation a question may have: a question needs time for a few model calls.
pub const MIN_RUN_LIFETIME: Duration = Duration::from_secs(120);
/// The longest: the identity service's limit (`RUN_DELEGATION_MAX_LIFETIME_SECS`, 900 s).
pub const MAX_RUN_LIFETIME: Duration =
    Duration::from_secs(contract::api::identity::RUN_DELEGATION_MAX_LIFETIME_SECS as u64);
/// The shortest run delegation with the `dashboard` tool (`dashboard_check.url` set): one whole
/// call of it ([`crate::tools::dashboard::CALL_LIMIT`], 335 s) and the time the question ends
/// before its delegation expires ([`crate::delegation::RUN_END_MARGIN`], 30 s), 365 seconds. A
/// shorter delegation would end every question before its dashboard write could finish.
pub const MIN_RUN_LIFETIME_WITH_DASHBOARDS: Duration =
    crate::tools::dashboard::CALL_LIMIT.saturating_add(crate::delegation::RUN_END_MARGIN);

/// The identity service's token exchange port.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// `http://<release>-identity-exchange:8082`. Without it the assistant has no tools.
    #[serde(default)]
    pub exchange_url: Option<String>,
    /// How long a question's run delegation lives; the question ends a little before
    /// (`delegation::RUN_END_MARGIN`). 2 to 15 minutes, whole seconds.
    #[serde(
        default = "default_run_lifetime",
        deserialize_with = "service_kit::config::duration"
    )]
    pub run_lifetime: Duration,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            exchange_url: None,
            run_lifetime: default_run_lifetime(),
        }
    }
}

fn default_run_lifetime() -> Duration {
    MAX_RUN_LIFETIME
}

/// A service the tools call as the user.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// `http://<host>[:<port>]`, the service's Kubernetes Service; ztunnel encrypts the hop.
    /// Without it the service's tools are left out.
    #[serde(default)]
    pub url: Option<String>,
}

/// `dashboard_check.*`: the dashboard check service. It is called as the assistant, not as the
/// user: the request carries no identity, and the mesh lets only the assistant call it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardCheck {
    /// `TRUSTIOT__DASHBOARD_CHECK__URL`: `http://<host>[:<port>]`, the service's Kubernetes
    /// Service (the chart sets it from `mesh.calls`).
    #[serde(default)]
    pub url: Option<String>,
}

/// The AI gateway (services/ai-gateway) and the model role the main agent uses there.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiGateway {
    /// `http://<host>[:<port>]`, the gateway's Kubernetes Service; ztunnel encrypts the hop.
    pub url: String,
    /// The model role of the main agent.
    #[serde(default = "default_role")]
    pub role: String,
    /// A larger response body fails the call.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

fn default_role() -> String {
    "main".into()
}

fn default_max_response_bytes() -> usize {
    32 << 20
}

/// One model call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// The main agent's effort, fixed for a conversation when it starts (changing it would
    /// invalidate the cached history). The router (Laysi/trustiot-v2#444) will choose it per
    /// conversation; until then every conversation uses this one.
    #[serde(default = "default_effort")]
    pub effort: Effort,
    /// `max_tokens` of one response.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    /// The deadline of one call, measured by the assistant. The gateway has its own time
    /// limits and answers 504 first in the normal case; this one also covers a gateway that
    /// does not answer at all.
    #[serde(
        default = "default_model_call_timeout",
        deserialize_with = "service_kit::config::duration"
    )]
    pub call_timeout: Duration,
    /// The lifetime of the cache breakpoints: `5m` (default) or `1h`.
    #[serde(default)]
    pub cache_ttl: CacheTtl,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            effort: default_effort(),
            max_output_tokens: default_max_output_tokens(),
            call_timeout: default_model_call_timeout(),
            cache_ttl: CacheTtl::default(),
        }
    }
}

fn default_effort() -> Effort {
    Effort::Medium
}

fn default_max_output_tokens() -> u32 {
    16_000
}

fn default_model_call_timeout() -> Duration {
    Duration::from_secs(10 * 60)
}

/// The token budget of one question (docs/architecture/ai-assistant.md, 「Effort 與預算」).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Tokens one question may use, counted as every token each model call processed: input,
    /// cache writes, cache reads and output (thinking included).
    #[serde(default = "default_budget_tokens")]
    pub tokens: u64,
    /// At this share of `tokens`, in percent, the main agent is told to stop calling tools and
    /// answer with what it has.
    #[serde(default = "default_wind_down_percent")]
    pub wind_down_percent: u8,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            tokens: default_budget_tokens(),
            wind_down_percent: default_wind_down_percent(),
        }
    }
}

fn default_budget_tokens() -> u64 {
    2_000_000
}

fn default_wind_down_percent() -> u8 {
    80
}

/// Tool calls.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    /// The deadline of one tool call.
    #[serde(
        default = "default_tool_call_timeout",
        deserialize_with = "service_kit::config::duration"
    )]
    pub call_timeout: Duration,
    /// How many tool calls of one model response run, at the same time; the calls after them
    /// get an error result that asks for them in the next step.
    #[serde(default = "default_max_calls_per_step")]
    pub max_calls_per_step: usize,
    /// The largest text one tool result may hand the model, in bytes.
    #[serde(default = "default_max_text_bytes")]
    pub max_text_bytes: usize,
    /// The most images one tool result may hand the model.
    #[serde(default = "default_max_images")]
    pub max_images: usize,
    /// The largest image, in bytes of base64 text.
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: usize,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            call_timeout: default_tool_call_timeout(),
            max_calls_per_step: default_max_calls_per_step(),
            max_text_bytes: default_max_text_bytes(),
            max_images: default_max_images(),
            max_image_bytes: default_max_image_bytes(),
        }
    }
}

fn default_tool_call_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_max_calls_per_step() -> usize {
    8
}

fn default_max_text_bytes() -> usize {
    64 << 10
}

fn default_max_images() -> usize {
    4
}

fn default_max_image_bytes() -> usize {
    MAX_IMAGE_BYTES
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
        check_gateway_url(&self.ai_gateway.url)
            .map_err(|message| Invalid::new("ai_gateway.url", message))?;
        if !is_role_name(&self.ai_gateway.role) {
            return Err(Invalid::new(
                "ai_gateway.role",
                "must be 1 to 32 lower-case letters, digits or `_`, starting with a letter",
            ));
        }
        if !(MIN_RESPONSE_BYTES..=MAX_RESPONSE_BYTES).contains(&self.ai_gateway.max_response_bytes)
        {
            return Err(Invalid::new(
                "ai_gateway.max_response_bytes",
                "must be between 1 KiB and 256 MiB",
            ));
        }
        if !(1..=MAX_OUTPUT_TOKENS).contains(&self.model.max_output_tokens) {
            return Err(Invalid::new(
                "model.max_output_tokens",
                "must be between 1 and 128000",
            ));
        }
        check_timeout("model.call_timeout", self.model.call_timeout)?;
        if !(1..=MAX_BUDGET_TOKENS).contains(&self.budget.tokens) {
            return Err(Invalid::new(
                "budget.tokens",
                "must be between 1 and 100000000",
            ));
        }
        if !(1..=99).contains(&self.budget.wind_down_percent) {
            return Err(Invalid::new(
                "budget.wind_down_percent",
                "must be between 1 and 99",
            ));
        }
        check_timeout("tools.call_timeout", self.tools.call_timeout)?;
        if !(1..=MAX_CALLS_PER_STEP).contains(&self.tools.max_calls_per_step) {
            return Err(Invalid::new(
                "tools.max_calls_per_step",
                "must be between 1 and 32",
            ));
        }
        if !(1..=MAX_RESPONSE_BYTES).contains(&self.tools.max_text_bytes) {
            return Err(Invalid::new(
                "tools.max_text_bytes",
                "must be between 1 byte and 256 MiB",
            ));
        }
        if self.tools.max_images > MAX_IMAGES {
            return Err(Invalid::new("tools.max_images", "must be at most 20"));
        }
        if !(1..=MAX_IMAGE_BYTES).contains(&self.tools.max_image_bytes) {
            return Err(Invalid::new(
                "tools.max_image_bytes",
                "must be between 1 byte and 5 MiB",
            ));
        }
        if let Some(url) = &self.identity.exchange_url {
            check_service_url(url)
                .map_err(|message| Invalid::new("identity.exchange_url", message))?;
        }
        let lifetime = self.identity.run_lifetime;
        if !(MIN_RUN_LIFETIME..=MAX_RUN_LIFETIME).contains(&lifetime)
            || lifetime.subsec_nanos() != 0
        {
            return Err(Invalid::new(
                "identity.run_lifetime",
                "must be whole seconds between 2 and 15 minutes",
            ));
        }
        if let Some(url) = &self.dashboard_check.url {
            check_service_url(url)
                .map_err(|message| Invalid::new("dashboard_check.url", message))?;
            if self.tools.max_text_bytes < crate::tools::dashboard::MIN_TEXT_BYTES {
                return Err(Invalid::new(
                    "tools.max_text_bytes",
                    "must be at least 64 KiB when dashboard_check.url is set: the dashboard \
                     tool hands the model a whole dashboard",
                ));
            }
            if self.identity.run_lifetime < MIN_RUN_LIFETIME_WITH_DASHBOARDS {
                return Err(Invalid::new(
                    "identity.run_lifetime",
                    format!(
                        "must be at least {} seconds when dashboard_check.url is set: one whole \
                         dashboard tool call ({} s) and the {} s a question ends before its \
                         delegation expires",
                        MIN_RUN_LIFETIME_WITH_DASHBOARDS.as_secs(),
                        crate::tools::dashboard::CALL_LIMIT.as_secs(),
                        crate::delegation::RUN_END_MARGIN.as_secs(),
                    ),
                ));
            }
        }
        for (key, peer) in [
            ("telemetry_query.url", &self.telemetry_query),
            ("assets.url", &self.assets),
        ] {
            let Some(url) = &peer.url else {
                continue;
            };
            check_service_url(url).map_err(|message| Invalid::new(key, message))?;
            if self.identity.exchange_url.is_none() {
                return Err(Invalid::new(
                    "identity.exchange_url",
                    "is required when a service the tools call is set: tools call as the user",
                ));
            }
        }
        Ok(())
    }
}

fn check_timeout(key: &str, timeout: Duration) -> Result<(), Invalid> {
    if timeout.is_zero() || timeout > MAX_CALL_TIMEOUT {
        return Err(Invalid::new(key, "must be more than 0 and at most 1 hour"));
    }
    Ok(())
}

/// Lower-case letters, digits and `_`, starting with a letter, at most 32 characters: the AI
/// gateway's rule for role names.
pub(crate) fn is_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= MAX_ROLE_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A plain `http` origin: the gateway is a Service inside the mesh, where ztunnel adds mutual
/// TLS, and the paths are the gateway's own (`/internal/roles/...`).
pub(crate) fn check_gateway_url(raw: &str) -> Result<(), &'static str> {
    check_origin(
        raw,
        "must not have a path: the gateway's paths start at /internal/",
    )
}

/// A plain `http` origin of a service inside the mesh, where ztunnel adds mutual TLS; the paths
/// are the service's own routes.
pub(crate) fn check_service_url(raw: &str) -> Result<(), &'static str> {
    check_origin(raw, "must not have a path: the routes start at the origin")
}

fn check_origin(raw: &str, with_path: &'static str) -> Result<(), &'static str> {
    let url = reqwest::Url::parse(raw).map_err(|_| "is not an absolute URL")?;
    if url.scheme() != "http" {
        return Err("must use http: ztunnel adds mutual TLS between services");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("has no host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry credentials");
    }
    if url.path() != "/" {
        return Err(with_path);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_urls_are_plain_http_origins() {
        assert!(check_gateway_url("http://ai-gateway:8080").is_ok());
        assert!(check_gateway_url("http://127.0.0.1:8080/").is_ok());
        for bad in [
            "https://ai-gateway:8080",
            "http://user:pass@ai-gateway:8080",
            "http://ai-gateway:8080/internal",
            "http://ai-gateway:8080?x=1",
            "ai-gateway:8080",
            "",
        ] {
            assert!(check_gateway_url(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn role_names_follow_the_gateway() {
        assert!(is_role_name("main"));
        assert!(is_role_name("worker_2"));
        for bad in ["", "Main", "2main", "ma-in", &"a".repeat(33)] {
            assert!(!is_role_name(bad), "{bad} was accepted");
        }
    }

    fn config() -> Config {
        Config {
            platform: Platform {
                internal_identity_keys_file: "/etc/trustiot/identity-keys/jwks.json".into(),
                postgres_host: "trustiot-postgres-rw".into(),
                postgres_port: 5432,
                postgres_database: "trustiot".into(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: "/etc/trustiot/postgres-ca/ca.crt".into(),
            },
            database: Database {
                username: "assistant_app".into(),
                password: "generated in the cluster".into(),
                max_connections: default_max_connections(),
            },
            ai_gateway: AiGateway {
                url: "http://ai-gateway:8080".into(),
                role: default_role(),
                max_response_bytes: default_max_response_bytes(),
            },
            model: Model::default(),
            budget: Budget::default(),
            tools: Tools::default(),
            identity: Identity::default(),
            telemetry_query: Peer::default(),
            assets: Peer::default(),
            dashboard_check: DashboardCheck::default(),
        }
    }

    fn key_of(config: &Config) -> Result<(), String> {
        config.validate().map_err(|error| error.key)
    }

    #[test]
    fn the_services_the_tools_call_are_plain_http_origins_and_need_the_exchange() {
        // No address at all: valid, the assistant runs without tools.
        assert_eq!(key_of(&config()), Ok(()));

        let mut all = config();
        all.identity.exchange_url = Some("http://trustiot-identity-exchange:8082".into());
        all.telemetry_query.url = Some("http://trustiot-telemetry-query:8080".into());
        all.assets.url = Some("http://trustiot-assets:8080".into());
        assert_eq!(key_of(&all), Ok(()));

        // A service without the exchange cannot be called as the user.
        let mut no_exchange = config();
        no_exchange.assets.url = Some("http://trustiot-assets:8080".into());
        assert_eq!(key_of(&no_exchange), Err("identity.exchange_url".into()));

        for (bad, key) in [
            ("https://trustiot-assets:8080", "assets.url"),
            ("http://trustiot-assets:8080/api", "assets.url"),
            ("http://user:secret@trustiot-assets:8080", "assets.url"),
        ] {
            let mut invalid = all_but_assets(&all);
            invalid.assets.url = Some(bad.into());
            assert_eq!(key_of(&invalid), Err(key.into()), "{bad}");
        }
        let mut bad_exchange = config();
        bad_exchange.identity.exchange_url = Some("http://identity:8082/internal".into());
        assert_eq!(key_of(&bad_exchange), Err("identity.exchange_url".into()));
        let mut bad_telemetry = config();
        bad_telemetry.identity.exchange_url = Some("http://identity:8082".into());
        bad_telemetry.telemetry_query.url = Some("telemetry-query:8080".into());
        assert_eq!(key_of(&bad_telemetry), Err("telemetry_query.url".into()));
    }

    /// The check is called as the assistant, so it needs no exchange; its address follows the
    /// same rules, and the tool's results need room for a whole dashboard.
    #[test]
    fn the_dashboard_check_is_a_plain_http_origin_and_needs_room_for_a_dashboard() {
        let mut check = config();
        check.dashboard_check.url = Some("http://trustiot-dashboard-check:8080".into());
        assert_eq!(key_of(&check), Ok(()));
        for bad in [
            "https://trustiot-dashboard-check:8080",
            "http://trustiot-dashboard-check:8080/internal/dashboard-check/v1/check",
            "http://user:secret@trustiot-dashboard-check:8080",
            "trustiot-dashboard-check:8080",
        ] {
            let mut invalid = config();
            invalid.dashboard_check.url = Some(bad.into());
            assert_eq!(key_of(&invalid), Err("dashboard_check.url".into()), "{bad}");
        }
        check.tools.max_text_bytes = crate::tools::dashboard::MIN_TEXT_BYTES - 1;
        assert_eq!(key_of(&check), Err("tools.max_text_bytes".into()));
        check.dashboard_check.url = None;
        assert_eq!(key_of(&check), Ok(()));
    }

    /// With the `dashboard` tool, a question's delegation leaves room for one whole call of it
    /// before the question ends; without the tool, two minutes stay enough.
    #[test]
    fn a_dashboard_check_needs_a_run_lifetime_with_room_for_one_dashboard_call() {
        assert_eq!(MIN_RUN_LIFETIME_WITH_DASHBOARDS, Duration::from_secs(365));
        let mut check = config();
        check.dashboard_check.url = Some("http://trustiot-dashboard-check:8080".into());
        for (lifetime, valid) in [
            (MAX_RUN_LIFETIME, true),
            (MIN_RUN_LIFETIME_WITH_DASHBOARDS, true),
            (
                MIN_RUN_LIFETIME_WITH_DASHBOARDS - Duration::from_secs(1),
                false,
            ),
            (MIN_RUN_LIFETIME, false),
        ] {
            check.identity.run_lifetime = lifetime;
            let expected = if valid {
                Ok(())
            } else {
                Err("identity.run_lifetime".to_owned())
            };
            assert_eq!(key_of(&check), expected, "{lifetime:?}");
        }
        check.dashboard_check.url = None;
        check.identity.run_lifetime = MIN_RUN_LIFETIME;
        assert_eq!(key_of(&check), Ok(()));
    }

    fn all_but_assets(all: &Config) -> Config {
        let mut config = config();
        config.identity.exchange_url = all.identity.exchange_url.clone();
        config.telemetry_query.url = all.telemetry_query.url.clone();
        config
    }

    #[test]
    fn the_run_lifetime_is_two_to_fifteen_whole_minutes_of_seconds() {
        assert_eq!(config().identity.run_lifetime, Duration::from_secs(900));
        for (lifetime, valid) in [
            (Duration::from_secs(120), true),
            (Duration::from_secs(600), true),
            (Duration::from_secs(900), true),
            (Duration::from_secs(119), false),
            (Duration::from_secs(901), false),
            (Duration::from_millis(300_500), false),
            (Duration::ZERO, false),
        ] {
            let mut config = config();
            config.identity.run_lifetime = lifetime;
            let expected = if valid {
                Ok(())
            } else {
                Err("identity.run_lifetime".to_owned())
            };
            assert_eq!(key_of(&config), expected, "{lifetime:?}");
        }
    }

    #[test]
    fn the_settings_read_from_the_environment() {
        let vars = [
            ("TRUSTIOT_POSTGRES_HOST", "trustiot-postgres-rw"),
            ("TRUSTIOT_POSTGRES_PORT", "5432"),
            ("TRUSTIOT_POSTGRES_DATABASE", "trustiot"),
            ("TRUSTIOT_POSTGRES_SSLMODE", "verify-full"),
            (
                "TRUSTIOT_POSTGRES_CA_FILE",
                "/etc/trustiot/postgres-ca/ca.crt",
            ),
            (
                "TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE",
                "/etc/trustiot/identity-keys/jwks.json",
            ),
            // Another platform setting the chart gives every service: ignored, not refused.
            ("TRUSTIOT_DEPLOYMENT_MODE", "saas"),
            ("TRUSTIOT__DATABASE__USERNAME", "assistant_app"),
            ("TRUSTIOT__DATABASE__PASSWORD", "from the password Secret"),
            ("TRUSTIOT__DATABASE__MAX_CONNECTIONS", "6"),
            ("TRUSTIOT__AI_GATEWAY__URL", "http://ai-gateway:8080"),
            (
                "TRUSTIOT__IDENTITY__EXCHANGE_URL",
                "http://identity-exchange:8082",
            ),
            ("TRUSTIOT__IDENTITY__RUN_LIFETIME", "10m"),
            (
                "TRUSTIOT__TELEMETRY_QUERY__URL",
                "http://telemetry-query:8080",
            ),
            ("TRUSTIOT__ASSETS__URL", "http://assets:8080"),
            (
                "TRUSTIOT__DASHBOARD_CHECK__URL",
                "http://dashboard-check:8080",
            ),
        ]
        .map(|(name, value)| {
            (
                std::ffi::OsString::from(name),
                std::ffi::OsString::from(value),
            )
        });
        let config = service_kit::config::load_from::<Config, _>("TRUSTIOT", vars)
            .expect("the settings load")
            .app;
        assert_eq!(
            config.identity.exchange_url.as_deref(),
            Some("http://identity-exchange:8082")
        );
        assert_eq!(config.identity.run_lifetime, Duration::from_secs(600));
        assert_eq!(
            config.telemetry_query.url.as_deref(),
            Some("http://telemetry-query:8080")
        );
        assert_eq!(config.assets.url.as_deref(), Some("http://assets:8080"));
        assert_eq!(
            config.dashboard_check.url.as_deref(),
            Some("http://dashboard-check:8080")
        );
        assert_eq!(config.database.max_connections, 6);
        assert_eq!(
            config.platform.internal_identity_keys_file,
            PathBuf::from("/etc/trustiot/identity-keys/jwks.json")
        );
        let role = config.service_role();
        assert_eq!(
            (role.host.as_str(), role.port, role.username.as_str()),
            ("trustiot-postgres-rw", 5432, "assistant_app")
        );
        assert_eq!(role.ssl_mode, SslMode::VerifyFull);
        // The password never reaches a log line through Debug.
        let debug = format!("{config:?}");
        assert!(!debug.contains("from the password Secret"), "{debug}");
    }

    #[test]
    fn the_database_login_and_pool_are_checked() {
        for (connections, valid) in [
            (MIN_DATABASE_CONNECTIONS - 1, false),
            (MIN_DATABASE_CONNECTIONS, true),
            (MAX_DATABASE_CONNECTIONS, true),
            (MAX_DATABASE_CONNECTIONS + 1, false),
        ] {
            let mut config = config();
            config.database.max_connections = connections;
            let expected = if valid {
                Ok(())
            } else {
                Err("database.max_connections".to_owned())
            };
            assert_eq!(key_of(&config), expected, "{connections} connections");
        }
        let mut blank = config();
        blank.database.username = " ".into();
        assert_eq!(key_of(&blank), Err("database.username".into()));
        let mut no_password = config();
        no_password.database.password = String::new();
        assert_eq!(key_of(&no_password), Err("database.password".into()));
    }

    #[test]
    fn the_defaults_are_valid_and_limits_are_enforced() {
        assert_eq!(config().validate(), Ok(()));

        let mut zero_timeout = config();
        zero_timeout.model.call_timeout = Duration::ZERO;
        assert_eq!(
            zero_timeout.validate().map_err(|error| error.key),
            Err("model.call_timeout".to_owned())
        );

        let mut huge_budget = config();
        huge_budget.budget.tokens = MAX_BUDGET_TOKENS + 1;
        assert_eq!(
            huge_budget.validate().map_err(|error| error.key),
            Err("budget.tokens".to_owned())
        );

        let mut no_wind_down = config();
        no_wind_down.budget.wind_down_percent = 100;
        assert_eq!(
            no_wind_down.validate().map_err(|error| error.key),
            Err("budget.wind_down_percent".to_owned())
        );

        let mut too_long_tool = config();
        too_long_tool.tools.call_timeout = MAX_CALL_TIMEOUT + Duration::from_secs(1);
        assert_eq!(
            too_long_tool.validate().map_err(|error| error.key),
            Err("tools.call_timeout".to_owned())
        );

        for calls in [0, MAX_CALLS_PER_STEP + 1] {
            let mut calls_per_step = config();
            calls_per_step.tools.max_calls_per_step = calls;
            assert_eq!(
                calls_per_step.validate().map_err(|error| error.key),
                Err("tools.max_calls_per_step".to_owned()),
                "{calls} calls per step"
            );
        }
    }
}
