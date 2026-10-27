//! The workstation's signature over a request to the certs service (services/certs/README.md,
//! "工作站簽章"; Laysi/trustiot-v2#79): ECDSA P-256 with SHA-256 by the workstation key over
//! [`workstation_signing_input`] (the method, the path, the time, a nonce and the SHA-256 of the
//! body), carried in four headers with the workstation certificate.
//!
//! Each attempt of a request is signed anew: a new time and a new nonce, so a request sent again
//! after a lost response is not refused as `replayed` and a stale one is not refused as `stale`.
//! The header values are never logged; the requests mark them sensitive.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER, workstation_signing_input,
};
use http::{HeaderName, HeaderValue, Method};
use rcgen::SigningKey as _;
use sha2::{Digest as _, Sha256};

use crate::Error;
use crate::identity::WorkstationKey;
use crate::secret::REDACTED;

/// Random bytes in a nonce: 16 bytes are 22 base64url characters, the shortest the certs service
/// accepts.
pub const NONCE_BYTES: usize = 16;

/// A request nonce: [`NONCE_BYTES`] bytes from the operating system's CSPRNG, base64url without
/// padding.
#[derive(Clone, PartialEq, Eq)]
pub struct Nonce(String);

impl Nonce {
    /// A new nonce.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness.
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0_u8; NONCE_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|_| Error::Failed("the operating system gave no randomness".to_owned()))?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// The nonce as it goes in the header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({REDACTED})")
    }
}

/// The four `trustiot-workstation-*` headers of one signed request. `Debug` shows only their
/// names.
pub struct SignedHeaders(Vec<(HeaderName, HeaderValue)>);

impl fmt::Debug for SignedHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name.as_str()))
            .finish()
    }
}

impl SignedHeaders {
    /// The headers, values marked sensitive, for [`crate::http::Call::headers`].
    #[must_use]
    pub fn into_headers(self) -> Vec<(HeaderName, HeaderValue)> {
        self.0
    }
}

/// Signs `method path` with `body` (the exact bytes sent) at `timestamp` (Unix seconds) with
/// `nonce`, by `key` whose certificate is `certificate_der`.
///
/// # Errors
///
/// When the key cannot sign or a value cannot be a header (neither happens with a P-256 key).
pub fn sign(
    key: &WorkstationKey,
    certificate_der: &[u8],
    method: &Method,
    path_and_query: &str,
    body: &[u8],
    timestamp: i64,
    nonce: &Nonce,
) -> Result<SignedHeaders, Error> {
    let body_sha256: [u8; 32] = Sha256::digest(body).into();
    let input = workstation_signing_input(
        method.as_str(),
        path_and_query,
        timestamp,
        nonce.as_str(),
        &body_sha256,
    );
    // rcgen signs ECDSA keys with ring's ECDSA_P256_SHA256_ASN1_SIGNING: SHA-256 and a DER
    // Ecdsa-Sig-Value, as the certs service verifies it.
    let signature = key
        .key_pair()
        .sign(&input)
        .map_err(|_| Error::Failed("the workstation key could not sign the request".to_owned()))?;
    let header = |name: &'static str, value: String| -> Result<(HeaderName, HeaderValue), Error> {
        let mut value = HeaderValue::try_from(value)
            .map_err(|_| Error::Failed(format!("{name} cannot be a header value")))?;
        value.set_sensitive(true);
        Ok((HeaderName::from_static(name), value))
    };
    Ok(SignedHeaders(vec![
        header(
            WORKSTATION_CERTIFICATE_HEADER,
            STANDARD.encode(certificate_der),
        )?,
        header(WORKSTATION_TIMESTAMP_HEADER, timestamp.to_string())?,
        header(WORKSTATION_NONCE_HEADER, nonce.as_str().to_owned())?,
        header(WORKSTATION_SIGNATURE_HEADER, STANDARD.encode(signature))?,
    ]))
}

#[cfg(test)]
mod tests {
    use x509_parser::prelude::FromDer as _;
    use x509_parser::x509::SubjectPublicKeyInfo;

    use super::*;

    fn value<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> &'a str {
        headers
            .iter()
            .find(|(known, _)| known.as_str() == name)
            .and_then(|(_, value)| value.to_str().ok())
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn nonces_are_22_base64url_characters_and_never_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let nonce = Nonce::generate().expect("random");
            assert_eq!(nonce.as_str().len(), 22);
            assert!(
                nonce
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "{}",
                nonce.as_str()
            );
            assert!(seen.insert(nonce.as_str().to_owned()), "a repeated nonce");
            assert_eq!(format!("{nonce:?}"), "Nonce(<redacted>)");
        }
    }

    #[test]
    fn the_signature_verifies_over_the_certs_services_input() {
        let key = WorkstationKey::generate().expect("a key");
        let certificate = b"not parsed here".to_vec();
        let body = br#"{"hardware_id":"gw-1"}"#;
        let nonce = Nonce::generate().expect("random");
        let path = "/api/certs/v1/gateway-registrations";
        let signed = sign(
            &key,
            &certificate,
            &Method::POST,
            path,
            body,
            1_793_000_000,
            &nonce,
        )
        .expect("signed");
        let shown = format!("{signed:?}");
        assert!(!shown.contains(nonce.as_str()), "{shown}");
        let headers = signed.into_headers();
        assert!(headers.iter().all(|(_, value)| value.is_sensitive()));
        assert_eq!(
            STANDARD
                .decode(value(&headers, WORKSTATION_CERTIFICATE_HEADER))
                .expect("base64"),
            certificate
        );
        assert_eq!(value(&headers, WORKSTATION_TIMESTAMP_HEADER), "1793000000");
        assert_eq!(value(&headers, WORKSTATION_NONCE_HEADER), nonce.as_str());

        let signature = STANDARD
            .decode(value(&headers, WORKSTATION_SIGNATURE_HEADER))
            .expect("base64");
        let public_key = key.public_key_der();
        let (_, spki) = SubjectPublicKeyInfo::from_der(&public_key).expect("an SPKI");
        let point = spki.subject_public_key.data.to_vec();
        let verify = |input: &[u8]| {
            ring_verify(&point, input, &signature);
        };
        let digest: [u8; 32] = Sha256::digest(body).into();
        verify(&workstation_signing_input(
            "POST",
            path,
            1_793_000_000,
            nonce.as_str(),
            &digest,
        ));
    }

    /// Verifies with rustls's ring provider, the same ECDSA P-256 SHA-256 ASN.1 verification the
    /// certs service runs through x509-parser.
    fn ring_verify(point: &[u8], input: &[u8], signature: &[u8]) {
        use rustls::pki_types::alg_id;
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        let verifier = algorithms
            .all
            .iter()
            .find(|algorithm| {
                algorithm.public_key_alg_id() == alg_id::ECDSA_P256
                    && algorithm.signature_alg_id() == alg_id::ECDSA_SHA256
            })
            .expect("ECDSA P-256 SHA-256");
        verifier
            .verify_signature(point, input, signature)
            .expect("the signature verifies");
        let mut other = input.to_vec();
        other.push(b'x');
        assert!(verifier.verify_signature(point, &other, signature).is_err());
    }
}
