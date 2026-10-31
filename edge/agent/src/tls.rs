//! The two TLS layers of the gateway's connection (decision 6).
//!
//! - **Outer TLS**, to Cloudflare (SaaS) or the customer's ingress (on-prem): it only crosses
//!   the network. Its trust anchors come only from the configured PEM files: the OS bundle for
//!   SaaS, the customer's or our CA on-prem, plus a factory's TLS-interception CA if one is
//!   added. There are no built-in roots (no webpki-roots, no native root store).
//! - **Inner mTLS**, inside the WebSocket, straight to the broker: the client authenticates with
//!   the gateway certificate and its TPM key, and checks the broker's certificate as decided in
//!   Laysi/trustiot-v2#556 (services/certs/README.md, 「閘道器對 broker 伺服器憑證的信任」):
//!   - the trust anchors are our root CAs only ([`BrokerTrust`]), each with its own subject:
//!     never the system's or WebPKI's roots, the outer layer's list, or an issuing CA;
//!   - the broker sends its certificate and the issuing CA that signed it, and the chain is
//!     built from those to a root;
//!   - SNI and the name check use the one fixed name [`BROKER_NAME`], and the name check cannot
//!     be turned off;
//!   - the certificate needs the extended key usage serverAuth (webpki's own check);
//!   - with a root CRL ([`RootCrl`]) every certificate in the chain below the root is checked
//!     against it: a revoked issuing CA is refused; a certificate whose issuer has no CRL here
//!     (the broker's own, under the issuing CA) passes; an expired root CRL still counts; a root
//!     CRL with an issuing distribution point is refused, since webpki would apply it only to
//!     the certificates in its scope;
//!   - the roots' and the root CRL's signatures are checked at load with the handshake's own
//!     signature algorithms, so nothing loads that every handshake would refuse.
//!
//! Both are TLS 1.3 only with ChaCha20-Poly1305 first: the gateway's Cortex-A72 has no AES
//! instructions, ChaCha20 took 21–34% less CPU there, and rustls would otherwise offer
//! AES-256-GCM first (Laysi/trustiot-v2#75). Cloudflare and Envoy follow the client's order;
//! Mosquitto follows its own, so the broker's gateway listener must set `ciphers_tls1.3` with
//! ChaCha20 first (decision 6; the chart sets it since Laysi/trustiot-v2#564). Session
//! resumption stays on in both, with a small in-memory cache: a resumed inner handshake needs
//! no TPM signature (about 70 ms less per reconnect), and revocation still works because SIGHUP
//! on the broker invalidates every ticket. The provider is ring; aws-lc-rs would add a CMake C
//! build and about 120 ms of start-up.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::UnixMillis;
use rustls::client::{ClientConfig, ResolvesClientCert, Resumption, WebPkiServerVerifier};
use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::{self, cipher_suite};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{
    CertificateDer, CertificateRevocationListDer, InvalidDnsNameError, ServerName, UnixTime,
};
use x509_parser::prelude::{FromDer, X509Certificate, X509Name};

use crate::identity::Identity;

/// The broker's name in its certificate, for the inner TLS's SNI and name check alike: one
/// fixed name for SaaS and on-prem, independent of `cloud.url` (Laysi/trustiot-v2#557;
/// services/certs/README.md, 「閘道器對 broker 伺服器憑證的信任」). Nothing resolves it; the
/// WebSocket carries the inner TLS to the broker.
pub const BROKER_NAME: &str = "mqtt-broker.trustiot.internal";

/// The server name the inner TLS sends and checks: always [`BROKER_NAME`].
///
/// # Errors
///
/// None in practice: a unit test checks that [`BROKER_NAME`] is a DNS name.
pub fn broker_name() -> Result<ServerName<'static>, InvalidDnsNameError> {
    ServerName::try_from(BROKER_NAME)
}

/// The largest root CRL file the agent reads ([`root_crl::MAX_BYTES`]). A root CRL lists
/// revoked issuing CAs, a few hundred bytes each.
pub const MAX_ROOT_CRL_BYTES: u64 = root_crl::MAX_BYTES;

/// The most characters of a certificate's or CRL's name an error message shows.
#[cfg(test)]
const MAX_NAME_CHARS: usize = root_crl::MAX_NAME_CHARS;

/// TLS sessions remembered for resumption, per layer. The agent talks to one server per layer.
pub const SESSION_CACHE: usize = 8;
/// The outer layer offers only HTTP/1.1, the protocol of the WebSocket upgrade.
pub const OUTER_ALPN: &[u8] = b"http/1.1";

/// The TLS 1.3 cipher suites in the order the gateway offers them.
pub fn cipher_suites() -> Vec<rustls::SupportedCipherSuite> {
    vec![
        cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
    ]
}

/// ring with [`cipher_suites`], and the signature algorithms the root CRL checks use
/// ([`root_crl::signature_algorithms`], ring's own), so the checks at load and the handshakes
/// accept the same signatures.
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: cipher_suites(),
        signature_verification_algorithms: root_crl::signature_algorithms(),
        ..ring::default_provider()
    }
}

