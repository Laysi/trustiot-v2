//! The checks a root CRL must pass before anything uses it (services/certs/README.md, 「根 CRL」,
//! 「格式」): the offline root CA's CRL that lists revoked issuing CAs, which the gateway's inner
//! TLS checks the broker's chain against (Laysi/trustiot-v2#556).
//!
//! Two programs use them, and they must agree byte for byte on what passes:
//!
//! - the gateway agent (`edge/agent/src/tls.rs`, `RootCrl`), which refuses to start with a root
//!   CRL that fails them;
//! - the certs service (`services/certs/src/authority.rs`), which adopts only a root CRL that
//!   passes them before it hands it to gateways, so it never hands out one the agent refuses.
//!
//! A root CRL passes when it is one PEM block `X509 CRL` of at most [`MAX_BYTES`], parses with
//! nothing after it, has no issuing distribution point, is signed by one of the trust anchors
//! (found by subject, verified with the TLS handshake's own signature algorithms,
//! [`signature_algorithms`]) whose key usage allows cRLSign, and webpki accepts it for the
//! handshake's server verifier (which refuses delta CRLs and unknown critical extensions).
//!
//! Errors are short English reasons; names from a certificate or CRL are shown without control
//! characters and cut short ([`shown`]), since anyone may have written the file.

use std::path::Path;
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, ring};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, CertificateRevocationListDer};
use x509_parser::oid_registry::OID_X509_EXT_ISSUER_DISTRIBUTION_POINT;
use x509_parser::prelude::{FromDer, X509Certificate, X509Name};
use x509_parser::revocation_list::CertificateRevocationList;

/// The largest root CRL file read. A root CRL lists revoked issuing CAs, a few hundred bytes
/// each.
pub const MAX_BYTES: u64 = 64 * 1024;

/// The most characters of a certificate's or CRL's name an error message shows.
pub const MAX_NAME_CHARS: usize = 200;

/// The signature algorithms the gateway's TLS handshake accepts: ring's. The agent's TLS
/// provider uses exactly these (`edge/agent/src/tls.rs`, `provider`), so a signature checked
/// here is one every handshake accepts, wherever it is checked.
#[must_use]
pub fn signature_algorithms() -> WebPkiSupportedAlgorithms {
    ring::default_provider().signature_verification_algorithms
}

/// ring with [`signature_algorithms`]: what webpki parses the CRL with here. The cipher suites
/// play no part in that.
fn provider() -> CryptoProvider {
    CryptoProvider {
        signature_verification_algorithms: signature_algorithms(),
        ..ring::default_provider()
    }
}

/// The trust anchors a root CRL must come from: root CA certificates with distinct subjects
/// (a root CRL names its issuer by subject only).
#[derive(Clone, Copy, Debug)]
pub struct Anchors<'a> {
    /// The root CA certificates, DER.
    pub certificates: &'a [CertificateDer<'static>],
    /// The same certificates as webpki's trust anchors.
    pub store: &'a Arc<RootCertStore>,
    /// Where they come from, named when the CRL's issuer is none of them.
    pub source: &'a Path,
}

/// webpki's trust anchors for `certificates`, for [`Anchors::store`].
///
/// # Errors
///
/// When webpki cannot use one of them as a trust anchor.
pub fn store(certificates: &[CertificateDer<'static>]) -> Result<Arc<RootCertStore>, String> {
    let mut store = RootCertStore::empty();
    for certificate in certificates {
        store
            .add(certificate.clone())
            .map_err(|error| format!("a root certificate cannot be a trust anchor: {error}"))?;
    }
    Ok(Arc::new(store))
}

/// A root CRL that passed [`check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedCrl {
    /// The CRL, DER.
    pub der: CertificateRevocationListDer<'static>,
    /// Its CRL number in decimal, if it has one.
    pub number: Option<String>,
    /// Its thisUpdate, Unix seconds.
    pub this_update: i64,
    /// Its nextUpdate, Unix seconds, if it has one.
    pub next_update: Option<i64>,
}

