//! The passwords of the service login roles and of the collector's ClickHouse account, and (through
//! [`crate::clickhouse_admin`], run by the bundled ClickHouse's init container with the same rules)
//! of the bundled ClickHouse's bootstrap admin account. Nobody creates, copies or rotates them
//! (docs/architecture/secrets.md):
//!
//! - the chart declares one Secret per consumer (each service that uses Postgres, and the
//!   collector) without any data, so it never renders a password;
//! - this module, run by the Job while it holds the migration lock, reads each Secret, generates
//!   a 256-bit password for every key it manages that is missing or unusable, and for every key
//!   when the Secret is not the object the Job filled (its uid differs from the one the Job
//!   recorded) or records an older or no password generation (`migrate.passwordGeneration`), and
//!   writes them with one JSON merge patch that carries the Secret's `resourceVersion`, so a
//!   concurrent change of the Secret makes the patch fail instead of being overwritten;
//! - generations only go up: a lower requested generation (a rollback renders the older value)
//!   keeps the passwords and the higher generation the Secret records, so a rollback and the
//!   re-upgrade after it do not rotate again;
//! - the data is bound to the object: a Secret created by someone else under the same name (and
//!   adopted by Helm) cannot carry a password of their choosing, because they cannot know the
//!   uid the API will give it; and the patch that binds a Secret also removes every label and
//!   annotation the chart and the Job do not set, its ownerReferences and its finalizers, so
//!   metadata someone put on it beforehand (a replicator's annotation, an owner that would
//!   garbage-collect it) never applies to generated passwords;
//! - the rest of the run then sets each role and account to the password in its Secret.
//!
//! The Job's ServiceAccount may only `get` and `patch` exactly these Secrets (the chart's Role
//! lists them by name), and only `get` the admin Secret, which ClickHouse's ServiceAccount alone
//! may `patch`. Nothing here logs a password, a hash of one, or a Secret's data: reports
//! and errors name roles, accounts and Secrets only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::config::{Binding, PasswordSettings};
use crate::secret::Password;
use crate::{Error, Report, http};

/// The annotation that records which password generation the Secret's passwords were made for.
pub const GENERATION_ANNOTATION: &str = "trustiot.io/password-generation";

/// The annotation that records the uid of the Secret the Job filled. The data is trusted only
/// while it equals the Secret's own `metadata.uid`; the chart never renders it.
pub const UID_ANNOTATION: &str = "trustiot.io/password-secret-uid";

/// The labels a password Secret keeps when the Job binds it: the chart's own for these Secrets
/// (deploy/chart/templates/passwords.yaml with "trustiot.componentLabels"; keep in step with the
/// chart, whose tests pin the rendered labels), and the two that Flux's helm-controller puts on
/// every object of a HelmRelease (deploy/flux), which the next upgrade would only add again.
const KEPT_LABELS: &[&str] = &[
    "app.kubernetes.io/component",
    "app.kubernetes.io/instance",
    "app.kubernetes.io/managed-by",
    "app.kubernetes.io/name",
    "app.kubernetes.io/part-of",
    "app.kubernetes.io/version",
    "helm.sh/chart",
    "helm.toolkit.fluxcd.io/name",
    "helm.toolkit.fluxcd.io/namespace",
    "trustiot.io/filled-by",
];

/// The annotations a password Secret keeps when the Job binds it: the chart's, Helm's ownership
/// annotations (without them the next upgrade could not adopt the Secret), and the Job's own.
const KEPT_ANNOTATIONS: &[&str] = &[
    "helm.sh/resource-policy",
    "meta.helm.sh/release-name",
    "meta.helm.sh/release-namespace",
    "trustiot.io/description",
    GENERATION_ANNOTATION,
    UID_ANNOTATION,
];

/// The field manager of the run's patches, as `metadata.managedFields` shows it.
pub const FIELD_MANAGER: &str = "trustiot-migrate";

const RETRY_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);

/// Patches refused because the Secret changed in between; after this many the run fails.
const MAX_CONFLICTS: usize = 5;

/// The passwords of one run: what each role and account must log in with.
#[derive(Debug, Default)]
pub struct Passwords {
    postgres: BTreeMap<String, Password>,
    collector: Option<(String, Password)>,
}

