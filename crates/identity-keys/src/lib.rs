//! The identity service's signing keys for internal identity tokens (decision 27), generated in
//! the cluster (Laysi/trustiot-v2#404; docs/architecture/secrets.md, 「內部身分簽章金鑰」).
//!
//! - [`SigningKeys`]: the key set the identity pod's init container (`migrate
//!   identity-signing-keys`) keeps in the Secret `<release>-identity-signing-keys` as
//!   [`SIGNING_KEYS_FILE`], and the identity service reads from the mounted Secret. Each key has a
//!   key id (its RFC 7638 thumbprint, or the id it had when a person made it), a generation, the
//!   Ed25519 private key, and once published the time it was published and the time it starts
//!   signing.
//! - [`SigningKeys::jwks`]: the public JWK Set derived from the set, which the init container
//!   writes to the ConfigMap `<release>-internal-identity-keys` as [`JWKS_FILE`] and every service
//!   verifies tokens with.
//! - [`SigningKeys::plan`] and [`SigningKeys::publish`]: what the init container does to the set
//!   on each run: import a person's key (the move from before Laysi/trustiot-v2#404), revoke,
//!   add a key for a new generation, retire keys whose tokens have expired.
//! - [`SigningKeys::signing_key`]: which key the identity service signs with at a given time.
//!
//! The rules in short: a new key is written to the Secret first, then published in the JWK Set,
//! then given `published_at` (that moment) and `signs_from` (`published_at` plus the propagation
//! time, or `published_at` itself when no other key can sign). Keys older than the newest signing
//! key go once its `signs_from` is `propagation + TOKEN_EXPIRY_SECS` in the past. Nothing here
//! formats key material: `Debug`, errors and [`Change`] carry key ids and times only.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::pkcs8::DecodePrivateKey as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeroize::{Zeroize as _, Zeroizing};

/// The key of the Secret's data, and the file name in the identity service's mount, that holds
/// the key set.
pub const SIGNING_KEYS_FILE: &str = "signing-keys.json";

/// The key of the ConfigMap's data, and the file name in every service's mount, that holds the
/// public JWK Set.
pub const JWKS_FILE: &str = "jwks.json";

/// Most keys a set may hold: the most a service accepts in its JWK Set
/// (`service_kit::identity::MAX_KEYS`). A rotation needs two or three at a time.
pub const MAX_KEYS: usize = 8;

/// Highest generation: 1 to 9 decimal digits, as the chart's schema allows.
pub const MAX_GENERATION: u64 = 999_999_999;

/// Longest key id, in bytes (`service_kit::identity::MAX_KEY_ID_LEN`).
pub const MAX_KEY_ID_LEN: usize = 64;

/// How long after a key stops signing its tokens can still verify, in seconds: the longest
/// lifetime a verifier accepts (300 s, `service_kit::identity::MAX_LIFETIME_SECS`) plus its clock
/// leeway (5 s, `LEEWAY_SECS`). Not the 60 s the identity service actually signs for, so the rule
/// holds whatever lifetime it uses up to the verifiers' cap.
pub const TOKEN_EXPIRY_SECS: i64 = 305;

/// Largest key set document read, in bytes; eight keys take about 2 KiB.
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// The generation of a key imported from a Secret a person made (before Laysi/trustiot-v2#404):
/// below every generation the chart requests, so removing the person's Secret from the values
/// makes the next run replace it.
pub const IMPORTED_GENERATION: u64 = 0;

/// One Ed25519 key of the set.
#[derive(Clone)]
pub struct SigningKey {
    kid: String,
    generation: u64,
    key: ed25519_dalek::SigningKey,
    published_at: Option<i64>,
    signs_from: Option<i64>,
}

impl SigningKey {
    /// A new key from the operating system's CSPRNG for `generation`, not yet published. Its key
    /// id is the RFC 7638 thumbprint of its public key.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness to give.
    pub fn generate(generation: u64) -> Result<Self, RandomError> {
        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(|error| RandomError(error.to_string()))?;
        Ok(Self::from_seed(&seed, generation))
    }

