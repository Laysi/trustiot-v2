//! Client certificates from certificate signing requests (decisions 6 and 12;
//! Laysi/trustiot-v2#78 acceptance 1, Laysi/trustiot-v2#534).
//!
//! The CA takes exactly one thing from a CSR: its public key, after checking that the CSR is
//! signed by the matching private key (proof of possession) and that the key is ECDSA P-256, the
//! curve of the gateway's TPM. Everything else in the CSR (subject, subject alternative names,
//! key usages, extended key usages, basic constraints, any other requested extension or
//! attribute) is ignored: the profile decides the certificate.
//!
//! Two profiles, signed by the same issuing CA and told apart by the extended key usage
//! (services/certs/README.md, "閘道器憑證" and "工作站憑證"):
//!
//! - [`Profile::Gateway`] ([`gateway_certificate`]): subject `CN=<gateway id>` only;
//!   extendedKeyUsage clientAuth only; valid for `gateway.validity`. The gateway id is the MQTT
//!   identity (`use_identity_as_username`).
//! - [`Profile::Workstation`] ([`workstation_certificate`]): subject `CN=workstation:<name>`
//!   only, which is never a gateway id (`:` is not in a gateway id); extendedKeyUsage
//!   [`WORKSTATION_PURPOSE`] only and never clientAuth, so a TLS server that authenticates
//!   clients, the MQTT broker among them, refuses it; valid for `workstation.validity`.
//!
//! Both: basicConstraints `cA=FALSE` (critical); keyUsage digitalSignature (critical); subject
//! and authority key identifiers; a random positive 128-bit serial number; valid from five
//! minutes before signing (clock skew), never past the issuing CA certificate. No subject
//! alternative name and no CRL distribution point: Mosquitto reads the CRLs from a file
//! (decision 6).
//!
//! Who may get a certificate for which gateway id is the caller's decision: the factory
//! registration (Laysi/trustiot-v2#79) and the renewal endpoint (Laysi/trustiot-v2#81).
//! Workstation certificates are issued only through the platform administrators' API
//! ([`crate::workstations`]). [`record`] puts a certificate in the ledger only when its issuing
//! CA already has a CRL, so nothing is handed out that the broker could not check.

use std::time::Duration;

use protocol::GatewayId;
use rcgen::{
    CertificateParams, CustomExtension, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SerialNumber, SubjectPublicKeyInfo,
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

/// The extended key usage of workstation certificates, "TrustIoT production workstation": an
/// OID under the UUID arc 2.25 (ITU-T X.667; UUID `edc7e7d8-ab9e-4c72-a01b-aa39eb83e905`), which
/// needs no registration. Neither clientAuth, serverAuth nor anyExtendedKeyUsage, so no TLS peer
/// accepts a workstation certificate; the registration API (Laysi/trustiot-v2#79) requires it.
pub const WORKSTATION_PURPOSE: &str = "2.25.316065004478888347166512336770143873285";

/// [`WORKSTATION_PURPOSE`] as DER content octets (no tag and length), the form verifiers
/// compare: the first two arcs as 2 × 40 + 25 = 105, then the UUID's 128 bits in base 128.
pub const WORKSTATION_PURPOSE_DER: [u8; 20] = [
    0x69, 0x83, 0xdb, 0xc7, 0xf3, 0xf6, 0x95, 0xb9, 0xf2, 0xb1, 0xe5, 0xa0, 0x8d, 0xea, 0xc7, 0x9e,
    0xdc, 0x8f, 0xd2, 0x05,
];

/// The extendedKeyUsage extension (RFC 5280 section 4.2.1.12).
const EXTENDED_KEY_USAGE_OID: [u64; 4] = [2, 5, 29, 37];

/// What every workstation certificate's common name starts with.
pub const WORKSTATION_SUBJECT_PREFIX: &str = "workstation:";

/// Longest workstation name, in bytes: with [`WORKSTATION_SUBJECT_PREFIX`] the common name stays
/// within the ledger's 64 characters.
pub const MAX_WORKSTATION_NAME_LEN: usize = 48;

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

/// The certificate profiles of the CA: which subject and extended key usage it gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// A gateway's MQTT client certificate.
    Gateway,
    /// A production workstation's certificate, which no TLS peer accepts.
    Workstation,
}

impl Profile {
    /// The profile's name in the ledger (`certs.issued_certificates.profile`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Workstation => "workstation",
        }
    }
}

