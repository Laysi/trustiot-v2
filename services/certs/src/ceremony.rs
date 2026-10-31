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
//! - The root CRL ([`root_crl`]; README.md, 「根 CRL」), which the gateway agent checks the
//!   broker's chain against (edge/agent/src/tls.rs, `RootCrl`): it lists the revoked issuing CA
//!   certificates of this root until [`ROOT_CRL_RETENTION_DAYS`] after each expires. Version 2; signed ecdsa-with-SHA384 without
//!   parameters; the issuer is the root's subject, byte for byte; an authority key identifier
//!   (the root's subject key identifier) and a CRL number, both non-critical, and no other
//!   extension: no issuing distribution point, no delta CRL indicator. thisUpdate an hour
//!   before the ceremony like the certificates (the offline machine's clock is the least
//!   trusted one), nextUpdate [`ROOT_CRL_DAYS`] after it. Each entry has the revocation time
//!   and, unless unspecified, a reason code.
//!
//!   A CRL lists serial numbers only, so the tool cannot tell from the previous root CRL when
//!   an entry may go: the operator gives it the certificates of the previous CRL's entries
//!   too, it drops the ones that expired more than [`ROOT_CRL_RETENTION_DAYS`] before the new
//!   thisUpdate (a gateway clock that runs behind still trusts an expired certificate, and an
//!   offline clock that runs ahead would drop it early), and it refuses an entry
//!   whose certificate is missing. Everything it reads is checked against the root: the
//!   previous CRL's issuer and signature, each certificate's issuer, signature and CA flag.

use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DistinguishedName,
    DnType, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P384_SHA384,
    PublicKeyData as _, RevocationReason, RevokedCertParams, SerialNumber,
};
use rustls_pki_types::CertificateDer;
use sha2::{Digest as _, Sha256};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::oid_registry::{
    OID_SIG_ECDSA_WITH_SHA384, OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER, OID_X509_EXT_CRL_NUMBER,
    OID_X509_EXT_REASON_CODE,
};
use x509_parser::prelude::FromDer;
use x509_parser::revocation_list::CertificateRevocationList;
use x509_parser::x509::{AlgorithmIdentifier, X509Version};

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
    #[error("the root certificate has expired")]
    RootExpired,
    #[error("the previous root CRL: {0}")]
    PreviousCrl(String),
    #[error("the previously revoked certificates: {0}")]
    PreviousCertificates(String),
    #[error("the certificates to revoke: {0}")]
    Revoke(String),
    #[error("the new root CRL: {0}")]
    RootCrl(String),
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

/// The DER of the one certificate in `root_pem`.
fn one_root(root_pem: &str) -> Result<Vec<u8>, CeremonyError> {
    let mut roots =
        pem::decode_all(root_pem, pem::CERTIFICATE).map_err(CeremonyError::RootCertificate)?;
    if roots.len() != 1 {
        return Err(CeremonyError::RootCertificate(
            "the file must hold exactly one certificate".to_owned(),
        ));
    }
    Ok(roots.remove(0))
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
    let root_der = one_root(root_pem)?;
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
    let root_der = one_root(root_pem)?;
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

/// How long a root CRL is valid: nextUpdate lies this many days after thisUpdate. The ceremony
/// happens at least once a year (the issuing CA's yearly rotation), plus a month's margin
/// (README.md, 「根 CRL」).
pub const ROOT_CRL_DAYS: i64 = 400;

/// How long after its certificate expires an entry stays on the root CRL. A gateway whose clock
/// runs behind (or is set back) still finds an expired issuing CA certificate valid, and an
/// offline machine whose clock runs ahead would otherwise drop an entry too early; one root CRL
/// validity covers both mistakes up to more than a year. Entries are small, so keeping them
/// longer costs little (README.md, 「根 CRL」).
pub const ROOT_CRL_RETENTION_DAYS: i64 = ROOT_CRL_DAYS;

/// The largest root CRL, PEM, that the gateway agent reads (`MAX_ROOT_CRL_BYTES` in
/// edge/agent/src/tls.rs): every agent given a larger one refuses to start. An entry takes
/// about 50 bytes of DER, so this holds several hundred revoked issuing CAs.
pub const MAX_ROOT_CRL_BYTES: usize = 64 * 1024;

/// Why a root CRL revokes an issuing CA certificate: its entry's reasonCode (RFC 5280 5.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRevocationReason {
    /// No reason given: the entry has no reasonCode, as RFC 5280 asks instead of the value 0.
    Unspecified,
    /// The issuing CA's private key was exposed, or may have been (cACompromise, 2).
    CaCompromise,
    /// The generation is replaced while its key is safe, for example because the broker's
    /// server key it signed was exposed (superseded, 4).
    Superseded,
    /// The generation is retired before it expires (cessationOfOperation, 5).
    CessationOfOperation,
}

