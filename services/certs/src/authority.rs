//! The CA material the service signs with (services/certs/README.md, "CA 階層與私鑰保管"): the
//! offline root's certificate, the issuing CA keys the init container keeps in the Secret
//! (`crates/ca-keys`), and the issuing CA certificates the root signed for them. An issuing CA
//! signs once its key and a certificate for that key are both present and the certificate chains
//! to the root.
//!
//! The private keys stay inside [`IssuingCa`]: nothing here serializes, logs or returns them, and
//! `Debug` shows key ids, generations, subjects and times only. Errors name files and problems,
//! never data.

use std::fmt;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ca_keys::{CaKeys, CaSigningKey, KeysError};
use rcgen::Issuer;
use rustls_pki_types::CertificateDer;
use service_kit::Context;
use sha2::{Digest, Sha256};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::prelude::FromDer;
use zeroize::Zeroizing;

use crate::config::CaFiles;
use crate::pem;

/// Largest certificate file read: a root and a few issuing CA certificates are a few KiB.
const MAX_CERTIFICATE_FILE_BYTES: u64 = 256 * 1024;

/// Seconds since the Unix epoch; a clock before the epoch reads as 0.
#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Why the CA material could not be used. Names files and problems, never data.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{file}: {problem}")]
    Read { file: String, problem: String },
    #[error("the issuing CA keys: {0}")]
    Keys(#[from] KeysError),
    #[error("the root certificate: {0}")]
    Root(String),
    #[error("issuing CA certificate {index}: {problem}")]
    Certificate { index: usize, problem: String },
}

/// One issuing CA: a key from the Secret and the certificate the root signed for it. `Debug`
/// never shows the key.
pub struct IssuingCa {
    key_id: String,
    generation: u64,
    subject: String,
    subject_key_id: Vec<u8>,
    not_before: i64,
    not_after: i64,
    certificate: Vec<u8>,
    issuer: Issuer<'static, CaSigningKey>,
}

impl fmt::Debug for IssuingCa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuingCa")
            .field("key_id", &self.key_id)
            .field("generation", &self.generation)
            .field("subject", &self.subject)
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl IssuingCa {
    /// The id of its key (`crates/ca-keys`).
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The generation of its key.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Its certificate's subject, as text.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Its certificate's notBefore, Unix seconds.
    #[must_use]
    pub fn not_before(&self) -> i64 {
        self.not_before
    }

    /// Its certificate's notAfter, Unix seconds.
    #[must_use]
    pub fn not_after(&self) -> i64 {
        self.not_after
    }

    /// Its certificate, DER.
    #[must_use]
    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate
    }

    /// Whether its certificate is valid at `now`.
    #[must_use]
    pub fn is_valid_at(&self, now: i64) -> bool {
        self.not_before <= now && now < self.not_after
    }

    /// Its certificate's subject key identifier, which its CRLs and the certificates it signs
    /// name as their authority key identifier.
    pub(crate) fn subject_key_id(&self) -> &[u8] {
        &self.subject_key_id
    }

    /// The signer of its certificates and CRLs.
    pub(crate) fn issuer(&self) -> &Issuer<'static, CaSigningKey> {
        &self.issuer
    }
}

/// The root and the issuing CAs that can sign, as loaded at one time.
#[derive(Debug)]
pub struct Authority {
    root: Vec<u8>,
    root_subject: String,
    issuing: Vec<Arc<IssuingCa>>,
    waiting: Vec<(String, u64)>,
    ignored: Vec<String>,
}

impl Authority {
    /// Reads the three files of `files` and builds the authority at `now`.
    ///
    /// # Errors
    ///
    /// When a file cannot be read or its content is not usable ([`Authority::from_parts`]).
    pub fn load(files: &CaFiles, now: i64) -> Result<Self, LoadError> {
        let keys = read_keys(&files.keys_file)?;
        let root = read_text(&files.root_certificate_file)?;
        let certificates = read_text(&files.certificates_file)?;
        Self::from_parts(keys, &root, &certificates, now)
    }