impl Passwords {
    /// The password of a Postgres login role, when the run manages it.
    #[must_use]
    pub fn postgres(&self, role: &str) -> Option<&Password> {
        self.postgres.get(role)
    }

    /// The collector's account and its password, when the run manages it.
    #[must_use]
    pub fn collector(&self) -> Option<&(String, Password)> {
        self.collector.as_ref()
    }
}

/// Why a key gets a new password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The Secret has no such key: a first install, or someone deleted it.
    Missing,
    /// The key does not hold an acceptable password.
    Unusable,
    /// The Secret records an older generation than the requested one, or none it can read: a
    /// rotation.
    Generation,
    /// The Secret is not the object the Job filled: it records another uid, or none (a Secret
    /// created by someone else, or restored from a backup).
    Unbound,
}

impl Why {
    fn describe(self, generation: u64) -> String {
        match self {
            Self::Missing => "it had none".to_owned(),
            Self::Unusable => "the stored one was not usable".to_owned(),
            Self::Generation => format!("password generation {generation}"),
            Self::Unbound => "the Secret is not the one the Job filled".to_owned(),
        }
    }
}

/// A password generation: 1 to 9 decimal digits (the chart's schema allows no other).
#[must_use]
pub fn parse_generation(text: &str) -> Option<u64> {
    if (1..=9).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// What a Secret holds, as far as the run reads it. No `Debug`: `data` holds passwords.
#[derive(Deserialize)]
struct Secret {
    #[serde(default)]
    metadata: Metadata,
    /// Values in base64, as the API returns them.
    #[serde(default)]
    data: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default)]
    resource_version: String,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    owner_references: Vec<Value>,
    #[serde(default)]
    finalizers: Vec<String>,
    /// Set while the Secret is being deleted.
    #[serde(default)]
    deletion_timestamp: Option<String>,
}

/// What the patch that binds a Secret removes: metadata neither the chart nor the Job sets. Names
/// only, which the report gives; never a value.
#[derive(Debug, Default, PartialEq, Eq)]
struct Foreign {
    annotations: Vec<String>,
    labels: Vec<String>,
    owner_references: bool,
    finalizers: bool,
}

