//! Signs internal identity tokens (decision 27) in the format `crates/service-kit` verifies:
//! a JWS compact serialization whose protected header is exactly
//! `{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"<key id>"}` and whose payload is
//! [`InternalClaims`] as JSON, signed with Ed25519.
//!
//! The keys are generated in the cluster (Laysi/trustiot-v2#404; docs/architecture/secrets.md,
//! 「內部身分簽章金鑰」): the identity pod's init container keeps them in the Secret
//! `<release>-identity-signing-keys`, which the chart mounts into this service only
//! (`TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE`), and their public keys in the ConfigMap every service
//! mounts (`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`). [`Issuer::watch`] reads both again every
//! [`RELOAD_INTERVAL`] and signs each token with the key [`SigningKeys::signing_key`] selects at
//! that moment: one whose `signs_from` has come, whose generation is not revoked
//! (`TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION`), and whose key id is in the public keys this
//! service itself reads, so it never signs with a key the services may not have. Every replica
//! applies the same rule to the same data, so they switch keys at the same time without a
//! restart. Without such a key the service is not ready and the ext_authz check answers 503.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{InternalClaims, InternalToken, ServiceName};
use ed25519_dalek::{Signer, SigningKey};
use hmac::{Hmac, KeyInit, Mac};
use identity_keys::{MAX_DOCUMENT_BYTES, SigningKeys};
use serde::Serialize;
use service_kit::Context;
use service_kit::identity::{ALGORITHM, PublishedKeys, RELOAD_INTERVAL, TOKEN_TYPE};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::session::SessionGrant;

/// Lifetime of every token, in seconds: `exp - iat`. Short, because a token is a bearer
/// credential for its audience until it expires (plus the verifiers' 5 s of clock skew), and
/// because a retired key's tokens must have expired before its public key goes (that wait uses
/// the longest lifetime the verifiers accept, 300 s, so it holds for any value up to it). Matches
/// the 2026-10-10 PoC.
pub const TOKEN_LIFETIME_SECS: i64 = 60;

/// The platform setting with the path of the signing keys file: `platform.identity_signing_keys_file`,
/// set by the chart as `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` on the identity service only.
pub const SIGNING_KEYS_FILE_SETTING: &str = "TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE";

/// The readiness check [`Issuer::watch`] adds: it fails while no key may sign.
pub const READINESS_CHECK: &str = "signing_key";

/// What the login-code key of a signing key is derived for ([`derive_code_key`]). Changing it
/// makes every code in use unusable.
const CODE_KEY_LABEL: &[u8] = b"trustiot-login-code-key:v1";

/// A key that login codes are hashed with (HMAC-SHA-256; Laysi/trustiot-v2#103,
/// `crate::login::code_hash`). Wiped on drop.
pub type CodeKey = Zeroizing<[u8; 32]>;

/// The login-code key of a signing key: HMAC-SHA-256 of [`CODE_KEY_LABEL`] under its private
/// key. It lives only where the signing keys live, outside the database, so a copy of the
/// database cannot test codes offline; it reveals nothing about the private key (HMAC is a
/// PRF) and rotates with it.
fn derive_code_key(key: &identity_keys::SigningKey) -> CodeKey {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.signer().as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC-SHA-256 takes keys of any length"));
    mac.update(CODE_KEY_LABEL);
    let derived: [u8; 32] = mac.finalize().into_bytes().into();
    Zeroizing::new(derived)
}

/// The protected header, serialized in this field order.
#[derive(Serialize)]
struct Header<'a> {
    alg: &'static str,
    typ: &'static str,
    kid: &'a str,
}

/// What [`Issuer::watch`] needs besides the public keys.
#[derive(Clone, Debug)]
pub struct IssuerSettings {
    /// The signing keys file (`TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE`).
    pub signing_keys_file: PathBuf,
    /// Keys of a lower generation never sign (`TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION`).
    pub revoke_before: u64,
    /// How long the signing keys and the published public keys may differ before it is an
    /// error (`TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`).
    pub propagation_seconds: u64,
    /// The maximum key age of the rotation CronJob (`TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS`,
    /// Laysi/trustiot-v2#429): a signing key older than that by more than the propagation time
    /// and [`identity_keys::ROTATION_OVERDUE_SECS`] is an error. 0: rotation by age is off.
    pub max_key_age_seconds: u64,
}

