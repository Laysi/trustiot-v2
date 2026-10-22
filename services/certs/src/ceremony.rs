//! The offline root's certificates (services/certs/README.md, "CA 階層與私鑰保管"), for the
//! `certs-ca` tool an operator runs on an offline machine. The root key never reaches a cluster:
//! the tool reads it from stdin (decrypted by `age` on that machine) and writes a new one only to
//! stdout (encrypted by `age` there). This module builds certificates from key pairs it is given
//! and never serializes a private key.
//!
//! - The root: ECDSA P-384, self-signed, `CN=<name>` only, basicConstraints `cA=TRUE,
//!   pathLenConstraint=1` (critical), keyUsage keyCertSign and cRLSign (critical), a subject key
//!   identifier, a random serial number.
//! - An issuing CA: the public key of the CSR the certs pod's init container made (ECDSA P-256,
//!   the CSR's signature verified); everything else in the CSR is ignored. `CN=<name>` only,
//!   basicConstraints `cA=TRUE, pathLenConstraint=0` (critical), keyUsage keyCertSign and cRLSign
//!   (critical), subject and authority key identifiers, a random serial number, valid from an
//!   hour before the ceremony (clock skew) for the given days but never past the root.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P384_SHA384, PublicKeyData as _, SerialNumber,
};
use rustls_pki_types::CertificateDer;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

use crate::issue::{self, IssueError, new_serial, timestamp};
use crate::pem;

/// The root key's algorithm.
pub const ROOT_ALGORITHM: &rcgen::SignatureAlgorithm = &PKCS_ECDSA_P384_SHA384;

/// Default validity of the root: 20 years.
pub const DEFAULT_ROOT_DAYS: u32 = 7305;

/// Default validity of an issuing CA: 2 years (rotated before 1 year is left, README).
pub const DEFAULT_ISSUING_DAYS: u32 = 730;

/// Longest validity of either: 30 years.
pub const MAX_DAYS: u32 = 10_958;

/// How far before the ceremony a certificate's validity starts, for cluster clocks that are
/// behind the offline machine's.
pub const BACKDATE_SECS: i64 = 3600;

/// Why a ceremony step failed. Names the problem, never key material.
#[derive(Debug, thiserror::Error)]
pub enum CeremonyError {
    #[error("the common name must be 1 to 64 printable ASCII characters")]
    CommonName,
    #[error("the validity must be 1 to {MAX_DAYS} days")]
    Days,
    #[error("the root key is not an ECDSA P-384 key")]
    RootKey,
    #[error("the root certificate: {0}")]
    RootCertificate(String),
    #[error("the root key does not belong to the root certificate")]
    RootKeyMismatch,
    #[error("the request: {0}")]
    Request(#[from] IssueError),
    #[error("the request is not one PEM certificate signing request")]
    RequestPem,
    #[error("the issuing CA's common name must differ from the root's")]
    SameName,
    #[error("the root certificate expires too soon to sign an issuing CA")]
    RootExpiring,
    #[error("cannot sign: {0}")]
    Sign(String),
    #[error("the certificate: {0}")]
    Certificate(String),
}

fn check_name(common_name: &str) -> Result<(), CeremonyError> {
    let printable = common_name.bytes().all(|byte| (0x20..0x7f).contains(&byte));
    if common_name.trim().is_empty() || common_name.len() > 64 || !printable {
        return Err(CeremonyError::CommonName);
    }
    Ok(())
}

fn check_days(days: u32) -> Result<i64, CeremonyError> {
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(CeremonyError::Days);
    }
    Ok(i64::from(days) * 86_400)
}

fn name(common_name: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    name
}

fn sign_error(error: &rcgen::Error) -> CeremonyError {
    CeremonyError::Sign(error.to_string())
}

/// A new root key ([`ROOT_ALGORITHM`]) from the operating system's CSPRNG.
///
/// # Errors
///
/// When there is no randomness.
pub fn new_root_key() -> Result<KeyPair, CeremonyError> {
    KeyPair::generate_for(ROOT_ALGORITHM).map_err(|error| sign_error(&error))
}

