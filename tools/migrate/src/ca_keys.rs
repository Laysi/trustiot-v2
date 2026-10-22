//! The certs service's issuing CA keys (decision 6; Laysi/trustiot-v2#78; services/certs/
//! README.md, 「CA 階層與私鑰保管」). Nobody creates, copies or types an issuing CA key:
//!
//! - the chart declares the Secret `<release>-certs-ca-keys` and the ConfigMap
//!   `<release>-certs-ca-requests` without any data;
//! - [`converge`], run as `migrate certs-ca-keys` by the certs pod's init container with the
//!   certs ServiceAccount's token, keeps the key set ([`ca_keys::CaKeys`]) in the Secret and the
//!   certificate signing request (CSR) of every key in the ConfigMap, in two steps that each are
//!   one patch carrying the object's `resourceVersion`:
//!   1. the Secret gets what [`ca_keys::CaKeys::plan`] decides: a first key, a key for a higher
//!      `certificateAuthority.keyGeneration`, keys below
//!      `certificateAuthority.removeKeysBeforeGeneration` gone;
//!   2. the ConfigMap gets the CSR of every key in the Secret and nothing else, written only
//!      while the Secret is still the version it was derived from.
//!
//!   A run that stops after the first step is finished by the next; two runs at once (two
//!   replicas starting) meet a 409 and the loser reads again and decides anew on what it finds.
//!   An operator signs a CSR from the ConfigMap with the offline root; until then the new key
//!   waits and the older ones keep signing.
//! - the Secret's data is trusted only while it records the Secret's own uid (the rules of
//!   src/passwords.rs): an object someone else made, restored or emptied is treated as lost and
//!   the run starts a new set. A key someone else may know must never reach the ConfigMap, where
//!   an operator would sign it. A lost set needs a new issuing CA certificate (README, 「簽發 CA
//!   金鑰遺失或外洩」). A Secret that is the bound object but holds a set this version cannot read
//!   (a newer format after a rollback, a damaged edit) stops the run instead: replacing it would
//!   discard the keys the running pods sign with.
//!
//! Only the certs ServiceAccount may `get` and `patch` these two objects (the chart's Role names
//! them), and only the init container mounts its token. Nothing here logs a private key or the
//! Secret's data: reports and errors name key ids, generations and request file names. The key
//! set, the patch that carries it and the Secret as read are wiped from memory when dropped;
//! what the HTTP client and TLS keep in their own buffers is not, which the init container
//! bounds by exiting once the run is done.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ca_keys::{CaKey, CaKeys, KEYS_FILE, MAX_DOCUMENT_BYTES, Plan, Request};
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use crate::config::CaKeySettings;
use crate::passwords::{
    self, Access, Answer, Api, Foreign, Hints, Kind, MAX_CONFLICTS, Secret, binding_metadata,
};
use crate::signing_keys::unix_now;
use crate::{Error, Report};

/// What the errors about the Secret tell an operator.
const SECRET_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the certs service's init \
              container fills it: run helm upgrade, which creates it again; the init container \
              then makes a new issuing CA key whose request waits for the offline root \
              (services/certs/README.md)",
    refused: "the chart's Role lets the certs service's ServiceAccount get and patch exactly this \
              Secret and the ConfigMap of the certificate signing requests",
};

/// What the errors about the ConfigMap tell an operator.
const CONFIG_MAP_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data: run helm upgrade, which creates \
              it again, then kubectl rollout restart deployment/<release>-certs so the init \
              container fills it (services/certs/README.md)",
    refused: "the chart's Role lets the certs service's ServiceAccount get and patch exactly this \
              ConfigMap and the Secret of the issuing CA keys",
};

/// The init container's run at the current time; see [`converge_at`].
///
/// # Errors
///
/// As [`converge_at`].
pub async fn converge(settings: &CaKeySettings, report: &mut Report) -> Result<(), Error> {
    converge_at(settings, report, unix_now()).await
}