/// Signs tokens with the key the schedule selects now. Cheap to clone; clones share the keys.
#[derive(Clone)]
pub struct Issuer {
    shared: Arc<Shared>,
}

struct Shared {
    /// The last usable signing keys file; `None` until one has been read.
    keys: RwLock<Option<SigningKeys>>,
    /// The public keys this service reads, as every other service does.
    published: PublishedKeys,
    revoke_before: u64,
}

impl Issuer {
    /// An issuer without signing keys yet: it signs nothing until [`Issuer::replace`].
    #[must_use]
    pub fn new(published: PublishedKeys, revoke_before: u64) -> Self {
        Self {
            shared: Arc::new(Shared {
                keys: RwLock::new(None),
                published,
                revoke_before,
            }),
        }
    }

    /// An issuer with `keys`, for tests and tools that sign with known keys.
    #[must_use]
    pub fn with_keys(keys: SigningKeys, published: PublishedKeys, revoke_before: u64) -> Self {
        let issuer = Self::new(published, revoke_before);
        issuer.replace(keys);
        issuer
    }

    /// Uses `keys` from now on.
    pub fn replace(&self, keys: SigningKeys) {
        *self
            .shared
            .keys
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(keys);
    }

    /// The key id tokens issued at `now` would name, if any key may sign then.
    #[must_use]
    pub fn signing_kid(&self, now: i64) -> Option<String> {
        self.with_signing_key(now, |key| key.kid().to_owned())
    }

    /// The key a new login code is hashed with at `now`: that of the key that signs at `now`,
    /// which every replica selects alike. `None` while no key may sign.
    #[must_use]
    pub fn code_key(&self, now: i64) -> Option<CodeKey> {
        self.with_signing_key(now, derive_code_key)
    }