impl RootRevocationReason {
    /// The names [`RootRevocationReason::from_name`] accepts, for messages.
    pub const NAMES: &'static str =
        "ca-compromise, superseded, cessation-of-operation or unspecified";

    /// The reason named `name` (`ca-compromise`, `superseded`, `cessation-of-operation` or
    /// `unspecified`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "unspecified" => Self::Unspecified,
            "ca-compromise" => Self::CaCompromise,
            "superseded" => Self::Superseded,
            "cessation-of-operation" => Self::CessationOfOperation,
            _ => return None,
        })
    }

    /// Its name, as [`RootRevocationReason::from_name`] takes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::CaCompromise => "ca-compromise",
            Self::Superseded => "superseded",
            Self::CessationOfOperation => "cessation-of-operation",
        }
    }

    /// The reason of a reasonCode value this tool writes; others were not made by it. 0 is
    /// never written (the entry has no reasonCode instead) but means the same.
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Unspecified,
            2 => Self::CaCompromise,
            4 => Self::Superseded,
            5 => Self::CessationOfOperation,
            _ => return None,
        })
    }

    fn rcgen(self) -> RevocationReason {
        match self {
            // rcgen leaves the reasonCode out for this one.
            Self::Unspecified => RevocationReason::Unspecified,
            Self::CaCompromise => RevocationReason::CaCompromise,
            Self::Superseded => RevocationReason::Superseded,
            Self::CessationOfOperation => RevocationReason::CessationOfOperation,
        }
    }
}

/// What a root CRL is made from, as the operator brings it to the offline machine. All of it is
/// public: certificates and CRLs, PEM.
#[derive(Clone, Copy, Debug)]
pub struct RootCrlInputs<'a> {
    /// The root certificate.
    pub root_pem: &'a str,
    /// The previous root CRL; `None` only for the first one, which is number 1.
    pub previous_crl_pem: Option<&'a str>,
    /// The issuing CA certificates the previous root CRL lists (PEM blocks, or empty): every one
    /// of its entries needs its certificate here, which tells whether the entry may go.
    /// Certificates here past the retention ([`ROOT_CRL_RETENTION_DAYS`] after notAfter) are
    /// ignored, so an operator may keep one file of every issuing CA certificate ever revoked;
    /// one within it must be listed by the previous root CRL or be revoked now.
    pub previous_certificates_pem: &'a str,
    /// The issuing CA certificates to revoke now (PEM blocks), or `None` to revoke nothing; a
    /// text without a certificate is refused, so an empty file cannot pass for a revocation.
    pub revoke_pem: Option<&'a str>,
    /// The reason of the certificates in `revoke_pem`.
    pub reason: RootRevocationReason,
}