/// Checks the root CRL in `bytes` (the file's content) against `anchors`, as the module
/// documentation describes. Whether it has expired is the caller's business: an expired root CRL
/// still counts (services/certs/README.md, 「根 CRL」, 「過期」).
///
/// # Errors
///
/// Why the CRL cannot be used, as a short reason without control characters from the file.
pub fn check(bytes: &[u8], anchors: &Anchors<'_>) -> Result<CheckedCrl, String> {
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_BYTES) {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    let crls = CertificateRevocationListDer::pem_slice_iter(bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("not PEM: {error}"))?;
    let [der] = <[_; 1]>::try_from(crls)
        .map_err(|crls: Vec<_>| format!("holds {} PEM blocks \"X509 CRL\", not one", crls.len()))?;
    let (rest, crl) = CertificateRevocationList::from_der(&der)
        .map_err(|error| format!("cannot be parsed: {error}"))?;
    if !rest.is_empty() {
        return Err("has bytes after the CRL".to_owned());
    }
    // webpki takes a CRL with an issuing distribution point as authoritative only for the
    // certificates in its scope; with unknown revocation status allowed, a root CRL scoped
    // past the issuing CAs would silently check nothing. Root CRLs have none.
    if crl
        .extensions()
        .iter()
        .any(|extension| extension.oid == OID_X509_EXT_ISSUER_DISTRIBUTION_POINT)
    {
        return Err(
            "has an issuing distribution point; a root CRL must not have one (services/certs/README.md, 「根 CRL」)".to_owned(),
        );
    }
    signed_by_an_anchor(&der, &crl, anchors)?;
    // webpki parses it again for the handshakes; refuse here what it would refuse there.
    WebPkiServerVerifier::builder_with_provider(Arc::clone(anchors.store), Arc::new(provider()))
        .with_crls([der.clone()])
        .build()
        .map_err(|error| error.to_string())?;
    let number = crl.crl_number().map(ToString::to_string);
    let this_update = crl.last_update().timestamp();
    let next_update = crl.next_update().map(|time| time.timestamp());
    Ok(CheckedCrl {
        der,
        number,
        this_update,
        next_update,
    })
}

/// Whether the trust anchor named as the issuer of `crl` (`der`) signed it, with an
/// algorithm the handshake accepts, and may sign CRLs. Root subjects are unique, so at most one
/// anchor has that name.
fn signed_by_an_anchor(
    der: &[u8],
    crl: &CertificateRevocationList<'_>,
    anchors: &Anchors<'_>,
) -> Result<(), String> {
    let anchor = anchors.certificates.iter().find_map(|anchor| {
        X509Certificate::from_der(anchor)
            .ok()
            .map(|(_, anchor)| anchor)
            .filter(|anchor| anchor.subject().as_raw() == crl.issuer().as_raw())
    });
    let Some(anchor) = anchor else {
        return Err(format!(
            "its issuer {} is not a trust anchor in {}",
            shown(crl.issuer()),
            anchors.source.display()
        ));
    };
    verify_signature(der, anchor.public_key().raw).map_err(|reason| {
        format!(
            "it is not signed by the trust anchor {}: {reason}",
            shown(anchor.subject())
        )
    })?;
    match anchor.key_usage() {
        Ok(Some(usage)) if !usage.value.crl_sign() => Err(format!(
            "its issuer {} may not sign CRLs (key usage without cRLSign)",
            shown(anchor.subject())
        )),
        Ok(_) => Ok(()),
        Err(error) => Err(format!("its issuer has unreadable key usage: {error}")),
    }
}

/// `name` for an error message: from a file anyone may have written (a forged root CRL, a
/// wrong CA file), so without control characters, which could forge or split log lines, and
/// cut to [`MAX_NAME_CHARS`].
#[must_use]
pub fn shown(name: &X509Name<'_>) -> String {
    name.to_string()
        .chars()
        .take(MAX_NAME_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// One DER element at the start of some input. Only what x509-parser already parsed is read
/// this way; public for tests that build encodings x509 libraries do not.
#[derive(Clone, Copy, Debug)]
pub struct DerElement<'a> {
    /// Its tag byte.
    pub tag: u8,
    /// The element with its tag and length.
    pub whole: &'a [u8],
    /// Its contents.
    pub contents: &'a [u8],
    /// What follows the element.
    pub rest: &'a [u8],
}

/// The DER element at the start of `input`, if its header is one this reader knows (a length of
/// at most four bytes) and the input holds all of it.
#[must_use]
pub fn der_element(input: &[u8]) -> Option<DerElement<'_>> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        let length = bytes
            .iter()
            .fold(0_usize, |length, &byte| (length << 8) | usize::from(byte));
        (length, rest)
    };
    if rest.len() < length {
        return None;
    }
    let header = input.len() - rest.len();
    let (whole, after) = input.split_at(header + length);
    Some(DerElement {
        tag,
        whole,
        contents: &rest[..length],
        rest: after,
    })
}

/// The contents of a BIT STRING with no unused bits.
fn whole_bytes(bit_string: &[u8]) -> Option<&[u8]> {
    match bit_string.split_first() {
        Some((0, bytes)) => Some(bytes),
        _ => None,
    }
}

