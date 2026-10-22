//! The certs service's issuing CA keys (Laysi/trustiot-v2#78; services/certs/README.md, "CA
//! 階層與私鑰保管"): the key set the cluster generates and keeps in one Secret, and the
//! certificate signing request (CSR) of each key that an operator signs with the offline root.
//! Formats and rules only, no Kubernetes and no I/O; two users:
//!
//! - the certs pod's init container (`tools/migrate`, `migrate certs-ca-keys`): [`CaKeys::plan`]
//!   decides which keys to add or remove, and the run writes the set into the Secret and every
//!   key's CSR ([`CaKeys::requests`]) into a ConfigMap;
//! - the certs service: [`CaKeys::parse`] reads the mounted Secret, and each key signs once an
//!   issuing CA certificate for it is configured ([`CaKey::into_signing_key`]).
//!
//! `Debug`, errors and the changes a plan reports name key ids, generations and times, never a
//! private key. The document text, the private keys this crate decodes and each key pair's
//! PKCS#8 copy are wiped from memory when dropped; ring's own key object is not.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
    SignatureAlgorithm, SigningKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x509_parser::prelude::FromDer;
use zeroize::{Zeroize, Zeroizing};

#[cfg(test)]
mod tests;

/// The Secret key and the file the certs service mounts: the key set's document.
pub const KEYS_FILE: &str = "issuing-ca-keys.json";

/// Most keys in a set: the current one, the next one waiting for its certificate, and the older
/// ones whose gateway certificates are still valid.
pub const MAX_KEYS: usize = 4;

/// Highest generation: nine decimal digits, as the chart's values take them.
pub const MAX_GENERATION: u64 = 999_999_999;

/// Largest document read; a set of [`MAX_KEYS`] keys is about 4 KiB.
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// The ConfigMap key of the CSR of the key of `generation`.
#[must_use]
pub fn request_file(generation: u64) -> String {
    format!("issuing-ca-generation-{generation}.csr.pem")
}

/// The key id of a public key: the lower-case hex SHA-256 of its DER SubjectPublicKeyInfo.
#[must_use]
pub fn key_id(subject_public_key_info: &[u8]) -> String {
    hex(&Sha256::digest(subject_public_key_info))
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Why a document, a key or a plan was refused. Names positions, ids and generations, never a
/// private key.
#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    #[error("the document is larger than {MAX_DOCUMENT_BYTES} bytes")]
    TooLarge,
    #[error("the document is not a key set (a JSON object with keys)")]
    NotADocument,
    #[error("the set would hold more than {MAX_KEYS} keys; remove old generations first")]
    TooMany,
    #[error("key {index}: {problem}")]
    Key { index: usize, problem: KeyProblem },
    #[error("generation {0} is outside 1 to {MAX_GENERATION}")]
    Generation(u64),
    #[error(
        "the requested generation {generation} is below the generation {remove_before} whose \
         older keys are removed, so its key would be removed at once"
    )]
    RemovesRequested { generation: u64, remove_before: u64 },
    #[error("cannot generate a key or its request: {0}")]
    Generate(String),
}

/// What is wrong with one stored key.
#[derive(Debug, thiserror::Error)]
pub enum KeyProblem {
    #[error("the generation is outside 1 to {MAX_GENERATION}")]
    Generation,
    #[error("the creation time is negative")]
    CreatedAt,
    #[error("the private key is not base64")]
    NotBase64,
    #[error("the private key is not an ECDSA P-256 PKCS#8 key")]
    NotP256,
    #[error("the id is not the SHA-256 of the key's public key")]
    WrongId,
    #[error("the request is not one PEM certificate signing request")]
    NotARequest,
    #[error("the request is not signed by this key or names another public key")]
    RequestMismatch,
    #[error("another key has the same id")]
    DuplicateId,
    #[error("another key has the same generation")]
    DuplicateGeneration,
}

/// An issuing CA's key pair, to sign with. Its PKCS#8 copy is wiped from memory when dropped
/// (ring's own key object is not). It never serializes and `Debug` shows nothing of it.
pub struct CaSigningKey(KeyPair);

impl Drop for CaSigningKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for CaSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaSigningKey(<redacted>)")
    }
}

impl PublicKeyData for CaSigningKey {
    fn der_bytes(&self) -> &[u8] {
        self.0.der_bytes()
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        PublicKeyData::algorithm(&self.0)
    }
}

impl SigningKey for CaSigningKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.0.sign(message)
    }
}

/// One issuing CA key: its id, generation, creation time, key pair and CSR. `Debug` shows the
/// id, generation and time only.
pub struct CaKey {
    id: String,
    generation: u64,
    created_at: i64,
    key: CaSigningKey,
    request: String,
}

impl fmt::Debug for CaKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaKey")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

