//! The internal identity signing keys (decision 27; Laysi/trustiot-v2#404). Nobody creates,
//! copies or rotates them (docs/architecture/secrets.md, 「內部身分簽章金鑰」):
//!
//! - the chart declares the Secret `<release>-identity-signing-keys` and the ConfigMap
//!   `<release>-internal-identity-keys` without any data;
//! - [`converge`], run as `migrate identity-signing-keys` by the identity pod's init container
//!   with the identity ServiceAccount's token, keeps the key set
//!   ([`identity_keys::SigningKeys`]) in the Secret and its public JWK Set in the ConfigMap, in
//!   three steps that each are one patch carrying the object's `resourceVersion`:
//!   1. the Secret gets what [`identity_keys::SigningKeys::plan`] decides (a first key, a key
//!      for a new generation, revoked and retired keys gone), new keys without times;
//!   2. the ConfigMap gets the public key of every key in the Secret;
//!   3. only then the Secret gives each new key its `published_at` (now) and `signs_from`.
//!
//!   A run that stops after any step is finished by the next; two runs at once meet a 409 and
//!   the loser reads again and finds the work done. The ConfigMap is written only while the
//!   Secret is still the version it was derived from, so a run that read an older Secret cannot
//!   take a newer key out of it.
//! - the Secret's data is trusted only while it records the Secret's own uid (the rules of
//!   src/passwords.rs): an object someone else made, restored or emptied is treated as lost, and
//!   the run starts a new set whose key signs at once.
//!
//! Only the identity ServiceAccount may `get` and `patch` these two objects (the chart's Role
//! names them), and only the init container mounts its token. Nothing here logs a private key
//! or the Secret's data: reports and errors name key ids, generations and times. The key set,
//! the patches that carry it and the Secret as read are wiped from memory when dropped; what the
//! HTTP client and TLS keep in their own buffers is not, which the init container bounds by
//! exiting once the run is done.

use std::io::Read as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use identity_keys::{
    JWKS_FILE, MAX_DOCUMENT_BYTES, Plan, Request, SIGNING_KEYS_FILE, SigningKey, SigningKeys,
};
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use crate::config::KeySettings;
use crate::passwords::{
    self, Access, Answer, Api, Foreign, Hints, Kind, MAX_CONFLICTS, Secret, binding_metadata,
};
use crate::{Error, Report};

/// What the errors about the Secret tell an operator.
const SECRET_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the identity service's init \
              container fills it: increase internalIdentity.keyGeneration and run helm upgrade, \
              which creates it again; the init container then starts a new key set whose key \
              signs at once (docs/runbooks/secrets.md)",
    refused: "the chart's Role lets the identity service's ServiceAccount get and patch exactly \
              this Secret and the ConfigMap of the public keys",
};

/// What the errors about the ConfigMap tell an operator.
const CONFIG_MAP_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data: run helm upgrade, which creates \
              it again, then kubectl rollout restart deployment/<release>-identity so the init \
              container fills it (docs/runbooks/secrets.md)",
    refused: "the chart's Role lets the identity service's ServiceAccount get and patch exactly \
              this ConfigMap and the Secret of the signing keys",
};

/// Largest key file of a person's key read (a PKCS#8 PEM Ed25519 key is about 120 bytes).
const MAX_LEGACY_KEY_BYTES: u64 = 16 * 1024;

/// Seconds since the Unix epoch. A clock before the epoch reads as 0: new keys then look
/// published long ago, which only makes them sign at once, and nothing retires.
#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// The init container's run at the current time; see [`converge_at`].
///
/// # Errors
///
/// As [`converge_at`].
pub async fn converge(settings: &KeySettings, report: &mut Report) -> Result<(), Error> {
    converge_at(settings, report, unix_now).await
}

