//! The model roles as built at startup: for each role, its models in fallback order, each with
//! a provider client that holds the provider's key and the role's limits.

use std::collections::BTreeMap;
use std::time::Duration;

use ai_clients::anthropic::AnthropicClient;
use ai_clients::decisions::DecisionsClient;
use ai_clients::{ApiKey, EndpointConfig, Tls};
use zeroize::Zeroizing;

use crate::config::{Config, Protocol, Role, split_model};
use crate::reject::{Kind, Rejection};

/// The longest margin kept between the gateway's own deadline and `http.request_timeout`, so
/// the gateway answers in the caller's protocol before service-kit's bare 504.
const MAX_MARGIN: Duration = Duration::from_secs(2);

/// What the gateway could not set up at startup. Messages name settings and variables, never
/// their values.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(
        "environment variable {variable} (providers.{provider}.key_env) is not set, is empty or \
         is not UTF-8"
    )]
    MissingKey { provider: String, variable: String },
    #[error(
        "environment variable {variable} (providers.{provider}.key_env) does not hold a usable \
         API key: it must be printable ASCII without spaces"
    )]
    InvalidKey { provider: String, variable: String },
    #[error(
        "roles.{role}: queue_wait plus the longer of timeouts.response and timeouts.first_token \
         must fit in http.request_timeout less {margin_ms} ms; raise \
         AI_GATEWAY__HTTP__REQUEST_TIMEOUT or lower the role's limits"
    )]
    Budget { role: String, margin_ms: u128 },
    #[error("roles.{role}: the client for {model} cannot be built: {reason}")]
    Client {
        role: String,
        model: String,
        reason: String,
    },
}

/// A provider client.
#[derive(Debug, Clone)]
pub enum Client {
    Anthropic(AnthropicClient),
    Decisions(DecisionsClient),
}

/// One model of a role.
#[derive(Debug)]
pub struct Target {
    /// `<provider>/<model>` as configured, for logs and the `ai-gateway-model` header.
    pub label: String,
    /// The model ID the request body gets.
    pub model: String,
    pub client: Client,
}

/// A model role: its protocol and its models in fallback order.
#[derive(Debug)]
pub struct RoleRoute {
    pub protocol: Protocol,
    pub targets: Vec<Target>,
}

/// The provider keys, kept to scrub them from provider error messages before they are passed
/// on. Wiped from memory on drop.
pub struct Keys(Vec<Zeroizing<String>>);

impl Keys {
    pub fn new(keys: Vec<String>) -> Self {
        Self(keys.into_iter().map(Zeroizing::new).collect())
    }

    /// `text` with every occurrence of every key replaced by `[REDACTED]`. All occurrences are
    /// found in the original text first and overlapping ones merged, so a key that overlaps or
    /// contains another (two providers' keys sharing a prefix) leaves no part visible.
    pub fn scrub(&self, text: &str) -> String {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for key in self.0.iter().filter(|key| !key.is_empty()) {
            let mut from = 0;
            while let Some(found) = text.get(from..).and_then(|rest| rest.find(key.as_str())) {
                let start = from + found;
                spans.push((start, start + key.len()));
                // Keys are ASCII (`ApiKey::new`), so the byte after a match start is a char
                // boundary, and overlapping occurrences are found too.
                from = start + 1;
            }
        }
        if spans.is_empty() {
            return text.to_owned();
        }
        spans.sort_unstable();
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut current: Option<(usize, usize)> = None;
        for (start, end) in spans {
            match current {
                Some((open, close)) if start <= close => current = Some((open, close.max(end))),
                _ => {
                    if let Some((open, close)) = current {
                        out.push_str(text.get(copied..open).unwrap_or_default());
                        out.push_str("[REDACTED]");
                        copied = close;
                    }
                    current = Some((start, end));
                }
            }
        }
        if let Some((open, close)) = current {
            out.push_str(text.get(copied..open).unwrap_or_default());
            out.push_str("[REDACTED]");
            copied = close;
        }
        out.push_str(text.get(copied..).unwrap_or_default());
        out
    }

    /// A short field the provider chose (an error `type` or `code`, a request ID) as the
    /// gateway passes it on or logs it: keys scrubbed, then cut to [`MAX_FIELD_CHARS`].
    pub fn scrub_field(&self, value: &str) -> String {
        self.scrub(value).chars().take(MAX_FIELD_CHARS).collect()
    }
}

/// The longest provider-chosen field passed on or logged, in characters.
pub const MAX_FIELD_CHARS: usize = 256;

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys([{} redacted])", self.0.len())
    }
}