    /// The keys a login code in use may have been hashed with: one for each key of the signing
    /// keys file whose generation is not revoked, whether or not it signs now, so a code issued
    /// just before a key switch still works. Empty until a file has been read.
    #[must_use]
    pub fn code_keys(&self) -> Vec<CodeKey> {
        self.shared
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|keys| {
                keys.keys()
                    .iter()
                    .filter(|key| key.generation() >= self.shared.revoke_before)
                    .map(derive_code_key)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn with_signing_key<R>(
        &self,
        now: i64,
        use_key: impl FnOnce(&identity_keys::SigningKey) -> R,
    ) -> Option<R> {
        let keys = self
            .shared
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let published = &self.shared.published;
        keys.as_ref()?
            .signing_key(now, self.shared.revoke_before, |key| {
                published.publishes(key.kid(), &key.verifying_key())
            })
            .map(use_key)
    }

    /// The key that signs at `now`, as its key id and `published_at`, if any key may sign.
    fn signing_published(&self, now: i64) -> Option<(String, Option<i64>)> {
        self.with_signing_key(now, |key| (key.kid().to_owned(), key.published_at()))
    }

    /// The key ids of the signing keys file with their public keys, if one has been read.
    fn public_keys(&self) -> Option<Vec<(String, ed25519_dalek::VerifyingKey)>> {
        self.shared
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|keys| {
                keys.keys()
                    .iter()
                    .map(|key| (key.kid().to_owned(), key.verifying_key()))
                    .collect()
            })
    }

    /// The key ids of the signing keys file, if one has been read: what logs may show of it.
    fn key_ids(&self) -> Option<Vec<String>> {
        self.public_keys()
            .map(|keys| keys.into_iter().map(|(kid, _)| kid).collect())
    }

    /// A token for `grant`, addressed to `audience`, issued at `now` (seconds since the Unix
    /// epoch) and valid for [`TOKEN_LIFETIME_SECS`], signed with the key that may sign at `now`.
    ///
    /// # Errors
    ///
    /// [`IssueError::NoSigningKey`] when no key may sign at `now`; otherwise only if the claims
    /// cannot be serialized or the result is not a valid header value, which the types rule out.
    /// Callers answer 503.
    pub fn issue(
        &self,
        grant: &SessionGrant,
        audience: ServiceName,
        now: i64,
    ) -> Result<InternalToken, IssueError> {
        let claims = InternalClaims {
            iss: ServiceName::Identity,
            sub: grant.user,
            aud: audience,
            iat: now,
            exp: now
                .checked_add(TOKEN_LIFETIME_SECS)
                .ok_or(IssueError::Build)?,
            tenant: grant.tenant,
            platform_admin: grant.platform_admin,
            // Signed for the ingress gateway's request: nobody acts for the user yet.
            act: None,
        };
        self.sign_claims(&claims, now)
    }

    /// Signs `claims` with the key that may sign at `now`. The caller builds claims the verifiers
    /// accept: issued by this service at `now`, living at most [`TOKEN_LIFETIME_SECS`]
    /// ([`Issuer::issue`] for a session, [`crate::exchange`] for a token exchange).
    ///
    /// # Errors
    ///
    /// As [`Issuer::issue`].
    pub(crate) fn sign_claims(
        &self,
        claims: &InternalClaims,
        now: i64,
    ) -> Result<InternalToken, IssueError> {
        let token = self
            .with_signing_key(now, |key| sign(key.signer(), key.kid(), claims))
            .ok_or(IssueError::NoSigningKey)??;
        InternalToken::new(token).map_err(|_| IssueError::Build)
    }

    /// The issuer of a running service: reads the signing keys file now and then every
    /// [`RELOAD_INTERVAL`] until the service stops, keeping the last usable keys when the file
    /// is missing or unusable (a WARN line says why, never what the file holds). Logs at INFO
    /// which key signs whenever that changes, and at ERROR when the signing keys and the
    /// `published` public keys have differed for longer than `propagation_seconds` (the init
    /// container writes both, so a lasting difference means one was changed by hand or the
    /// kubelet stopped updating this pod's files), and once when the signing key is overdue for
    /// its rotation by age (`max_key_age_seconds`). Adds the readiness check
    /// [`READINESS_CHECK`].
    pub async fn watch<T>(
        context: &Context<T>,
        settings: &IssuerSettings,
        published: PublishedKeys,
    ) -> Self {
        let issuer = Self::new(published.clone(), settings.revoke_before);
        let mut file = KeysFile::new(settings.signing_keys_file.clone());
        let propagation = i64::try_from(settings.propagation_seconds).unwrap_or(i64::MAX);
        let max_key_age = match settings.max_key_age_seconds {
            0 => None,
            seconds => Some(i64::try_from(seconds).unwrap_or(i64::MAX)),
        };
        let mut monitor = Monitor::new(propagation).with_max_key_age(max_key_age);
        file.reload(&issuer).await;
        monitor.observe(unix_now(), &issuer, &published);
        let ready = issuer.clone();
        context.health().add_check(READINESS_CHECK, move || {
            let signing = ready.signing_kid(unix_now()).is_some();
            async move {
                if signing {
                    Ok(())
                } else {
                    Err("no signing key may sign now")
                }
            }
        });
        let shutdown = context.shutdown().clone();
        let watched = issuer.clone();
        context.spawn(async move {
            let mut every = tokio::time::interval(RELOAD_INTERVAL);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            every.tick().await;
            loop {
                tokio::select! {
                    () = shutdown.stopping() => return,
                    _ = every.tick() => {
                        file.reload(&watched).await;
                        monitor.observe(unix_now(), &watched, &published);
                    }
                }
            }
        });
        issuer
    }
}

impl fmt::Debug for Issuer {
    /// Only the key ids: the private keys never reach a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Issuer")
            .field("key_ids", &self.key_ids())
            .field("revoke_before", &self.shared.revoke_before)
            .finish_non_exhaustive()
    }
}

/// The signing keys file and what was last read from it.
struct KeysFile {
    path: PathBuf,
    /// The text of the keys in use, to skip parsing an unchanged file. Wiped when dropped.
    loaded: Option<Zeroizing<String>>,
    /// The problem last logged, to log each one once.
    problem: Option<String>,
}