/// Why a TLS layer cannot be configured.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A trust file cannot be read.
    #[error("reading the CA file {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: rustls_pki_types::pem::Error,
    },
    /// A trust file holds no certificate that can be a trust anchor.
    #[error("the CA file {0} holds no usable CA certificate")]
    NoUsableCertificate(PathBuf),
    /// A certificate of our CA file cannot be a trust anchor.
    #[error("a certificate in the CA file {path} cannot be a trust anchor: {source}")]
    BadCertificate {
        /// The file.
        path: PathBuf,
        /// Why.
        source: rustls::Error,
    },
    /// No trust file was given.
    #[error("no CA file given")]
    NoTrust,
    /// A certificate in our CA file is not a root CA certificate.
    #[error(
        "the CA file {path} may hold only root CA certificates (self-issued, self-signed, cA set), but certificate {index} {reason}"
    )]
    NotARoot {
        /// The file.
        path: PathBuf,
        /// Which certificate, from 1.
        index: usize,
        /// What it is instead.
        reason: String,
    },
    /// Two root certificates in our CA file have the same subject. A root CRL names its issuer
    /// by subject only, so webpki would check one root's CRL with the other root's key.
    #[error(
        "the CA file {path}: certificates {first} and {index} have the same subject {subject}; a new root needs a new subject"
    )]
    DuplicateRoot {
        /// The file.
        path: PathBuf,
        /// The earlier certificate, from 1.
        first: usize,
        /// The later certificate, from 1.
        index: usize,
        /// The subject, without control characters.
        subject: String,
    },
    /// The root CRL file cannot be read.
    #[error("reading the root CRL {path}: {source}")]
    CrlRead {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The root CRL file cannot be used: not one PEM `X509 CRL`, too large, not signed by a
    /// trust anchor of the CA file, or refused by webpki.
    #[error("the root CRL {path} cannot be used: {reason}")]
    BadCrl {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The broker would refuse the gateway certificate: it does not chain to our CA, or does
    /// not allow client authentication.
    #[error("the gateway certificate does not pass as a client certificate of {path}: {source}")]
    ClientCertificate {
        /// The CA file.
        path: PathBuf,
        /// Why.
        source: rustls::Error,
    },
    /// The client certificate check could not be set up.
    #[error("certificate verifier: {0}")]
    Verifier(String),
    /// rustls refused the settings.
    #[error("TLS settings: {0}")]
    Rustls(#[from] rustls::Error),
}

fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let read_error = |source| TlsError::Read {
        path: path.to_owned(),
        source,
    };
    CertificateDer::pem_file_iter(path)
        .map_err(read_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(read_error)
}

/// The outer TLS client: trusts the certificates in `trust` (each file must hold at least one
/// usable CA certificate; certificates an OS bundle carries that webpki cannot use are
/// skipped), offers ALPN `http/1.1`.
///
/// # Errors
///
/// An empty list, or a file that cannot be read or holds no usable certificate.
pub fn outer_config(trust: &[PathBuf]) -> Result<Arc<ClientConfig>, TlsError> {
    if trust.is_empty() {
        return Err(TlsError::NoTrust);
    }
    let mut roots = RootCertStore::empty();
    for path in trust {
        let (added, _ignored) = roots.add_parsable_certificates(read_certificates(path)?);
        if added == 0 {
            return Err(TlsError::NoUsableCertificate(path.clone()));
        }
    }
    let mut config = ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![OUTER_ALPN.to_vec()];
    config.resumption = Resumption::in_memory_sessions(SESSION_CACHE);
    Ok(Arc::new(config))
}

/// The trust anchors of the inner TLS: the root CA certificates of our CA file (`broker.ca`).
/// The file may hold several roots (for a future root rollover) and nothing else: an issuing
/// CA or a leaf there is a configuration error, because an issuing CA as a trust anchor would
/// keep being trusted after the root CRL revokes it, and a changed issuing CA would need every
/// gateway's configuration changed (services/certs/README.md). Each root needs its own
/// subject: a root CRL names its issuer only by subject.
#[derive(Clone, Debug)]
pub struct BrokerTrust {
    path: PathBuf,
    anchors: Vec<CertificateDer<'static>>,
    roots: Arc<RootCertStore>,
}

impl BrokerTrust {
    /// Reads our CA file and checks that every certificate in it is a root CA certificate:
    /// self-issued (subject equal to issuer), self-signed (its signature verifies with its own
    /// key, with an algorithm the handshake accepts) and a CA (basicConstraints with cA set),
    /// and that no two have the same subject.
    ///
    /// # Errors
    ///
    /// A file that cannot be read, holds no certificate, holds a certificate that is not a
    /// root, or holds two roots with the same subject.
    pub fn load(ca: &Path) -> Result<Self, TlsError> {
        Self::from_certificates(ca, read_certificates(ca)?)
    }

    /// The trust anchors `anchors`, checked as [`BrokerTrust::load`] checks those of a file; `ca`
    /// is the file they are for, only named in errors. `trustiot-agent provision install` checks
    /// a root this way before it writes the root to `broker.ca`.
    ///
    /// # Errors
    ///
    /// As [`BrokerTrust::load`], except that nothing is read.
    pub fn from_certificates(
        ca: &Path,
        anchors: Vec<CertificateDer<'static>>,
    ) -> Result<Self, TlsError> {
        if anchors.is_empty() {
            return Err(TlsError::NoUsableCertificate(ca.to_owned()));
        }
        let mut roots = RootCertStore::empty();
        let mut subjects: Vec<Vec<u8>> = Vec::new();
        for (index, anchor) in anchors.iter().enumerate() {
            let (subject, shown) = check_root(anchor).map_err(|reason| TlsError::NotARoot {
                path: ca.to_owned(),
                index: index + 1,
                reason,
            })?;
            if let Some(first) = subjects.iter().position(|seen| *seen == subject) {
                return Err(TlsError::DuplicateRoot {
                    path: ca.to_owned(),
                    first: first + 1,
                    index: index + 1,
                    subject: shown,
                });
            }
            subjects.push(subject);
            roots
                .add(anchor.clone())
                .map_err(|source| TlsError::BadCertificate {
                    path: ca.to_owned(),
                    source,
                })?;
        }
        Ok(Self {
            path: ca.to_owned(),
            anchors,
            roots: Arc::new(roots),
        })
    }

    /// The CA file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `der` is one of the root CA certificates, byte for byte.
    pub fn is_anchor(&self, der: &CertificateDer<'_>) -> bool {
        self.anchors
            .iter()
            .any(|anchor| anchor.as_ref() == der.as_ref())
    }
}

/// The subject of `der` (raw, and shown without control characters) if it is a root CA
/// certificate, or why it is not one.
fn check_root(der: &CertificateDer<'_>) -> Result<(Vec<u8>, String), String> {
    let (rest, certificate) =
        X509Certificate::from_der(der).map_err(|error| format!("cannot be parsed: {error}"))?;
    if !rest.is_empty() {
        return Err("has bytes after the certificate".to_owned());
    }
    if certificate.subject().as_raw() != certificate.issuer().as_raw() {
        return Err(format!(
            "is issued by {}, not by itself (an issuing CA or a leaf, not a root)",
            shown(certificate.issuer())
        ));
    }
    match certificate.basic_constraints() {
        Ok(Some(constraints)) if constraints.value.ca => {}
        Ok(_) => return Err("is not a CA certificate (basicConstraints cA is not set)".to_owned()),
        Err(error) => return Err(format!("has unreadable basicConstraints: {error}")),
    }
    // A root whose key or algorithm the handshake cannot use would fail every connection.
    verify_as_the_handshake_does(der, certificate.public_key().raw)
        .map_err(|reason| format!("is not signed by its own key: {reason}"))?;
    Ok((
        certificate.subject().as_raw().to_vec(),
        shown(certificate.subject()),
    ))
}

/// `name` for an error message, without control characters and cut short
/// ([`root_crl::shown`]).
fn shown(name: &X509Name<'_>) -> String {
    root_crl::shown(name)
}

/// Checks that the key in `spki` (a SubjectPublicKeyInfo) signed `signed` (a certificate or a
/// CRL) the way the handshake's webpki checks it: only with the provider's signature algorithms
/// ([`provider`], [`root_crl::verify_signature`]), matched by their exact AlgorithmIdentifier
/// bytes. So nothing loads here that every handshake would then refuse, and the two lists cannot
/// drift apart.
pub(crate) fn verify_as_the_handshake_does(signed: &[u8], spki: &[u8]) -> Result<(), String> {
    root_crl::verify_signature(signed, spki)
}

/// The root CRL the inner TLS checks the broker's chain against (`broker.crl`), as loaded at
/// start. The newer-number replacement, its periodic fetch, clearing the resumption cache on a
/// replacement and reporting the CRL in the heartbeat are Laysi/trustiot-v2#563; until then a
/// new root CRL takes a restart, which also starts a new resumption cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootCrl {
    /// `broker.crl` is not set: revocation is not checked.
    NotConfigured,
    /// `broker.crl` names a file that does not exist: no root CRL received yet, revocation is
    /// not checked.
    Missing(PathBuf),
    /// A root CRL signed by one of the trust anchors.
    Loaded {
        /// The file.
        path: PathBuf,
        /// The CRL in DER.
        der: CertificateRevocationListDer<'static>,
        /// Its CRL number, in decimal, if it has one.
        number: Option<String>,
        /// Its nextUpdate, if it has one.
        next_update: Option<UnixMillis>,
        /// Whether nextUpdate has passed by this clock. An expired root CRL still counts: the
        /// root CA signs it offline, and a late ceremony must not cut off every gateway.
        expired: bool,
    },
}

impl RootCrl {
    /// Loads the root CRL at `path` (`None`: not configured). The file is one PEM block
    /// `X509 CRL` of at most [`MAX_ROOT_CRL_BYTES`] without an issuing distribution point; it
    /// is accepted only when one of the trust anchors in `trust` signed it, checked here rather
    /// than left to the first handshake, so a corrupted or forged file stops the agent from
    /// starting instead of being ignored. A file that does not exist means no root CRL yet.
    ///
    /// # Errors
    ///
    /// The file exists but cannot be read, or cannot be used ([`TlsError::BadCrl`]).
    pub fn load(path: Option<&Path>, trust: &BrokerTrust, now: UnixTime) -> Result<Self, TlsError> {
        let Some(path) = path else {
            return Ok(Self::NotConfigured);
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::Missing(path.to_owned()));
            }
            Err(source) => {
                return Err(TlsError::CrlRead {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let mut bytes = Vec::new();
        file.take(MAX_ROOT_CRL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| TlsError::CrlRead {
                path: path.to_owned(),
                source,
            })?;
        Self::from_bytes(path, &bytes, trust, now)
    }

    /// [`RootCrl::load`] on `bytes`, read by the caller from `path` (which names it in errors).
    ///
    /// # Errors
    ///
    /// The bytes cannot be used ([`TlsError::BadCrl`]).
    pub fn from_bytes(
        path: &Path,
        bytes: &[u8],
        trust: &BrokerTrust,
        now: UnixTime,
    ) -> Result<Self, TlsError> {
        let checked = root_crl::check(
            bytes,
            &root_crl::Anchors {
                certificates: &trust.anchors,
                store: &trust.roots,
                source: &trust.path,
            },
        )
        .map_err(|reason| TlsError::BadCrl {
            path: path.to_owned(),
            reason,
        })?;
        let now_s = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
        let expired = checked.next_update.is_some_and(|next| next < now_s);
        Ok(Self::Loaded {
            path: path.to_owned(),
            der: checked.der,
            number: checked.number,
            next_update: checked
                .next_update
                .map(|secs| UnixMillis::new(u64::try_from(secs).unwrap_or(0).saturating_mul(1000))),
            expired,
        })
    }

    fn crls(&self) -> Vec<CertificateRevocationListDer<'static>> {
        match self {
            Self::Loaded { der, .. } => vec![der.clone()],
            Self::NotConfigured | Self::Missing(_) => Vec::new(),
        }
    }
}

/// The inner mTLS client: checks the broker's certificate against `trust` and `crl` (see the
/// module documentation) and authenticates with `identity`.
///
/// # Errors
///
/// rustls refuses the settings.
pub fn inner_config(
    trust: &BrokerTrust,
    crl: &RootCrl,
    identity: &Identity,
) -> Result<Arc<ClientConfig>, TlsError> {
    // Revocation (Laysi/trustiot-v2#556): every certificate below the root is checked against
    // the root CRL, not only the broker's own (no `only_check_end_entity_revocation`); a
    // certificate whose issuer has no CRL here passes (the gateway has no issuing CA's CRL); an
    // expired root CRL still counts (no `enforce_revocation_expiration`). Without a root CRL,
    // nothing is checked.
    let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::clone(&trust.roots), Arc::new(provider()))
            .with_crls(crl.crls())
            .allow_unknown_revocation_status()
            .build()
            .map_err(|error| TlsError::Verifier(error.to_string()))?;
    let key = Arc::new(CertifiedKey::new(
        identity.chain.clone(),
        Arc::clone(&identity.key),
    ));
    let mut config = ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_webpki_verifier(verifier)
        .with_client_cert_resolver(Arc::new(ClientCertificate(key)));
    config.resumption = Resumption::in_memory_sessions(SESSION_CACHE);
    Ok(Arc::new(config))
}

/// The inner mTLS client settings in use, which certificate renewal replaces while the agent
/// runs ([`crate::renewal`]). A connection takes the settings when it opens and keeps them; the
/// next one takes the new settings. Each replacement comes with a fresh resumption cache (see
/// [`inner_config`]): a session resumed from a ticket of the old certificate would keep showing
/// the broker the old certificate's identity, since a resumed TLS 1.3 handshake sends none.
#[derive(Clone)]
pub struct InnerConfig(Arc<std::sync::Mutex<Arc<ClientConfig>>>);

impl std::fmt::Debug for InnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InnerConfig").finish_non_exhaustive()
    }
}