/// Everything a request needs: the roles, the body limit, the time budget of a call and the
/// keys to scrub.
#[derive(Debug)]
pub struct Gateway {
    roles: BTreeMap<String, RoleRoute>,
    max_request_bytes: usize,
    budget: Duration,
    keys: Keys,
}

impl Gateway {
    /// Builds every role's clients. `key` reads a provider key by variable name (the process
    /// environment in production); each provider's key is read once. `request_timeout` is
    /// service-kit's `http.request_timeout`: every role's first attempt must fit in it.
    pub fn build(
        config: &Config,
        key: &dyn Fn(&str) -> Option<String>,
        tls: &Tls,
        request_timeout: Duration,
    ) -> Result<Self, BuildError> {
        let margin = MAX_MARGIN.min(request_timeout / 10);
        let budget = request_timeout.saturating_sub(margin);

        let mut secrets = BTreeMap::new();
        for (name, provider) in &config.providers {
            let missing = || BuildError::MissingKey {
                provider: name.clone(),
                variable: provider.key_env.clone(),
            };
            let secret = Zeroizing::new(key(&provider.key_env).ok_or_else(missing)?);
            if secret.is_empty() {
                return Err(missing());
            }
            ApiKey::new(secret.as_str()).map_err(|_| BuildError::InvalidKey {
                provider: name.clone(),
                variable: provider.key_env.clone(),
            })?;
            secrets.insert(name.as_str(), secret);
        }

        let mut roles = BTreeMap::new();
        for (name, role) in &config.roles {
            let first_attempt = role.queue_wait.saturating_add(role.timeouts.until_answer());
            if first_attempt > budget {
                return Err(BuildError::Budget {
                    role: name.clone(),
                    margin_ms: margin.as_millis(),
                });
            }
            roles.insert(name.clone(), build_role(name, role, config, &secrets, tls)?);
        }

        let keys = Keys(secrets.into_values().collect());
        Ok(Self {
            roles,
            max_request_bytes: config.max_request_bytes,
            budget,
            keys,
        })
    }

    /// The role named in the request path, if it speaks `protocol`.
    pub fn route(&self, role: &str, protocol: Protocol) -> Result<&RoleRoute, Rejection> {
        let route = self
            .roles
            .get(role)
            .ok_or_else(|| Rejection::new(Kind::NotFound, "no such model role"))?;
        if route.protocol != protocol {
            return Err(Rejection::new(
                Kind::InvalidRequest,
                format!(
                    "this model role speaks {}; send the request to its path",
                    route.protocol.name()
                ),
            ));
        }
        Ok(route)
    }

    /// The roles, for the startup log.
    pub fn roles(&self) -> impl Iterator<Item = (&str, &RoleRoute)> {
        self.roles
            .iter()
            .map(|(name, route)| (name.as_str(), route))
    }