/// An entry of a root CRL, for the operator to read before the CRL goes anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCrlEntry {
    /// The certificate's serial number, lower-case hex.
    pub serial: String,
    /// The certificate's subject (`CN=...`).
    pub subject: String,
    /// The key id of the certificate's public key (`ca_keys::key_id`, as `root sign` prints
    /// it): a root CRL revokes certificates, not keys, so every certificate of a revoked key
    /// must be listed.
    pub key_id: String,
    /// The certificate's notAfter, Unix seconds.
    pub not_after: i64,
    /// The revocation time, Unix seconds: the root CRL's thisUpdate for a new entry, the
    /// previous root CRL's for one it already listed.
    pub revoked_at: i64,
    /// The reason.
    pub reason: RootRevocationReason,
    /// Whether this root CRL revokes it, rather than carrying it over from the previous one.
    pub new: bool,
}

/// A signed root CRL and what it says.
#[derive(Clone, Debug)]
pub struct SignedRootCrl {
    /// One PEM block `X509 CRL`, at most [`MAX_ROOT_CRL_BYTES`].
    pub pem: String,
    /// The CRL number: the previous one's plus one, or 1.
    pub number: u64,
    /// SHA-256 of the DER root CRL, lowercase hex, for the ceremony record.
    pub sha256: String,
    /// SHA-256 of the previous root CRL's DER, lowercase hex; `None` for the first one. The
    /// record of the previous ceremony tells whether it is the CRL that was signed last.
    pub previous_sha256: Option<String>,
    /// thisUpdate, Unix seconds.
    pub this_update: i64,
    /// nextUpdate, Unix seconds: [`ROOT_CRL_DAYS`] after thisUpdate.
    pub next_update: i64,
    /// The entries: the previous root CRL's first, in its order, then the new ones.
    pub listed: Vec<RootCrlEntry>,
    /// The previous root CRL's entries it drops, because their certificates expired more than
    /// [`ROOT_CRL_RETENTION_DAYS`] before thisUpdate.
    pub removed: Vec<RootCrlEntry>,
}

/// An issuing CA certificate of the root, as a root CRL entry needs it.
struct IssuingCertificate {
    serial: Vec<u8>,
    subject: String,
    key_id: String,
    not_after: i64,
}

impl IssuingCertificate {
    /// Whether a root CRL made at `this_update` (Unix seconds) still lists it once revoked.
    fn retained_at(&self, this_update: i64) -> bool {
        self.not_after
            .saturating_add(ROOT_CRL_RETENTION_DAYS * 86_400)
            >= this_update
    }
}

/// An entry of the previous root CRL.
struct PreviousEntry {
    serial: Vec<u8>,
    revoked_at: i64,
    reason: RootRevocationReason,
}

/// Whether `serial`, the content octets of a DER INTEGER, is a positive number in its shortest
/// form: what a CRL entry repeats and webpki compares with a certificate's, byte for byte.
fn canonical_serial(serial: &[u8]) -> bool {
    match serial {
        [] | [0] => false,
        [first, ..] if *first >= 0x80 => false,
        [0, second, ..] => *second >= 0x80,
        _ => true,
    }
}

fn crl_time(seconds: i64) -> Result<time::OffsetDateTime, CeremonyError> {
    timestamp(seconds).map_err(|error| CeremonyError::Sign(error.to_string()))
}

fn is_ecdsa_sha384(algorithm: &AlgorithmIdentifier<'_>) -> bool {
    algorithm.algorithm == OID_SIG_ECDSA_WITH_SHA384 && algorithm.parameters.is_none()
}