/// The init container's run, with `clock` for the current time (seconds since the Unix epoch):
/// the three steps of the module documentation, until the Secret and the ConfigMap agree and
/// every key is published.
///
/// # Errors
///
/// When either object is missing or being deleted, the API refuses access or does not answer
/// within `settings.wait`, another writer keeps changing them, there is no randomness, the set
/// would hold too many keys, or a person's key cannot be imported. The identity pod then does not
/// start; the pods already running keep their keys.
pub async fn converge_at(
    settings: &KeySettings,
    report: &mut Report,
    clock: impl Fn() -> i64,
) -> Result<(), Error> {
    let invalid =
        |key: &str| Error::SigningKeys(format!("TRUSTIOT__KEYS__{key}: not a generation"));
    let generation =
        passwords::parse_generation(&settings.generation).ok_or_else(|| invalid("GENERATION"))?;
    let revoke_before = passwords::parse_generation(&settings.revoke_before_generation)
        .ok_or_else(|| invalid("REVOKE_BEFORE_GENERATION"))?;
    let propagation = i64::try_from(settings.propagation_seconds).map_err(|_| {
        Error::SigningKeys("TRUSTIOT__KEYS__PROPAGATION_SECONDS: too large".to_owned())
    })?;
    let legacy = legacy_key(settings)?;
    let request = Request {
        generation,
        revoke_before,
        legacy: legacy.as_ref(),
    };
    let access = Access {
        api_url: &settings.api_url,
        namespace: &settings.namespace,
        token_file: &settings.token_file,
        ca_file: &settings.ca_file,
    };
    let secrets = Api::new(access, settings.wait, SECRET_HINTS)?;
    let config_maps = Api::of(Kind::ConfigMap, access, settings.wait, CONFIG_MAP_HINTS)?;
    let objects = Objects {
        secrets: &secrets,
        secret: &settings.secret,
        config_maps: &config_maps,
        config_map: &settings.config_map,
    };
    for _ in 0..MAX_CONFLICTS {
        // Step 1: the key set the request asks for, new keys unpublished.
        let found = objects.read_secret().await?;
        let uid = found.metadata.uid.clone();
        let Stored { keys, problem } = stored(&found);
        let bound = passwords::is_bound(&found.metadata);
        let foreign = if bound {
            Foreign::default()
        } else {
            Foreign::of(&found.metadata)
        };
        let Plan { keys, changes } = keys
            .plan(&request, clock(), SigningKey::generate)
            .map_err(|error| Error::SigningKeys(error.to_string()))?;
        let mut version = found.metadata.resource_version.clone();
        if !changes.is_empty() || problem.is_some() || !bound {
            let others: Vec<String> = found
                .data
                .keys()
                .filter(|key| *key != SIGNING_KEYS_FILE)
                .cloned()
                .collect();
            let body = secret_patch(&version, &uid, &keys, &foreign, &others);
            let Some(stored) = objects.write_secret(&body, &keys).await? else {
                continue;
            };
            version = stored;
            if !foreign.is_empty() {
                report.change(format!(
                    "removed from Secret {} what neither the chart nor the init container sets: \
                     {}",
                    settings.secret,
                    foreign.describe()
                ));
            }
            if !others.is_empty() {
                report.change(format!(
                    "removed from Secret {} the data it does not hold: {}",
                    settings.secret,
                    others.join(", ")
                ));
            }
            if let Some(problem) = problem {
                report.change(format!(
                    "started a new key set in Secret {}: {problem}",
                    settings.secret
                ));
            }
            for change in &changes {
                report.change(format!("{change} in Secret {}", settings.secret));
            }
        }

        // Step 2: every key's public key in the ConfigMap.
        if !objects.publish(&keys, &version, report).await? {
            continue;
        }

        // Step 3: the new keys' times.
        let mut keys = keys;
        if keys.has_unpublished() {
            let published = keys.publish(clock(), propagation);
            let body = secret_patch(&version, &uid, &keys, &Foreign::default(), &[]);
            if objects.write_secret(&body, &keys).await?.is_none() {
                continue;
            }
            for change in &published {
                report.change(format!("{change} in Secret {}", settings.secret));
            }
        }
        let now = clock();
        let signing = keys
            .signing_key(now, revoke_before, |_| true)
            .map(|key| key.kid().to_owned());
        tracing::info!(
            secret = %settings.secret,
            config_map = %settings.config_map,
            keys = ?keys.key_ids().collect::<Vec<_>>(),
            signing = signing.as_deref().unwrap_or("none yet"),
            retire_at = keys.retire_at(now),
            "the internal identity signing keys are in place"
        );
        return Ok(());
    }
    Err(Error::Secret {
        secret: settings.secret.clone(),
        problem: format!(
            "changed {MAX_CONFLICTS} times while the init container worked on it; something else \
             writes it or the ConfigMap"
        ),
    })
}