impl KeysFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            loaded: None,
            problem: None,
        }
    }

    async fn reload(&mut self, issuer: &Issuer) {
        let path = self.path.clone();
        let read = tokio::task::spawn_blocking(move || read(&path))
            .await
            .unwrap_or_else(|_| Err("reading it failed".to_owned()));
        let text = match read {
            Ok(text) => text,
            Err(problem) => return self.unusable(problem, issuer),
        };
        if self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.as_str() == text.as_str())
        {
            return;
        }
        match SigningKeys::parse(&text) {
            Ok(keys) if keys.is_empty() => {
                self.unusable("the file holds no keys".to_owned(), issuer);
            }
            Ok(keys) => {
                tracing::info!(
                    file = %self.path.display(),
                    keys = ?keys.key_ids().collect::<Vec<_>>(),
                    "loaded the internal identity signing keys"
                );
                issuer.replace(keys);
                self.loaded = Some(text);
                self.problem = None;
            }
            Err(error) => self.unusable(format!("not a usable key set: {error}"), issuer),
        }
    }

    fn unusable(&mut self, problem: String, issuer: &Issuer) {
        self.loaded = None;
        if self.problem.as_ref() == Some(&problem) {
            return;
        }
        match issuer.key_ids() {
            Some(kept) => tracing::warn!(
                file = %self.path.display(),
                problem = %problem,
                kept = ?kept,
                "the signing keys file is not usable; keeping the last usable keys"
            ),
            None => tracing::warn!(
                file = %self.path.display(),
                problem = %problem,
                "no usable signing keys yet; not ready"
            ),
        }
        self.problem = Some(problem);
    }
}

/// The file's text: at most [`MAX_DOCUMENT_BYTES`], UTF-8, wiped when dropped. Errors say why,
/// never what it holds.
fn read(path: &Path) -> Result<Zeroizing<String>, String> {
    let file = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "the file does not exist (yet)".to_owned(),
        kind => format!("cannot open the file ({kind})"),
    })?;
    let limit = u64::try_from(MAX_DOCUMENT_BYTES).unwrap_or(u64::MAX);
    // Room for the largest file up front, so the buffer never moves while it grows and leaves an
    // unwiped copy of the keys behind.
    let mut text = Zeroizing::new(String::with_capacity(MAX_DOCUMENT_BYTES + 1));
    file.take(limit.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|_| "the file is not UTF-8 text".to_owned())?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "the file is larger than {MAX_DOCUMENT_BYTES} bytes"
        ));
    }
    Ok(text)
}

/// What the watch loop reports: which key signs, whether the signing keys and the published
/// public keys have differed for too long, and whether the signing key is overdue for its
/// rotation by age.
struct Monitor {
    propagation: i64,
    /// The rotation CronJob's maximum key age; `None` while rotation by age is off.
    max_key_age: Option<i64>,
    /// The key id last reported as overdue, so each key is reported once.
    overdue: Option<String>,
    /// The key id last reported as signing (`Some(None)`: reported that none may sign).
    signing: Option<Option<String>>,
    /// Since when the two sets have differed (Unix time).
    differ_since: Option<i64>,
    /// Whether the difference was reported as an error.
    reported: bool,
}

impl Monitor {
    fn new(propagation: i64) -> Self {
        Self {
            propagation,
            max_key_age: None,
            overdue: None,
            signing: None,
            differ_since: None,
            reported: false,
        }
    }

    fn with_max_key_age(mut self, max_key_age: Option<i64>) -> Self {
        self.max_key_age = max_key_age;
        self
    }

    /// Logs an ERROR, once per key, when the key that signs at `now` should have been replaced
    /// by the rotation CronJob two days ago or more: the CronJob does not run (suspended, its
    /// image cannot be pulled, the controller is down) or keeps failing. Key id and times only.
    fn observe_age(&mut self, now: i64, issuer: &Issuer) {
        let Some(max_key_age) = self.max_key_age else {
            return;
        };
        let Some((kid, Some(published_at))) = issuer.signing_published(now) else {
            return;
        };
        let overdue_at = published_at
            .saturating_add(max_key_age)
            .saturating_add(self.propagation)
            .saturating_add(identity_keys::ROTATION_OVERDUE_SECS);
        if now < overdue_at || self.overdue.as_deref() == Some(kid.as_str()) {
            return;
        }
        tracing::error!(
            kid = %kid,
            published_at = %identity_keys::rfc3339(published_at),
            max_key_age_seconds = max_key_age,
            "the internal identity signing key is older than internalIdentity.maxKeyAgeSeconds \
             allows, two days past its rotation: the rotation CronJob \
             <release>-identity-key-rotation does not run or keeps failing. kubectl get \
             cronjob,jobs shows its last runs and kubectl logs job/<job> why \
             (docs/runbooks/secrets.md)"
        );
        self.overdue = Some(kid);
    }