/// A production workstation's name, as a platform administrator gives it: 1 to
/// [`MAX_WORKSTATION_NAME_LEN`] ASCII letters, digits, `.`, `_` or `-`, starting with a letter
/// or digit (the characters of a gateway id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkstationName(String);

impl WorkstationName {
    /// `name`, when it follows the rule.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        let valid = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.len() <= MAX_WORKSTATION_NAME_LEN
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        valid.then(|| Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The common name of its certificates: `workstation:<name>`.
    #[must_use]
    pub fn subject(&self) -> String {
        format!("{WORKSTATION_SUBJECT_PREFIX}{}", self.0)
    }
}

/// A certificate the issuing CA signed.
#[derive(Clone, Debug)]
pub struct IssuedCertificate {
    /// The serial number's content octets ([`SERIAL_BYTES`]).
    pub serial: Vec<u8>,
    /// The key id of the issuing CA that signed it.
    pub ca_key_id: String,
    /// The profile that decided it.
    pub profile: Profile,
    /// Its common name: the gateway id, or `workstation:<name>`.
    pub subject: String,
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
) -> Result<IssuedCertificate, IssueError> {
    sign(
        ca,
        request,
        Profile::Gateway,
        gateway.as_str().to_owned(),
        validity,
        now,
    )
}

/// Signs a workstation certificate for `workstation` with `ca` at `now` (Unix seconds), valid
/// for `validity`, for the public key of `request` (DER). See the module documentation for the
/// profile.
///
/// # Errors
///
/// As [`gateway_certificate`].
pub fn workstation_certificate(
    ca: &IssuingCa,
    request: &[u8],
    workstation: &WorkstationName,
    validity: Duration,
    now: i64,
) -> Result<IssuedCertificate, IssueError> {
    sign(
        ca,
        request,
        Profile::Workstation,
        workstation.subject(),
        validity,
        now,
    )
}

fn sign(
    ca: &IssuingCa,
    request: &[u8],
    profile: Profile,
    subject: String,
    validity: Duration,
    now: i64,
) -> Result<IssuedCertificate, IssueError> {
    let key = requested_key(request)?;
    let validity = i64::try_from(validity.as_secs()).unwrap_or(i64::MAX);
    let not_before = now.saturating_sub(CLOCK_SKEW_SECS).max(ca.not_before());
    let not_after = now.saturating_add(validity).min(ca.not_after());
    if not_after.saturating_sub(now) < MIN_REMAINING_SECS {
        return Err(IssueError::IssuingCaExpiring);
    }
    let serial = new_serial()?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, subject.as_str());
    params.distinguished_name = name;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    match profile {
        Profile::Gateway => {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        }
        // rcgen's purposes take arcs of 64 bits; the UUID arc has 128, so the extension is
        // written here. rcgen adds no extendedKeyUsage of its own while its list is empty.
        Profile::Workstation => {
            params.custom_extensions = vec![CustomExtension::from_oid_content(
                &EXTENDED_KEY_USAGE_OID,
                workstation_extended_key_usage(),
            )];
        }
    }
    params.use_authority_key_identifier_extension = true;
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.not_before = timestamp(not_before)?;
    params.not_after = timestamp(not_after)?;
    let certificate = params
        .signed_by(&key, ca.issuer())
        .map_err(|error| IssueError::Sign(error.to_string()))?;
    Ok(IssuedCertificate {
        serial,
        ca_key_id: ca.key_id().to_owned(),
        profile,
        subject,
        not_before,
        not_after,
        der: certificate.der().to_vec(),
    })
}

/// The workstation profile's extendedKeyUsage value, DER: `SEQUENCE { OBJECT IDENTIFIER
/// WORKSTATION_PURPOSE }`. Both lengths fit in one octet.
fn workstation_extended_key_usage() -> Vec<u8> {
    const OID_LEN: u8 = 20;
    const _: () = assert!(WORKSTATION_PURPOSE_DER.len() == OID_LEN as usize);
    let mut der = vec![0x30, OID_LEN + 2, 0x06, OID_LEN];
    der.extend_from_slice(&WORKSTATION_PURPOSE_DER);
    der
}

pub(crate) fn timestamp(seconds: i64) -> Result<OffsetDateTime, IssueError> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| IssueError::Sign("a time out of range".to_owned()))
}