/// Checks that the key in `spki` (a SubjectPublicKeyInfo) signed `signed` (a certificate or a
/// CRL: a SEQUENCE of the signed part, the signature algorithm and the signature) the way the
/// handshake's webpki checks it: only with [`signature_algorithms`], matched by their exact
/// AlgorithmIdentifier bytes. So nothing passes here that every handshake would then refuse.
///
/// # Errors
///
/// Why the signature does not pass.
pub fn verify_signature(signed: &[u8], spki: &[u8]) -> Result<(), String> {
    let malformed = || "the signed structure is not DER that webpki reads".to_owned();
    let expect = |input, tag| {
        der_element(input)
            .filter(|element| element.tag == tag)
            .ok_or_else(malformed)
    };
    let outer = expect(signed, 0x30)?;
    let message = expect(outer.contents, 0x30)?;
    let algorithm = expect(message.rest, 0x30)?;
    let signature = expect(algorithm.rest, 0x03)?;
    let signature = whole_bytes(signature.contents).ok_or_else(malformed)?;
    let spki = expect(spki, 0x30)?;
    let key_algorithm = expect(spki.contents, 0x30)?;
    let key = expect(key_algorithm.rest, 0x03)?;
    let key = whole_bytes(key.contents).ok_or_else(malformed)?;
    let mut algorithm_known = false;
    for candidate in signature_algorithms().all {
        if candidate.signature_alg_id().as_ref() != algorithm.contents {
            continue;
        }
        algorithm_known = true;
        if candidate.public_key_alg_id().as_ref() != key_algorithm.contents {
            continue;
        }
        return candidate
            .verify_signature(key, message.whole, signature)
            .map_err(|_| "the signature does not verify".to_owned());
    }
    Err(if algorithm_known {
        "the signature algorithm does not fit the key".to_owned()
    } else {
        "the signature algorithm is not one the TLS handshake accepts".to_owned()
    })
}

#[cfg(test)]
mod tests {
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType, IsCa, Issuer,
        KeyIdMethod, KeyPair, KeyUsagePurpose, SerialNumber,
    };

    use super::*;

    /// A root like the production one (ECDSA P-384, keyCertSign and cRLSign): its certificate
    /// and the issuer that signs its CRLs.
    fn root(name: &str) -> (CertificateDer<'static>, Issuer<'static, KeyPair>) {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, name);
        let der = params.self_signed(&key).unwrap().der().clone();
        (der, Issuer::new(params, key))
    }

    fn crl(issuer: &Issuer<'static, KeyPair>, number: u64) -> String {
        CertificateRevocationListParams {
            this_update: rcgen::date_time_ymd(2020, 1, 1),
            next_update: rcgen::date_time_ymd(2099, 1, 1),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(issuer)
        .unwrap()
        .pem()
        .unwrap()
    }

    fn check_with(bytes: &[u8], root: &CertificateDer<'static>) -> Result<CheckedCrl, String> {
        let certificates = [root.clone()];
        check(
            bytes,
            &Anchors {
                certificates: &certificates,
                store: &store(&certificates).unwrap(),
                source: Path::new("root.pem"),
            },
        )
    }

    // The agent's tests (edge/agent/src/tls.rs) go through every refusal; these cover the
    // crate on its own.
    #[test]
    fn a_crl_of_the_anchor_passes_with_its_number_and_dates() {
        let (der, issuer) = root("root-crl test root");
        let checked = check_with(crl(&issuer, 7).as_bytes(), &der).unwrap();
        assert_eq!(checked.number.as_deref(), Some("7"));
        assert_eq!(checked.this_update, 1_577_836_800);
        assert_eq!(checked.next_update, Some(4_070_908_800));
    }

    #[test]
    fn a_crl_of_another_root_or_with_text_around_it_is_refused() {
        let (der, _) = root("root-crl test root");
        let (_, impostor) = root("root-crl test root");
        let reason = check_with(crl(&impostor, 1).as_bytes(), &der).unwrap_err();
        assert!(reason.contains("does not verify"), "{reason}");
        let (_, other) = root("root-crl other root");
        let reason = check_with(crl(&other, 1).as_bytes(), &der).unwrap_err();
        assert_eq!(
            reason,
            "its issuer CN=root-crl other root is not a trust anchor in root.pem"
        );
        let (_, issuer) = root("root-crl test root");
        let twice = crl(&issuer, 1).repeat(2);
        let reason = check_with(twice.as_bytes(), &der).unwrap_err();
        assert!(reason.contains("holds 2 PEM blocks"), "{reason}");
    }
}
