//! The identity service's public keys as the deployment publishes them: a JWK Set in a file
//! (`/etc/trustiot/internal-identity/jwks.json`, the ConfigMap `<release>-internal-identity-keys`
//! that every pod mounts), read again every [`RELOAD_INTERVAL`] so a rotation reaches a running
//! service without a restart (docs/architecture/secrets.md, 「內部身分簽章金鑰」).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use super::{IdentityKeys, InvalidKeys};
use crate::Context;

/// How often a service reads the keys file again.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// The name of the readiness check [`PublishedKeys::watch`] adds: it fails until the service has
/// a usable set.
pub const READINESS_CHECK: &str = "internal_identity_keys";

/// Largest keys file read. Eight public keys take about 1 KiB.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The public keys a service verifies internal identity tokens with right now: none until the
/// first usable set arrives, then always the last usable one. Swapped whole, so a verification
/// sees one set or the next, never a mix. Cheap to clone; clones share the set.
#[derive(Clone, Default)]
pub struct PublishedKeys {
    current: Arc<RwLock<Option<IdentityKeys>>>,
}

impl PublishedKeys {
    /// No keys yet: every token is refused with [`IdentityError::NoKeys`](super::IdentityError).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The set in use, if there is one.
    #[must_use]
    pub fn current(&self) -> Option<IdentityKeys> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the set in use has the key id `kid`.
    #[must_use]
    pub fn contains(&self, kid: &str) -> bool {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|keys| keys.get(kid).is_some())
    }

    /// Whether the set in use has the key id `kid` for exactly this public key: a key whose key
    /// id is published with another public key is not published.
    #[must_use]
    pub fn publishes(&self, kid: &str, key: &ed25519_dalek::VerifyingKey) -> bool {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|keys| keys.get(kid))
            .is_some_and(|published| published.as_bytes() == key.as_bytes())
    }

    /// Uses `keys` from now on.
    pub fn replace(&self, keys: IdentityKeys) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(keys);
    }

    /// The keys of the file at `path` (the platform setting [`KEYS_FILE_SETTING`]), kept current:
    /// read now and then every [`RELOAD_INTERVAL`] until the service stops. A usable JWK Set
    /// replaces the one in use, with an INFO line that names its key ids; a missing, empty or
    /// unusable file keeps the last usable set, with a WARN line that says why (never the
    /// file's content) when the problem first appears. Adds the readiness check
    /// [`READINESS_CHECK`], which fails until a usable set has arrived.
    ///
    /// [`KEYS_FILE_SETTING`]: super::KEYS_FILE_SETTING
    pub async fn watch<T>(context: &Context<T>, path: impl Into<PathBuf>) -> Self {
        let keys = Self::empty();
        let mut file = KeysFile::new(path.into());
        file.reload(&keys).await;
        let ready = keys.clone();
        context.health().add_check(READINESS_CHECK, move || {
            let usable = ready.current().is_some();
            async move {
                if usable {
                    Ok(())
                } else {
                    Err("no usable public keys of the identity service yet")
                }
            }
        });
        let shutdown = context.shutdown().clone();
        let watched = keys.clone();
        context.spawn(async move {
            let mut every = tokio::time::interval(RELOAD_INTERVAL);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            every.tick().await;
            loop {
                tokio::select! {
                    () = shutdown.stopping() => return,
                    _ = every.tick() => file.reload(&watched).await,
                }
            }
        });
        keys
    }
}

impl From<IdentityKeys> for PublishedKeys {
    /// A fixed set, for tests and tools that verify with known keys.
    fn from(keys: IdentityKeys) -> Self {
        let published = Self::empty();
        published.replace(keys);
        published
    }
}

impl std::fmt::Debug for PublishedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PublishedKeys")
            .field(&self.current())
            .finish()
    }
}

/// The keys file and what was last read from it.
pub struct KeysFile {
    path: PathBuf,
    /// The text of the set in use, to skip parsing an unchanged file.
    loaded: Option<String>,
    /// The problem last logged, to log each one once.
    problem: Option<String>,
}

