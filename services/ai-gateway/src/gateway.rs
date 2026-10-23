//! The model roles. The configuration decides which roles exist, their protocol and limits, and
//! their default models; the platform administrator's model lists in the database (src/store.rs)
//! replace a role's models until they are reset. A [`Snapshot`] holds every role's models as of
//! one settings generation, each model with a provider client that holds the provider's key and
//! the role's limits.
//!
//! With a store attached ([`Gateway::with_store`]), every forwarded request reads the settings
//! generation once and loads the settings again only when the database has a newer one
//! (README.md, 「多副本」). A request keeps the snapshot it got for its whole call: every model it
//! falls back to and the stream it relays. A client is reused across snapshots for the same role
//! and `<provider>/<model>` while any snapshot or request still holds it, so reloading never
//! resets a model's concurrency count.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use ai_clients::anthropic::AnthropicClient;
use ai_clients::decisions::DecisionsClient;
use ai_clients::{ApiKey, EndpointConfig, Tls};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::config::{Config, Protocol, Role, RoleRules, split_model};
use crate::reject::{Kind, Rejection};
use crate::store::{Store, Stored, StoredRole};

/// The longest margin kept between the gateway's own deadline and `http.request_timeout`, so
/// the gateway answers in the caller's protocol before service-kit's bare 504.
const MAX_MARGIN: Duration = Duration::from_secs(2);

/// The generation of the snapshot built from the configuration alone, before the database was
/// read: lower than any settings generation (which starts at 0), so the first forwarded request
/// loads the settings.
pub const CONFIGURED_ONLY: i64 = -1;

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

/// Why a forwarded request could not check the settings. The call is refused with 503.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("the settings could not be read from the database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("the settings check did not finish within database.timeout or the call's budget")]
    TimedOut,
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
    /// `<provider>/<model>`, for logs and the `ai-gateway-model` header.
    pub label: String,
    /// The model ID the request body gets.
    pub model: String,
    pub client: Client,
}

/// A model role as one snapshot has it: its protocol and its models in fallback order.
#[derive(Debug)]
pub struct RoleRoute {
    pub protocol: Protocol,
    pub targets: Vec<Arc<Target>>,
    /// The version of the stored model list in use; `None` when the role uses its configured
    /// models.
    pub stored_version: Option<i64>,
}

/// A stored model list that the gateway does not use because it no longer fits the
/// configuration; the role uses its configured models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ignored {
    pub version: i64,
    pub reason: String,
}

/// Every role's models as of one settings generation.
#[derive(Debug)]
pub struct Snapshot {
    generation: i64,
    routes: BTreeMap<String, Arc<RoleRoute>>,
    stored: Vec<StoredRole>,
    ignored: BTreeMap<String, Ignored>,
}

impl Snapshot {
    /// The settings generation it was loaded from; [`CONFIGURED_ONLY`] before the first load.
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// A configured role.
    pub fn route(&self, role: &str) -> Option<&Arc<RoleRoute>> {
        self.routes.get(role)
    }

    pub fn routes(&self) -> impl Iterator<Item = (&str, &Arc<RoleRoute>)> {
        self.routes
            .iter()
            .map(|(name, route)| (name.as_str(), route))
    }

    /// The stored model lists as read, the ignored ones included.
    pub fn stored(&self) -> &[StoredRole] {
        &self.stored
    }