impl Foreign {
    fn of(metadata: &Metadata) -> Self {
        let outside = |keys: &BTreeMap<String, String>, kept: &[&str]| {
            keys.keys()
                .filter(|key| !kept.contains(&key.as_str()))
                .cloned()
                .collect()
        };
        Self {
            annotations: outside(&metadata.annotations, KEPT_ANNOTATIONS),
            labels: outside(&metadata.labels, KEPT_LABELS),
            owner_references: !metadata.owner_references.is_empty(),
            finalizers: !metadata.finalizers.is_empty(),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(
            self.annotations
                .iter()
                .map(|key| format!("annotation {key}")),
        );
        parts.extend(self.labels.iter().map(|key| format!("label {key}")));
        if self.owner_references {
            parts.push("ownerReferences".to_owned());
        }
        if self.finalizers {
            parts.push("finalizers".to_owned());
        }
        parts.join(", ")
    }
}

/// Whether the Secret is the object tools/migrate filled: it records its own uid.
fn is_bound(metadata: &Metadata) -> bool {
    !metadata.uid.is_empty() && metadata.annotations.get(UID_ANNOTATION) == Some(&metadata.uid)
}

/// What the run does with one Secret: the generation it records, and for each owner the
/// password its key holds or why it gets a new one.
struct Plan {
    generation: u64,
    keys: Vec<(String, Result<Password, Why>)>,
    /// Removed by the patch: empty unless the run binds the Secret now.
    foreign: Foreign,
}

/// What to do with `secret` when the run asks for generation `requested`. Every key is renewed
/// when the Secret is not bound to its own uid, or records no generation it can read, or an older
/// one; a newer recorded generation wins over the requested one (generations only go up). Then
/// each key is kept when it holds a usable password, and renewed when it is missing or unusable.
fn plan(secret: &Secret, owners: &[&str], requested: u64) -> Plan {
    let annotations = &secret.metadata.annotations;
    let bound = is_bound(&secret.metadata);
    let recorded = annotations
        .get(GENERATION_ANNOTATION)
        .and_then(|text| parse_generation(text));
    let (renew_all, generation) = match recorded {
        _ if !bound => (Some(Why::Unbound), requested),
        Some(recorded) if recorded >= requested => (None, recorded),
        _ => (Some(Why::Generation), requested),
    };
    let keys = owners
        .iter()
        .map(|owner| {
            let decided = match (secret.data.get(*owner), renew_all) {
                (None, _) => Err(Why::Missing),
                (Some(_), Some(why)) => Err(why),
                (Some(value), None) => STANDARD
                    .decode(value)
                    .ok()
                    .and_then(|bytes| Password::from_bytes(owner, bytes).ok())
                    .ok_or(Why::Unusable),
            };
            ((*owner).to_owned(), decided)
        })
        .collect();
    // An already bound Secret keeps its metadata. Whoever changed it holds patch on it, which
    // already lets them write a password of their choosing that the Job then sets, so patch on
    // these Secrets is for trusted principals only (the chart grants it to the Job alone;
    // docs/architecture/secrets.md).
    let foreign = if bound {
        Foreign::default()
    } else {
        Foreign::of(&secret.metadata)
    };
    Plan {
        generation,
        keys,
        foreign,
    }
}

/// The merge patch that stores `generated`, records `generation` and the Secret's `uid`, and
/// removes `foreign`, valid only while the Secret is still at `resource_version`.
fn patch_body(
    resource_version: &str,
    uid: &str,
    generation: u64,
    generated: &[(String, Password)],
    foreign: &Foreign,
) -> String {
    let data: Map<String, Value> = generated
        .iter()
        .map(|(owner, password)| {
            (
                owner.clone(),
                Value::String(STANDARD.encode(password.expose())),
            )
        })
        .collect();
    let mut annotations: Map<String, Value> = foreign
        .annotations
        .iter()
        .map(|key| (key.clone(), Value::Null))
        .collect();
    annotations.insert(
        GENERATION_ANNOTATION.to_owned(),
        Value::String(generation.to_string()),
    );
    annotations.insert(UID_ANNOTATION.to_owned(), Value::String(uid.to_owned()));
    let mut metadata = Map::new();
    metadata.insert(
        "resourceVersion".to_owned(),
        Value::String(resource_version.to_owned()),
    );
    metadata.insert("annotations".to_owned(), Value::Object(annotations));
    if !foreign.labels.is_empty() {
        let labels = foreign
            .labels
            .iter()
            .map(|key| (key.clone(), Value::Null))
            .collect();
        metadata.insert("labels".to_owned(), Value::Object(labels));
    }
    if foreign.owner_references {
        metadata.insert("ownerReferences".to_owned(), Value::Null);
    }
    if foreign.finalizers {
        metadata.insert("finalizers".to_owned(), Value::Null);
    }
    json!({ "metadata": metadata, "data": data }).to_string()
}

/// What a request to the API came to.
enum Answer {
    Secret(Secret),
    /// HTTP 409: the Secret changed since it was read.
    Conflict,
}

/// How to reach the Secrets of the release namespace.
#[derive(Clone, Copy)]
pub(crate) struct Access<'a> {
    pub api_url: &'a str,
    pub namespace: &'a str,
    pub token_file: &'a Path,
    pub ca_file: &'a Path,
}

impl<'a> From<&'a PasswordSettings> for Access<'a> {
    fn from(settings: &'a PasswordSettings) -> Self {
        Self {
            api_url: &settings.api_url,
            namespace: &settings.namespace,
            token_file: &settings.token_file,
            ca_file: &settings.ca_file,
        }
    }
}

/// What the errors of one caller tell an operator to do: when the Secret does not exist, and
/// when the API refuses access to it.
#[derive(Clone, Copy)]
pub(crate) struct Hints {
    pub missing: &'static str,
    pub refused: &'static str,
}

/// The migration Job's hints for the Secrets it fills.
const JOB_FILLS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the Job fills it: run helm \
              upgrade, which creates it again, with migrate.revision changed so the Job runs \
              again (docs/runbooks/migrations.md)",
    refused: "the chart's Role lets the Job's ServiceAccount get and patch exactly the Secrets it \
              fills",
};