    fn observe(&mut self, now: i64, issuer: &Issuer, published: &PublishedKeys) {
        let signing = issuer.signing_kid(now);
        if self.signing.as_ref() != Some(&signing) {
            match &signing {
                Some(kid) => tracing::info!(kid, "signing internal identity tokens"),
                None => tracing::warn!(
                    revoke_before = issuer.shared.revoke_before,
                    "no signing key may sign now (none loaded, none scheduled to sign yet, all \
                     revoked, or none in the published public keys); not ready"
                ),
            }
            self.signing = Some(signing);
        }
        self.observe_age(now, issuer);
        let Some(secret) = issuer.public_keys() else {
            return;
        };
        let public: Vec<String> = published
            .current()
            .map(|keys| keys.key_ids().map(str::to_owned).collect())
            .unwrap_or_default();
        // A key id published with another public key counts as unpublished.
        let unpublished: Vec<&String> = secret
            .iter()
            .filter(|(kid, key)| !published.publishes(kid, key))
            .map(|(kid, _)| kid)
            .collect();
        let unknown: Vec<&String> = public
            .iter()
            .filter(|kid| !secret.iter().any(|(own, _)| own == *kid))
            .collect();
        if unpublished.is_empty() && unknown.is_empty() {
            if self.reported {
                tracing::info!("the published public keys match the signing keys again");
            }
            self.differ_since = None;
            self.reported = false;
            return;
        }
        let since = *self.differ_since.get_or_insert(now);
        if !self.reported && now.saturating_sub(since) > self.propagation {
            tracing::error!(
                unpublished = ?unpublished,
                unknown = ?unknown,
                seconds = now.saturating_sub(since),
                "the public keys every service reads have differed from the signing keys for \
                 longer than internalIdentity.keyPropagationSeconds; services reject tokens of an \
                 unpublished key and accept an unknown one. kubectl rollout restart \
                 deployment/<release>-identity rewrites the ConfigMap from the Secret \
                 (docs/runbooks/secrets.md)"
            );
            self.reported = true;
        }
    }
}

/// `<header>.<payload>.<signature>`, each base64url without padding; the signature covers the
/// first two segments as written.
fn sign(key: &SigningKey, kid: &str, claims: &InternalClaims) -> Result<String, IssueError> {
    let header = Header {
        alg: ALGORITHM,
        typ: TOKEN_TYPE,
        kid,
    };
    let header = serde_json::to_vec(&header).map_err(|_| IssueError::Build)?;
    let payload = serde_json::to_vec(claims).map_err(|_| IssueError::Build)?;
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(payload)
    );
    let signature = key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// Seconds since the Unix epoch. A clock before the epoch reads as 0, so tokens come out expired
/// and services reject them rather than accept them.
#[must_use]
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Why no token was signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IssueError {
    /// No key may sign now: none loaded, none whose `signs_from` has come, every one revoked, or
    /// none in the published public keys. The service is not ready meanwhile.
    #[error("no signing key may sign now")]
    NoSigningKey,
    /// A token could not be built; the types make this unreachable in practice.
    #[error("could not build an internal identity token")]
    Build,
}

#[cfg(test)]
pub(crate) mod test_keys {
    //! Throwaway keys derived in code from a one-byte seed: nothing outside the tests trusts
    //! them, and no key material is stored in the repository.

    use ed25519_dalek::SigningKey;
    use identity_keys::SigningKeys;
    use serde_json::json;
    use service_kit::identity::IdentityKeys;

    use super::*;

    pub(crate) fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The public JWK `x` member.
    pub(crate) fn x(seed: u8) -> String {
        URL_SAFE_NO_PAD.encode(signing_key(seed).verifying_key().as_bytes())
    }

    /// A key set with these (seed, kid) entries.
    pub(crate) fn keys(entries: &[(u8, &str)]) -> IdentityKeys {
        let keys: Vec<_> = entries
            .iter()
            .map(|(seed, kid)| json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": x(*seed)}))
            .collect();
        IdentityKeys::from_jwks(&json!({ "keys": keys }).to_string()).expect("valid key set")
    }

    /// Signing keys with these (seed, kid, signs_from) entries, each of generation 1 and
    /// published at 0.
    pub(crate) fn signing_keys(entries: &[(u8, &str, i64)]) -> SigningKeys {
        let keys: Vec<_> = entries
            .iter()
            .map(|(seed, kid, signs_from)| {
                json!({
                    "kid": kid, "generation": 1, "x": x(*seed),
                    "d": URL_SAFE_NO_PAD.encode([*seed; 32]),
                    "published_at": 0, "signs_from": signs_from
                })
            })
            .collect();
        SigningKeys::parse(&json!({ "keys": keys }).to_string()).expect("valid signing keys")
    }

    /// An issuer that signs with the key of `seed` under the key id `entries` gives it, with
    /// `entries` as the published public keys.
    pub(crate) fn issuer(seed: u8, entries: &[(u8, &str)]) -> Issuer {
        let kid = entries
            .iter()
            .find(|(candidate, _)| *candidate == seed)
            .map_or("unpublished", |(_, kid)| kid);
        Issuer::with_keys(signing_keys(&[(seed, kid, 0)]), keys(entries).into(), 0)
    }
}

#[cfg(test)]
mod tests {
    use contract::{TenantGrant, TenantRole};
    use serde_json::{Value, json};
    use service_kit::identity::{IdentityError, IdentityVerifier};