    /// The stored model lists not in use, by role, and why. A list of a role the configuration
    /// no longer has is here too.
    pub fn ignored(&self) -> &BTreeMap<String, Ignored> {
        &self.ignored
    }
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

/// A configured provider, with its key for the clients built when the settings change.
struct ProviderSpec {
    protocol: Protocol,
    base_url: String,
    /// Checked by `ApiKey::new` at startup. Wiped from memory on drop.
    secret: Zeroizing<String>,
}

/// The database side: the store, the bound of a settings check, and the lock that lets one
/// request at a time load new settings.
struct Live {
    store: Store,
    timeout: Duration,
    reload: tokio::sync::Mutex<()>,
}

/// Clients by role and `<provider>/<model>`, held weakly: a client lives as long as a snapshot
/// or a request holds it.
type Clients = HashMap<(String, String), Weak<Target>>;

/// Everything a request needs: the roles, the body limit, the time budget of a call, the keys
/// to scrub, and the current snapshot of the settings.
pub struct Gateway {
    roles: BTreeMap<String, Role>,
    rules: RoleRules,
    providers: BTreeMap<String, ProviderSpec>,
    tls: Tls,
    max_request_bytes: usize,
    budget: Duration,
    keys: Keys,
    /// Each role's configured models, held for the life of the gateway.
    configured: BTreeMap<String, Arc<RoleRoute>>,
    clients: Mutex<Clients>,
    current: RwLock<Arc<Snapshot>>,
    live: Option<Live>,
}

/// Names the roles and limits, never a key.
impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("roles", &self.roles.keys().collect::<Vec<_>>())
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("max_request_bytes", &self.max_request_bytes)
            .field("budget", &self.budget)
            .field("keys", &self.keys)
            .field("store", &self.live.is_some())
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// Builds every role's clients for its configured models. `key` reads a provider key by
    /// variable name (the process environment in production); each provider's key is read
    /// once. `request_timeout` is service-kit's `http.request_timeout`: every role's first
    /// attempt must fit in it. Without [`Gateway::with_store`] the roles keep their configured
    /// models.
    pub fn build(
        config: &Config,
        key: &dyn Fn(&str) -> Option<String>,
        tls: &Tls,
        request_timeout: Duration,
    ) -> Result<Self, BuildError> {
        let margin = MAX_MARGIN.min(request_timeout / 10);
        let budget = request_timeout.saturating_sub(margin);

        let mut providers = BTreeMap::new();
        let mut keys = Vec::new();
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
            keys.push(Zeroizing::new(secret.as_str().to_owned()));
            providers.insert(
                name.clone(),
                ProviderSpec {
                    protocol: provider.protocol,
                    base_url: provider.base_url().to_owned(),
                    secret,
                },
            );
        }

        for (name, role) in &config.roles {
            let first_attempt = role.queue_wait.saturating_add(role.timeouts.until_answer());
            if first_attempt > budget {
                return Err(BuildError::Budget {
                    role: name.clone(),
                    margin_ms: margin.as_millis(),
                });
            }
        }