/// The Secrets endpoint of the release namespace.
pub(crate) struct Api {
    client: http::Client,
    base: String,
    token_file: PathBuf,
    wait: Duration,
    hints: Hints,
}

impl Api {
    pub(crate) fn new(access: Access<'_>, wait: Duration, hints: Hints) -> Result<Self, Error> {
        let client = if access.api_url.starts_with("https://") {
            http::trusting(access.ca_file).map_err(|error| Error::Kube(error.to_string()))?
        } else {
            http::plain()
        };
        Ok(Self {
            client,
            base: format!(
                "{}/api/v1/namespaces/{}/secrets",
                access.api_url.trim_end_matches('/'),
                access.namespace
            ),
            token_file: access.token_file.to_path_buf(),
            wait,
            hints,
        })
    }

    /// `GET` or, with a body, `PATCH` the Secret `name`. An API that does not answer, answers
    /// 429 or a 5xx is asked again until the wait runs out; every other answer is final.
    async fn request(&self, name: &str, patch: Option<&str>) -> Result<Answer, Error> {
        let url = format!("{}/{name}", self.base);
        let verb = if patch.is_some() { "patch" } else { "get" };
        let failed = |problem: String| Error::Secret {
            secret: name.to_owned(),
            problem,
        };
        let started = Instant::now();
        let mut reported = None::<Instant>;
        loop {
            let token = tokio::fs::read_to_string(&self.token_file)
                .await
                .map_err(|error| {
                    failed(format!(
                        "cannot read the ServiceAccount token {}: {error}",
                        self.token_file.display()
                    ))
                })?;
            let auth = http::Auth::Bearer(token.trim());
            let sent = match patch {
                Some(body) => {
                    self.client
                        .merge_patch(
                            &url,
                            &[("fieldManager", FIELD_MANAGER)],
                            auth,
                            body.to_owned(),
                        )
                        .await
                }
                None => self.client.get(&url, auth).await,
            };
            let retry = match sent {
                Err(error) => format!("the API did not answer: {error}"),
                Ok(response) => match response.status.as_u16() {
                    200 => {
                        return serde_json::from_str::<Secret>(&response.body)
                            .map(Answer::Secret)
                            .map_err(|_| {
                                failed("the API answered something that is not a Secret".to_owned())
                            });
                    }
                    409 if patch.is_some() => return Ok(Answer::Conflict),
                    404 => return Err(failed(self.hints.missing.to_owned())),
                    status @ (401 | 403) => {
                        return Err(failed(format!(
                            "the API refused to {verb} it (HTTP {status}); {}",
                            self.hints.refused
                        )));
                    }
                    status @ (429 | 500..=599) => format!("the API answered HTTP {status}"),
                    status => {
                        return Err(failed(format!(
                            "the API refused to {verb} it: HTTP {status}{}",
                            reason(&response.body)
                        )));
                    }
                },
            };
            if started.elapsed() >= self.wait {
                return Err(failed(format!("gave up after {:?}: {retry}", self.wait)));
            }
            if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                tracing::warn!(secret = name, state = %retry, "cannot {verb} the Secret yet; retrying");
                reported = Some(Instant::now());
            }
            tokio::time::sleep(RETRY_EVERY).await;
        }
    }
}

/// The `reason` of a Kubernetes `Status` body (for example `Invalid`), never its message, which
/// may quote what the request carried.
fn reason(body: &str) -> String {
    #[derive(Deserialize)]
    struct Status {
        #[serde(default)]
        reason: String,
    }
    match serde_json::from_str::<Status>(body) {
        Ok(status)
            if !status.reason.is_empty()
                && status
                    .reason
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()) =>
        {
            format!(" ({})", status.reason)
        }
        _ => String::new(),
    }
}