impl CaKey {
    /// A new ECDSA P-256 key from the operating system's CSPRNG for `generation`, created at
    /// `now` (Unix seconds), with its CSR.
    ///
    /// The CSR's subject only tells the operator which key it is (`TrustIoT issuing CA key,
    /// generation <n>`); the offline root decides the issuing CA certificate's subject and
    /// extensions when it signs (services/certs, `certs-ca root sign`).
    ///
    /// # Errors
    ///
    /// When the generation is out of range or there is no randomness.
    pub fn generate(generation: u64, now: i64) -> Result<Self, KeysError> {
        if !(1..=MAX_GENERATION).contains(&generation) {
            return Err(KeysError::Generation(generation));
        }
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map(CaSigningKey)
            .map_err(|error| KeysError::Generate(error.to_string()))?;
        let mut params = CertificateParams::default();
        let mut subject = DistinguishedName::new();
        subject.push(
            DnType::CommonName,
            format!("TrustIoT issuing CA key, generation {generation}"),
        );
        params.distinguished_name = subject;
        let request = params
            .serialize_request(&key)
            .and_then(|request| request.pem())
            .map_err(|error| KeysError::Generate(error.to_string()))?;
        Ok(Self {
            id: key_id(&key.subject_public_key_info()),
            generation,
            created_at: now.max(0),
            key,
            request,
        })
    }

    /// The key id ([`key_id`] of the public key).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The generation it was made for (the chart's `certificateAuthority.keyGeneration`).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// When it was made, in Unix seconds.
    #[must_use]
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    /// Its DER SubjectPublicKeyInfo, which an issuing CA certificate for it carries.
    #[must_use]
    pub fn subject_public_key_info(&self) -> Vec<u8> {
        self.key.subject_public_key_info()
    }

    /// Its CSR, PEM.
    #[must_use]
    pub fn request_pem(&self) -> &str {
        &self.request
    }

    /// The key pair, to sign with. The certs service turns it into the issuer of its issuing CA
    /// certificate.
    #[must_use]
    pub fn into_signing_key(self) -> CaSigningKey {
        self.key
    }
}

/// The key set of [`KEYS_FILE`], oldest generation first. `Debug` shows ids, generations and
/// times only.
#[derive(Debug, Default)]
pub struct CaKeys {
    keys: Vec<CaKey>,
}

/// One key as the document stores it. No `Debug`: `private_key` is the private key.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    id: String,
    generation: u64,
    created_at: i64,
    private_key: StoredPrivateKey,
    request: String,
}

/// A PKCS#8 private key as the document stores it (base64). Wiped from memory when dropped, on
/// every path. No `Debug`.
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

/// What a plan asks for: the chart's `certificateAuthority.keyGeneration` and
/// `certificateAuthority.removeKeysBeforeGeneration`.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// A key of at least this generation must exist; when none does, one is added.
    pub generation: u64,
    /// Keys of a lower generation are removed (0 removes none).
    pub remove_before: u64,
}

/// The set a plan decided on and what changed, one line per change (ids and generations only).
#[derive(Debug)]
pub struct Plan {
    pub keys: CaKeys,
    pub changes: Vec<String>,
}

impl CaKeys {
    /// The keys, oldest generation first.
    #[must_use]
    pub fn keys(&self) -> &[CaKey] {
        &self.keys
    }

    /// The keys, to take them over.
    #[must_use]
    pub fn into_keys(self) -> Vec<CaKey> {
        self.keys
    }