        let mut gateway = Self {
            roles: config.roles.clone(),
            rules: RoleRules::of(config),
            providers,
            tls: tls.clone(),
            max_request_bytes: config.max_request_bytes,
            budget,
            keys: Keys(keys),
            configured: BTreeMap::new(),
            clients: Mutex::new(HashMap::new()),
            current: RwLock::new(Arc::new(Snapshot {
                generation: CONFIGURED_ONLY,
                routes: BTreeMap::new(),
                stored: Vec::new(),
                ignored: BTreeMap::new(),
            })),
            live: None,
        };
        let mut configured = BTreeMap::new();
        for (name, role) in &config.roles {
            let route =
                gateway
                    .route_for(name, &role.models, None)
                    .map_err(|(model, reason)| BuildError::Client {
                        role: name.clone(),
                        model,
                        reason,
                    })?;
            configured.insert(name.clone(), Arc::new(route));
        }
        gateway.configured = configured.clone();
        gateway.current = RwLock::new(Arc::new(Snapshot {
            generation: CONFIGURED_ONLY,
            routes: configured,
            stored: Vec::new(),
            ignored: BTreeMap::new(),
        }));
        Ok(gateway)
    }

    /// Takes the roles' models from `store` from now on: every forwarded request checks the
    /// settings generation, waiting at most `timeout` (and never past the call's budget).
    #[must_use]
    pub fn with_store(mut self, store: Store, timeout: Duration) -> Self {
        self.live = Some(Live {
            store,
            timeout,
            reload: tokio::sync::Mutex::new(()),
        });
        self
    }

    /// Checks the role named in the request path: it must be configured and speak `protocol`.
    /// Neither depends on the stored settings.
    pub fn check_role(&self, role: &str, protocol: Protocol) -> Result<(), Rejection> {
        let expected = self
            .rules
            .roles
            .get(role)
            .ok_or_else(|| Rejection::new(Kind::NotFound, "no such model role"))?;
        if *expected != protocol {
            return Err(Rejection::new(
                Kind::InvalidRequest,
                format!(
                    "this model role speaks {}; send the request to its path",
                    expected.name()
                ),
            ));
        }
        Ok(())
    }

    /// What the configuration allows a stored model list to be (src/store.rs).
    pub fn rules(&self) -> &RoleRules {
        &self.rules
    }

    /// The snapshot in memory, without checking the database.
    pub fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The snapshot a forwarded request uses: the one in memory when the database's settings
    /// generation is not newer, otherwise the settings loaded again. Waits at most the store's
    /// timeout and never past `deadline`. Without a store, the configured models.
    ///
    /// # Errors
    ///
    /// When the database cannot be read in time; the call must then be refused.
    pub async fn snapshot(&self, deadline: Instant) -> Result<Arc<Snapshot>, SettingsError> {
        let Some(live) = &self.live else {
            return Ok(self.current());
        };
        let limit = deadline.min(Instant::now() + live.timeout);
        match tokio::time::timeout_at(limit, self.refresh(live)).await {
            Ok(result) => result,
            Err(_) => Err(SettingsError::TimedOut),
        }
    }

    async fn refresh(&self, live: &Live) -> Result<Arc<Snapshot>, SettingsError> {
        let generation = live.store.generation().await?;
        let current = self.current();
        // An older generation than the one in memory only comes from restoring an older backup
        // under a running gateway, which needs a restart (README.md, 「資料庫」).
        if generation <= current.generation {
            return Ok(current);
        }
        let _reloading = live.reload.lock().await;
        let current = self.current();
        if generation <= current.generation {
            return Ok(current);
        }
        let stored = live.store.load().await?;
        let snapshot = Arc::new(self.assemble(stored));
        let mut slot = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if snapshot.generation > slot.generation {
            *slot = Arc::clone(&snapshot);
        }
        Ok(Arc::clone(&slot))
    }

    /// A snapshot of `stored`: each configured role with its stored model list when that list
    /// still fits the configuration, otherwise with its configured models (logged at ERROR and
    /// marked in [`Snapshot::ignored`]).
    pub fn assemble(&self, stored: Stored) -> Snapshot {
        let mut routes = BTreeMap::new();
        let mut ignored = BTreeMap::new();
        // A row without models is a reset role: it uses its configured models.
        let by_role: BTreeMap<&str, (&StoredRole, &[String])> = stored
            .roles
            .iter()
            .filter_map(|row| Some((row.role.as_str(), (row, row.models.as_deref()?))))
            .collect();
        for (name, configured) in &self.configured {
            let route = match by_role.get(name.as_str()) {
                None => Arc::clone(configured),
                Some((row, models)) => match self.stored_route(name, models, row.version) {
                    Ok(route) => {
                        tracing::info!(
                            role = %name,
                            version = row.version,
                            models = %models.join(","),
                            generation = stored.generation,
                            "model role uses stored models"
                        );
                        Arc::new(route)
                    }
                    Err(reason) => {
                        ignore(&mut ignored, row, reason);
                        Arc::clone(configured)
                    }
                },
            };
            routes.insert(name.clone(), route);
        }
        for (role, (row, _)) in &by_role {
            if !self.configured.contains_key(*role) {
                ignore(
                    &mut ignored,
                    row,
                    "no such model role is configured".to_owned(),
                );
            }
        }
        self.prune_clients();
        Snapshot {
            generation: stored.generation,
            routes,
            stored: stored.roles,
            ignored,
        }
    }

    fn stored_route(
        &self,
        name: &str,
        models: &[String],
        version: i64,
    ) -> Result<RoleRoute, String> {
        match self.rules.check(name, models) {
            Some(Ok(())) => self
                .route_for(name, models, Some(version))
                .map_err(|(model, reason)| format!("the client for {model}: {reason}")),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("the role's configured models do not check".to_owned()),
        }
    }

    /// A role's route for `models`, reusing a live client for each model the role already has.
    /// The error names the model and the reason.
    fn route_for(
        &self,
        name: &str,
        models: &[String],
        stored_version: Option<i64>,
    ) -> Result<RoleRoute, (String, String)> {
        let role = self
            .roles
            .get(name)
            .ok_or_else(|| (String::new(), "the role is not configured".to_owned()))?;
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        let mut protocol = None;
        let mut targets = Vec::with_capacity(models.len());
        for reference in models {
            let key = (name.to_owned(), reference.clone());
            let target = match clients.get(&key).and_then(Weak::upgrade) {
                Some(target) => target,
                None => {
                    let target = Arc::new(
                        self.build_target(name, role, reference)
                            .map_err(|reason| (reference.clone(), reason))?,
                    );
                    clients.insert(key, Arc::downgrade(&target));
                    target
                }
            };
            protocol = Some(match &target.client {
                Client::Anthropic(_) => Protocol::AnthropicMessages,
                Client::Decisions(_) => Protocol::OpenaiDecisions,
            });
            targets.push(target);
        }
        let protocol =
            protocol.ok_or_else(|| (String::new(), "the role lists no model".to_owned()))?;
        Ok(RoleRoute {
            protocol,
            targets,
            stored_version,
        })
    }

    fn build_target(&self, name: &str, role: &Role, reference: &str) -> Result<Target, String> {
        // The configuration and the store checked every reference names a configured provider.
        let (provider_name, model) =
            split_model(reference).ok_or_else(|| "not <provider>/<model>".to_owned())?;
        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| "the provider is not configured".to_owned())?;
        let endpoint = EndpointConfig {
            name: format!("{name}:{reference}"),
            base_url: provider.base_url.clone(),
            max_concurrent: role.max_concurrent,
            queue_wait: role.queue_wait,
            timeouts: role.timeouts.to_client(),
            max_response_bytes: role.max_response_bytes,
            max_event_bytes: role.max_event_bytes,
        };
        let api_key = ApiKey::new(provider.secret.as_str())
            .map_err(|_| "the provider key is not usable".to_owned())?;
        // The clients' errors name the endpoint and the reason, never the key or the URL.
        let client = match provider.protocol {
            Protocol::AnthropicMessages => AnthropicClient::new(endpoint, api_key, &self.tls)
                .map(Client::Anthropic)
                .map_err(|error| error.to_string())?,
            Protocol::OpenaiDecisions => DecisionsClient::new(endpoint, api_key, &self.tls)
                .map(Client::Decisions)
                .map_err(|error| error.to_string())?,
        };
        Ok(Target {
            label: reference.to_owned(),
            model: model.to_owned(),
            client,
        })
    }

    /// Forgets the clients no snapshot or request holds any more.
    fn prune_clients(&self) {
        self.clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, target| target.strong_count() > 0);
    }

    /// Whether the database answers the settings query (readiness).
    ///
    /// # Errors
    ///
    /// When it does not. Without a store, never.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        match &self.live {
            Some(live) => live.store.generation().await.map(|_| ()),
            None => Ok(()),
        }
    }

    pub fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    /// How long a call may take until its response, or a stream's first output, across every
    /// model it tries, the settings check included.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