impl InnerConfig {
    /// Settings that start as `config`.
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(config)))
    }

    /// The settings for a connection opening now.
    pub fn current(&self) -> Arc<ClientConfig> {
        // Held only for a clone or a replacement, never across a panic point that leaves the
        // slot broken.
        Arc::clone(
            &self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Makes `config` the settings of every connection that opens from now on.
    pub fn replace(&self, config: Arc<ClientConfig>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }
}

/// Whether the clock's time lies inside the gateway certificate's validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validity {
    /// Valid now.
    Current,
    /// Expired or not yet valid by this clock; otherwise a certificate the broker accepts.
    OutsideNow,
}

/// Checks the gateway certificate as the broker will, with webpki's client certificate check:
/// the chain (with the issuing CA in `identity.certificate`) must lead to a root in `trust`,
/// and the extended key usage must allow client authentication (`identity::load` already
/// checked the key usage). A certificate that fails only because of the time is
/// [`Validity::OutsideNow`], not an error: the gateway's clock may be wrong (its hardware RTC
/// never set, or its time lost), the broker judges the validity by its own clock, and refusing
/// to start would only turn a wrong clock into an outage.
///
/// # Errors
///
/// The CA file cannot be used, or the broker would refuse the certificate whatever the time.
pub fn check_client_certificate(
    trust: &BrokerTrust,
    identity: &Identity,
    now: UnixTime,
) -> Result<Validity, TlsError> {
    check_client_certificate_under(trust, &RootCrl::NotConfigured, identity, now)
}