    /// Whether the set has no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key ids, oldest generation first.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(CaKey::id)
    }

    /// Reads the document of [`KEYS_FILE`]: `{"keys":[{"id","generation","created_at",
    /// "private_key","request"}, ...]}`. Every key needs a generation from 1 to
    /// [`MAX_GENERATION`] that no other key has, a creation time of at least 0, an ECDSA P-256
    /// PKCS#8 private key in base64, the id [`key_id`] of its public key, and a PEM CSR signed by
    /// it. At most [`MAX_KEYS`] keys; an empty list is an empty set.
    ///
    /// # Errors
    ///
    /// When the document is not such a set; the error names the position of the key and the
    /// problem, never a value.
    pub fn parse(text: &str) -> Result<Self, KeysError> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err(KeysError::TooLarge);
        }
        let document: Document = serde_json::from_str(text).map_err(|_| KeysError::NotADocument)?;
        if document.keys.len() > MAX_KEYS {
            return Err(KeysError::TooMany);
        }
        let mut keys: Vec<CaKey> = Vec::with_capacity(document.keys.len());
        for (index, stored) in document.keys.into_iter().enumerate() {
            let key = from_stored(stored).map_err(|problem| KeysError::Key { index, problem })?;
            let duplicate = if keys.iter().any(|existing| existing.id == key.id) {
                Some(KeyProblem::DuplicateId)
            } else if keys
                .iter()
                .any(|existing| existing.generation == key.generation)
            {
                Some(KeyProblem::DuplicateGeneration)
            } else {
                None
            };
            if let Some(problem) = duplicate {
                return Err(KeysError::Key { index, problem });
            }
            keys.push(key);
        }
        keys.sort_by_key(CaKey::generation);
        Ok(Self { keys })
    }

    /// The document of [`KEYS_FILE`]: one line of JSON, the keys oldest generation first, the
    /// same text for the same set. Wiped from memory when dropped.
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
                    id: key.id.clone(),
                    generation: key.generation,
                    created_at: key.created_at,
                    private_key: StoredPrivateKey(STANDARD.encode(key.key.0.serialized_der())),
                    request: key.request.clone(),
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

    /// Every key's CSR by its ConfigMap key ([`request_file`]), the same map for the same set.
    #[must_use]
    pub fn requests(&self) -> BTreeMap<String, String> {
        self.keys
            .iter()
            .map(|key| (request_file(key.generation), key.request.clone()))
            .collect()
    }

    /// What the set should become for `request` at `now` (Unix seconds), with `generate` making
    /// a new key ([`CaKey::generate`] outside tests):
    ///
    /// - keys of a generation below `request.remove_before` are removed;
    /// - when no remaining key has at least `request.generation`, a key of that generation is
    ///   added. A lower request adds nothing: generations only go up, so a rollback of the
    ///   chart's values does not make another key.
    ///
    /// # Errors
    ///
    /// When the request would remove the key it asks for, the set would hold more than
    /// [`MAX_KEYS`] keys, or `generate` fails.
    pub fn plan(
        self,
        request: Request,
        now: i64,
        generate: impl FnOnce(u64, i64) -> Result<CaKey, KeysError>,
    ) -> Result<Plan, KeysError> {
        if !(1..=MAX_GENERATION).contains(&request.generation) {
            return Err(KeysError::Generation(request.generation));
        }
        if request.generation < request.remove_before {
            return Err(KeysError::RemovesRequested {
                generation: request.generation,
                remove_before: request.remove_before,
            });
        }
        let mut changes = Vec::new();
        let mut keys = Vec::with_capacity(self.keys.len() + 1);
        for key in self.keys {
            if key.generation < request.remove_before {
                changes.push(format!(
                    "removed issuing CA key {} of generation {} (below {})",
                    key.id, key.generation, request.remove_before
                ));
            } else {
                keys.push(key);
            }
        }
        if keys.iter().all(|key| key.generation < request.generation) {
            if keys.len() >= MAX_KEYS {
                return Err(KeysError::TooMany);
            }
            let key = generate(request.generation, now)?;
            changes.push(format!(
                "added issuing CA key {} of generation {}; its certificate signing request is {}",
                key.id,
                key.generation,
                request_file(key.generation)
            ));
            keys.push(key);
        }
        keys.sort_by_key(CaKey::generation);
        Ok(Plan {
            keys: Self { keys },
            changes,
        })
    }
}

fn from_stored(stored: Stored) -> Result<CaKey, KeyProblem> {
    if !(1..=MAX_GENERATION).contains(&stored.generation) {
        return Err(KeyProblem::Generation);
    }
    if stored.created_at < 0 {
        return Err(KeyProblem::CreatedAt);
    }
    let der = STANDARD
        .decode(stored.private_key.0.as_bytes())
        .map(Zeroizing::new)
        .map_err(|_| KeyProblem::NotBase64)?;
    let key = KeyPair::try_from(der.as_slice())
        .map(CaSigningKey)
        .map_err(|_| KeyProblem::NotP256)?;
    if PublicKeyData::algorithm(&key) != &PKCS_ECDSA_P256_SHA256 {
        return Err(KeyProblem::NotP256);
    }
    let spki = key.subject_public_key_info();
    if key_id(&spki) != stored.id {
        return Err(KeyProblem::WrongId);
    }
    check_request(&stored.request, &spki)?;
    Ok(CaKey {
        id: stored.id,
        generation: stored.generation,
        created_at: stored.created_at,
        key,
        request: stored.request,
    })
}

/// Whether `pem` is exactly one CSR for the public key `spki`, signed by its key.
fn check_request(pem: &str, spki: &[u8]) -> Result<(), KeyProblem> {
    let (rest, block) =
        x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|_| KeyProblem::NotARequest)?;
    if block.label != "CERTIFICATE REQUEST" || !rest.iter().all(u8::is_ascii_whitespace) {
        return Err(KeyProblem::NotARequest);
    }
    let (rest, request) =
        x509_parser::certification_request::X509CertificationRequest::from_der(&block.contents)
            .map_err(|_| KeyProblem::NotARequest)?;
    if !rest.is_empty() {
        return Err(KeyProblem::NotARequest);
    }
    if request.certification_request_info.subject_pki.raw != spki
        || request.verify_signature().is_err()
    {
        return Err(KeyProblem::RequestMismatch);
    }
    Ok(())
}
