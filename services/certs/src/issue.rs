//! Gateway client certificates from certificate signing requests (decisions 6 and 12;
//! Laysi/trustiot-v2#78 acceptance 1).
//!
//! The CA takes exactly one thing from a CSR: its public key, after checking that the CSR is
//! signed by the matching private key (proof of possession) and that the key is ECDSA P-256, the
//! curve of the gateway's TPM. Everything else in the CSR (subject, subject alternative names,
//! key usages, extended key usages, basic constraints, any other requested extension or
//! attribute) is ignored: the profile below decides the certificate.
//!
//! The gateway profile: subject `CN=<gateway id>` only; basicConstraints `cA=FALSE` (critical);
//! keyUsage digitalSignature (critical); extendedKeyUsage clientAuth only; subject and authority
//! key identifiers; a random positive 128-bit serial number; valid from five minutes before
//! signing (clock skew) for `gateway.validity`, never past the issuing CA certificate. No
//! subject alternative name and no CRL distribution point: Mosquitto reads the CRLs from a file
//! (decision 6), and the gateway id is the MQTT identity (`use_identity_as_username`).
//!
//! Who may get a certificate for which gateway id is the caller's decision: the factory
//! registration (Laysi/trustiot-v2#79) and the renewal endpoint (Laysi/trustiot-v2#81).

use std::time::Duration;

use protocol::GatewayId;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, SerialNumber, SubjectPublicKeyInfo,
};
use sqlx::PgConnection;
use time::OffsetDateTime;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::FromDer;

use crate::authority::IssuingCa;

/// Largest CSR accepted, DER. A P-256 CSR is about 250 bytes; the limit leaves room for
/// attributes a client adds, which are ignored.
pub const MAX_REQUEST_BYTES: usize = 4096;

/// How far before signing a certificate's validity starts, for clocks that are behind.
pub const CLOCK_SKEW_SECS: i64 = 300;

/// Fewest seconds a new certificate must be valid for; less means the issuing CA certificate
/// is about to expire and a newer issuing CA should sign.
pub const MIN_REMAINING_SECS: i64 = 3600;

/// Octets of a serial number: 128 random bits, the first octet's top bit cleared (positive) and
/// its next bit set (always 16 octets in DER).
pub const SERIAL_BYTES: usize = 16;

/// Why no certificate was issued. Names the problem, never the request's content.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("the request is larger than {MAX_REQUEST_BYTES} bytes")]
    TooLarge,
    #[error("the request is not one DER certificate signing request")]
    NotARequest,
    #[error("the request is not signed with ecdsa-with-SHA256")]
    UnsupportedSignature,
    #[error("the request's public key is not ECDSA P-256")]
    UnsupportedKey,
    #[error("the request's signature does not verify with its public key")]
    BadSignature,
    #[error(
        "the issuing CA certificate expires in less than {MIN_REMAINING_SECS} seconds; a newer \
         issuing CA must sign"
    )]
    IssuingCaExpiring,
    #[error("no randomness for the serial number")]
    Random,
    #[error("cannot sign: {0}")]
    Sign(String),
}

/// A certificate the issuing CA signed.
#[derive(Clone, Debug)]
pub struct GatewayCertificate {
    /// The serial number's content octets ([`SERIAL_BYTES`]).
    pub serial: Vec<u8>,
    /// The key id of the issuing CA that signed it.
    pub ca_key_id: String,
    /// The gateway id, its common name.
    pub gateway: GatewayId,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
    /// The certificate, DER.
    pub der: Vec<u8>,
}

/// The public key of `request` (DER), once it is shown to be a P-256 key whose private key
/// signed the request with ecdsa-with-SHA256. Nothing else is read from the request.
///
/// # Errors
///
/// When the request is too large, not exactly one CSR, signed with another algorithm, for
/// another kind of key, or its signature does not verify.
pub fn requested_key(request: &[u8]) -> Result<SubjectPublicKeyInfo, IssueError> {
    if request.len() > MAX_REQUEST_BYTES {
        return Err(IssueError::TooLarge);
    }
    let (rest, parsed) =
        X509CertificationRequest::from_der(request).map_err(|_| IssueError::NotARequest)?;
    if !rest.is_empty() {
        return Err(IssueError::NotARequest);
    }
    let algorithm = &parsed.signature_algorithm;
    if algorithm.algorithm != OID_SIG_ECDSA_WITH_SHA256 || algorithm.parameters.is_some() {
        return Err(IssueError::UnsupportedSignature);
    }
    let spki = &parsed.certification_request_info.subject_pki;
    let key = SubjectPublicKeyInfo::from_der(spki.raw).map_err(|_| IssueError::UnsupportedKey)?;
    if rcgen::PublicKeyData::algorithm(&key) != &PKCS_ECDSA_P256_SHA256 {
        return Err(IssueError::UnsupportedKey);
    }
    // ring parses the point (on the curve, uncompressed) and checks the signature.
    parsed
        .verify_signature()
        .map_err(|_| IssueError::BadSignature)?;
    Ok(key)
}

