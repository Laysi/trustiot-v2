//! The passwords of the service login roles and of the collector's ClickHouse account. Nobody
//! creates, copies or rotates them (docs/architecture/secrets.md):
//!
//! - the chart declares one Secret per consumer (each service that uses Postgres, and the
//!   collector) without any data, so it never renders a password;
//! - this module, run by the Job while it holds the migration lock, reads each Secret, generates
//!   a 256-bit password for every key it manages that is missing, unusable, or was made for
//!   another password generation (`migrate.passwordGeneration`), and writes it with one JSON
//!   merge patch that carries the Secret's `resourceVersion`, so a concurrent change of the Secret
//!   makes the patch fail instead of being overwritten;
//! - the rest of the run then sets each role and account to the password in its Secret.
//!
//! The Job's ServiceAccount may only `get` and `patch` exactly these Secrets (the chart's Role
//! lists them by name). Nothing here logs a password, a hash of one, or a Secret's data: reports
//! and errors name roles, accounts and Secrets only.

use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// The Secret's passwords were made for another generation, or it records none: a rotation.
    Generation,
}

impl Why {
    fn describe(self, generation: &str) -> String {
        match self {
            Self::Missing => "it had none".to_owned(),
            Self::Unusable => "the stored one was not usable".to_owned(),
            Self::Generation => format!("password generation {generation}"),
        }
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
    annotations: BTreeMap<String, String>,
}

/// For each owner: the password its key holds, or why it needs a new one.
fn plan(
    secret: &Secret,
    owners: &[&str],
    generation: &str,
) -> Vec<(String, Result<Password, Why>)> {
    let current = secret
        .metadata
        .annotations
        .get(GENERATION_ANNOTATION)
        .map(String::as_str)
        == Some(generation);
    owners
        .iter()
        .map(|owner| {
            let decided = match secret.data.get(*owner) {
                None => Err(Why::Missing),
                Some(_) if !current => Err(Why::Generation),
                Some(value) => STANDARD
                    .decode(value)
                    .ok()
                    .and_then(|bytes| Password::from_bytes(owner, bytes).ok())
                    .ok_or(Why::Unusable),
            };
            ((*owner).to_owned(), decided)
        })
        .collect()
}

/// The merge patch that stores `generated` and records `generation`, valid only while the
/// Secret is still at `resource_version`.
fn patch_body(
    resource_version: &str,
    generation: &str,
    generated: &[(String, Password)],
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
    json!({
        "metadata": {
            "resourceVersion": resource_version,
            "annotations": { GENERATION_ANNOTATION: generation },
        },
        "data": data,
    })
    .to_string()
}

/// What a request to the API came to.
enum Answer {
    Secret(Secret),
    /// HTTP 409: the Secret changed since it was read.
    Conflict,
}

/// The Secrets endpoint of the release namespace.
struct Api {
    client: http::Client,
    base: String,
    token_file: PathBuf,
    wait: Duration,
}

impl Api {
    fn new(settings: &PasswordSettings, wait: Duration) -> Result<Self, Error> {
        let client = if settings.api_url.starts_with("https://") {
            http::trusting(&settings.ca_file).map_err(|error| Error::Kube(error.to_string()))?
        } else {
            http::plain()
        };
        Ok(Self {
            client,
            base: format!(
                "{}/api/v1/namespaces/{}/secrets",
                settings.api_url.trim_end_matches('/'),
                settings.namespace
            ),
            token_file: settings.token_file.clone(),
            wait,
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
                    404 => {
                        return Err(failed(
                            "does not exist. The chart declares it without data and the Job fills \
                             it: run helm upgrade, which creates it again, with migrate.revision \
                             changed so the Job runs again (docs/runbooks/migrations.md)"
                                .to_owned(),
                        ));
                    }
                    status @ (401 | 403) => {
                        return Err(failed(format!(
                            "the API refused to {verb} it (HTTP {status}); the chart's Role lets the \
                             Job's ServiceAccount get and patch exactly the Secrets it fills"
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

/// Fills one Secret and returns the password of each owner.
async fn fill(
    api: &Api,
    secret: &str,
    owners: &[&str],
    generation: &str,
    report: &mut Report,
) -> Result<BTreeMap<String, Password>, Error> {
    for _ in 0..MAX_CONFLICTS {
        let found = match api.request(secret, None).await? {
            Answer::Secret(found) => found,
            // Only a patch is answered with a conflict.
            Answer::Conflict => continue,
        };
        let mut passwords = BTreeMap::new();
        let mut generated = Vec::new();
        for (owner, decided) in plan(&found, owners, generation) {
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
        let body = patch_body(&found.metadata.resource_version, generation, &new);
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
    let api = Api::new(settings, wait)?;
    let mut filled: BTreeMap<(String, String), Password> = BTreeMap::new();
    for (secret, owners) in &by_secret {
        for (owner, password) in fill(&api, secret, owners, &settings.generation, report).await? {
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

    fn secret(generation: Option<&str>, data: &[(&str, &str)]) -> Secret {
        let mut annotations = BTreeMap::new();
        if let Some(generation) = generation {
            annotations.insert(GENERATION_ANNOTATION.to_owned(), generation.to_owned());
        }
        Secret {
            metadata: Metadata {
                resource_version: "7".to_owned(),
                annotations,
            },
            data: data
                .iter()
                .map(|(key, value)| ((*key).to_owned(), STANDARD.encode(value)))
                .collect(),
        }
    }

    fn decisions(found: &Secret, owners: &[&str]) -> Vec<(String, Result<String, Why>)> {
        plan(found, owners, "2")
            .into_iter()
            .map(|(owner, decided)| (owner, decided.map(|password| password.expose().to_owned())))
            .collect()
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
        // generation (someone filled it by hand).
        for recorded in [Some("1"), None] {
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
    fn the_patch_carries_the_resource_version_the_generation_and_only_the_new_keys() {
        let password = Password::new("a_app", GOOD.to_owned()).expect("valid");
        let body: Value =
            serde_json::from_str(&patch_body("41", "3", &[("a_app".to_owned(), password)]))
                .expect("JSON");
        assert_eq!(
            body,
            json!({
                "metadata": {
                    "resourceVersion": "41",
                    "annotations": { GENERATION_ANNOTATION: "3" },
                },
                "data": { "a_app": STANDARD.encode(GOOD) },
            })
        );
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