/// The init container's run at `now` (seconds since the Unix epoch): the two steps of the module
/// documentation, until the Secret holds the set the chart asks for and the ConfigMap holds
/// exactly its requests.
///
/// # Errors
///
/// When either object is missing or being deleted, the API refuses access or does not answer
/// within `settings.wait`, another writer keeps changing them, there is no randomness, or the set
/// would hold too many keys. The certs pod then does not start; the pods already running keep
/// their keys.
pub async fn converge_at(
    settings: &CaKeySettings,
    report: &mut Report,
    now: i64,
) -> Result<(), Error> {
    let invalid = |key: &str| Error::CaKeys(format!("TRUSTIOT__KEYS__{key}: not a generation"));
    let generation =
        passwords::parse_generation(&settings.generation).ok_or_else(|| invalid("GENERATION"))?;
    let remove_before = passwords::parse_generation(&settings.remove_before_generation)
        .ok_or_else(|| invalid("REMOVE_BEFORE_GENERATION"))?;
    let request = Request {
        generation,
        remove_before,
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
        // Step 1: the key set the request asks for.
        let found = objects.read_secret().await?;
        let uid = found.metadata.uid.clone();
        let Stored { keys, problem } = stored(&found, &settings.secret)?;
        let bound = passwords::is_bound(&found.metadata);
        let foreign = if bound {
            Foreign::default()
        } else {
            Foreign::of(&found.metadata)
        };
        let Plan { keys, changes } = keys
            .plan(request, now, CaKey::generate)
            .map_err(|error| Error::CaKeys(error.to_string()))?;
        let mut version = found.metadata.resource_version.clone();
        if !changes.is_empty() || problem.is_some() || !bound {
            let others: Vec<String> = found
                .data
                .keys()
                .filter(|key| *key != KEYS_FILE)
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
                tracing::error!(
                    secret = %settings.secret,
                    problem = %problem,
                    "the issuing CA keys were lost and a new set was started; the CA signs again \
                     once the offline root signs the new key's request (services/certs/README.md, \
                     「簽發 CA 金鑰遺失或外洩」)"
                );
                report.change(format!(
                    "started a new issuing CA key set in Secret {}: {problem}",
                    settings.secret
                ));
            }
            for change in &changes {
                report.change(format!("{change} in Secret {}", settings.secret));
            }
        }

        // Step 2: every key's request in the ConfigMap.
        if !objects.publish(&keys, &version, report).await? {
            continue;
        }
        let generations: Vec<u64> = keys.keys().iter().map(CaKey::generation).collect();
        // Key ids, generations and file names only, never a key.
        tracing::info!(
            secret = %settings.secret,
            config_map = %settings.config_map,
            keys = ?keys.key_ids().collect::<Vec<_>>(),
            generations = ?generations,
            requests = ?keys.requests().keys().collect::<Vec<_>>(),
            "the issuing CA keys are in place"
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
                "is being deleted (metadata.deletionTimestamp is set). Once it is gone, run helm \
                 upgrade, which creates it again (services/certs/README.md)",
            ));
        }
        Ok(found)
    }

    /// Patches the Secret with `body`, which writes `keys`: the new `resourceVersion`, or `None`
    /// when the Secret changed since it was read.
    async fn write_secret(&self, body: &str, keys: &CaKeys) -> Result<Option<String>, Error> {
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
                // admission webhook that rewrites Secrets, say) would leave the requests in the
                // ConfigMap describing keys the certs service does not have.
                let written = keys.to_json();
                let kept = stored
                    .data
                    .get(KEYS_FILE)
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

    /// Makes the ConfigMap hold exactly the requests of `keys`, read from the Secret at
    /// `secret_version`. `false` when the Secret has changed since: the caller starts over, so a
    /// run that read an older Secret never publishes a request for a key the Secret no longer
    /// holds, nor takes a ConfigMap that still shows older requests as done.
    async fn publish(
        &self,
        keys: &CaKeys,
        secret_version: &str,
        report: &mut Report,
    ) -> Result<bool, Error> {
        let requests = keys.requests();
        for _ in 0..MAX_CONFLICTS {
            let Answer::Secret(found) = self.config_maps.request(self.config_map, None).await?
            else {
                return Err(self.config_map_error("the API answered a read with a conflict"));
            };
            if found.metadata.deletion_timestamp.is_some() {
                return Err(self.config_map_error(
                    "is being deleted (metadata.deletionTimestamp is set). Once it is gone, run \
                     helm upgrade, which creates it again, then kubectl rollout restart \
                     deployment/<release>-certs (services/certs/README.md)",
                ));
            }
            if found.data != requests {
                // Read after the ConfigMap: any run that changes the Secret later also writes the
                // ConfigMap later, and this patch fails if it does so first.
                if self.read_secret().await?.metadata.resource_version != secret_version {
                    return Ok(false);
                }
                let body =
                    config_map_patch(&found.metadata.resource_version, &found.data, &requests);
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
                        if stored.data != requests {
                            return Err(self.config_map_error(
                                "the API stored other requests than were written",
                            ));
                        }
                        report.change(format!(
                            "published the certificate signing requests {} in ConfigMap {}",
                            requests.keys().cloned().collect::<Vec<_>>().join(", "),
                            self.config_map
                        ));
                    }
                }
            }
            // The ConfigMap shows this run's requests, which are current only while the Secret
            // still holds their keys; otherwise the caller starts over.
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
    keys: CaKeys,
    problem: Option<String>,
}