/// [`check_client_certificate`], also against the root CRL: an issuing CA the root revoked fails.
/// The gateway certificate's own status is unknown here (no issuing CA's CRL) and allowed, as in
/// the handshake's own check of the broker's chain.
///
/// # Errors
///
/// As [`check_client_certificate`].
pub fn check_client_certificate_under(
    trust: &BrokerTrust,
    root_crl: &RootCrl,
    identity: &Identity,
    now: UnixTime,
) -> Result<Validity, TlsError> {
    check_chain_under(
        trust,
        root_crl,
        &identity.chain,
        (identity.not_before, identity.not_after),
        now,
    )
}

/// [`check_client_certificate_under`] on a chain alone (the gateway certificate first, then the
/// issuing CA), valid from `validity.0` to `validity.1` by the gateway certificate: the key is
/// not checked.
///
/// # Errors
///
/// As [`check_client_certificate`].
pub fn check_chain_under(
    trust: &BrokerTrust,
    root_crl: &RootCrl,
    chain: &[CertificateDer<'static>],
    validity: (UnixMillis, UnixMillis),
    now: UnixTime,
) -> Result<Validity, TlsError> {
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::clone(&trust.roots), Arc::new(provider()))
            .with_crls(root_crl.crls())
            .allow_unknown_revocation_status()
            .build()
            .map_err(|error| TlsError::Verifier(error.to_string()))?;
    let refused = |source| TlsError::ClientCertificate {
        path: trust.path.clone(),
        source,
    };
    let Some((leaf, intermediates)) = chain.split_first() else {
        return Err(TlsError::Verifier("no certificate".to_owned()));
    };
    match verifier.verify_client_cert(leaf, intermediates, now) {
        Ok(_) => Ok(Validity::Current),
        Err(error) if is_about_time(&error) => {
            // Once more, at a time the certificate is valid: everything but the time must hold.
            let middle = validity.0.get() / 2 + validity.1.get() / 2;
            let inside = UnixTime::since_unix_epoch(Duration::from_millis(middle));
            verifier
                .verify_client_cert(leaf, intermediates, inside)
                .map(|_| Validity::OutsideNow)
                .map_err(refused)
        }
        Err(error) => Err(refused(error)),
    }
}

fn is_about_time(error: &rustls::Error) -> bool {
    matches!(
        error,
        rustls::Error::InvalidCertificate(
            CertificateError::Expired
                | CertificateError::ExpiredContext { .. }
                | CertificateError::NotValidYet
                | CertificateError::NotValidYetContext { .. }
        )
    )
}

/// Always presents the gateway certificate; rustls then asks the key for a signature scheme
/// the broker accepts (the TPM key offers only ECDSA P-256 SHA-256).
#[derive(Debug)]
struct ClientCertificate(Arc<CertifiedKey>);