    /// The authority of `keys`, the root certificate `root_pem` and the issuing CA certificates
    /// `certificates_pem` (PEM; may be empty) at `now`.
    ///
    /// The root must be one self-signed CA certificate with keyCertSign that is valid at `now`.
    /// Every issuing CA certificate must be signed by it, name it as issuer, expire no later than
    /// it (so no issuing CA signs once the root has expired), be a CA with
    /// keyCertSign and cRLSign, carry a subject key identifier and a subject other than the
    /// root's and every other issuing CA's (Mosquitto finds a CRL by its issuer's name), and at
    /// most one certificate may name the same key. A certificate for a key the set does not hold
    /// (a retired key) or that has expired is ignored and reported; a key without a certificate
    /// waits for one ([`Authority::waiting`]).
    ///
    /// # Errors
    ///
    /// When the root or a certificate breaks these rules.
    pub fn from_parts(
        keys: CaKeys,
        root_pem: &str,
        certificates_pem: &str,
        now: i64,
    ) -> Result<Self, LoadError> {
        let root_der = one_certificate(root_pem)?;
        let (_, root) =
            X509Certificate::from_der(&root_der).map_err(|_| root_error("not a certificate"))?;
        check_root(&root, now)?;
        let root_subject = root.subject().to_string();
        let blocks = pem::decode_all(certificates_pem, pem::CERTIFICATE).map_err(|problem| {
            LoadError::Certificate {
                index: 0,
                problem: format!("the file is not PEM certificates: {problem}"),
            }
        })?;
        let mut keys: Vec<Option<ca_keys::CaKey>> =
            keys.into_keys().into_iter().map(Some).collect();
        let spkis: Vec<Vec<u8>> = keys
            .iter()
            .flatten()
            .map(ca_keys::CaKey::subject_public_key_info)
            .collect();
        let mut issuing: Vec<Arc<IssuingCa>> = Vec::new();
        let mut ignored = Vec::new();
        let mut subjects = vec![root.subject().as_raw().to_vec()];
        for (number, der) in blocks.into_iter().enumerate() {
            let index = number + 1;
            let invalid = |problem: &str| LoadError::Certificate {
                index,
                problem: problem.to_owned(),
            };
            let (rest, certificate) =
                X509Certificate::from_der(&der).map_err(|_| invalid("not a certificate"))?;
            if !rest.is_empty() {
                return Err(invalid("trailing bytes after the certificate"));
            }
            let subject_key_id = check_issuing(&certificate, &root).map_err(|p| invalid(&p))?;
            let raw_subject = certificate.subject().as_raw().to_vec();
            if subjects.contains(&raw_subject) {
                return Err(invalid(
                    "its subject is the root's or another issuing CA's; every issuing CA needs a \
                     name of its own",
                ));
            }
            subjects.push(raw_subject);
            let subject = certificate.subject().to_string();
            let not_before = certificate.validity().not_before.timestamp();
            let not_after = certificate.validity().not_after.timestamp();
            let spki = certificate.public_key().raw;
            let Some(position) = spkis.iter().position(|known| known.as_slice() == spki) else {
                ignored.push(format!(
                    "{subject}: no issuing CA key in the Secret has its public key (a retired \
                     key)"
                ));
                continue;
            };
            let Some(key) = keys[position].take() else {
                return Err(invalid(
                    "another certificate in the file is for the same key; keep one",
                ));
            };
            if not_after <= now {
                ignored.push(format!("{subject}: expired"));
                continue;
            }
            let (key_id, generation) = (key.id().to_owned(), key.generation());
            let issuer = Issuer::from_ca_cert_der(
                &CertificateDer::from(der.as_slice()),
                key.into_signing_key(),
            )
            .map_err(|_| invalid("rcgen cannot use it as an issuer"))?;
            issuing.push(Arc::new(IssuingCa {
                key_id,
                generation,
                subject,
                subject_key_id,
                not_before,
                not_after,
                certificate: der,
                issuer,
            }));
        }
        issuing.sort_by_key(|ca| ca.generation);
        let waiting = keys
            .into_iter()
            .flatten()
            .map(|key| (key.id().to_owned(), key.generation()))
            .collect();
        Ok(Self {
            root: root_der,
            root_subject,
            issuing,
            waiting,
            ignored,
        })
    }

    /// The issuing CA that signs new certificates at `now`: of those valid at `now`, the one of
    /// the highest generation.
    #[must_use]
    pub fn current(&self, now: i64) -> Option<&Arc<IssuingCa>> {
        self.issuing.iter().rev().find(|ca| ca.is_valid_at(now))
    }