fn ignore(ignored: &mut BTreeMap<String, Ignored>, row: &StoredRole, reason: String) {
    tracing::error!(
        role = %row.role,
        version = row.version,
        reason = %reason,
        "ignored the stored models of a model role; it uses its configured models"
    );
    ignored.insert(
        row.role.clone(),
        Ignored {
            version: row.version,
            reason,
        },
    );
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
            ("AI_GATEWAY_POSTGRES_HOST", "127.0.0.1"),
            ("AI_GATEWAY_POSTGRES_PORT", "5432"),
            ("AI_GATEWAY_POSTGRES_DATABASE", "trustiot"),
            ("AI_GATEWAY_POSTGRES_SSLMODE", "verify-full"),
            ("AI_GATEWAY_POSTGRES_CA_FILE", "/nonexistent/ca.pem"),
            ("AI_GATEWAY__DATABASE__USERNAME", "ai_gateway_app"),
            ("AI_GATEWAY__DATABASE__PASSWORD", "not-a-real-password"),
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

    fn row(role: &str, models: &[&str], version: i64) -> StoredRole {
        StoredRole {
            role: role.to_owned(),
            models: Some(models.iter().map(|model| (*model).to_owned()).collect()),
            version,
            updated_at: "2026-10-23T00:00:00.000000Z".to_owned(),
            updated_by: "admin".to_owned(),
        }
    }

    fn labels(route: &RoleRoute) -> Vec<&str> {
        route
            .targets
            .iter()
            .map(|target| target.label.as_str())
            .collect()
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
        let snapshot = gateway.current();
        assert_eq!(snapshot.generation(), CONFIGURED_ONLY);
        let route = snapshot.route("main").unwrap();
        assert_eq!(route.targets[0].label, "anthropic/claude-opus-5-5");
        assert_eq!(route.targets[0].model, "claude-opus-5-5");
        assert!(
            gateway
                .check_role("main", Protocol::AnthropicMessages)
                .is_ok()
        );
        assert!(
            gateway
                .check_role("other", Protocol::AnthropicMessages)
                .is_err()
        );
        assert!(
            gateway
                .check_role("main", Protocol::OpenaiDecisions)
                .is_err()
        );
        let debug = format!("{gateway:?}");
        assert!(!debug.contains(&key), "{debug}");
    }

    #[test]
    fn stored_models_replace_the_configured_ones_and_reuse_their_clients() {
        let config = config(&[
            (
                "AI_GATEWAY__ROLES__MAIN__MODELS",
                "anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5",
            ),
            (
                "AI_GATEWAY__PROVIDERS__OPENAI__PROTOCOL",
                "openai-decisions",
            ),
            ("AI_GATEWAY__PROVIDERS__OPENAI__KEY_ENV", "OPENAI_API_KEY"),
            (
                "AI_GATEWAY__PROVIDERS__OPENAI__BASE_URL",
                "http://127.0.0.1:9",
            ),
        ]);
        let key = key_text();
        let gateway = Gateway::build(
            &config,
            &|_| Some(key.clone()),
            &Tls::none(),
            Duration::from_secs(3600),
        )
        .unwrap();
        let configured = gateway.current();
        let opus = Arc::clone(&configured.route("main").unwrap().targets[0]);

        let snapshot = gateway.assemble(Stored {
            generation: 3,
            roles: vec![row(
                "main",
                &["anthropic/claude-haiku-5-5", "anthropic/claude-opus-5-5"],
                3,
            )],
        });
        let route = snapshot.route("main").unwrap();
        assert_eq!(
            labels(route),
            ["anthropic/claude-haiku-5-5", "anthropic/claude-opus-5-5"]
        );
        assert_eq!(route.stored_version, Some(3));
        // The same client, so the same concurrency count.
        assert!(Arc::ptr_eq(&route.targets[1], &opus));
        assert!(snapshot.ignored().is_empty());

        // A list that no longer fits is ignored, and so is one of a role that is gone.
        let snapshot = gateway.assemble(Stored {
            generation: 5,
            roles: vec![
                row("main", &["openai/o5"], 4),
                row("gone", &["anthropic/claude-opus-5-5"], 5),
            ],
        });
        let route = snapshot.route("main").unwrap();
        assert_eq!(
            labels(route),
            ["anthropic/claude-opus-5-5", "anthropic/claude-sonnet-5-5"]
        );
        assert_eq!(route.stored_version, None);
        assert_eq!(snapshot.ignored()["main"].version, 4);
        assert!(
            snapshot.ignored()["main"]
                .reason
                .contains("anthropic-messages"),
            "{:?}",
            snapshot.ignored()
        );
        assert!(snapshot.ignored().contains_key("gone"));
        assert_eq!(snapshot.stored().len(), 2);

        // A reset role (a row without models) uses its configured models and is not ignored,
        // whether the chart still has it or not.
        let reset = |role: &str, version: i64| StoredRole {
            models: None,
            ..row(role, &[], version)
        };
        let snapshot = gateway.assemble(Stored {
            generation: 7,
            roles: vec![reset("gone", 6), reset("main", 7)],
        });
        let route = snapshot.route("main").unwrap();
        assert_eq!(
            labels(route),
            ["anthropic/claude-opus-5-5", "anthropic/claude-sonnet-5-5"]
        );
        assert_eq!(route.stored_version, None);
        assert!(snapshot.ignored().is_empty(), "{:?}", snapshot.ignored());
        assert_eq!(snapshot.stored().len(), 2);
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