/// Why a certificate was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The issuing CA that signed it has no CRL yet: the broker could not check the certificate
    /// (services/certs/README.md, "發簽發 CA 憑證" step 5). Hand it out only once recorded.
    #[error("the issuing CA {0} has no CRL yet; nothing was recorded")]
    NoCrl(String),
    /// The database failed, or the serial number is already recorded.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

/// Records `certificate` in the ledger (`certs.issued_certificates`) on `connection`, usually in
/// the caller's transaction, so a certificate is handed out only once it is recorded. The row is
/// written only when the certificate's issuing CA has a CRL in `certs.crls` (CRLs are never
/// removed), so every recorded certificate can be checked against one.
///
/// # Errors
///
/// [`RecordError::NoCrl`] when the issuing CA has no CRL yet; [`RecordError::Database`] when
/// the database fails, or the serial number is already recorded (128 random bits: a sign of a
/// broken random source).
pub async fn record(
    connection: &mut PgConnection,
    certificate: &IssuedCertificate,
) -> Result<(), RecordError> {
    let inserted = sqlx::query(
        "INSERT INTO certs.issued_certificates
             (serial, ca_key_id, profile, subject, not_before, not_after, certificate)
         SELECT $1, $2, $3, $4, $5, $6, $7
         WHERE EXISTS (SELECT 1 FROM certs.crls WHERE ca_key_id = $2)",
    )
    .bind(&certificate.serial)
    .bind(&certificate.ca_key_id)
    .bind(certificate.profile.as_str())
    .bind(&certificate.subject)
    .bind(utc(certificate.not_before))
    .bind(utc(certificate.not_after))
    .bind(&certificate.der)
    .execute(connection)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(RecordError::NoCrl(certificate.ca_key_id.clone()));
    }
    tracing::info!(
        serial = %hex(&certificate.serial),
        ca_key_id = %certificate.ca_key_id,
        profile = certificate.profile.as_str(),
        subject = %certificate.subject,
        not_after = certificate.not_after,
        "recorded an issued certificate"
    );
    Ok(())
}

/// `seconds` as a UTC time for Postgres; out-of-range times clamp to the epoch.
pub(crate) fn utc(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

/// Lower-case hex, for serial numbers in logs and the API.
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

    /// The content octets of the dotted OID `oid`, computed independently of the constant.
    fn encode_oid(oid: &str) -> Vec<u8> {
        let arcs: Vec<u128> = oid.split('.').map(|arc| arc.parse().unwrap()).collect();
        let mut out = Vec::new();
        for arc in std::iter::once(arcs[0] * 40 + arcs[1]).chain(arcs[2..].iter().copied()) {
            let mut groups = vec![u8::try_from(arc & 0x7f).unwrap()];
            let mut rest = arc >> 7;
            while rest > 0 {
                groups.push(u8::try_from(rest & 0x7f).unwrap() | 0x80);
                rest >>= 7;
            }
            out.extend(groups.iter().rev());
        }
        out
    }

    #[test]
    fn the_workstation_purpose_is_the_uuid_oid() {
        let uuid = u128::from_str_radix("edc7e7d8ab9e4c72a01baa39eb83e905", 16).unwrap();
        assert_eq!(WORKSTATION_PURPOSE, format!("2.25.{uuid}"));
        assert_eq!(
            encode_oid(WORKSTATION_PURPOSE),
            WORKSTATION_PURPOSE_DER.to_vec()
        );
        let value = workstation_extended_key_usage();
        assert_eq!(&value[..4], [0x30, 22, 0x06, 20]);
        assert_eq!(value.len(), 24);
    }

    #[test]
    fn workstation_names_follow_the_gateway_id_characters() {
        for name in [
            "a",
            "ws-01",
            "Line_A.7",
            &"x".repeat(MAX_WORKSTATION_NAME_LEN),
        ] {
            let parsed = WorkstationName::parse(name).expect(name);
            assert_eq!(parsed.subject(), format!("workstation:{name}"));
            // The common name is never a gateway id.
            assert!(GatewayId::new(parsed.subject()).is_err(), "{name}");
        }
        for name in [
            "",
            "-ws",
            ".ws",
            "ws 01",
            "ws/01",
            "ws:01",
            "ws\n",
            "電腦",
            &"x".repeat(MAX_WORKSTATION_NAME_LEN + 1),
        ] {
            assert!(WorkstationName::parse(name).is_none(), "{name:?}");
        }
    }
}