    /// Every issuing CA with a usable certificate, oldest generation first. Each one's CRL is
    /// kept current while its certificate is valid, since the certificates it signed may still
    /// be.
    #[must_use]
    pub fn issuing(&self) -> &[Arc<IssuingCa>] {
        &self.issuing
    }

    /// The keys (id, generation) that have no certificate yet: their CSR waits in the ConfigMap
    /// for the offline root.
    #[must_use]
    pub fn waiting(&self) -> &[(String, u64)] {
        &self.waiting
    }

    /// Certificates in the file that are not used, and why.
    #[must_use]
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    /// The root certificate's subject.
    #[must_use]
    pub fn root_subject(&self) -> &str {
        &self.root_subject
    }

    /// The root certificate, DER.
    #[must_use]
    pub fn root_der(&self) -> &[u8] {
        &self.root
    }

    /// The chain a client needs: the root, then every issuing CA certificate, PEM.
    #[must_use]
    pub fn chain_pem(&self) -> String {
        let mut text = pem::encode(pem::CERTIFICATE, &self.root);
        for ca in &self.issuing {
            text.push_str(&pem::encode(pem::CERTIFICATE, &ca.certificate));
        }
        text
    }
}

fn root_error(problem: &str) -> LoadError {
    LoadError::Root(problem.to_owned())
}

fn one_certificate(text: &str) -> Result<Vec<u8>, LoadError> {
    let mut blocks = pem::decode_all(text, pem::CERTIFICATE).map_err(|problem| {
        root_error(&format!("the file is not one PEM certificate: {problem}"))
    })?;
    if blocks.len() != 1 {
        return Err(root_error("the file must hold exactly one certificate"));
    }
    Ok(blocks.remove(0))
}

fn check_root(root: &X509Certificate<'_>, now: i64) -> Result<(), LoadError> {
    if root.subject().as_raw() != root.issuer().as_raw() || root.verify_signature(None).is_err() {
        return Err(root_error("it is not self-signed"));
    }
    if !is_ca(root) {
        return Err(root_error("it is not a CA (basicConstraints cA)"));
    }
    if !root
        .key_usage()
        .ok()
        .flatten()
        .is_some_and(|usage| usage.value.key_cert_sign())
    {
        return Err(root_error("its key usage lacks keyCertSign"));
    }
    let validity = root.validity();
    if !(validity.not_before.timestamp() <= now && now < validity.not_after.timestamp()) {
        return Err(root_error("it is not valid now"));
    }
    Ok(())
}

fn is_ca(certificate: &X509Certificate<'_>) -> bool {
    certificate
        .basic_constraints()
        .ok()
        .flatten()
        .is_some_and(|constraints| constraints.value.ca)
}

/// Checks an issuing CA certificate against the root; returns its subject key identifier.
fn check_issuing(
    certificate: &X509Certificate<'_>,
    root: &X509Certificate<'_>,
) -> Result<Vec<u8>, String> {
    if certificate.issuer().as_raw() != root.subject().as_raw() {
        return Err("its issuer is not the root".to_owned());
    }
    if certificate
        .verify_signature(Some(root.public_key()))
        .is_err()
    {
        return Err("it is not signed by the root".to_owned());
    }
    // Past the root's notAfter no client can build a chain to the root, so an issuing CA that
    // outlived it would keep signing certificates nobody accepts. `certs-ca root sign` ends every
    // issuing CA at the root's notAfter at the latest.
    if certificate.validity().not_after.timestamp() > root.validity().not_after.timestamp() {
        return Err(
            "it is valid after the root expires; sign it again with `certs-ca root sign`, \
             which ends it at the root's notAfter"
                .to_owned(),
        );
    }
    if !is_ca(certificate) {
        return Err("it is not a CA (basicConstraints cA)".to_owned());
    }
    let usable = certificate
        .key_usage()
        .ok()
        .flatten()
        .is_some_and(|usage| usage.value.key_cert_sign() && usage.value.crl_sign());
    if !usable {
        return Err("its key usage lacks keyCertSign or cRLSign".to_owned());
    }
    certificate
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "it has no subject key identifier".to_owned())
}