/// The issuing CA certificates of `root` (DER `root_der`) in `text`: each issued and signed by
/// the root, a CA, with a canonical serial number, none twice.
fn issuing_certificates(
    text: &str,
    root: &X509Certificate<'_>,
    root_der: &[u8],
) -> Result<Vec<IssuingCertificate>, String> {
    let mut certificates: Vec<IssuingCertificate> = Vec::new();
    for (index, der) in pem::decode_all(text, pem::CERTIFICATE)?
        .into_iter()
        .enumerate()
    {
        let block = index + 1;
        let (rest, certificate) = X509Certificate::from_der(&der)
            .map_err(|_| format!("block {block} is not a certificate"))?;
        if !rest.is_empty() {
            return Err(format!("block {block} has bytes after the certificate"));
        }
        let subject = certificate.subject().to_string();
        if der == root_der || certificate.subject().as_raw() == root.subject().as_raw() {
            return Err(format!(
                "block {block} ({subject}) is the root itself; a root CRL revokes issuing CAs"
            ));
        }
        if certificate.issuer().as_raw() != root.subject().as_raw() {
            return Err(format!(
                "block {block} ({subject}) was not issued by this root (its issuer is {})",
                certificate.issuer()
            ));
        }
        if certificate
            .verify_signature(Some(root.public_key()))
            .is_err()
        {
            return Err(format!(
                "block {block} ({subject}) is not signed by this root's key"
            ));
        }
        let ca = matches!(
            certificate.basic_constraints(),
            Ok(Some(constraints)) if constraints.value.ca
        );
        if !ca {
            return Err(format!("block {block} ({subject}) is not a CA certificate"));
        }
        let serial = certificate.raw_serial();
        if !canonical_serial(serial) {
            return Err(format!(
                "block {block} ({subject}) has a serial number that is not a positive DER \
                 integer in its shortest form"
            ));
        }
        if certificates.iter().any(|seen| seen.serial == serial) {
            return Err(format!(
                "block {block} ({subject}) repeats serial number {}",
                issue::hex(serial)
            ));
        }
        certificates.push(IssuingCertificate {
            serial: serial.to_vec(),
            subject,
            key_id: ca_keys::key_id(certificate.public_key().raw),
            not_after: certificate.validity().not_after.timestamp(),
        });
    }
    Ok(certificates)
}

/// The next CRL number and the entries after the previous root CRL `text`, which `root` must
/// have issued and signed before `now` (Unix seconds).
fn previous_crl(
    text: &str,
    root: &X509Certificate<'_>,
    now: i64,
) -> Result<(u64, Vec<PreviousEntry>, String), String> {
    let mut crls = pem::decode_all(text, pem::CRL)?;
    if crls.len() != 1 {
        return Err(format!(
            "the file holds {} PEM blocks \"X509 CRL\", not one",
            crls.len()
        ));
    }
    let der = crls.remove(0);
    let (rest, crl) =
        CertificateRevocationList::from_der(&der).map_err(|_| "not a CRL".to_owned())?;
    if !rest.is_empty() {
        return Err("bytes after the CRL".to_owned());
    }
    if crl.issuer().as_raw() != root.subject().as_raw() {
        return Err(format!(
            "not issued by this root (its issuer is {})",
            crl.issuer()
        ));
    }
    crl.verify_signature(root.public_key())
        .map_err(|_| "not signed by this root's key".to_owned())?;
    // Only what this tool writes: an extension it does not know would be lost.
    for extension in crl.extensions() {
        if extension.oid != OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER
            && extension.oid != OID_X509_EXT_CRL_NUMBER
        {
            return Err(format!(
                "it has the extension {}, which this tool never writes",
                extension.oid
            ));
        }
    }
    let number = crl
        .crl_number()
        .ok_or_else(|| "it has no CRL number".to_owned())?;
    let next = u64::try_from(number)
        .ok()
        .and_then(|number| number.checked_add(1))
        .ok_or_else(|| format!("its CRL number {number} is too large"))?;
    if crl.last_update().timestamp() > now {
        return Err(
            "its thisUpdate lies after this machine's time; check the clock, then start over"
                .to_owned(),
        );
    }
    let mut entries: Vec<PreviousEntry> = Vec::new();
    for revoked in crl.iter_revoked_certificates() {
        let serial = revoked.raw_serial();
        let shown = issue::hex(serial);
        if !canonical_serial(serial) {
            return Err(format!(
                "entry {shown} is not a positive DER integer in its shortest form"
            ));
        }
        if entries.iter().any(|seen| seen.serial == serial) {
            return Err(format!("it lists serial number {shown} twice"));
        }
        let mut reason = RootRevocationReason::Unspecified;
        for extension in revoked.extensions() {
            match extension.parsed_extension() {
                ParsedExtension::ReasonCode(code)
                    if extension.oid == OID_X509_EXT_REASON_CODE && !extension.critical =>
                {
                    reason = RootRevocationReason::from_code(code.0).ok_or_else(|| {
                        format!(
                            "entry {shown} has the reason code {}, which this tool never writes",
                            code.0
                        )
                    })?;
                }
                _ => {
                    return Err(format!(
                        "entry {shown} has the extension {}, which this tool never writes",
                        extension.oid
                    ));
                }
            }
        }
        entries.push(PreviousEntry {
            serial: serial.to_vec(),
            revoked_at: revoked.revocation_date.timestamp(),
            reason,
        });
    }
    Ok((next, entries, issue::hex(&Sha256::digest(&der))))
}