    /// The key with this 32-byte seed (the Ed25519 private key) for `generation`, not yet
    /// published, under its thumbprint.
    #[must_use]
    pub fn from_seed(seed: &[u8; 32], generation: u64) -> Self {
        let key = ed25519_dalek::SigningKey::from_bytes(seed);
        let kid = thumbprint(key.verifying_key().as_bytes());
        Self {
            kid,
            generation,
            key,
            published_at: None,
            signs_from: None,
        }
    }

    /// A key a person made before Laysi/trustiot-v2#404, in PKCS#8 PEM (`openssl genpkey
    /// -algorithm ed25519`), under the key id `public_keys` (the chart's
    /// `internalIdentity.publicKeys` as a JWK Set) lists for its public key. It gets
    /// [`IMPORTED_GENERATION`] and is not yet published.
    ///
    /// # Errors
    ///
    /// When the PEM is not an Ed25519 PKCS#8 private key, `public_keys` is not a JWK Set of
    /// `kid` and `x` members, or its public key is listed under no key id or under more than
    /// one. Errors never quote the PEM.
    pub fn import(pem: &str, public_keys: &str) -> Result<Self, ImportError> {
        let key = ed25519_dalek::SigningKey::from_pkcs8_pem(pem)
            .map_err(|_| ImportError::NotPkcs8Ed25519)?;
        #[derive(Deserialize)]
        struct Set {
            keys: Vec<Listed>,
        }
        #[derive(Deserialize)]
        struct Listed {
            kid: String,
            x: String,
        }
        let set: Set =
            serde_json::from_str(public_keys).map_err(|_| ImportError::PublicKeysInvalid)?;
        let x = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let mut matching = set.keys.into_iter().filter(|listed| listed.x == x);
        let kid = match (matching.next(), matching.next()) {
            (Some(listed), None) => listed.kid,
            (None, _) => return Err(ImportError::NotListed),
            (Some(_), Some(_)) => return Err(ImportError::ListedTwice),
        };
        if !is_key_id(&kid) {
            return Err(ImportError::PublicKeysInvalid);
        }
        Ok(Self {
            kid,
            generation: IMPORTED_GENERATION,
            key,
            published_at: None,
            signs_from: None,
        })
    }

    /// The key id tokens name in their header.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The generation the key was made for.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// When the public key was published (seconds since the Unix epoch); `None` until then.
    #[must_use]
    pub fn published_at(&self) -> Option<i64> {
        self.published_at
    }

    /// When the key starts signing (seconds since the Unix epoch); `None` until it is published.
    #[must_use]
    pub fn signs_from(&self) -> Option<i64> {
        self.signs_from
    }

    /// Whether the key may sign at `now`: it is published and its `signs_from` has come.
    #[must_use]
    pub fn signs_at(&self, now: i64) -> bool {
        self.signs_from.is_some_and(|from| from <= now)
    }

    /// The public key.
    #[must_use]
    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.key.verifying_key()
    }

    /// The public key, base64url without padding (the JWK `x`).
    #[must_use]
    pub fn x(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes())
    }

    /// The private key, to sign with.
    #[must_use]
    pub fn signer(&self) -> &ed25519_dalek::SigningKey {
        &self.key
    }

    fn same_key(&self, other: &Self) -> bool {
        self.kid == other.kid
            && self.generation == other.generation
            && self.published_at == other.published_at
            && self.signs_from == other.signs_from
            && self.key.as_bytes() == other.key.as_bytes()
    }
}

impl fmt::Debug for SigningKey {
    /// The key id, generation and times; never the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .field("generation", &self.generation)
            .field("published_at", &self.published_at)
            .field("signs_from", &self.signs_from)
            .finish_non_exhaustive()
    }
}