/// The Secret and the ConfigMap of one release.
struct Objects<'a> {
    secrets: &'a Api,
    secret: &'a str,
    config_maps: &'a Api,
    config_map: &'a str,
}

impl Objects<'_> {
    /// The Secret, which must have a uid and not be being deleted.
    async fn read_secret(&self) -> Result<Secret, Error> {
        let Answer::Secret(found) = self.secrets.request(self.secret, None).await? else {
            return Err(self.secret_error("the API answered a read with a conflict"));
        };
        if found.metadata.uid.is_empty() {
            return Err(self.secret_error("the API returned it without metadata.uid"));
        }
        if found.metadata.deletion_timestamp.is_some() {
            return Err(self.secret_error(
                "is being deleted (metadata.deletionTimestamp is set). Once it is gone, increase \
                 internalIdentity.keyGeneration and run helm upgrade, which creates it again \
                 (docs/runbooks/secrets.md)",
            ));
        }
        Ok(found)
    }

    /// Patches the Secret with `body`, which writes `keys`: the new `resourceVersion`, or `None`
    /// when the Secret changed since it was read.
    async fn write_secret(&self, body: &str, keys: &SigningKeys) -> Result<Option<String>, Error> {
        match self.secrets.request(self.secret, Some(body)).await? {
            Answer::Conflict => {
                tracing::info!(
                    secret = self.secret,
                    "the Secret changed while the init container wrote it; reading it again"
                );
                Ok(None)
            }
            Answer::Secret(stored) => {
                // The API returns the stored object; anything but the set just written (an
                // admission webhook that rewrites Secrets, say) would leave the identity service
                // signing with keys the services do not have.
                let written = keys.to_json();
                let kept = stored
                    .data
                    .get(SIGNING_KEYS_FILE)
                    .and_then(|value| STANDARD.decode(value).ok())
                    .map(Zeroizing::new)
                    .is_some_and(|bytes| bytes.as_slice() == written.as_bytes());
                if !kept {
                    return Err(
                        self.secret_error("the API stored another key set than was written")
                    );
                }
                Ok(Some(stored.metadata.resource_version.clone()))
            }
        }
    }

    /// Makes the ConfigMap hold the public keys of `keys`, read from the Secret at
    /// `secret_version`. `false` when the Secret has changed since: the caller starts over, so a
    /// run that read an older Secret never writes its keys over a newer run's, nor takes a
    /// ConfigMap that still shows its older keys as done.
    async fn publish(
        &self,
        keys: &SigningKeys,
        secret_version: &str,
        report: &mut Report,
    ) -> Result<bool, Error> {
        let jwks = keys.jwks();
        for _ in 0..MAX_CONFLICTS {
            let Answer::Secret(found) = self.config_maps.request(self.config_map, None).await?
            else {
                return Err(self.config_map_error("the API answered a read with a conflict"));
            };
            if found.metadata.deletion_timestamp.is_some() {
                return Err(self.config_map_error(
                    "is being deleted (metadata.deletionTimestamp is set). Once it is gone, run \
                     helm upgrade, which creates it again, then kubectl rollout restart \
                     deployment/<release>-identity (docs/runbooks/secrets.md)",
                ));
            }
            if found.data.get(JWKS_FILE) != Some(&jwks) {
                // Read after the ConfigMap: any run that changes the Secret later also writes the
                // ConfigMap later, and this patch fails if it does so first.
                if self.read_secret().await?.metadata.resource_version != secret_version {
                    return Ok(false);
                }
                let body = json!({
                    "metadata": {"resourceVersion": found.metadata.resource_version},
                    "data": {JWKS_FILE: jwks},
                })
                .to_string();
                match self
                    .config_maps
                    .request(self.config_map, Some(&body))
                    .await?
                {
                    Answer::Conflict => {
                        tracing::info!(
                            config_map = self.config_map,
                            "the ConfigMap changed while the init container wrote it; reading it \
                             again"
                        );
                        continue;
                    }
                    Answer::Secret(stored) => {
                        if stored.data.get(JWKS_FILE) != Some(&jwks) {
                            return Err(self.config_map_error(
                                "the API stored other public keys than were written",
                            ));
                        }
                        report.change(format!(
                            "published the public keys {} in ConfigMap {}",
                            keys.key_ids().collect::<Vec<_>>().join(", "),
                            self.config_map
                        ));
                    }
                }
            }
            // The ConfigMap shows this run's keys, which are current only while the Secret still
            // holds them: another run may have changed the Secret (a revocation, say) without
            // having published its keys yet, and the identity pod must not start on the old ones.
            // The caller starts over then and publishes what the Secret holds now.
            return Ok(self.read_secret().await?.metadata.resource_version == secret_version);
        }
        Err(self.config_map_error(&format!(
            "changed {MAX_CONFLICTS} times while the init container wrote it; something else \
             writes it"
        )))
    }

    fn secret_error(&self, problem: &str) -> Error {
        Error::Secret {
            secret: self.secret.to_owned(),
            problem: problem.to_owned(),
        }
    }

    fn config_map_error(&self, problem: &str) -> Error {
        Error::ConfigMap {
            config_map: self.config_map.to_owned(),
            problem: problem.to_owned(),
        }
    }
}