/// The subject key identifier of `root`, which a root CRL's authority key identifier repeats.
fn subject_key_id(root: &X509Certificate<'_>) -> Result<Vec<u8>, CeremonyError> {
    root.extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
        .ok_or_else(|| {
            CeremonyError::RootCertificate("it has no subject key identifier".to_owned())
        })
}

/// Checks the root CRL `der` that [`root_crl`] just signed against what the gateway agent and
/// webpki require (README.md, 「根 CRL」, 「格式」), so that a change in how rcgen encodes it
/// fails here, on the offline machine, rather than on every gateway.
fn check_signed(
    der: &[u8],
    root: &X509Certificate<'_>,
    root_key_id: &[u8],
    number: u64,
    entries: usize,
) -> Result<(), String> {
    let (rest, crl) =
        CertificateRevocationList::from_der(der).map_err(|_| "cannot be parsed".to_owned())?;
    if !rest.is_empty() {
        return Err("has bytes after the CRL".to_owned());
    }
    if crl.version() != Some(X509Version::V2) {
        return Err("is not version 2".to_owned());
    }
    if !is_ecdsa_sha384(&crl.signature_algorithm) || !is_ecdsa_sha384(&crl.tbs_cert_list.signature)
    {
        return Err("is not signed ecdsa-with-SHA384 without parameters".to_owned());
    }
    if crl.issuer().as_raw() != root.subject().as_raw() {
        return Err("names its issuer differently from the root's subject".to_owned());
    }
    crl.verify_signature(root.public_key())
        .map_err(|_| "does not verify with the root's key".to_owned())?;
    let (mut key_id, mut crl_number) = (false, false);
    for extension in crl.extensions() {
        if extension.critical {
            return Err(format!("has the critical extension {}", extension.oid));
        }
        match extension.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(id)
                if !key_id
                    && id.key_identifier.as_ref().map(|id| id.0) == Some(root_key_id)
                    && id.authority_cert_issuer.is_none()
                    && id.authority_cert_serial.is_none() =>
            {
                key_id = true;
            }
            ParsedExtension::CRLNumber(value)
                if !crl_number && u64::try_from(value).ok() == Some(number) =>
            {
                crl_number = true;
            }
            _ => return Err(format!("has the unexpected extension {}", extension.oid)),
        }
    }
    if !key_id || !crl_number {
        return Err("lacks the authority key identifier or the CRL number".to_owned());
    }
    if crl.next_update().is_none() {
        return Err("has no nextUpdate".to_owned());
    }
    if crl.iter_revoked_certificates().count() != entries {
        return Err("does not list every entry".to_owned());
    }
    Ok(())
}