/// The self-signed root certificate of `root_key` named `common_name`, valid from `now` (Unix
/// seconds) for `days`, PEM.
///
/// # Errors
///
/// When the name, the days or the key is unusable, or signing fails.
pub fn root_certificate(
    root_key: &KeyPair,
    common_name: &str,
    days: u32,
    now: i64,
) -> Result<String, CeremonyError> {
    check_name(common_name)?;
    let validity = check_days(days)?;
    if root_key.algorithm() != ROOT_ALGORITHM {
        return Err(CeremonyError::RootKey);
    }
    let mut params = CertificateParams::default();
    params.distinguished_name = name(common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(SerialNumber::from_slice(&new_serial()?));
    params.not_before = timestamp(now.saturating_sub(BACKDATE_SECS))?;
    params.not_after = timestamp(now.saturating_add(validity))?;
    let certificate = params
        .self_signed(root_key)
        .map_err(|error| sign_error(&error))?;
    Ok(certificate.pem())
}

/// The issuing CA certificate for the CSR `request_pem`, named `common_name`, signed by
/// `root_key` as the root of `root_pem`, valid from `now` (Unix seconds) for `days` but never
/// past the root, PEM.
///
/// # Errors
///
/// When the key does not belong to the root certificate, the request is not a P-256 CSR signed
/// by its key, the name or days are unusable, the root expires within a day, or signing fails.
pub fn issuing_certificate(
    root_key: &KeyPair,
    root_pem: &str,
    request_pem: &str,
    common_name: &str,
    days: u32,
    now: i64,
) -> Result<String, CeremonyError> {
    check_name(common_name)?;
    let validity = check_days(days)?;
    if root_key.algorithm() != ROOT_ALGORITHM {
        return Err(CeremonyError::RootKey);
    }
    let mut roots =
        pem::decode_all(root_pem, pem::CERTIFICATE).map_err(CeremonyError::RootCertificate)?;
    if roots.len() != 1 {
        return Err(CeremonyError::RootCertificate(
            "the file must hold exactly one certificate".to_owned(),
        ));
    }
    let root_der = roots.remove(0);
    let (_, root) = X509Certificate::from_der(&root_der)
        .map_err(|_| CeremonyError::RootCertificate("not a certificate".to_owned()))?;
    if root.public_key().raw != root_key.subject_public_key_info().as_slice() {
        return Err(CeremonyError::RootKeyMismatch);
    }
    if root
        .subject()
        .iter_common_name()
        .any(|cn| cn.as_str().ok() == Some(common_name))
    {
        return Err(CeremonyError::SameName);
    }
    let root_not_before = root.validity().not_before.timestamp();
    let root_not_after = root.validity().not_after.timestamp();
    let not_after = now.saturating_add(validity).min(root_not_after);
    if not_after.saturating_sub(now) < 86_400 {
        return Err(CeremonyError::RootExpiring);
    }
    let mut requests = pem::decode_all(request_pem, "CERTIFICATE REQUEST")
        .map_err(|_| CeremonyError::RequestPem)?;
    if requests.len() != 1 {
        return Err(CeremonyError::RequestPem);
    }
    let key = issue::requested_key(&requests.remove(0))?;
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(root_der.as_slice()), root_key)
        .map_err(|error| sign_error(&error))?;
    let mut params = CertificateParams::default();
    params.distinguished_name = name(common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(SerialNumber::from_slice(&new_serial()?));
    params.not_before = timestamp(now.saturating_sub(BACKDATE_SECS).max(root_not_before))?;
    params.not_after = timestamp(not_after)?;
    let certificate = params
        .signed_by(&key, &issuer)
        .map_err(|error| sign_error(&error))?;
    Ok(certificate.pem())
}

/// Checks that `root_key` is the key of the root certificate `root_pem`, for the check of a
/// backup copy of the root key (README.md, 「根 CA 私鑰的備份」), and returns the root's key id
/// (the hex SHA-256 of its public key).
///
/// # Errors
///
/// When the key is not a [`ROOT_ALGORITHM`] key, the text is not exactly one certificate, or the
/// key is another one.
pub fn check_root_key(root_key: &KeyPair, root_pem: &str) -> Result<String, CeremonyError> {
    if root_key.algorithm() != ROOT_ALGORITHM {
        return Err(CeremonyError::RootKey);
    }
    let mut roots =
        pem::decode_all(root_pem, pem::CERTIFICATE).map_err(CeremonyError::RootCertificate)?;
    if roots.len() != 1 {
        return Err(CeremonyError::RootCertificate(
            "the file must hold exactly one certificate".to_owned(),
        ));
    }
    let root_der = roots.remove(0);
    let (_, root) = X509Certificate::from_der(&root_der)
        .map_err(|_| CeremonyError::RootCertificate("not a certificate".to_owned()))?;
    if root.public_key().raw != root_key.subject_public_key_info().as_slice() {
        return Err(CeremonyError::RootKeyMismatch);
    }
    Ok(ca_keys::key_id(root.public_key().raw))
}

/// The key id (`ca_keys::key_id`, the hex SHA-256 of the public key) of the one certificate in
/// `certificate_pem`, which the certs service reports for each key waiting for its certificate:
/// the operator compares the two before setting the certificate, since the request reached the
/// offline machine through a ConfigMap (README.md, 「發簽發 CA 憑證」).
///
/// # Errors
///
/// When the text is not exactly one PEM certificate.
pub fn certificate_key_id(certificate_pem: &str) -> Result<String, CeremonyError> {
    let mut certificates =
        pem::decode_all(certificate_pem, pem::CERTIFICATE).map_err(CeremonyError::Certificate)?;
    if certificates.len() != 1 {
        return Err(CeremonyError::Certificate(
            "the text must hold exactly one certificate".to_owned(),
        ));
    }
    let der = certificates.remove(0);
    let (_, certificate) = X509Certificate::from_der(&der)
        .map_err(|_| CeremonyError::Certificate("not a certificate".to_owned()))?;
    Ok(ca_keys::key_id(certificate.public_key().raw))
}