/// A new random serial number ([`SERIAL_BYTES`] octets).
///
/// # Errors
///
/// When the operating system has no randomness.
pub fn new_serial() -> Result<Vec<u8>, IssueError> {
    let mut serial = vec![0_u8; SERIAL_BYTES];
    getrandom::fill(&mut serial).map_err(|_| IssueError::Random)?;
    serial[0] = (serial[0] & 0x7f) | 0x40;
    Ok(serial)
}

/// Signs a gateway certificate for `gateway` with `ca` at `now` (Unix seconds), valid for
/// `validity`, for the public key of `request` (DER). See the module documentation for the
/// profile.
///
/// # Errors
///
/// When the request is refused ([`requested_key`]), the issuing CA certificate expires within
/// [`MIN_REMAINING_SECS`], or signing fails.
pub fn gateway_certificate(
    ca: &IssuingCa,
    request: &[u8],
    gateway: &GatewayId,
    validity: Duration,
    now: i64,
) -> Result<GatewayCertificate, IssueError> {
    let key = requested_key(request)?;
    let validity = i64::try_from(validity.as_secs()).unwrap_or(i64::MAX);
    let not_before = now.saturating_sub(CLOCK_SKEW_SECS).max(ca.not_before());
    let not_after = now.saturating_add(validity).min(ca.not_after());
    if not_after.saturating_sub(now) < MIN_REMAINING_SECS {
        return Err(IssueError::IssuingCaExpiring);
    }
    let serial = new_serial()?;
    let mut params = CertificateParams::default();
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, gateway.as_str());
    params.distinguished_name = subject;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.not_before = timestamp(not_before)?;
    params.not_after = timestamp(not_after)?;
    let certificate = params
        .signed_by(&key, ca.issuer())
        .map_err(|error| IssueError::Sign(error.to_string()))?;
    Ok(GatewayCertificate {
        serial,
        ca_key_id: ca.key_id().to_owned(),
        gateway: gateway.clone(),
        not_before,
        not_after,
        der: certificate.der().to_vec(),
    })
}

pub(crate) fn timestamp(seconds: i64) -> Result<OffsetDateTime, IssueError> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| IssueError::Sign("a time out of range".to_owned()))
}

/// Records `certificate` in the ledger (`certs.issued_certificates`) on `connection`, usually in
/// the caller's transaction, so a certificate is handed out only once it is recorded.
///
/// # Errors
///
/// When the database fails, or the serial number is already recorded (128 random bits: a
/// sign of a broken random source).
pub async fn record(
    connection: &mut PgConnection,
    certificate: &GatewayCertificate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO certs.issued_certificates
             (serial, ca_key_id, profile, subject, not_before, not_after, certificate)
         VALUES ($1, $2, 'gateway', $3, $4, $5, $6)",
    )
    .bind(&certificate.serial)
    .bind(&certificate.ca_key_id)
    .bind(certificate.gateway.as_str())
    .bind(utc(certificate.not_before))
    .bind(utc(certificate.not_after))
    .bind(&certificate.der)
    .execute(connection)
    .await?;
    tracing::info!(
        serial = %hex(&certificate.serial),
        ca_key_id = %certificate.ca_key_id,
        gateway_id = %certificate.gateway,
        not_after = certificate.not_after,
        "recorded an issued gateway certificate"
    );
    Ok(())
}

/// `seconds` as a UTC time for Postgres; out-of-range times clamp to the epoch.
pub(crate) fn utc(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

/// Lower-case hex, for serial numbers in logs.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials_are_positive_sixteen_octets_and_never_repeat() {
        let first = new_serial().expect("random");
        let second = new_serial().expect("random");
        assert_ne!(first, second);
        for serial in [first, second] {
            assert_eq!(serial.len(), SERIAL_BYTES);
            assert_eq!(serial[0] & 0xc0, 0x40, "{}", hex(&serial));
        }
    }

    #[test]
    fn garbage_is_not_a_request() {
        assert!(matches!(
            requested_key(&[0x30, 0x03, 0x02, 0x01, 0x00]),
            Err(IssueError::NotARequest)
        ));
        assert!(matches!(
            requested_key(&vec![0; MAX_REQUEST_BYTES + 1]),
            Err(IssueError::TooLarge)
        ));
    }
}