/// Signs the next root CRL of the root in `inputs.root_pem` with `root_key` at `now` (Unix
/// seconds): the previous root CRL's entries whose certificates expired at most
/// [`ROOT_CRL_RETENTION_DAYS`] before the new thisUpdate (or not at all), plus the certificates
/// in `inputs.revoke_pem`, numbered one more than the previous root CRL (1 for the first). See
/// the module documentation for the format.
///
/// # Errors
///
/// When the key is not the root's, the root has expired, the previous root CRL is not this
/// root's or not one this tool made, an entry of it has no certificate in
/// `inputs.previous_certificates_pem`, a certificate there is within the retention although
/// the previous root CRL does not list it and it is not revoked now, a certificate is not an
/// issuing CA of this root, `inputs.revoke_pem` holds no certificate, a certificate to revoke
/// is already revoked or expired more than [`ROOT_CRL_RETENTION_DAYS`] ago, or the result
/// would not be what the gateway agent accepts (its size included).
pub fn root_crl(
    root_key: &KeyPair,
    inputs: &RootCrlInputs<'_>,
    now: i64,
) -> Result<SignedRootCrl, CeremonyError> {
    if root_key.algorithm() != ROOT_ALGORITHM {
        return Err(CeremonyError::RootKey);
    }
    let root_der = one_root(inputs.root_pem)?;
    let (_, root) = X509Certificate::from_der(&root_der)
        .map_err(|_| CeremonyError::RootCertificate("not a certificate".to_owned()))?;
    if root.public_key().raw != root_key.subject_public_key_info().as_slice() {
        return Err(CeremonyError::RootKeyMismatch);
    }
    if root.validity().not_after.timestamp() <= now {
        return Err(CeremonyError::RootExpired);
    }
    let root_key_id = subject_key_id(&root)?;
    let this_update = now
        .saturating_sub(BACKDATE_SECS)
        .max(root.validity().not_before.timestamp());
    let next_update = this_update.saturating_add(ROOT_CRL_DAYS * 86_400);

    let (number, previous, previous_sha256) = match inputs.previous_crl_pem {
        None => (1, Vec::new(), None),
        Some(text) => {
            let (number, previous, sha256) =
                previous_crl(text, &root, now).map_err(CeremonyError::PreviousCrl)?;
            (number, previous, Some(sha256))
        }
    };
    let previous_certificates =
        issuing_certificates(inputs.previous_certificates_pem, &root, &root_der)
            .map_err(CeremonyError::PreviousCertificates)?;
    let revoke = match inputs.revoke_pem {
        None => Vec::new(),
        Some(text) => {
            let revoke =
                issuing_certificates(text, &root, &root_der).map_err(CeremonyError::Revoke)?;
            if revoke.is_empty() {
                return Err(CeremonyError::Revoke(
                    "it holds no certificate; leave the certificates to revoke out instead"
                        .to_owned(),
                ));
            }
            revoke
        }
    };

    // Each entry with its serial number's DER content octets.
    let mut listed: Vec<(&[u8], RootCrlEntry)> = Vec::new();
    let mut removed = Vec::new();
    for entry in &previous {
        let certificate = previous_certificates
            .iter()
            .find(|certificate| certificate.serial == entry.serial)
            .ok_or_else(|| {
                CeremonyError::PreviousCertificates(format!(
                    "the previous root CRL lists serial number {}, but its certificate is not \
                     among them, and only the certificate tells when the entry may go",
                    issue::hex(&entry.serial)
                ))
            })?;
        let shown = RootCrlEntry {
            serial: issue::hex(&entry.serial),
            subject: certificate.subject.clone(),
            key_id: certificate.key_id.clone(),
            not_after: certificate.not_after,
            revoked_at: entry.revoked_at,
            reason: entry.reason,
            new: false,
        };
        if certificate.retained_at(this_update) {
            listed.push((&entry.serial, shown));
        } else {
            removed.push(shown);
        }
    }
    for certificate in &previous_certificates {
        let in_previous = previous
            .iter()
            .any(|entry| entry.serial == certificate.serial);
        let revoked_now = revoke
            .iter()
            .any(|other| other.serial == certificate.serial);
        if !in_previous && !revoked_now && certificate.retained_at(this_update) {
            return Err(CeremonyError::PreviousCertificates(format!(
                "{} (serial number {}) would still be listed, but the previous root CRL does not \
                 list it; to revoke it now, give it as a certificate to revoke",
                certificate.subject,
                issue::hex(&certificate.serial)
            )));
        }
    }
    for certificate in &revoke {
        let shown = issue::hex(&certificate.serial);
        if previous
            .iter()
            .any(|entry| entry.serial == certificate.serial)
        {
            return Err(CeremonyError::Revoke(format!(
                "{} (serial number {shown}) is already revoked by the previous root CRL",
                certificate.subject
            )));
        }
        if !certificate.retained_at(this_update) {
            return Err(CeremonyError::Revoke(format!(
                "{} (serial number {shown}) expired more than {ROOT_CRL_RETENTION_DAYS} days ago",
                certificate.subject
            )));
        }
        listed.push((
            &certificate.serial,
            RootCrlEntry {
                serial: shown,
                subject: certificate.subject.clone(),
                key_id: certificate.key_id.clone(),
                not_after: certificate.not_after,
                revoked_at: this_update,
                reason: inputs.reason,
                new: true,
            },
        ));
    }

    let mut revoked_certs = Vec::with_capacity(listed.len());
    for (serial, entry) in &listed {
        revoked_certs.push(RevokedCertParams {
            serial_number: SerialNumber::from_slice(serial),
            revocation_time: crl_time(entry.revoked_at)?,
            reason_code: Some(entry.reason.rcgen()),
            invalidity_date: None,
        });
    }
    let params = CertificateRevocationListParams {
        this_update: crl_time(this_update)?,
        next_update: crl_time(next_update)?,
        crl_number: SerialNumber::from(number),
        issuing_distribution_point: None,
        revoked_certs,
        key_identifier_method: KeyIdMethod::PreSpecified(root_key_id.clone()),
    };
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(root_der.as_slice()), root_key)
        .map_err(|error| sign_error(&error))?;
    let der = params
        .signed_by(&issuer)
        .map_err(|error| sign_error(&error))?
        .der()
        .to_vec();
    check_signed(&der, &root, &root_key_id, number, listed.len())
        .map_err(CeremonyError::RootCrl)?;
    let pem = pem::encode(pem::CRL, &der);
    if pem.len() > MAX_ROOT_CRL_BYTES {
        return Err(CeremonyError::RootCrl(format!(
            "it is {} bytes, more than the {MAX_ROOT_CRL_BYTES} the gateway agent reads",
            pem.len()
        )));
    }
    let sha256 = issue::hex(&Sha256::digest(&der));
    Ok(SignedRootCrl {
        pem,
        number,
        sha256,
        previous_sha256,
        this_update,
        next_update,
        listed: listed.into_iter().map(|(_, entry)| entry).collect(),
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_serial_number_must_be_positive_and_shortest() {
        assert!(canonical_serial(&[0x01]));
        assert!(canonical_serial(&[0x40, 0x00]));
        assert!(canonical_serial(&[0x00, 0x80]));
        assert!(!canonical_serial(&[]));
        assert!(!canonical_serial(&[0x00]));
        assert!(!canonical_serial(&[0x80]));
        assert!(!canonical_serial(&[0x00, 0x7f]));
    }

    #[test]
    fn reason_names_and_codes_round_trip() {
        // RFC 5280 5.3.1 CRLReason values.
        for (reason, code) in [
            (RootRevocationReason::Unspecified, 0),
            (RootRevocationReason::CaCompromise, 2),
            (RootRevocationReason::Superseded, 4),
            (RootRevocationReason::CessationOfOperation, 5),
        ] {
            assert_eq!(RootRevocationReason::from_name(reason.name()), Some(reason));
            assert_eq!(RootRevocationReason::from_code(code), Some(reason));
            assert_eq!(i64::from(code), reason.rcgen() as i64);
        }
        assert_eq!(RootRevocationReason::from_name("key-compromise"), None);
        assert_eq!(RootRevocationReason::from_code(1), None);
    }
}