/// The key set a Secret holds, and why the run starts a new one instead, if it does.
struct Stored {
    keys: SigningKeys,
    problem: Option<String>,
}

/// The key set in `found`: empty, with the reason, when the Secret is not the object the init
/// container filled (it records another uid, or none) or does not hold a usable set; the reason
/// names the problem, never the data.
fn stored(found: &Secret) -> Stored {
    let lost = |problem: String| Stored {
        keys: SigningKeys::default(),
        problem: Some(problem),
    };
    let Some(value) = found.data.get(SIGNING_KEYS_FILE) else {
        return Stored {
            keys: SigningKeys::default(),
            problem: passwords::is_bound(&found.metadata)
                .then(|| format!("it held no {SIGNING_KEYS_FILE}")),
        };
    };
    if !passwords::is_bound(&found.metadata) {
        return lost(
            "it is not the object the init container filled (another uid, or none recorded), so \
             its data is not trusted"
                .to_owned(),
        );
    }
    let Some(bytes) = STANDARD.decode(value).ok().map(Zeroizing::new) else {
        return lost(format!("its {SIGNING_KEYS_FILE} is not base64"));
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return lost(format!("its {SIGNING_KEYS_FILE} is not UTF-8"));
    };
    match SigningKeys::parse(text) {
        Ok(keys) => Stored {
            keys,
            problem: None,
        },
        Err(error) => lost(format!("its {SIGNING_KEYS_FILE} is not usable: {error}")),
    }
}

/// The merge patch that writes `keys` into the Secret at `resource_version`, binds it to `uid`,
/// and removes `foreign` metadata and the `others` data keys.
fn secret_patch(
    resource_version: &str,
    uid: &str,
    keys: &SigningKeys,
    foreign: &Foreign,
    others: &[String],
) -> Zeroizing<String> {
    let metadata = binding_metadata(resource_version, uid, Map::new(), foreign);
    let document = keys.to_json();
    let mut data: Map<String, Value> = others
        .iter()
        .map(|key| (key.clone(), Value::Null))
        .collect();
    data.insert(
        SIGNING_KEYS_FILE.to_owned(),
        Value::String(STANDARD.encode(document.as_bytes())),
    );
    let body = json!({ "metadata": metadata, "data": data });
    // Room for the largest patch up front (the key set in base64 is at most 4/3 of
    // MAX_DOCUMENT_BYTES, plus the metadata), so the buffer never moves while it grows and leaves
    // an unwiped copy of the keys behind.
    let mut buffer = Zeroizing::new(Vec::with_capacity(2 * MAX_DOCUMENT_BYTES));
    serde_json::to_writer(&mut *buffer, &body).expect("a JSON value always serializes");
    let text = Zeroizing::new(
        String::from_utf8(std::mem::take(&mut *buffer)).expect("serde_json writes UTF-8"),
    );
    if let Value::Object(mut body) = body
        && let Some(Value::Object(data)) = body.remove("data")
    {
        for (_, value) in data {
            if let Value::String(encoded) = value {
                drop(Zeroizing::new(encoded));
            }
        }
    }
    text
}