    pub fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    /// How long a call may take until its response, or a stream's first output, across every
    /// model it tries.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

fn build_role(
    name: &str,
    role: &Role,
    config: &Config,
    secrets: &BTreeMap<&str, Zeroizing<String>>,
    tls: &Tls,
) -> Result<RoleRoute, BuildError> {
    let mut protocol = None;
    let mut targets = Vec::with_capacity(role.models.len());
    for reference in &role.models {
        let client_error = |reason: String| BuildError::Client {
            role: name.to_owned(),
            model: reference.clone(),
            reason,
        };
        // Validation already checked every reference names a configured provider.
        let (provider_name, model) =
            split_model(reference).ok_or_else(|| client_error("not <provider>/<model>".into()))?;
        let provider = config
            .providers
            .get(provider_name)
            .ok_or_else(|| client_error("the provider is not configured".into()))?;
        let secret = secrets
            .get(provider_name)
            .ok_or_else(|| client_error("the provider has no key".into()))?;
        let endpoint = EndpointConfig {
            name: format!("{name}:{reference}"),
            base_url: provider.base_url().to_owned(),
            max_concurrent: role.max_concurrent,
            queue_wait: role.queue_wait,
            timeouts: role.timeouts.to_client(),
            max_response_bytes: role.max_response_bytes,
            max_event_bytes: role.max_event_bytes,
        };
        let api_key = ApiKey::new(secret.as_str())
            .map_err(|_| client_error("the provider key is not usable".into()))?;
        // The clients' errors name the endpoint and the reason, never the key or the URL.
        let client = match provider.protocol {
            Protocol::AnthropicMessages => AnthropicClient::new(endpoint, api_key, tls)
                .map(Client::Anthropic)
                .map_err(|error| client_error(error.to_string()))?,
            Protocol::OpenaiDecisions => DecisionsClient::new(endpoint, api_key, tls)
                .map(Client::Decisions)
                .map_err(|error| client_error(error.to_string()))?,
        };
        protocol = Some(provider.protocol);
        targets.push(Target {
            label: reference.clone(),
            model: model.to_owned(),
            client,
        });
    }
    let protocol = protocol.ok_or_else(|| BuildError::Client {
        role: name.to_owned(),
        model: String::new(),
        reason: "the role lists no model".into(),
    })?;
    Ok(RoleRoute { protocol, targets })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use service_kit::config::load_from;

    use super::*;
    use crate::config::ENV_PREFIX;

    fn config(extra: &[(&str, &str)]) -> Config {
        let mut vars = vec![
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL",
                "anthropic-messages",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV",
                "ANTHROPIC_API_KEY",
            ),
            (
                "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
                "http://127.0.0.1:9",
            ),
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5",
            ),
        ];
        vars.extend_from_slice(extra);
        load_from::<Config, _>(
            ENV_PREFIX,
            vars.iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        )
        .unwrap()
        .app
    }

    fn key_text() -> String {
        ["test", "key", &std::process::id().to_string()].join("-")
    }

    #[test]
    fn a_missing_or_unusable_key_names_the_variable_only() {
        let config = config(&[]);
        let hour = Duration::from_secs(3600);
        let error = Gateway::build(&config, &|_| None, &Tls::none(), hour).unwrap_err();
        assert!(matches!(error, BuildError::MissingKey { .. }), "{error}");
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");

        let spaced = format!("{} x", key_text());
        let error =
            Gateway::build(&config, &|_| Some(spaced.clone()), &Tls::none(), hour).unwrap_err();
        assert!(matches!(error, BuildError::InvalidKey { .. }), "{error}");
        assert!(!error.to_string().contains(&key_text()), "{error}");
    }

    #[test]
    fn every_role_must_fit_in_the_request_timeout() {
        let config = config(&[("AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE", "300s")]);
        let key = key_text();
        let error = Gateway::build(
            &config,
            &|_| Some(key.clone()),
            &Tls::none(),
            Duration::from_secs(30),
        )
        .unwrap_err();
        assert!(matches!(error, BuildError::Budget { .. }), "{error}");
        assert!(
            error
                .to_string()
                .contains("AI_GATEWAY__HTTP__REQUEST_TIMEOUT")
        );

        let gateway = Gateway::build(
            &config,
            &|_| Some(key.clone()),
            &Tls::none(),
            Duration::from_secs(302),
        )
        .unwrap();
        assert_eq!(gateway.budget(), Duration::from_secs(300));
        let route = gateway.route("main", Protocol::AnthropicMessages).unwrap();
        assert_eq!(route.targets[0].label, "anthropic/claude-opus-5-5");
        assert_eq!(route.targets[0].model, "claude-opus-5-5");
        assert!(gateway.route("other", Protocol::AnthropicMessages).is_err());
        assert!(gateway.route("main", Protocol::OpenaiDecisions).is_err());
    }

    #[test]
    fn keys_are_scrubbed_and_never_printed() {
        let key = key_text();
        let keys = Keys::new(vec![key.clone()]);
        assert_eq!(
            keys.scrub(&format!("bad key {key}, again {key}.")),
            "bad key [REDACTED], again [REDACTED]."
        );
        assert_eq!(keys.scrub("nothing here, ok"), "nothing here, ok");
        assert!(!format!("{keys:?}").contains(&key));

        assert_eq!(keys.scrub_field(&format!("x_{key}")), "x_[REDACTED]");
        let long = format!("{}{key}", "y".repeat(MAX_FIELD_CHARS));
        assert_eq!(keys.scrub_field(&long), "y".repeat(MAX_FIELD_CHARS));
    }

    #[test]
    fn overlapping_keys_leave_no_part_visible_in_any_order() {
        // One key a prefix of another, and a third overlapping the longer one's end.
        let key = key_text();
        let short = format!("{key}-a");
        let long = format!("{key}-a-longer");
        let tail = "a-longer-tail".to_owned();
        let text = format!("1 {long}-tail 2 {short} 3 é{short}é {key}");
        for order in [
            vec![short.clone(), long.clone(), tail.clone()],
            vec![tail.clone(), long.clone(), short.clone()],
            vec![long.clone(), short.clone(), tail.clone()],
        ] {
            let scrubbed = Keys::new(order).scrub(&text);
            assert_eq!(
                scrubbed,
                format!("1 [REDACTED] 2 [REDACTED] 3 é[REDACTED]é {key}")
            );
        }
    }
}