/// The RFC 7638 thumbprint of an Ed25519 public key: the SHA-256 of
/// `{"crv":"Ed25519","kty":"OKP","x":"<x>"}`, base64url without padding (43 characters).
#[must_use]
pub fn thumbprint(public_key: &[u8; 32]) -> String {
    let canonical = format!(
        r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#,
        URL_SAFE_NO_PAD.encode(public_key)
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// Whether `text` can be a key id: 1 to [`MAX_KEY_ID_LEN`] visible ASCII characters, as
/// service-kit and the chart's schema accept.
#[must_use]
pub fn is_key_id(text: &str) -> bool {
    (1..=MAX_KEY_ID_LEN).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_graphic())
}

/// The key set: the keys in the order they were added. Nothing in it is shown by `Debug` but
/// key ids, generations and times.
#[derive(Clone, Debug, Default)]
pub struct SigningKeys {
    keys: Vec<SigningKey>,
}

/// One key as the document stores it. No `Debug`: `d` is the private key.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    kid: String,
    generation: u64,
    x: String,
    d: StoredPrivateKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signs_from: Option<i64>,
}

/// A private key as the document stores it (base64url). Wiped from memory when dropped, on every
/// path: a key that turns out invalid, the keys after it, and a document that fails to parse
/// halfway. No `Debug`.
#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct StoredPrivateKey(String);

impl Drop for StoredPrivateKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    keys: Vec<Stored>,
}

/// A public JWK as every service reads it.
#[derive(Serialize)]
struct PublicJwk<'a> {
    kty: &'static str,
    crv: &'static str,
    kid: &'a str,
    x: String,
    alg: &'static str,
    #[serde(rename = "use")]
    use_: &'static str,
}

#[derive(Serialize)]
struct PublicSet<'a> {
    keys: Vec<PublicJwk<'a>>,
}

impl SigningKeys {
    /// The keys, oldest first.
    #[must_use]
    pub fn keys(&self) -> &[SigningKey] {
        &self.keys
    }