/// The key a person made before Laysi/trustiot-v2#404, while the chart still names it.
fn legacy_key(settings: &KeySettings) -> Result<Option<SigningKey>, Error> {
    let (Some(path), Some(public_keys)) = (&settings.legacy_key_file, &settings.legacy_public_keys)
    else {
        return Ok(None);
    };
    let pem = read_small(path).map_err(|problem| {
        Error::SigningKeys(format!(
            "the key of internalIdentity.signingKeySecret ({}): {problem}",
            path.display()
        ))
    })?;
    SigningKey::import(&pem, public_keys)
        .map(Some)
        .map_err(|error| {
            Error::SigningKeys(format!(
                "the key of internalIdentity.signingKeySecret cannot be imported: {error}"
            ))
        })
}

fn read_small(path: &Path) -> Result<Zeroizing<String>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("cannot open it: {error}"))?;
    // Room for the largest file up front, so the buffer never moves while it grows.
    let mut text = Zeroizing::new(String::with_capacity(
        usize::try_from(MAX_LEGACY_KEY_BYTES + 1).unwrap_or(0),
    ));
    file.take(MAX_LEGACY_KEY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "it is not a text file".to_owned())?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_LEGACY_KEY_BYTES {
        return Err("it is too large to be a key".to_owned());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn secret(uid: &str, recorded: Option<&str>, data: &[(&str, &[u8])]) -> Secret {
        let mut annotations = BTreeMap::new();
        if let Some(recorded) = recorded {
            annotations.insert(passwords::UID_ANNOTATION.to_owned(), recorded.to_owned());
        }
        let text = json!({
            "metadata": {"resourceVersion": "3", "uid": uid, "annotations": annotations},
            "data": data
                .iter()
                .map(|(key, value)| ((*key).to_owned(), Value::String(STANDARD.encode(value))))
                .collect::<Map<String, Value>>(),
        })
        .to_string();
        serde_json::from_str(&text).expect("a Secret")
    }

    fn one_key() -> SigningKeys {
        let Plan { mut keys, .. } = SigningKeys::default()
            .plan(
                &Request {
                    generation: 1,
                    revoke_before: 0,
                    legacy: None,
                },
                100,
                |generation| Ok(SigningKey::from_seed(&[9; 32], generation)),
            )
            .expect("plan");
        keys.publish(100, 600);
        keys
    }

    #[test]
    fn only_a_bound_secret_with_a_usable_set_is_trusted() {
        let document = one_key().to_json();
        let bound = stored(&secret(
            "u1",
            Some("u1"),
            &[(SIGNING_KEYS_FILE, document.as_bytes())],
        ));
        assert!(bound.problem.is_none());
        assert!(bound.keys.same_as(&one_key()));

        // A fresh install: an empty Secret nobody bound yet.
        let fresh = stored(&secret("u1", None, &[]));
        assert!(fresh.problem.is_none());
        assert!(fresh.keys.is_empty());

        let cases: [(Secret, &str); 5] = [
            (secret("u1", Some("u1"), &[]), "held no"),
            (
                secret(
                    "u2",
                    Some("u1"),
                    &[(SIGNING_KEYS_FILE, document.as_bytes())],
                ),
                "not the object",
            ),
            (
                secret("u1", None, &[(SIGNING_KEYS_FILE, document.as_bytes())]),
                "not the object",
            ),
            (
                secret("u1", Some("u1"), &[(SIGNING_KEYS_FILE, b"\xff")]),
                "not UTF-8",
            ),
            (
                secret("u1", Some("u1"), &[(SIGNING_KEYS_FILE, b"{\"keys\":7}")]),
                "not usable",
            ),
        ];
        for (found, expected) in cases {
            let Stored { keys, problem } = stored(&found);
            assert!(keys.is_empty());
            let problem = problem.expect("a reason");
            assert!(problem.contains(expected), "{problem}");
        }
    }

    #[test]
    fn the_patch_binds_writes_the_set_and_removes_the_rest() {
        let keys = one_key();
        let foreign = Foreign::of(&secret("u1", None, &[]).metadata);
        let body: Value = serde_json::from_str(&secret_patch(
            "3",
            "u1",
            &keys,
            &foreign,
            &["planted".to_owned()],
        ))
        .expect("JSON");
        assert_eq!(
            body,
            json!({
                "metadata": {
                    "resourceVersion": "3",
                    "annotations": {passwords::UID_ANNOTATION: "u1"},
                },
                "data": {
                    "planted": null,
                    SIGNING_KEYS_FILE: STANDARD.encode(keys.to_json().as_bytes()),
                },
            })
        );
    }
}