fn read_keys(path: &Path) -> Result<CaKeys, LoadError> {
    let limit = u64::try_from(ca_keys::MAX_DOCUMENT_BYTES).unwrap_or(u64::MAX);
    let text = read_limited(path, limit)?;
    Ok(CaKeys::parse(&text)?)
}

fn read_text(path: &Path) -> Result<Zeroizing<String>, LoadError> {
    read_limited(path, MAX_CERTIFICATE_FILE_BYTES)
}

/// Reads at most `limit` bytes of text from `path`, into a buffer that is wiped when dropped
/// and never moves while it grows.
fn read_limited(path: &Path, limit: u64) -> Result<Zeroizing<String>, LoadError> {
    let failed = |problem: String| LoadError::Read {
        file: path.display().to_string(),
        problem,
    };
    let file =
        std::fs::File::open(path).map_err(|error| failed(format!("cannot open it: {error}")))?;
    let mut text = Zeroizing::new(String::with_capacity(
        usize::try_from(limit + 1).unwrap_or(0),
    ));
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|_| failed("it is not a text file".to_owned()))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > limit {
        return Err(failed(format!("it is larger than {limit} bytes")));
    }
    Ok(text)
}

/// The authority as last loaded, shared by the CRL job, the monitor and the HTTP handlers, and
/// reloaded when the files change.
#[derive(Clone, Debug, Default)]
pub struct CaState {
    inner: Arc<RwLock<Loaded>>,
}

#[derive(Debug, Default)]
struct Loaded {
    authority: Option<Arc<Authority>>,
    problem: Option<String>,
    fingerprint: Option<[u8; 32]>,
}