    /// Whether the set has no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key ids, oldest first.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(SigningKey::kid)
    }

    /// Reads the document of [`SIGNING_KEYS_FILE`].
    ///
    /// Every key needs a key id of 1 to [`MAX_KEY_ID_LEN`] visible ASCII characters that no
    /// other key uses, a generation up to [`MAX_GENERATION`], a 32-byte private key `d` and its
    /// public key `x` (both base64url without padding, the one canonical spelling), and either
    /// both times or neither, with `signs_from` not before `published_at`. At most
    /// [`MAX_KEYS`] keys; a document of none is an empty set.
    ///
    /// # Errors
    ///
    /// When the document is not such a set; the error names the position of the key and the
    /// problem, never a value.
    pub fn parse(text: &str) -> Result<Self, InvalidSigningKeys> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err(InvalidSigningKeys::TooLarge);
        }
        let document: Document =
            serde_json::from_str(text).map_err(|_| InvalidSigningKeys::NotADocument)?;
        if document.keys.len() > MAX_KEYS {
            return Err(InvalidSigningKeys::TooMany);
        }
        let mut keys: Vec<SigningKey> = Vec::with_capacity(document.keys.len());
        for (index, stored) in document.keys.into_iter().enumerate() {
            let key = from_stored(stored)
                .map_err(|problem| InvalidSigningKeys::Key { index, problem })?;
            if keys.iter().any(|existing| existing.kid == key.kid) {
                return Err(InvalidSigningKeys::Key {
                    index,
                    problem: KeyProblem::DuplicateKeyId,
                });
            }
            keys.push(key);
        }
        Ok(Self { keys })
    }

    /// The document of [`SIGNING_KEYS_FILE`]: one line of JSON, the keys in order, the same text
    /// for the same set. Wiped from memory when dropped.
    ///
    /// # Panics
    ///
    /// Never: the document always serializes.
    #[must_use]
    pub fn to_json(&self) -> Zeroizing<String> {
        let document = Document {
            keys: self
                .keys
                .iter()
                .map(|key| Stored {
                    kid: key.kid.clone(),
                    generation: key.generation,
                    x: key.x(),
                    d: StoredPrivateKey(URL_SAFE_NO_PAD.encode(key.key.as_bytes())),
                    published_at: key.published_at,
                    signs_from: key.signs_from,
                })
                .collect(),
        };
        // Room for the largest document up front, so the buffer never moves while it grows and
        // leaves a copy of the keys behind; the private keys in `document` are wiped when it
        // drops.
        let mut buffer = Zeroizing::new(Vec::with_capacity(MAX_DOCUMENT_BYTES));
        serde_json::to_writer(&mut *buffer, &document)
            .expect("a key set always serializes to JSON");
        Zeroizing::new(
            String::from_utf8(std::mem::take(&mut *buffer)).expect("serde_json writes UTF-8"),
        )
    }

    /// The public JWK Set of every key in the set, published or not, as [`JWKS_FILE`] holds it:
    /// `kty`, `crv`, `kid`, `x`, `alg` and `use` per key, in the set's order, the same text for
    /// the same set. Never a private member.
    ///
    /// # Panics
    ///
    /// Never: the set always serializes.
    #[must_use]
    pub fn jwks(&self) -> String {
        let set = PublicSet {
            keys: self
                .keys
                .iter()
                .map(|key| PublicJwk {
                    kty: "OKP",
                    crv: "Ed25519",
                    kid: &key.kid,
                    x: key.x(),
                    alg: "EdDSA",
                    use_: "sig",
                })
                .collect(),
        };
        serde_json::to_string(&set).expect("a public key set always serializes to JSON")
    }

    /// The key the identity service signs with at `now`: among the keys that sign at `now`
    /// ([`SigningKey::signs_at`]), whose generation is not below `revoke_before`, and whose key id
    /// is `published` (in the JWK Set the service itself currently sees), the one with the
    /// latest `signs_from` (then the highest generation). `None` when no key qualifies: the
    /// service then signs nothing.
    #[must_use]
    pub fn signing_key(
        &self,
        now: i64,
        revoke_before: u64,
        published: impl Fn(&SigningKey) -> bool,
    ) -> Option<&SigningKey> {
        self.keys
            .iter()
            .filter(|key| key.signs_at(now) && key.generation >= revoke_before)
            .filter(|key| published(key))
            .max_by_key(|key| (key.signs_from, key.generation))
    }

    /// When the keys older than the newest key that signs at `now` may go, if there is such a
    /// key: its `signs_from` plus the time it waited after being published (the propagation
    /// time) plus [`TOKEN_EXPIRY_SECS`].
    #[must_use]
    pub fn retire_at(&self, now: i64) -> Option<i64> {
        let newest = self
            .keys
            .iter()
            .filter(|key| key.signs_at(now))
            .max_by_key(|key| (key.signs_from, key.generation))?;
        let (published_at, signs_from) = (newest.published_at?, newest.signs_from?);
        Some(
            signs_from
                .saturating_add(signs_from.saturating_sub(published_at))
                .saturating_add(TOKEN_EXPIRY_SECS),
        )
    }

    /// What one run of the init container makes of the set at `now`, in this order:
    ///
    /// 1. **Import**: with a person's key configured (`request.legacy`), an empty set gets it.
    ///    Only into an empty set, so a key the cluster already replaced never comes back, and
    ///    only while `revoke_before` is 0, since the imported key is of generation 0.
    /// 2. **Revoke**: keys of a generation below `request.revoke_before` go at once.
    /// 3. **Add**: an empty set, or (without a person's key configured) a set whose highest
    ///    generation is below `request.generation`, gets a new key from `generate`. Keys that
    ///    have not started signing yet go first: they never signed a token.
    /// 4. **Retire**: without a person's key configured, keys older than the newest signing key
    ///    go once [`SigningKeys::retire_at`] has come.
    ///
    /// While a person's key is configured nothing is added or retired except as revocation
    /// requires, so the move from it never changes the signing key while pods of the release
    /// before still run (docs/architecture/secrets.md). New and imported keys are not published:
    /// [`SigningKeys::publish`] does that after the JWK Set is written.
    ///
    /// # Errors
    ///
    /// When `generate` fails, or the set would hold more than [`MAX_KEYS`] keys (the error says
    /// when the oldest can go).
    pub fn plan(
        mut self,
        request: &Request,
        now: i64,
        mut generate: impl FnMut(u64) -> Result<SigningKey, RandomError>,
    ) -> Result<Plan, PlanError> {
        let mut changes = Vec::new();
        if self.keys.is_empty()
            && request.revoke_before == 0
            && let Some(legacy) = request.legacy
        {
            let mut imported = legacy.clone();
            imported.generation = IMPORTED_GENERATION;
            imported.published_at = None;
            imported.signs_from = None;
            changes.push(Change::Imported {
                kid: imported.kid.clone(),
            });
            self.keys.push(imported);
        }
        let revoke_before = request.revoke_before;
        self.keys.retain(|key| {
            let keep = key.generation >= revoke_before;
            if !keep {
                changes.push(Change::Revoked {
                    kid: key.kid.clone(),
                    generation: key.generation,
                });
            }
            keep
        });
        let legacy_configured = request.legacy.is_some();
        let highest = self.keys.iter().map(SigningKey::generation).max();
        let add = match highest {
            None => true,
            Some(highest) => !legacy_configured && highest < request.generation,
        };
        if add {
            self.keys.retain(|key| {
                let keep = key.signs_at(now);
                if !keep {
                    changes.push(Change::Dropped {
                        kid: key.kid.clone(),
                    });
                }
                keep
            });
            let key = generate(request.generation)?;
            changes.push(Change::Generated {
                kid: key.kid.clone(),
                generation: key.generation,
            });
            self.keys.push(key);
        }
        if !legacy_configured
            && let Some(retire_at) = self.retire_at(now)
            && now >= retire_at
            && let Some(newest) = self
                .keys
                .iter()
                .filter(|key| key.signs_at(now))
                .max_by_key(|key| (key.signs_from, key.generation))
                .and_then(SigningKey::signs_from)
        {
            self.keys.retain(|key| {
                let keep = key.signs_from.is_none_or(|from| from >= newest);
                if !keep {
                    changes.push(Change::Retired {
                        kid: key.kid.clone(),
                    });
                }
                keep
            });
        }
        if self.keys.len() > MAX_KEYS {
            return Err(PlanError::TooMany {
                retire_at: self.retire_at(now),
            });
        }
        Ok(Plan {
            keys: self,
            changes,
        })
    }

    /// Whether a key waits to be published.
    #[must_use]
    pub fn has_unpublished(&self) -> bool {
        self.keys.iter().any(|key| key.published_at.is_none())
    }

    /// Publishes every key that is not yet, at `now`, once its public key is in the JWK Set the
    /// services read: it signs from `now + propagation` when another key already signs at `now`,
    /// and from `now` when none does (a first install, a lost Secret, a revocation, an import).
    pub fn publish(&mut self, now: i64, propagation: i64) -> Vec<Change> {
        let others_sign = self.keys.iter().any(|key| key.signs_at(now));
        let signs_from = if others_sign {
            now.saturating_add(propagation)
        } else {
            now
        };
        let mut changes = Vec::new();
        for key in &mut self.keys {
            if key.published_at.is_none() {
                key.published_at = Some(now);
                key.signs_from = Some(signs_from);
                changes.push(Change::Published {
                    kid: key.kid.clone(),
                    signs_from,
                });
            }
        }
        changes
    }

    /// Whether two sets hold the same keys with the same times, in the same order.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.keys.len() == other.keys.len()
            && self
                .keys
                .iter()
                .zip(&other.keys)
                .all(|(one, two)| one.same_key(two))
    }
}

