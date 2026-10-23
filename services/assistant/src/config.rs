//! The assistant's settings (README.md, 「設定」): where the AI gateway is, the model call, the
//! token budget of a question and the limits of tool calls. They come from `TRUSTIOT__*`
//! variables or the TOML file named by `TRUSTIOT_CONFIG_FILE`, through service-kit, and are
//! checked at startup. Error messages name keys, never values.

use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};

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

/// The assistant's own settings (service-kit adds `http`, `shutdown` and `telemetry`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ai_gateway: AiGateway,
    #[serde(default)]
    pub model: Model,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub tools: Tools,
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
        return Err("must not have a path: the gateway's paths start at /internal/");
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
            ai_gateway: AiGateway {
                url: "http://ai-gateway:8080".into(),
                role: default_role(),
                max_response_bytes: default_max_response_bytes(),
            },
            model: Model::default(),
            budget: Budget::default(),
            tools: Tools::default(),
        }
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