/// The key set in `found` (the Secret `name`): empty, with the reason, when the Secret is not
/// the object the init container filled (it records another uid, or none) or holds no set; the
/// reason names the problem, never the data.
///
/// # Errors
///
/// When the Secret is the object the init container filled but its set cannot be read: a newer
/// format after a rollback of the migrate image, or a damaged edit. Starting a new set would
/// discard the keys the running certs pods sign with and cut off every gateway until the offline
/// ceremony, so the run stops instead and names the way out.
fn stored(found: &Secret, name: &str) -> Result<Stored, Error> {
    let lost = |problem: String| Stored {
        keys: CaKeys::default(),
        problem: Some(problem),
    };
    let Some(value) = found.data.get(KEYS_FILE) else {
        return Ok(Stored {
            keys: CaKeys::default(),
            problem: passwords::is_bound(&found.metadata)
                .then(|| format!("it held no {KEYS_FILE}")),
        });
    };
    if !passwords::is_bound(&found.metadata) {
        return Ok(lost(
            "it is not the object the init container filled (another uid, or none recorded), so \
             its data is not trusted"
                .to_owned(),
        ));
    }
    let unreadable = |problem: String| {
        Error::CaKeys(format!(
            "Secret {name}: its {KEYS_FILE} is not a key set this version can read ({problem}). \
             The certs pods already running keep signing with the keys they loaded. Roll the \
             migrate image forward to the version that wrote it; only if those keys are lost \
             anyway, remove the key {KEYS_FILE} from the Secret so the next start makes a new set \
             and its request waits for the offline root (services/certs/README.md, 「簽發 CA 金鑰\
             遺失或外洩」)"
        ))
    };
    let Some(bytes) = STANDARD.decode(value).ok().map(Zeroizing::new) else {
        return Err(unreadable("not base64".to_owned()));
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err(unreadable("not UTF-8".to_owned()));
    };
    CaKeys::parse(text)
        .map(|keys| Stored {
            keys,
            problem: None,
        })
        .map_err(|error| unreadable(error.to_string()))
}

/// The merge patch that writes `keys` into the Secret at `resource_version`, binds it to `uid`,
/// and removes `foreign` metadata and the `others` data keys.
fn secret_patch(
    resource_version: &str,
    uid: &str,
    keys: &CaKeys,
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
        KEYS_FILE.to_owned(),
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

/// The merge patch that makes a ConfigMap at `resource_version` holding `found` hold exactly
/// `requests`.
fn config_map_patch(
    resource_version: &str,
    found: &BTreeMap<String, String>,
    requests: &BTreeMap<String, String>,
) -> String {
    let mut data: Map<String, Value> = found
        .keys()
        .filter(|key| !requests.contains_key(*key))
        .map(|key| (key.clone(), Value::Null))
        .collect();
    for (file, request) in requests {
        data.insert(file.clone(), Value::String(request.clone()));
    }
    json!({
        "metadata": {"resourceVersion": resource_version},
        "data": data,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_map_patch_sets_every_request_and_removes_the_rest() {
        let found = BTreeMap::from([
            (
                "issuing-ca-generation-1.csr.pem".to_owned(),
                "old".to_owned(),
            ),
            ("planted".to_owned(), "x".to_owned()),
        ]);
        let requests = BTreeMap::from([
            (
                "issuing-ca-generation-1.csr.pem".to_owned(),
                "one".to_owned(),
            ),
            (
                "issuing-ca-generation-2.csr.pem".to_owned(),
                "two".to_owned(),
            ),
        ]);
        let body: Value =
            serde_json::from_str(&config_map_patch("7", &found, &requests)).expect("JSON");
        assert_eq!(
            body,
            json!({
                "metadata": {"resourceVersion": "7"},
                "data": {
                    "planted": null,
                    "issuing-ca-generation-1.csr.pem": "one",
                    "issuing-ca-generation-2.csr.pem": "two",
                },
            })
        );
    }
}