fn from_stored(stored: Stored) -> Result<SigningKey, KeyProblem> {
    let Stored {
        kid,
        generation,
        x,
        d,
        published_at,
        signs_from,
    } = stored;
    if !is_key_id(&kid) {
        return Err(KeyProblem::KeyId);
    }
    if generation > MAX_GENERATION {
        return Err(KeyProblem::Generation);
    }
    let seed = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(d.0.as_bytes())
            .map_err(|_| KeyProblem::PrivateKey)?,
    );
    let seed: &[u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| KeyProblem::PrivateKey)?;
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    if URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()) != x {
        return Err(KeyProblem::PublicKeyMismatch);
    }
    match (published_at, signs_from) {
        (None, None) => {}
        (Some(published), Some(from)) if published >= 0 && from >= published => {}
        _ => return Err(KeyProblem::Times),
    }
    Ok(SigningKey {
        kid,
        generation,
        key,
        published_at,
        signs_from,
    })
}

/// What a run asks for: the chart's `internalIdentity.keyGeneration` and
/// `internalIdentity.revokeBeforeGeneration`, and the key a person made before
/// Laysi/trustiot-v2#404 while the chart still names it (`internalIdentity.signingKeySecret`).
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub generation: u64,
    pub revoke_before: u64,
    pub legacy: Option<&'a SigningKey>,
}