impl CaState {
    /// Loads `files` now and every `files.reload_interval` until shutdown, keeping the last
    /// usable authority while the files are broken.
    ///
    /// Whether an issuing CA can sign is not a readiness condition: a fresh install, a key
    /// rotation and a lost key set all wait for the offline ceremony, which takes longer than
    /// any rollout may (a Deployment that never becomes ready fails the Helm release and Flux
    /// rolls it back, taking the waiting request with it). The monitor reports it instead
    /// ([`CaState::signing`], the gauge `certs.ca.signing` and [`CaState::log_signing`]).
    pub fn watch<T>(context: &Context<T>, files: CaFiles) -> Self {
        let state = Self::default();
        state.reload(&files, unix_now());
        let reloading = state.clone();
        let shutdown = context.shutdown().clone();
        context.spawn(async move {
            let mut ticks = tokio::time::interval(files.reload_interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks.tick().await;
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.stopping() => break,
                    _ = ticks.tick() => {
                        reloading.reload(&files, unix_now());
                    }
                }
            }
        });
        state
    }

    /// The authority, if one was loaded.
    #[must_use]
    pub fn authority(&self) -> Option<Arc<Authority>> {
        self.read().authority.clone()
    }

    /// `Ok` when an issuing CA can sign at `now`; otherwise why not.
    ///
    /// # Errors
    ///
    /// When no authority is loaded or none of its issuing CAs is valid at `now`.
    pub fn signing(&self, now: i64) -> Result<(), String> {
        let loaded = self.read();
        match &loaded.authority {
            Some(authority) if authority.current(now).is_some() => Ok(()),
            Some(authority) => Err(format!(
                "no issuing CA certificate is valid now; keys waiting for one: {}",
                describe_waiting(authority)
            )),
            None => Err(loaded
                .problem
                .clone()
                .unwrap_or_else(|| "the CA files are not loaded yet".to_owned())),
        }
    }

    /// Logs at `now` whether the service can sign and which keys wait for a certificate, as
    /// every load of the files does; while no authority is loaded, a WARN with the reason.
    /// The monitor calls it at every check, so the waiting key id the ceremony needs is in the
    /// recent log however long the ceremony takes.
    pub fn log_signing(&self, now: i64) {
        let loaded = self.read();
        match &loaded.authority {
            Some(authority) => log_waiting(authority, now),
            None => tracing::warn!(
                problem = loaded
                    .problem
                    .as_deref()
                    .unwrap_or("the CA files are not loaded yet"),
                "no issuing CA is loaded, so the service cannot sign gateway certificates"
            ),
        }
    }

    /// Reads the files again and, when they changed, builds a new authority. Returns whether
    /// the authority changed.
    pub fn reload(&self, files: &CaFiles, now: i64) -> bool {
        let fingerprint = fingerprint(files);
        if fingerprint.is_some() && fingerprint == self.read().fingerprint {
            return false;
        }
        let loaded = Authority::load(files, now);
        let mut state = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        state.fingerprint = fingerprint;
        match loaded {
            Ok(authority) => {
                log_loaded(&authority, now);
                state.authority = Some(Arc::new(authority));
                state.problem = None;
                true
            }
            Err(error) => {
                let problem = format!("the CA files are not usable: {error}");
                // Once per problem: a file that cannot be read has no fingerprint, so it is
                // loaded again at every tick (for example until the kubelet adds the keys file
                // the init container just wrote).
                if state.problem.as_deref() != Some(problem.as_str()) {
                    tracing::error!(
                        problem = %problem,
                        kept_previous = state.authority.is_some(),
                        "cannot load the issuing CA"
                    );
                }
                state.problem = Some(problem);
                false
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Loaded> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// A state holding `authority`, for tests and tools.
    #[must_use]
    pub fn of(authority: Authority) -> Self {
        let state = Self::default();
        state
            .inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .authority = Some(Arc::new(authority));
        state
    }
}

/// The SHA-256 over the three files' contents, or `None` when one cannot be read (the load then
/// reports why).
fn fingerprint(files: &CaFiles) -> Option<[u8; 32]> {
    let mut digest = Sha256::new();
    for path in [
        &files.keys_file,
        &files.root_certificate_file,
        &files.certificates_file,
    ] {
        let bytes = std::fs::read(path).ok().map(Zeroizing::new)?;
        digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(bytes.as_slice());
    }
    Some(digest.finalize().into())
}

fn describe_waiting(authority: &Authority) -> String {
    if authority.waiting().is_empty() {
        return "none".to_owned();
    }
    authority
        .waiting()
        .iter()
        .map(|(id, generation)| {
            format!(
                "{id} (generation {generation}, request {})",
                ca_keys::request_file(*generation)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Logs what was loaded: ids, generations, subjects and times only.
fn log_loaded(authority: &Authority, now: i64) {
    let issuing: Vec<String> = authority
        .issuing()
        .iter()
        .map(|ca| {
            format!(
                "{} (generation {}, key {}, valid until {})",
                ca.subject(),
                ca.generation(),
                ca.key_id(),
                humantime_seconds(ca.not_after())
            )
        })
        .collect();
    let current = authority.current(now).map(|ca| ca.key_id().to_owned());
    tracing::info!(
        root = %authority.root_subject(),
        issuing = ?issuing,
        current = current.as_deref().unwrap_or("none"),
        waiting = %describe_waiting(authority),
        ignored = ?authority.ignored(),
        "loaded the issuing CA"
    );
    log_waiting(authority, now);
}

/// Logs the keys that wait for an issuing CA certificate (key id, generation, request file), at
/// WARN while no issuing CA can sign and at INFO while one can (a rotation); a WARN without
/// waiting keys when none can sign. Both waiting lines contain `keys wait for a certificate`,
/// which the runbook searches for (services/certs/README.md, 「發簽發 CA 憑證」).
fn log_waiting(authority: &Authority, now: i64) {
    let signing = authority.current(now).is_some();
    if authority.waiting().is_empty() {
        if !signing {
            tracing::warn!(
                "no issuing CA certificate is valid now and no key waits for one, so the service \
                 cannot sign gateway certificates (services/certs/README.md, 「輪替」)"
            );
        }
        return;
    }
    let waiting = describe_waiting(authority);
    if signing {
        tracing::info!(
            waiting = %waiting,
            "issuing CA keys wait for a certificate from the offline root; the current issuing \
             CA keeps signing (services/certs/README.md, 「輪替」)"
        );
    } else {
        tracing::warn!(
            waiting = %waiting,
            "no issuing CA certificate is valid now, so the service cannot sign gateway \
             certificates; issuing CA keys wait for a certificate from the offline root \
             (services/certs/README.md, 「發簽發 CA 憑證」)"
        );
    }
}

/// `seconds` since the epoch as RFC 3339 text.
fn humantime_seconds(seconds: i64) -> String {
    let at = UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
    humantime::format_rfc3339_seconds(at).to_string()
}