impl ResolvesClientCert for ClientCertificate {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _schemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateRevocationListParams, CrlDistributionPoint,
        CrlIssuingDistributionPoint, CrlScope, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyIdMethod, KeyPair, KeyUsagePurpose, SerialNumber, SigningKey as _,
    };
    use root_crl::der_element;
    use rustls::ProtocolVersion;
    use testkit::mqtt::{Authority, ScratchDir};

    use super::*;

    #[test]
    fn the_broker_name_is_one_fixed_dns_name() {
        assert_eq!(BROKER_NAME, "mqtt-broker.trustiot.internal");
        assert!(
            matches!(broker_name(), Ok(ServerName::DnsName(name)) if name.as_ref() == BROKER_NAME)
        );
        assert_eq!(BROKER_NAME, testkit::mqtt::BROKER_NAME);
    }

    fn trust(dir: &ScratchDir, pems: &[&str]) -> Result<BrokerTrust, TlsError> {
        BrokerTrust::load(&dir.write("broker-ca.pem", pems.concat()))
    }

    fn not_a_root(result: Result<BrokerTrust, TlsError>) -> (usize, String) {
        match result {
            Err(TlsError::NotARoot { index, reason, .. }) => (index, reason),
            other => panic!("expected NotARoot, got {other:?}"),
        }
    }

    /// A root CA like the production one: ECDSA P-384, keyCertSign and cRLSign (or the given
    /// key usages), signing CRLs.
    fn p384_root(name: &str, usages: Vec<KeyUsagePurpose>) -> (String, Issuer<'static, KeyPair>) {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        params.key_usages = usages;
        params.distinguished_name.push(DnType::CommonName, name);
        let pem = params.self_signed(&key).unwrap().pem();
        // rcgen signs a CRL only for an issuer whose key usages allow it or are empty; the
        // certificate above keeps its own key usages.
        params.key_usages = Vec::new();
        (pem, Issuer::new(params, key))
    }

    fn p384_crl(issuer: &Issuer<'static, KeyPair>, number: u64) -> String {
        crl_params(number).signed_by(issuer).unwrap().pem().unwrap()
    }

    fn crl_params(number: u64) -> CertificateRevocationListParams {
        CertificateRevocationListParams {
            this_update: rcgen::date_time_ymd(2020, 1, 1),
            next_update: rcgen::date_time_ymd(2099, 1, 1),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: KeyIdMethod::Sha256,
        }
    }

    #[test]
    fn the_inner_trust_anchors_are_root_ca_certificates_only() {
        let dir = ScratchDir::new("agent-tls-anchors");
        let root = Authority::new("testkit root CA");
        let (p384, _) = p384_root(
            "testkit P-384 root CA",
            vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign],
        );
        // One root, or several (a root rollover), each a trust anchor.
        assert_eq!(trust(&dir, &[&root.pem]).unwrap().anchors.len(), 1);
        assert_eq!(trust(&dir, &[&root.pem, &p384]).unwrap().anchors.len(), 2);

        // An issuing CA, alone or after a root.
        let issuing = root.issuing("testkit issuing CA");
        let (index, reason) = not_a_root(trust(&dir, &[&issuing.pem]));
        assert_eq!(index, 1);
        assert!(reason.contains("issued by CN=testkit root CA"), "{reason}");
        let (index, _) = not_a_root(trust(&dir, &[&root.pem, &issuing.pem]));
        assert_eq!(index, 2);
        // A leaf: the broker's own certificate.
        let leaf = issuing.server(&[BROKER_NAME]);
        let (_, reason) = not_a_root(trust(&dir, &[&leaf.cert_pem]));
        assert!(reason.contains("not by itself"), "{reason}");
        // Self-issued and self-signed, but not a CA.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![BROKER_NAME.to_owned()]).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, BROKER_NAME);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let self_signed = params.self_signed(&key).unwrap().pem();
        let (_, reason) = not_a_root(trust(&dir, &[&self_signed]));
        assert!(reason.contains("not a CA certificate"), "{reason}");
        // Self-issued (subject equal to issuer) but signed by another key.
        let same_name = root.issuing("testkit root CA");
        let (_, reason) = not_a_root(trust(&dir, &[&same_name.pem]));
        assert!(reason.contains("not signed by its own key"), "{reason}");
        // Nothing at all.
        assert!(matches!(
            trust(&dir, &[]),
            Err(TlsError::NoUsableCertificate(_))
        ));
    }

    fn load(
        dir: &ScratchDir,
        trust: &BrokerTrust,
        contents: Option<&[u8]>,
    ) -> Result<RootCrl, TlsError> {
        let path = dir.path().join("root-crl.pem");
        match contents {
            Some(contents) => std::fs::write(&path, contents).unwrap(),
            None => drop(std::fs::remove_file(&path)),
        }
        RootCrl::load(Some(&path), trust, UnixTime::now())
    }

    fn bad_crl(result: Result<RootCrl, TlsError>) -> String {
        match result {
            Err(TlsError::BadCrl { reason, .. }) => reason,
            other => panic!("expected BadCrl, got {other:?}"),
        }
    }

    #[test]
    fn a_root_crl_is_loaded_only_when_a_trust_anchor_signed_it() {
        let dir = ScratchDir::new("agent-tls-crl");
        let root = Authority::new("testkit root CA");
        let issuing = root.issuing("testkit issuing CA");
        let trust = trust(&dir, &[&root.pem]).unwrap();

        // Not configured, or configured but not received yet: no revocation check.
        assert_eq!(
            RootCrl::load(None, &trust, UnixTime::now()).unwrap(),
            RootCrl::NotConfigured
        );
        assert!(matches!(load(&dir, &trust, None), Ok(RootCrl::Missing(_))));
        assert!(RootCrl::NotConfigured.crls().is_empty());

        // Signed by the root: loaded with its number; expired or not, it counts.
        let current = root.crl(7, &[&issuing], (2099, 1, 1));
        let RootCrl::Loaded {
            number,
            next_update,
            expired,
            ..
        } = load(&dir, &trust, Some(current.as_bytes())).unwrap()
        else {
            panic!("not loaded")
        };
        assert_eq!(
            (number.as_deref(), expired),
            (Some("7"), false),
            "{next_update:?}"
        );
        assert_eq!(
            next_update.map(crate::identity::utc).as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        let expired = root.crl(8, &[], (2021, 1, 1));
        let loaded = load(&dir, &trust, Some(expired.as_bytes())).unwrap();
        assert!(
            matches!(loaded, RootCrl::Loaded { expired: true, .. }),
            "{loaded:?}"
        );
        assert_eq!(loaded.crls().len(), 1);

        // Signed by a key with the root's name but not the root's.
        let forged = Authority::new("testkit root CA").crl(9, &[], (2099, 1, 1));
        let reason = bad_crl(load(&dir, &trust, Some(forged.as_bytes())));
        assert!(reason.contains("does not verify"), "{reason}");
        // Signed by a CA that is not a trust anchor: another root, or our issuing CA.
        for (what, crl) in [
            (
                "another root",
                Authority::new("another root CA").crl(9, &[], (2099, 1, 1)),
            ),
            ("the issuing CA", issuing.crl(9, &[], (2099, 1, 1))),
        ] {
            let reason = bad_crl(load(&dir, &trust, Some(crl.as_bytes())));
            assert!(reason.contains("is not a trust anchor"), "{what}: {reason}");
        }

        // Not exactly one PEM CRL, or not one at all.
        let reason = bad_crl(load(
            &dir,
            &trust,
            Some(format!("{current}{current}").as_bytes()),
        ));
        assert!(reason.contains("holds 2 PEM blocks"), "{reason}");
        for (what, contents) in [
            ("a certificate", root.pem.as_bytes()),
            (
                "text",
                b"not a CRL
"
                .as_slice(),
            ),
            ("nothing", b"".as_slice()),
        ] {
            let reason = bad_crl(load(&dir, &trust, Some(contents)));
            assert!(reason.contains("holds 0 PEM blocks"), "{what}: {reason}");
        }
        let corrupt = current.replacen("MI", "M!", 1);
        let reason = bad_crl(load(&dir, &trust, Some(corrupt.as_bytes())));
        assert!(reason.contains("not PEM"), "{reason}");
        // The DER inside the PEM changed: no longer a CRL, or no longer the root's signature.
        let der = CertificateRevocationListDer::from_pem_slice(current.as_bytes()).unwrap();
        let mut tampered = der.to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        let reason = bad_crl(load(&dir, &trust, Some(crl_pem(&tampered).as_bytes())));
        assert!(
            reason.contains("does not verify") || reason.contains("cannot be parsed"),
            "{reason}"
        );
        let mut trailing = der.to_vec();
        trailing.push(0);
        let reason = bad_crl(load(&dir, &trust, Some(crl_pem(&trailing).as_bytes())));
        assert!(reason.contains("bytes after the CRL"), "{reason}");
        // Too large to be a root CRL.
        let large = vec![b'A'; usize::try_from(MAX_ROOT_CRL_BYTES).unwrap() + 1];
        let reason = bad_crl(load(&dir, &trust, Some(&large)));
        assert!(reason.contains("larger than"), "{reason}");
        // A directory where the file should be.
        let path = dir.path().join("a-directory");
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            RootCrl::load(Some(&path), &trust, UnixTime::now()),
            Err(TlsError::CrlRead { .. })
        ));
    }

    #[test]
    fn a_root_crl_needs_a_root_that_may_sign_crls() {
        let dir = ScratchDir::new("agent-tls-crl-sign");
        // The production root's algorithm (ECDSA P-384), signing a root CRL.
        let (root, issuer) = p384_root(
            "testkit P-384 root CA",
            vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign],
        );
        let trust = trust(&dir, &[&root]).unwrap();
        let loaded = load(&dir, &trust, Some(p384_crl(&issuer, 1).as_bytes())).unwrap();
        assert!(matches!(loaded, RootCrl::Loaded { .. }), "{loaded:?}");
        // The same, but the root's key usage leaves out cRLSign.
        let (root, issuer) = p384_root("testkit P-384 root CA", vec![KeyUsagePurpose::KeyCertSign]);
        let trust = trust_of(&dir, &root);
        let reason = bad_crl(load(&dir, &trust, Some(p384_crl(&issuer, 1).as_bytes())));
        assert!(reason.contains("may not sign CRLs"), "{reason}");
    }

    fn trust_of(dir: &ScratchDir, pem: &str) -> BrokerTrust {
        trust(dir, &[pem]).unwrap()
    }

    /// `der` as one PEM block `X509 CRL`.
    fn crl_pem(der: &[u8]) -> String {
        pem_block("X509 CRL", der)
    }

    fn pem_block(label: &str, der: &[u8]) -> String {
        use base64::Engine as _;
        let base64 = base64::engine::general_purpose::STANDARD.encode(der);
        format!("-----BEGIN {label}-----\n{base64}\n-----END {label}-----\n")
    }

    /// One DER element: `tag` around `contents`.
    fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut element = vec![tag];
        if contents.len() < 0x80 {
            element.push(u8::try_from(contents.len()).unwrap());
        } else {
            let length = contents.len().to_be_bytes();
            let skip = length.iter().take_while(|&&byte| byte == 0).count();
            element.push(0x80 | u8::try_from(length.len() - skip).unwrap());
            element.extend_from_slice(&length[skip..]);
        }
        element.extend_from_slice(contents);
        element
    }

    /// `signed` (a certificate or a CRL in DER) signed again by `key`: with `algorithm` (the
    /// AlgorithmIdentifier's contents) as its signature algorithm, inside and outside the
    /// signed part, if given, and with `extension` (a whole Extension) added to a CRL's
    /// crlExtensions if given. Builds what rcgen cannot.
    fn re_signed(
        signed: &[u8],
        key: &KeyPair,
        algorithm: Option<&[u8]>,
        extension: Option<&[u8]>,
    ) -> Vec<u8> {
        let outer = der_element(signed).unwrap();
        let signed_part = der_element(outer.contents).unwrap();
        let original = der_element(signed_part.rest).unwrap();
        let algorithm = algorithm.unwrap_or(original.contents);
        let mut fields = Vec::new();
        let mut algorithm_written = false;
        let mut input = signed_part.contents;
        while let Some(field) = der_element(input) {
            match (field.tag, extension) {
                // The first SEQUENCE is the signature algorithm (in a certificate after the
                // version and the serial, in a CRL after the version).
                (0x30, _) if !algorithm_written => {
                    fields.extend(tlv(0x30, algorithm));
                    algorithm_written = true;
                }
                // A CRL's crlExtensions.
                (0xA0, Some(extension)) => {
                    let list = der_element(field.contents).unwrap().contents;
                    fields.extend(tlv(0xA0, &tlv(0x30, &[list, extension].concat())));
                }
                _ => fields.extend_from_slice(field.whole),
            }
            input = field.rest;
        }
        let signed_part = tlv(0x30, &fields);
        let signature = key.sign(&signed_part).unwrap();
        tlv(
            0x30,
            &[
                signed_part,
                tlv(0x30, algorithm),
                tlv(0x03, &[&[0][..], &signature].concat()),
            ]
            .concat(),
        )
    }

    /// The AlgorithmIdentifier contents of ecdsa-with-SHA256 (1.2.840.10045.4.3.2).
    const ECDSA_WITH_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];

    /// A P-256 root that can sign CRLs, and its key.
    fn p256_root(name: &str) -> (String, Issuer<'static, KeyPair>) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, name);
        let pem = params.self_signed(&key).unwrap().pem();
        (pem, Issuer::new(params, key))
    }

    fn der_of(pem: &str) -> Vec<u8> {
        CertificateDer::from_pem_slice(pem.as_bytes())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn two_roots_with_the_same_subject_are_refused() {
        let dir = ScratchDir::new("agent-tls-duplicate");
        // A same-name rollover: the root CRL would name both by subject alone.
        let first = Authority::new("testkit root CA");
        let second = Authority::new("testkit root CA");
        let other = Authority::new("testkit other root CA");
        for (what, pems, expected) in [
            (
                "same name, other key",
                vec![&first.pem, &other.pem, &second.pem],
                (1, 3),
            ),
            ("the same root twice", vec![&first.pem, &first.pem], (1, 2)),
        ] {
            let pems: Vec<&str> = pems.into_iter().map(String::as_str).collect();
            match trust(&dir, &pems) {
                Err(TlsError::DuplicateRoot {
                    first,
                    index,
                    subject,
                    ..
                }) => {
                    assert_eq!((first, index), expected, "{what}");
                    assert_eq!(subject, "CN=testkit root CA", "{what}");
                }
                other => panic!("{what}: expected DuplicateRoot, got {other:?}"),
            }
        }
        // Different names are fine.
        assert_eq!(
            trust(&dir, &[&first.pem, &other.pem])
                .unwrap()
                .anchors
                .len(),
            2
        );
    }

    #[test]
    fn names_in_errors_have_no_control_characters_and_are_cut_short() {
        let dir = ScratchDir::new("agent-tls-names");
        let name = format!(
            "evil\n2026-10-23T00:00:00Z INFO forged line\r\x1b[2K{}",
            "x".repeat(500)
        );
        let evil = Authority::new(&name);
        let root = Authority::new("testkit root CA");
        let clean = |error: &TlsError| {
            let text = error.to_string();
            assert!(!text.chars().any(char::is_control), "{text:?}");
            assert!(
                text.contains("evil 2026-10-23T00:00:00Z INFO forged line"),
                "{text}"
            );
            assert!(!text.contains(&"x".repeat(MAX_NAME_CHARS)), "{text}");
        };
        // The issuer of a certificate that is not a root.
        clean(&trust(&dir, &[&evil.issuing("testkit issuing CA").pem]).unwrap_err());
        // The subject of two roots with the same name.
        clean(&trust(&dir, &[&evil.pem, &evil.pem]).unwrap_err());
        // The issuer of a root CRL from a CA that is not a trust anchor.
        let trust = trust(&dir, &[&root.pem]).unwrap();
        let crl = evil.crl(1, &[], (2099, 1, 1));
        clean(&load(&dir, &trust, Some(crl.as_bytes())).unwrap_err());
    }

    #[test]
    fn signatures_are_checked_only_with_the_algorithms_the_handshake_accepts() {
        let dir = ScratchDir::new("agent-tls-algorithms");
        let (root, issuer) = p256_root("testkit root CA");
        let trust = trust(&dir, &[&root]).unwrap();
        let crl = crl_params(1).signed_by(&issuer).unwrap();
        // Signed again as rcgen signed it: loads (the helper keeps a valid CRL valid).
        let again = re_signed(crl.der(), issuer.key(), None, None);
        assert!(matches!(
            load(&dir, &trust, Some(crl_pem(&again).as_bytes())),
            Ok(RootCrl::Loaded { .. })
        ));
        // ecdsa-with-SHA256 with an explicit NULL parameter: the same signature, but webpki
        // matches algorithms by their exact bytes and would refuse it in every handshake.
        let with_null = [ECDSA_WITH_SHA256, &[0x05, 0x00]].concat();
        let odd = re_signed(crl.der(), issuer.key(), Some(&with_null), None);
        let reason = bad_crl(load(&dir, &trust, Some(crl_pem(&odd).as_bytes())));
        assert!(
            reason.contains("not one the TLS handshake accepts"),
            "{reason}"
        );
        // The same for a root's own signature.
        let odd_root = re_signed(&der_of(&root), issuer.key(), Some(&with_null), None);
        let (_, reason) = not_a_root(trust_from_der(&dir, &odd_root));
        assert!(
            reason.contains("not one the TLS handshake accepts"),
            "{reason}"
        );
        let same_root = re_signed(&der_of(&root), issuer.key(), None, None);
        assert!(trust_from_der(&dir, &same_root).is_ok());
    }

    fn trust_from_der(dir: &ScratchDir, der: &[u8]) -> Result<BrokerTrust, TlsError> {
        BrokerTrust::load(&dir.write("broker-ca.pem", pem_block("CERTIFICATE", der)))
    }

    #[test]
    fn a_root_crl_with_an_issuing_distribution_point_is_refused() {
        let dir = ScratchDir::new("agent-tls-idp");
        let (root, issuer) = p256_root("testkit root CA");
        let trust = trust(&dir, &[&root]).unwrap();
        // With this scope webpki would not use the CRL for an issuing CA at all, and unknown
        // revocation status is allowed: nothing would be checked.
        for scope in [Some(CrlScope::UserCertsOnly), None] {
            let mut params = crl_params(1);
            params.issuing_distribution_point = Some(CrlIssuingDistributionPoint {
                distribution_point: CrlDistributionPoint {
                    uris: vec!["https://crl.trustiot.example/root.crl".to_owned()],
                },
                scope,
            });
            let crl = params.signed_by(&issuer).unwrap().pem().unwrap();
            let reason = bad_crl(load(&dir, &trust, Some(crl.as_bytes())));
            assert!(
                reason.contains("issuing distribution point"),
                "{scope:?}: {reason}"
            );
        }
    }

    #[test]
    fn what_webpki_refuses_in_a_crl_is_refused_at_load() {
        let dir = ScratchDir::new("agent-tls-webpki");
        let (root, issuer) = p256_root("testkit root CA");
        let trust = trust(&dir, &[&root]).unwrap();
        let crl = crl_params(1).signed_by(&issuer).unwrap();
        // x509-parser reads both and the root's signature verifies; only webpki refuses them.
        let unknown_critical = tlv(
            0x30,
            &[
                // 1.3.6.1.4.1.32473.1 (the documentation enterprise number, RFC 5612)
                tlv(
                    0x06,
                    &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x81, 0xFD, 0x59, 0x01],
                ),
                vec![0x01, 0x01, 0xFF],
                tlv(0x04, &tlv(0x05, &[])),
            ]
            .concat(),
        );
        let delta = tlv(
            0x30,
            &[
                // deltaCRLIndicator (2.5.29.27), base CRL number 1
                tlv(0x06, &[0x55, 0x1D, 0x1B]),
                vec![0x01, 0x01, 0xFF],
                tlv(0x04, &tlv(0x02, &[0x01])),
            ]
            .concat(),
        );
        for (extension, refusal) in [
            (unknown_critical, "UnsupportedCriticalExtension"),
            (delta, "UnsupportedDeltaCrl"),
        ] {
            let odd = re_signed(crl.der(), issuer.key(), None, Some(&extension));
            let reason = bad_crl(load(&dir, &trust, Some(crl_pem(&odd).as_bytes())));
            assert!(
                reason.contains(&format!("provided CRL could not be parsed: {refusal}")),
                "{reason}"
            );
        }
    }

    #[test]
    fn a_root_crl_of_exactly_the_size_limit_is_read() {
        let dir = ScratchDir::new("agent-tls-size");
        let root = Authority::new("testkit root CA");
        let trust = trust(&dir, &[&root.pem]).unwrap();
        let crl = root.crl(1, &[], (2099, 1, 1));
        let limit = usize::try_from(MAX_ROOT_CRL_BYTES).unwrap();
        // Explanatory text before the PEM block (RFC 7468) fills the file up to the limit.
        let padded = |size: usize| {
            let mut text = String::new();
            while text.len() + crl.len() < size {
                let line = (size - crl.len() - text.len()).min(64);
                text.push_str(&"x".repeat(line - 1));
                text.push('\n');
            }
            text.push_str(&crl);
            assert_eq!(text.len(), size);
            text
        };
        assert!(matches!(
            load(&dir, &trust, Some(padded(limit).as_bytes())),
            Ok(RootCrl::Loaded { .. })
        ));
        let reason = bad_crl(load(&dir, &trust, Some(padded(limit + 1).as_bytes())));
        assert!(reason.contains("larger than"), "{reason}");
    }

    #[test]
    fn chacha20_comes_first_and_only_tls_1_3_is_offered() {
        let suites = cipher_suites();
        assert_eq!(
            suites[0].suite(),
            rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
        );
        assert!(
            suites
                .iter()
                .all(|suite| suite.version().version == ProtocolVersion::TLSv1_3),
            "{suites:?}"
        );
        assert_eq!(provider().cipher_suites[0].suite(), suites[0].suite());
    }

    #[test]
    fn the_outer_layer_has_no_trust_anchor_but_the_configured_ones() {
        assert!(matches!(outer_config(&[]), Err(TlsError::NoTrust)));
        let dir = std::env::temp_dir().join(format!("agent-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(
            outer_config(std::slice::from_ref(&empty)),
            Err(TlsError::NoUsableCertificate(_))
        ));
        let missing = dir.join("missing.pem");
        assert!(matches!(
            outer_config(&[missing]),
            Err(TlsError::Read { .. })
        ));

        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "test root");
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, params.self_signed(&key).unwrap().pem()).unwrap();
        let config = outer_config(std::slice::from_ref(&ca)).unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // One file with one root: that root is everything the outer layer trusts.
        assert!(
            outer_config(&[ca.clone(), empty]).is_err(),
            "every listed file must be usable"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