/// The password that the Secret `secret` holds for `owner`, read once: `None` while the Secret
/// is not the object tools/migrate filled (not filled yet, or created by someone else) or the key
/// holds no usable password. Never a value someone else put into an object of their own.
///
/// # Errors
///
/// [`Error::Secret`] when the Secret is missing, is being deleted, or the API refuses access or
/// does not answer within the API's wait.
pub(crate) async fn read_bound(
    api: &Api,
    secret: &str,
    owner: &str,
) -> Result<Option<Password>, Error> {
    let found = match api.request(secret, None).await? {
        Answer::Secret(found) => found,
        // Only a patch is answered with a conflict.
        Answer::Conflict => return Ok(None),
    };
    if found.metadata.deletion_timestamp.is_some() {
        return Err(Error::Secret {
            secret: secret.to_owned(),
            problem: "is being deleted (metadata.deletionTimestamp is set)".to_owned(),
        });
    }
    if !is_bound(&found.metadata) {
        return Ok(None);
    }
    Ok(found
        .data
        .get(owner)
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|bytes| Password::from_bytes(owner, bytes).ok()))
}

/// Fills one Secret and returns the password of each owner.
pub(crate) async fn fill(
    api: &Api,
    secret: &str,
    owners: &[&str],
    requested: u64,
    report: &mut Report,
) -> Result<BTreeMap<String, Password>, Error> {
    for _ in 0..MAX_CONFLICTS {
        let found = match api.request(secret, None).await? {
            Answer::Secret(found) => found,
            // Only a patch is answered with a conflict.
            Answer::Conflict => continue,
        };
        if found.metadata.uid.is_empty() {
            return Err(Error::Secret {
                secret: secret.to_owned(),
                problem: "the API returned it without metadata.uid".to_owned(),
            });
        }
        if found.metadata.deletion_timestamp.is_some() {
            // Writing passwords into it, and setting roles to them, would leave nothing for the
            // services to read once it is gone (and the patch that binds it removes finalizers
            // that may be holding it).
            return Err(Error::Secret {
                secret: secret.to_owned(),
                problem: "is being deleted (metadata.deletionTimestamp is set). Once it is gone, \
                          run helm upgrade, which creates it again, with migrate.revision changed \
                          so the Job runs again (docs/runbooks/migrations.md)"
                    .to_owned(),
            });
        }
        let Plan {
            generation,
            keys,
            foreign,
        } = plan(&found, owners, requested);
        let mut passwords = BTreeMap::new();
        let mut generated = Vec::new();
        for (owner, decided) in keys {
            match decided {
                Ok(password) => {
                    passwords.insert(owner, password);
                }
                Err(why) => {
                    let password =
                        Password::generate().map_err(|error| Error::Random(error.to_string()))?;
                    generated.push((owner, password, why));
                }
            }
        }
        if generated.is_empty() {
            return Ok(passwords);
        }
        let new: Vec<(String, Password)> = generated
            .iter()
            .map(|(owner, password, _)| (owner.clone(), password.clone()))
            .collect();
        let body = patch_body(
            &found.metadata.resource_version,
            &found.metadata.uid,
            generation,
            &new,
            &foreign,
        );
        match api.request(secret, Some(&body)).await? {
            Answer::Conflict => {
                tracing::info!(
                    secret,
                    "the Secret changed while the run filled it; reading it again"
                );
            }
            Answer::Secret(stored) => {
                // The API returns the stored object; anything but the values just written (an
                // admission webhook that rewrites Secrets, say) would leave a role and its
                // Secret apart, so it stops the run.
                for (owner, password) in &new {
                    let kept = stored
                        .data
                        .get(owner)
                        .and_then(|value| STANDARD.decode(value).ok())
                        .is_some_and(|bytes| bytes == password.expose().as_bytes());
                    if !kept {
                        return Err(Error::Secret {
                            secret: secret.to_owned(),
                            problem: format!(
                                "the API stored another value for {owner} than the run wrote"
                            ),
                        });
                    }
                }
                if !foreign.is_empty() {
                    report.change(format!(
                        "removed from Secret {secret} what neither the chart nor the Job sets: {}",
                        foreign.describe()
                    ));
                }
                for (owner, password, why) in generated {
                    report.change(format!(
                        "generated the password of {owner} in Secret {secret} ({})",
                        why.describe(generation)
                    ));
                    passwords.insert(owner, password);
                }
                return Ok(passwords);
            }
        }
    }
    Err(Error::Secret {
        secret: secret.to_owned(),
        problem: format!(
            "changed {MAX_CONFLICTS} times while the run filled it; something else writes it"
        ),
    })
}

