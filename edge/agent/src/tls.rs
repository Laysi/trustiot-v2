//! The two TLS layers of the gateway's connection (decision 6).
//!
//! - **Outer TLS**, to Cloudflare (SaaS) or the customer's ingress (on-prem): it only crosses
//!   the network. Its trust anchors come only from the configured PEM files: the OS bundle for
//!   SaaS, the customer's or our CA on-prem, plus a factory's TLS-interception CA if one is
//!   added. There are no built-in roots (no webpki-roots, no native root store).
//! - **Inner mTLS**, inside the WebSocket, straight to the broker: it trusts only our CA, and
//!   the client authenticates with the gateway certificate and its TPM key.
//!
//! Both are TLS 1.3 only with ChaCha20-Poly1305 first: the gateway's Cortex-A72 has no AES
//! instructions, ChaCha20 took 21–34% less CPU there, and rustls would otherwise offer
//! AES-256-GCM first (Laysi/trustiot-v2#75). Cloudflare and Envoy follow the client's order;
//! Mosquitto follows its own, so the broker's gateway listener sets `ciphers_tls1.3` with
//! ChaCha20 first (decision 6). Session resumption stays on in both, with a small in-memory
//! cache: a resumed inner handshake needs no TPM signature (about 70 ms less per reconnect),
//! and revocation still works because SIGHUP on the broker invalidates every ticket. The
//! provider is ring; aws-lc-rs would add a CMake C build and about 120 ms of start-up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::{ClientConfig, ResolvesClientCert, Resumption};
use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::{self, cipher_suite};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, UnixTime};

use crate::identity::Identity;

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

/// ring with [`cipher_suites`].
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: cipher_suites(),
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
    #[error("checking the client certificate: {0}")]
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

/// The inner mTLS client: trusts only the certificates in `ca` (every one must be a usable
/// trust anchor) and authenticates with `identity`.
///
/// # Errors
///
/// A CA file that cannot be read, is empty, or holds a certificate that is not a trust anchor.
pub fn inner_config(ca: &Path, identity: &Identity) -> Result<Arc<ClientConfig>, TlsError> {
    let roots = broker_roots(ca)?;
    let key = Arc::new(CertifiedKey::new(
        identity.chain.clone(),
        Arc::clone(&identity.key),
    ));
    let mut config = ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_client_cert_resolver(Arc::new(ClientCertificate(key)));
    config.resumption = Resumption::in_memory_sessions(SESSION_CACHE);
    Ok(Arc::new(config))
}

/// Our CA file as trust anchors; every certificate in it must be one.
fn broker_roots(ca: &Path) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    let certificates = read_certificates(ca)?;
    if certificates.is_empty() {
        return Err(TlsError::NoUsableCertificate(ca.to_owned()));
    }
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|source| TlsError::BadCertificate {
                path: ca.to_owned(),
                source,
            })?;
    }
    Ok(roots)
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
/// the chain must lead to our CA in `ca`, and the extended key usage must allow client
/// authentication (`identity::load` already checked the key usage). A certificate that fails only because of the time
/// is [`Validity::OutsideNow`], not an error: a gateway without a real-time clock may start
/// with a wrong one, and refusing to start would leave it unable to fix it.
///
/// # Errors
///
/// The CA file cannot be used, or the broker would refuse the certificate whatever the time.
pub fn check_client_certificate(
    ca: &Path,
    identity: &Identity,
    now: UnixTime,
) -> Result<Validity, TlsError> {
    let verifier = WebPkiClientVerifier::builder_with_provider(
        Arc::new(broker_roots(ca)?),
        Arc::new(provider()),
    )
    .build()
    .map_err(|error| TlsError::Verifier(error.to_string()))?;
    let refused = |source| TlsError::ClientCertificate {
        path: ca.to_owned(),
        source,
    };
    let Some((leaf, intermediates)) = identity.chain.split_first() else {
        return Err(TlsError::Verifier("no certificate".to_owned()));
    };
    match verifier.verify_client_cert(leaf, intermediates, now) {
        Ok(_) => Ok(Validity::Current),
        Err(error) if is_about_time(&error) => {
            // Once more, at a time the certificate is valid: everything but the time must hold.
            let middle = identity.not_before.get() / 2 + identity.not_after.get() / 2;
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
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
    use rustls::ProtocolVersion;

    use super::*;

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