    use super::test_keys::{issuer, keys, signing_key, signing_keys};
    use super::*;

    const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
    const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

    fn grant() -> SessionGrant {
        SessionGrant {
            user: USER.parse().expect("user"),
            tenant: Some(TenantGrant {
                id: TENANT.parse().expect("tenant"),
                role: TenantRole::Editor,
            }),
            platform_admin: true,
        }
    }

    fn segment(token: &str, index: usize) -> Value {
        let part = token.split('.').nth(index).expect("segment");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).expect("base64url")).expect("JSON")
    }

    fn kid_of(token: &InternalToken) -> Value {
        segment(token.expose(), 0)["kid"].clone()
    }

    #[test]
    fn tokens_pass_service_kit_verification_for_their_audience_only() {
        let issuer = issuer(1, &[(1, "2026-10-a")]);
        let token = issuer
            .issue(&grant(), ServiceName::Assets, unix_now())
            .expect("token");
        let keys = keys(&[(1, "2026-10-a")]);
        let user = IdentityVerifier::new(ServiceName::Assets, keys.clone())
            .verify(token.expose())
            .expect("assets accepts its token");
        assert_eq!(user.user_id(), grant().user);
        assert_eq!(user.tenant(), grant().tenant);
        assert!(user.is_platform_admin());
        // The same token replayed at another service is refused there.
        for other in [
            ServiceName::Certs,
            ServiceName::Identity,
            ServiceName::Alerts,
        ] {
            assert_eq!(
                IdentityVerifier::new(other, keys.clone())
                    .verify(token.expose())
                    .err(),
                Some(IdentityError::WrongAudience),
                "{other}"
            );
        }
    }

    #[test]
    fn the_header_and_claims_are_exactly_the_contract() {
        let issuer = issuer(1, &[(1, "2026-10-a")]);
        let now = 1_791_590_400;
        let token = issuer
            .issue(&grant(), ServiceName::Certs, now)
            .expect("token");
        assert_eq!(token.expose().split('.').count(), 3);
        assert_eq!(
            segment(token.expose(), 0),
            json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": "2026-10-a"})
        );
        assert_eq!(
            segment(token.expose(), 1),
            json!({
                "iss": "identity",
                "sub": USER,
                "aud": "certs",
                "iat": now,
                "exp": now + 60,
                "tenant": {"id": TENANT, "role": "editor"},
                "platform_admin": true
            })
        );
    }

    /// The replay window of a captured token is its 60 s lifetime plus the verifiers' 5 s of
    /// clock skew, and no more.
    #[test]
    fn a_token_stops_working_after_its_lifetime_and_the_skew() {
        let issuer = issuer(1, &[(1, "k")]);
        let verifier = IdentityVerifier::new(ServiceName::Assets, keys(&[(1, "k")]));
        let now = unix_now();
        let fresh = issuer
            .issue(&grant(), ServiceName::Assets, now - 60)
            .expect("token");
        assert!(verifier.verify(fresh.expose()).is_ok(), "within exp + 5 s");
        let stale = issuer
            .issue(&grant(), ServiceName::Assets, now - TOKEN_LIFETIME_SECS - 5)
            .expect("token");
        assert_eq!(
            verifier.verify(stale.expose()).err(),
            Some(IdentityError::Expired)
        );
    }

    /// Acceptance (Laysi/trustiot-v2#404): the key switches at its `signs_from`, by the clock
    /// alone, in every replica that reads the same files.
    #[test]
    fn the_signing_key_switches_at_its_scheduled_time() {
        let published = PublishedKeys::from(keys(&[(1, "old"), (2, "new")]));
        let issuer = Issuer::with_keys(
            signing_keys(&[(1, "old", 0), (2, "new", 1000)]),
            published.clone(),
            0,
        );
        let at = |now| {
            issuer
                .issue(&grant(), ServiceName::Assets, now)
                .map(|token| kid_of(&token))
        };
        assert_eq!(at(999), Ok(json!("old")));
        assert_eq!(at(1000), Ok(json!("new")));
        assert_eq!(issuer.signing_kid(1000).as_deref(), Some("new"));

        // A replica whose public keys file has not got the new key yet keeps the old key.
        published.replace(keys(&[(1, "old")]));
        assert_eq!(at(1000), Ok(json!("old")));
        // One whose public keys lack every signing key signs nothing.
        published.replace(keys(&[(3, "other")]));
        assert_eq!(at(1000), Err(IssueError::NoSigningKey));
    }

    #[test]
    fn revoked_generations_and_missing_keys_never_sign() {
        let published = PublishedKeys::from(keys(&[(1, "k")]));
        let revoked = Issuer::with_keys(signing_keys(&[(1, "k", 0)]), published.clone(), 2);
        assert_eq!(
            revoked
                .issue(&grant(), ServiceName::Assets, unix_now())
                .err(),
            Some(IssueError::NoSigningKey)
        );
        let empty = Issuer::new(published, 0);
        assert_eq!(empty.signing_kid(unix_now()), None);
        assert_eq!(
            empty.issue(&grant(), ServiceName::Assets, unix_now()).err(),
            Some(IssueError::NoSigningKey)
        );
        // A key scheduled for later does not sign before then.
        let later = Issuer::with_keys(
            signing_keys(&[(1, "k", i64::MAX)]),
            PublishedKeys::from(keys(&[(1, "k")])),
            0,
        );
        assert_eq!(later.signing_kid(unix_now()), None);
    }

    /// Laysi/trustiot-v2#103: login codes are hashed with a key derived from the key that signs,
    /// and checked against the keys of every signing key that is not revoked.
    #[test]
    fn login_code_keys_come_from_the_signing_keys() {
        let issuer = Issuer::with_keys(
            signing_keys(&[(1, "old", 0), (2, "new", 1000)]),
            PublishedKeys::from(keys(&[(1, "old"), (2, "new")])),
            0,
        );
        let old = issuer.code_key(999).expect("the old key signs at 999");
        let new = issuer.code_key(1000).expect("the new key signs at 1000");
        assert_ne!(old, new);
        assert_eq!(issuer.code_key(1000), Some(new.clone()), "stable");
        // Derived, not the private key itself.
        assert_ne!(*old, [1_u8; 32]);
        assert_ne!(*new, [2_u8; 32]);
        // A code hashed just before the switch can still be checked after it.
        let all = issuer.code_keys();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&old) && all.contains(&new));

        // A revoked generation hashes and checks nothing; neither does an issuer without keys.
        let published = PublishedKeys::from(keys(&[(1, "k")]));
        let revoked = Issuer::with_keys(signing_keys(&[(1, "k", 0)]), published.clone(), 2);
        assert_eq!(revoked.code_key(unix_now()), None);
        assert!(revoked.code_keys().is_empty());
        let empty = Issuer::new(published, 0);
        assert_eq!(empty.code_key(unix_now()), None);
        assert!(empty.code_keys().is_empty());
    }

    /// Acceptance (Laysi/trustiot-v2#61): the private key never appears in error messages or
    /// debug output, in any of its encodings.
    #[test]
    fn errors_and_debug_output_never_show_the_private_key() {
        let seed = [1_u8; 32];
        let secrets = [
            URL_SAFE_NO_PAD.encode(seed),
            base64::engine::general_purpose::STANDARD.encode(seed),
            seed.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        ];
        let outputs = [
            format!("{:?}", issuer(1, &[(1, "k")])),
            IssueError::NoSigningKey.to_string(),
            format!("{:?}", IssueError::Build),
        ];
        for output in outputs {
            for secret in &secrets {
                assert!(!output.contains(secret.as_str()), "{output}");
            }
        }
        assert!(format!("{:?}", issuer(1, &[(1, "k")])).contains("\"k\""));
        assert_eq!(
            signing_key(1).to_bytes(),
            seed,
            "the secrets above are this key"
        );
    }

    /// A directory of its own under the system's temporary directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "identity-{name}-{}-{:?}",
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

    fn publish(path: &Path, text: &str) {
        let next = path.with_extension("next");
        std::fs::write(&next, text).expect("write");
        std::fs::rename(&next, path).expect("rename");
    }

    #[tokio::test]
    async fn the_last_usable_signing_keys_stay_until_usable_ones_replace_them() {
        let scratch = Scratch::new("signing-keys");
        let path = scratch.0.join("signing-keys.json");
        let issuer = Issuer::new(PublishedKeys::from(keys(&[(1, "a"), (2, "b")])), 0);
        let mut file = KeysFile::new(path.clone());
        let now = unix_now();

        // Not there yet (a fresh install before the kubelet updates the volume).
        file.reload(&issuer).await;
        assert_eq!(issuer.signing_kid(now), None);
        publish(&path, r#"{"keys":[]}"#);
        file.reload(&issuer).await;
        assert_eq!(issuer.signing_kid(now), None);

        publish(&path, &signing_keys(&[(1, "a", 0)]).to_json());
        file.reload(&issuer).await;
        assert_eq!(issuer.signing_kid(now).as_deref(), Some("a"));

        // Broken, emptied or gone: the last usable keys stay.
        let pem_line = format!("-----{} PRIVATE KEY-----", "BEGIN");
        for broken in ["", "not JSON", r#"{"keys":[]}"#, pem_line.as_str()] {
            publish(&path, broken);
            file.reload(&issuer).await;
            assert_eq!(issuer.signing_kid(now).as_deref(), Some("a"), "{broken}");
        }
        std::fs::remove_file(&path).expect("remove");
        file.reload(&issuer).await;
        assert_eq!(issuer.signing_kid(now).as_deref(), Some("a"));

        // A rotation arrives without a restart.
        publish(&path, &signing_keys(&[(1, "a", 0), (2, "b", 1)]).to_json());
        file.reload(&issuer).await;
        assert_eq!(issuer.signing_kid(now).as_deref(), Some("b"));
    }

    #[test]
    fn a_lasting_difference_between_the_two_files_is_reported_once() {
        let published = PublishedKeys::from(keys(&[(1, "a")]));
        let issuer = Issuer::with_keys(
            signing_keys(&[(1, "a", 0), (2, "b", 0)]),
            published.clone(),
            0,
        );
        let mut monitor = Monitor::new(600);
        monitor.observe(1000, &issuer, &published);
        assert_eq!(monitor.differ_since, Some(1000));
        assert!(!monitor.reported);
        monitor.observe(1600, &issuer, &published);
        assert!(!monitor.reported, "within the propagation time");
        monitor.observe(1601, &issuer, &published);
        assert!(monitor.reported);
        published.replace(keys(&[(1, "a"), (2, "b")]));
        monitor.observe(1602, &issuer, &published);
        assert_eq!((monitor.differ_since, monitor.reported), (None, false));
        assert_eq!(monitor.signing, Some(Some("b".to_owned())));
    }

    /// Laysi/trustiot-v2#429: a signing key two days past its rotation by age is reported once;
    /// without a maximum age, never.
    #[test]
    fn a_signing_key_overdue_for_its_rotation_is_reported_once() {
        let published = PublishedKeys::from(keys(&[(1, "a"), (2, "b")]));
        // Both published at 0; "a" signs from 0, "b" from 200000, after "a" is overdue.
        let issuer = Issuer::with_keys(
            signing_keys(&[(1, "a", 0), (2, "b", 200_000)]),
            published.clone(),
            0,
        );
        let overdue_at = 1000 + 600 + identity_keys::ROTATION_OVERDUE_SECS;
        let mut off = Monitor::new(600);
        off.observe(overdue_at, &issuer, &published);
        assert_eq!(off.overdue, None);

        let mut monitor = Monitor::new(600).with_max_key_age(Some(1000));
        monitor.observe(overdue_at - 1, &issuer, &published);
        assert_eq!(monitor.overdue, None, "not yet overdue");
        monitor.observe(overdue_at, &issuer, &published);
        assert_eq!(monitor.overdue.as_deref(), Some("a"));
        // The next key is reported on its own once it is overdue too.
        monitor.observe(200_000, &issuer, &published);
        assert_eq!(monitor.overdue.as_deref(), Some("b"));
    }
}