/// Fills every Secret of `settings` and returns the passwords the run sets. Makes no request
/// when nothing is bound.
///
/// # Errors
///
/// [`Error::Secret`] when a Secret is missing, the API refuses access, or the API did not answer
/// within `wait`; [`Error::Random`] without randomness.
pub async fn converge(
    settings: &PasswordSettings,
    wait: Duration,
    report: &mut Report,
) -> Result<Passwords, Error> {
    let invalid = |error: service_kit::config::Invalid| {
        Error::Kube(format!("{}: {}", error.key, error.message))
    };
    let postgres = settings.postgres().map_err(invalid)?;
    let collector = settings.clickhouse_collector().map_err(invalid)?;
    let mut by_secret: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for Binding { owner, secret } in postgres.iter().chain(&collector) {
        by_secret.entry(secret).or_default().push(owner);
    }
    if by_secret.is_empty() {
        return Ok(Passwords::default());
    }
    let requested = parse_generation(&settings.generation).ok_or_else(|| {
        Error::Kube(format!(
            "TRUSTIOT__PASSWORDS__GENERATION: {:?} is not a password generation (1 to 9 decimal digits)",
            settings.generation
        ))
    })?;
    let api = Api::new(settings.into(), wait, JOB_FILLS)?;
    let mut filled: BTreeMap<(String, String), Password> = BTreeMap::new();
    for (secret, owners) in &by_secret {
        for (owner, password) in fill(&api, secret, owners, requested, report).await? {
            filled.insert(((*secret).to_owned(), owner), password);
        }
    }
    let mut take = |binding: &Binding| {
        filled
            .remove(&(binding.secret.clone(), binding.owner.clone()))
            .ok_or_else(|| Error::Secret {
                secret: binding.secret.clone(),
                problem: format!("no password for {} after filling it", binding.owner),
            })
    };
    let mut passwords = Passwords::default();
    for binding in &postgres {
        passwords
            .postgres
            .insert(binding.owner.clone(), take(binding)?);
    }
    if let Some(binding) = &collector {
        passwords.collector = Some((binding.owner.clone(), take(binding)?));
    }
    Ok(passwords)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "6a1f2c3e-0000-4000-8000-000000000001";

    /// Each owner and the password it keeps, or why it gets a new one.
    type Decisions = Vec<(String, Result<String, Why>)>;

    /// A Secret the Job filled (bound to its uid) that records `generation`.
    fn secret(generation: Option<&str>, data: &[(&str, &str)]) -> Secret {
        let mut annotations = BTreeMap::new();
        if let Some(generation) = generation {
            annotations.insert(GENERATION_ANNOTATION.to_owned(), generation.to_owned());
        }
        annotations.insert(UID_ANNOTATION.to_owned(), UID.to_owned());
        Secret {
            metadata: Metadata {
                resource_version: "7".to_owned(),
                uid: UID.to_owned(),
                annotations,
                ..Metadata::default()
            },
            data: data
                .iter()
                .map(|(key, value)| ((*key).to_owned(), STANDARD.encode(value)))
                .collect(),
        }
    }

    fn decisions(found: &Secret, owners: &[&str]) -> Decisions {
        decisions_for(found, owners, 2).1
    }

    fn decisions_for(found: &Secret, owners: &[&str], requested: u64) -> (u64, Decisions) {
        let Plan {
            generation, keys, ..
        } = plan(found, owners, requested);
        let keys = keys
            .into_iter()
            .map(|(owner, decided)| (owner, decided.map(|password| password.expose().to_owned())))
            .collect();
        (generation, keys)
    }

    const GOOD: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn keys_are_kept_only_when_present_usable_and_of_this_generation() {
        let owners = ["a_app", "b_app"];
        // A first install: an empty Secret without the annotation.
        assert_eq!(
            decisions(&secret(None, &[]), &owners),
            [
                ("a_app".to_owned(), Err(Why::Missing)),
                ("b_app".to_owned(), Err(Why::Missing)),
            ]
        );
        // A rotation renews every key, usable or not; so does a Secret that records no
        // generation (someone filled it by hand), or one it cannot read.
        for recorded in [Some("1"), None, Some("v2"), Some("02x"), Some("1234567890")] {
            assert_eq!(
                decisions(
                    &secret(recorded, &[("a_app", GOOD), ("b_app", GOOD)]),
                    &owners
                ),
                [
                    ("a_app".to_owned(), Err(Why::Generation)),
                    ("b_app".to_owned(), Err(Why::Generation)),
                ]
            );
        }
        // This generation: keep what is usable, renew only what is missing or unusable.
        assert_eq!(
            decisions(
                &secret(Some("2"), &[("a_app", GOOD), ("b_app", "short")]),
                &owners
            ),
            [
                ("a_app".to_owned(), Ok(GOOD.to_owned())),
                ("b_app".to_owned(), Err(Why::Unusable)),
            ]
        );
        assert_eq!(
            decisions(&secret(Some("2"), &[("b_app", GOOD)]), &owners),
            [
                ("a_app".to_owned(), Err(Why::Missing)),
                ("b_app".to_owned(), Ok(GOOD.to_owned())),
            ]
        );
        // Not base64, or a value with a newline: unusable, never passed on.
        let mut broken = secret(Some("2"), &[("b_app", &format!("{GOOD}\n"))]);
        broken.data.insert("a_app".to_owned(), "%%%".to_owned());
        assert_eq!(
            decisions(&broken, &owners),
            [
                ("a_app".to_owned(), Err(Why::Unusable)),
                ("b_app".to_owned(), Err(Why::Unusable)),
            ]
        );
    }

    #[test]
    fn generations_only_go_up() {
        let owners = ["a_app", "b_app"];
        let full = secret(Some("3"), &[("a_app", GOOD), ("b_app", GOOD)]);
        // A rollback asks for an older generation: the passwords stay, and the Secret keeps
        // recording 3, so the re-upgrade to 3 changes nothing either.
        for requested in [1, 2, 3] {
            assert_eq!(
                decisions_for(&full, &owners, requested),
                (
                    3,
                    vec![
                        ("a_app".to_owned(), Ok(GOOD.to_owned())),
                        ("b_app".to_owned(), Ok(GOOD.to_owned())),
                    ]
                ),
                "requested {requested}"
            );
        }
        // A key lost under an older requested generation is filled under the recorded one.
        assert_eq!(
            decisions_for(&secret(Some("3"), &[("a_app", GOOD)]), &owners, 1),
            (
                3,
                vec![
                    ("a_app".to_owned(), Ok(GOOD.to_owned())),
                    ("b_app".to_owned(), Err(Why::Missing)),
                ]
            )
        );
        // A newer one rotates and records it.
        assert_eq!(
            decisions_for(&full, &owners, 4),
            (
                4,
                vec![
                    ("a_app".to_owned(), Err(Why::Generation)),
                    ("b_app".to_owned(), Err(Why::Generation)),
                ]
            )
        );
    }

    #[test]
    fn data_is_trusted_only_in_the_object_the_job_filled() {
        let owners = ["a_app", "b_app"];
        // Someone created the Secret with a password of their choosing and a matching
        // generation, but cannot know the uid the API gave it: every key is renewed.
        let mut planted = secret(Some("2"), &[("a_app", GOOD), ("b_app", GOOD)]);
        planted.metadata.uid = "6a1f2c3e-0000-4000-8000-000000000002".to_owned();
        let mut unrecorded = secret(Some("2"), &[("a_app", GOOD), ("b_app", GOOD)]);
        unrecorded.metadata.annotations.remove(UID_ANNOTATION);
        for found in [&planted, &unrecorded] {
            assert_eq!(
                decisions_for(found, &owners, 2),
                (
                    2,
                    vec![
                        ("a_app".to_owned(), Err(Why::Unbound)),
                        ("b_app".to_owned(), Err(Why::Unbound)),
                    ]
                )
            );
        }
        // Even with a newer generation recorded: the generation of an unbound Secret is not
        // trusted either.
        let mut newer = secret(Some("9"), &[("a_app", GOOD)]);
        newer.metadata.uid = "other".to_owned();
        assert_eq!(decisions_for(&newer, &owners, 2).0, 2);
    }

    #[test]
    fn generations_are_decimal_digits() {
        assert_eq!(parse_generation("1"), Some(1));
        assert_eq!(parse_generation("999999999"), Some(999_999_999));
        for bad in ["", "v2", "1.0", "-1", " 1", "1234567890", "٣"] {
            assert_eq!(parse_generation(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_patch_carries_the_resource_version_the_generation_the_uid_and_only_the_new_keys() {
        let password = Password::new("a_app", GOOD.to_owned()).expect("valid");
        let body: Value = serde_json::from_str(&patch_body(
            "41",
            UID,
            3,
            &[("a_app".to_owned(), password)],
            &Foreign::default(),
        ))
        .expect("JSON");
        assert_eq!(
            body,
            json!({
                "metadata": {
                    "resourceVersion": "41",
                    "annotations": { GENERATION_ANNOTATION: "3", UID_ANNOTATION: UID },
                },
                "data": { "a_app": STANDARD.encode(GOOD) },
            })
        );
    }

    /// A Secret someone else made: chart and Helm metadata, plus their own.
    fn foreign_secret() -> Secret {
        let pairs = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        Secret {
            metadata: Metadata {
                resource_version: "9".to_owned(),
                uid: UID.to_owned(),
                annotations: pairs(&[
                    ("helm.sh/resource-policy", "keep"),
                    ("meta.helm.sh/release-name", "r"),
                    ("meta.helm.sh/release-namespace", "ns"),
                    (GENERATION_ANNOTATION, "1"),
                    ("replicator.example/replicate-to", "elsewhere"),
                    ("reflector.example/reflection-allowed", "true"),
                ]),
                labels: pairs(&[
                    ("app.kubernetes.io/instance", "r"),
                    ("trustiot.io/filled-by", "migrate"),
                    ("copy-me", "yes"),
                ]),
                owner_references: vec![json!({"kind": "ConfigMap", "name": "x", "uid": "u"})],
                finalizers: vec!["example.com/hold".to_owned()],
                deletion_timestamp: None,
            },
            data: BTreeMap::new(),
        }
    }

    #[test]
    fn binding_a_secret_removes_metadata_neither_the_chart_nor_the_job_sets() {
        let found = foreign_secret();
        let foreign = plan(&found, &["a_app"], 1).foreign;
        assert_eq!(
            foreign,
            Foreign {
                annotations: vec![
                    "reflector.example/reflection-allowed".to_owned(),
                    "replicator.example/replicate-to".to_owned()
                ],
                labels: vec!["copy-me".to_owned()],
                owner_references: true,
                finalizers: true,
            }
        );
        assert_eq!(
            foreign.describe(),
            "annotation reflector.example/reflection-allowed, annotation \
             replicator.example/replicate-to, label copy-me, ownerReferences, finalizers"
        );
        let password = Password::new("a_app", GOOD.to_owned()).expect("valid");
        let body: Value = serde_json::from_str(&patch_body(
            "9",
            UID,
            1,
            &[("a_app".to_owned(), password)],
            &foreign,
        ))
        .expect("JSON");
        assert_eq!(
            body,
            json!({
                "metadata": {
                    "resourceVersion": "9",
                    "annotations": {
                        "reflector.example/reflection-allowed": null,
                        "replicator.example/replicate-to": null,
                        GENERATION_ANNOTATION: "1",
                        UID_ANNOTATION: UID,
                    },
                    "labels": { "copy-me": null },
                    "ownerReferences": null,
                    "finalizers": null,
                },
                "data": { "a_app": STANDARD.encode(GOOD) },
            })
        );
        // Once bound, the Secret's metadata is left alone.
        let mut bound = foreign_secret();
        bound
            .metadata
            .annotations
            .insert(UID_ANNOTATION.to_owned(), UID.to_owned());
        assert!(plan(&bound, &["a_app"], 1).foreign.is_empty());
    }

    #[test]
    fn only_a_plain_status_reason_is_quoted() {
        assert_eq!(
            reason(r#"{"kind":"Status","reason":"Invalid","message":"data[x]: v"}"#),
            " (Invalid)"
        );
        assert_eq!(reason(r#"{"reason":"a value: with text"}"#), "");
        assert_eq!(reason("not json"), "");
    }
}