/// The set a run arrived at, and what changed on the way.
#[derive(Debug)]
pub struct Plan {
    pub keys: SigningKeys,
    pub changes: Vec<Change>,
}

/// One change to the set; key ids and times only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A person's key became the set's first key.
    Imported { kid: String },
    /// A key of a generation below the revocation went at once.
    Revoked { kid: String, generation: u64 },
    /// A key that had not started signing went, replaced by a newer one.
    Dropped { kid: String },
    /// A new key was made for `generation`.
    Generated { kid: String, generation: u64 },
    /// An older key went after its tokens had expired.
    Retired { kid: String },
    /// A key was published and signs from `signs_from`.
    Published { kid: String, signs_from: i64 },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Imported { kid } => write!(
                f,
                "imported the signing key of internalIdentity.signingKeySecret as key {kid}"
            ),
            Self::Revoked { kid, generation } => write!(
                f,
                "revoked signing key {kid} (generation {generation}, below \
                 internalIdentity.revokeBeforeGeneration)"
            ),
            Self::Dropped { kid } => write!(
                f,
                "dropped signing key {kid}, which had not started signing, for a newer generation"
            ),
            Self::Generated { kid, generation } => {
                write!(f, "generated signing key {kid} (generation {generation})")
            }
            Self::Retired { kid } => write!(
                f,
                "retired signing key {kid}: every token it signed has expired"
            ),
            Self::Published { kid, signs_from } => write!(
                f,
                "published signing key {kid}; it signs from {signs_from} (Unix time)"
            ),
        }
    }
}

/// Why a document is not a key set. Never quotes the document.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSigningKeys {
    #[error("the key set is larger than {MAX_DOCUMENT_BYTES} bytes")]
    TooLarge,
    #[error("the key set is not a JSON object with a `keys` array of keys")]
    NotADocument,
    #[error("the key set has more than {MAX_KEYS} keys")]
    TooMany,
    #[error("key {index} of the key set: {problem}")]
    Key { index: usize, problem: KeyProblem },
}

/// What is wrong with one key of the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyProblem {
    #[error("the key id is not 1 to {MAX_KEY_ID_LEN} visible ASCII characters")]
    KeyId,
    #[error("another key has the same key id")]
    DuplicateKeyId,
    #[error("the generation is above {MAX_GENERATION}")]
    Generation,
    #[error("the private key is not 32 bytes in base64url without padding")]
    PrivateKey,
    #[error("the public key is not the private key's")]
    PublicKeyMismatch,
    #[error(
        "published_at and signs_from must both be set, or neither, with signs_from not earlier"
    )]
    Times,
}

/// Why a person's key cannot be imported. Never quotes the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    #[error("the key file is not an Ed25519 private key in PKCS#8 PEM")]
    NotPkcs8Ed25519,
    #[error("internalIdentity.publicKeys is not a JWK Set with a kid and x per key")]
    PublicKeysInvalid,
    #[error("the key's public key is not in internalIdentity.publicKeys")]
    NotListed,
    #[error("the key's public key is in internalIdentity.publicKeys under more than one key id")]
    ListedTwice,
}

/// The operating system gave no randomness.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no randomness for a new signing key: {0}")]
pub struct RandomError(pub String);

/// Why a run cannot arrive at a set.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Random(#[from] RandomError),
    #[error(
        "the key set would hold more than {MAX_KEYS} keys; generations went up faster than old \
         keys could retire{}",
        .retire_at.map(|at| format!(" (the oldest can go from {at}, Unix time, at the next identity pod start)")).unwrap_or_default()
    )]
    TooMany { retire_at: Option<i64> },
}

#[cfg(test)]
mod tests;