impl KeysFile {
    /// The file at `path`, not read yet.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            loaded: None,
            problem: None,
        }
    }

    /// Reads the file once and puts a usable set into `keys` (see [`PublishedKeys::watch`]).
    pub async fn reload(&mut self, keys: &PublishedKeys) {
        let path = self.path.clone();
        let read = tokio::task::spawn_blocking(move || read(&path))
            .await
            .unwrap_or_else(|_| Err("reading it failed".to_owned()));
        let text = match read {
            Ok(text) => text,
            Err(problem) => return self.unusable(problem, keys),
        };
        if self.loaded.as_deref() == Some(text.as_str()) {
            return;
        }
        match IdentityKeys::from_jwks(&text) {
            Ok(set) => {
                tracing::info!(
                    file = %self.path.display(),
                    keys = ?set.key_ids().collect::<Vec<_>>(),
                    "verifying internal identity tokens with these public keys of the identity service"
                );
                keys.replace(set);
                self.loaded = Some(text);
                self.problem = None;
            }
            Err(error) => self.unusable(unusable_set(&error), keys),
        }
    }

    fn unusable(&mut self, problem: String, keys: &PublishedKeys) {
        self.loaded = None;
        if self.problem.as_ref() == Some(&problem) {
            return;
        }
        match keys.current() {
            Some(kept) => tracing::warn!(
                file = %self.path.display(),
                problem = %problem,
                kept = ?kept.key_ids().collect::<Vec<_>>(),
                "the identity service's public keys file is not usable; keeping the last usable keys"
            ),
            None => tracing::warn!(
                file = %self.path.display(),
                problem = %problem,
                "no usable public keys of the identity service yet; not ready"
            ),
        }
        self.problem = Some(problem);
    }
}

fn unusable_set(error: &InvalidKeys) -> String {
    format!("not a usable JWK Set: {error}")
}

/// The file's text: at most [`MAX_FILE_BYTES`], UTF-8. Errors say why, never what it holds.
fn read(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "the file does not exist (yet)".to_owned(),
        kind => format!("cannot open the file ({kind})"),
    })?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "the file is not UTF-8 text".to_owned())?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(format!("the file is larger than {MAX_FILE_BYTES} bytes"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::identity::test_keys::TestKey;

    /// A directory of its own under the system's temporary directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "service-kit-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Replaces the file as the kubelet updates a ConfigMap volume: a new file renamed over it.
    fn publish(path: &Path, text: &str) {
        let next = path.with_extension("next");
        std::fs::write(&next, text).expect("write");
        std::fs::rename(&next, path).expect("rename");
    }

    fn jwks(entries: &[(u8, &str)]) -> String {
        let keys: Vec<_> = entries
            .iter()
            .map(|(seed, kid)| TestKey::new(*seed).jwk(kid))
            .collect();
        json!({ "keys": keys }).to_string()
    }

    fn ids(keys: &PublishedKeys) -> Vec<String> {
        keys.current()
            .map(|set| set.key_ids().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn the_last_usable_set_stays_until_a_usable_one_replaces_it() {
        let scratch = Scratch::new("keys-file");
        let path = scratch.0.join("jwks.json");
        let keys = PublishedKeys::empty();
        let mut file = KeysFile::new(path.clone());

        // Not there yet (a fresh install): no keys.
        file.reload(&keys).await;
        assert!(keys.current().is_none());
        publish(&path, "");
        file.reload(&keys).await;
        assert!(keys.current().is_none());

        publish(&path, &jwks(&[(1, "a")]));
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["a"]);

        // A rotation adds a key without a restart.
        publish(&path, &jwks(&[(1, "a"), (2, "b")]));
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["a", "b"]);

        // Broken, emptied, holding a private key, or gone: the last usable set stays.
        let mut private = TestKey::new(3).jwk("c");
        private["d"] = json!("presence-is-enough");
        for broken in [
            "not JSON".to_owned(),
            String::new(),
            json!({"keys": []}).to_string(),
            json!({ "keys": [private] }).to_string(),
        ] {
            publish(&path, &broken);
            file.reload(&keys).await;
            assert_eq!(ids(&keys), ["a", "b"], "{broken}");
        }
        std::fs::remove_file(&path).expect("remove");
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["a", "b"]);

        // The same set again after a problem is loaded again; then the old key goes.
        publish(&path, &jwks(&[(1, "a"), (2, "b")]));
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["a", "b"]);
        publish(&path, &jwks(&[(2, "b")]));
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["b"]);

        // Too large to be a key set.
        publish(
            &path,
            &" ".repeat(usize::try_from(MAX_FILE_BYTES).expect("small") + 1),
        );
        file.reload(&keys).await;
        assert_eq!(ids(&keys), ["b"]);
    }
}
