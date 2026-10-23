//! Certificates from certificate signing requests (decisions 6 and 12; Laysi/trustiot-v2#78
//! acceptance 1, Laysi/trustiot-v2#534, Laysi/trustiot-v2#557).
//!
//! The CA takes exactly one thing from a CSR: its public key, after checking that the CSR is
//! signed by the matching private key (proof of possession) and that the key is ECDSA P-256, the
//! curve of the gateway's TPM. Everything else in the CSR (subject, subject alternative names,
//! key usages, extended key usages, basic constraints, any other requested extension or
//! attribute) is ignored: the profile decides the certificate.
//!
//! Three profiles, signed by the same issuing CA and told apart by the extended key usage
//! (services/certs/README.md, "閘道器憑證", "工作站憑證" and "broker 伺服器憑證"):
//!
//! - [`Profile::Gateway`] ([`gateway_certificate`]): subject `CN=<gateway id>` only;
//!   extendedKeyUsage clientAuth only; valid for `gateway.validity`. The gateway id is the MQTT
//!   identity (`use_identity_as_username`).
//! - [`Profile::Workstation`] ([`workstation_certificate`]): subject `CN=workstation:<name>`
//!   only, which is never a gateway id (`:` is not in a gateway id); extendedKeyUsage
//!   [`WORKSTATION_PURPOSE`] only and never clientAuth, so a TLS server that authenticates
//!   clients, the MQTT broker among them, refuses it; valid for `workstation.validity`.
//! - [`Profile::Broker`] ([`broker_certificate`]): the MQTT broker's TLS server certificate;
//!   subject `CN=`[`BROKER_SERVER_NAME`] only; the DNS subject alternative names of
//!   [`BrokerNames`] (that name, the same in every deployment, then the broker's Service name);
//!   extendedKeyUsage serverAuth only, so the broker refuses it as a gateway's client
//!   certificate; valid for `broker.validity`.
//!
//! All: basicConstraints `cA=FALSE` (critical); keyUsage digitalSignature (critical); subject
//! and authority key identifiers; a random positive 128-bit serial number; valid from five
//! minutes before signing (clock skew), never past the issuing CA certificate. No CRL
//! distribution point: Mosquitto reads the CRLs from a file (decision 6). Only the broker
//! profile has subject alternative names.
//!
//! Who may get a certificate for which gateway id is the caller's decision: the factory
//! registration (Laysi/trustiot-v2#79) and the renewal endpoint (Laysi/trustiot-v2#81).
//! Workstation certificates are issued only through the platform administrators' API
//! ([`crate::workstations`]), broker certificates only on the port the mesh opens to the MQTT
//! broker alone ([`crate::broker`]). [`record`] puts a certificate in the ledger only when its issuing
//! CA already has a CRL, so nothing is handed out that the broker could not check.

use std::time::Duration;

use protocol::GatewayId;
use rcgen::{
    CertificateParams, CustomExtension, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber, SubjectPublicKeyInfo,
};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::FromDer;

use crate::authority::{Authority, CaState, IssuingCa, unix_now};

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

// The workstation profile's purpose and subject prefix live in `contract::certs`, which the
// workstation tool (factory/workstation) shares, so the issuer and its verifiers cannot drift.
pub use contract::certs::{
    WORKSTATION_PURPOSE, WORKSTATION_PURPOSE_DER, WORKSTATION_SUBJECT_PREFIX,
};

/// The extendedKeyUsage extension (RFC 5280 section 4.2.1.12).
const EXTENDED_KEY_USAGE_OID: [u64; 4] = [2, 5, 29, 37];

/// Longest workstation name, in bytes: with [`WORKSTATION_SUBJECT_PREFIX`] the common name stays
/// within the ledger's 64 characters.
pub const MAX_WORKSTATION_NAME_LEN: usize = 48;

/// Serializes a workstation certificate's revocation with the factory registrations it signs
/// (Laysi/trustiot-v2#79): a registration holds this transaction lock shared from before it
/// reads the certificate's ledger row until it commits, and a revocation takes it exclusively
/// before it updates the row. A revocation committed first is seen by the registration's read; a
/// registration that read the row first commits before the revocation can. A row lock would
/// need an UPDATE grant the platform role does not have. `$1` is the certificate's serial number;
/// the first key is a fixed class, so these locks never meet the one-key locks of
/// `service_kit::job_lock` or the outbox, and a hash collision only makes one wait for the other.
pub const LOCK_WORKSTATION_SHARED: &str =
    "SELECT pg_advisory_xact_lock_shared(1953983347, hashtext(encode($1, 'hex')))";

/// The exclusive form of [`LOCK_WORKSTATION_SHARED`], for a revocation.
pub const LOCK_WORKSTATION: &str =
    "SELECT pg_advisory_xact_lock(1953983347, hashtext(encode($1, 'hex')))";

/// The name every MQTT client verifies the broker's server certificate against, in every
/// deployment, SaaS and on premises alike (services/certs/README.md, "broker 伺服器憑證"). It
/// depends on no host name of a deployment, so a gateway's trust settings never change with one.
/// It is under `.internal`, the top-level domain ICANN reserved for private use on 2024-07-29,
/// which no public CA can certify; nothing resolves it: a client connects to the address it is
/// given and checks this name. It is also the certificate's common name and its subject in the
/// ledger.
pub const BROKER_SERVER_NAME: &str = "mqtt-broker.trustiot.internal";

/// Longest DNS name a broker certificate carries: 253 characters as text (RFC 1035).
pub const MAX_DNS_NAME_LEN: usize = 253;

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
    #[error("the gateway id is the MQTT broker's server name, which no gateway may have")]
    ReservedGatewayId,
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
    /// The MQTT broker's TLS server certificate (Laysi/trustiot-v2#557).
    Broker,
}

impl Profile {
    /// The profile's name in the ledger (`certs.issued_certificates.profile`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Workstation => "workstation",
            Self::Broker => "broker",
        }
    }
}

/// The DNS names of the broker's server certificate: [`BROKER_SERVER_NAME`] first, then the
/// broker's Service name when the deployment sets one (`broker.service_name`: the chart's
/// `<release>-mqtt-broker.<namespace>.svc`, the name Laysi/trustiot-v2#84 promised in-cluster
/// clients). The caller of the endpoint never chooses them: a CSR's names are ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerNames(Vec<String>);

impl BrokerNames {
    /// [`BROKER_SERVER_NAME`], then `service_name` when given and different.
    ///
    /// # Errors
    ///
    /// When `service_name` is not a DNS name ([`is_dns_name`]); the message names the rule.
    pub fn new(service_name: Option<&str>) -> Result<Self, String> {
        let mut names = vec![BROKER_SERVER_NAME.to_owned()];
        if let Some(name) = service_name {
            if !is_dns_name(name) {
                return Err(format!(
                    "must be a lower-case DNS name of at most {MAX_DNS_NAME_LEN} characters: two \
                     or more labels of 1 to 63 letters, digits and inner hyphens"
                ));
            }
            if name != BROKER_SERVER_NAME {
                names.push(name.to_owned());
            }
        }
        Ok(Self(names))
    }

    /// The names, [`BROKER_SERVER_NAME`] first.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// Whether `name` is a lower-case DNS host name: at most [`MAX_DNS_NAME_LEN`] characters, two
/// or more labels of 1 to 63 ASCII lower-case letters, digits or hyphens, none starting or
/// ending with a hyphen, the last one with a letter. So no wildcard, no trailing dot and no IP
/// address.
#[must_use]
pub fn is_dns_name(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |label: &&str| {
        let bytes = label.as_bytes();
        (1..=63).contains(&bytes.len())
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            && bytes.first() != Some(&b'-')
            && bytes.last() != Some(&b'-')
    };
    name.len() <= MAX_DNS_NAME_LEN
        && labels.len() >= 2
        && labels.iter().all(label_ok)
        && labels
            .last()
            .is_some_and(|last| last.bytes().any(|byte| byte.is_ascii_lowercase()))
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
    /// Its common name: the gateway id, `workstation:<name>`, or [`BROKER_SERVER_NAME`].
    pub subject: String,
    /// Its DNS subject alternative names: empty but for the broker profile.
    pub names: Vec<String>,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
    /// The certificate, DER.
    pub der: Vec<u8>,
    /// The certificate of the issuing CA that signed it, DER: the rest of the chain a TLS
    /// server sends.
    pub issuer_der: Vec<u8>,
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
/// When the request is refused ([`requested_key`]), the gateway id is [`BROKER_SERVER_NAME`]
/// (a gateway id may have dots, and a client that falls back to the common name when a
/// certificate has no DNS name, as OpenSSL's host check does, would otherwise read a gateway
/// certificate as the broker's; its extended key usage already refuses it, this is the second
/// line), the issuing CA certificate expires within [`MIN_REMAINING_SECS`], or signing fails.
pub fn gateway_certificate(
    ca: &IssuingCa,
    request: &[u8],
    gateway: &GatewayId,
    validity: Duration,
    now: i64,
) -> Result<IssuedCertificate, IssueError> {
    if gateway.as_str().eq_ignore_ascii_case(BROKER_SERVER_NAME) {
        return Err(IssueError::ReservedGatewayId);
    }
    sign(
        ca,
        request,
        Profile::Gateway,
        gateway.as_str().to_owned(),
        &[],
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
        &[],
        validity,
        now,
    )
}

/// Signs the MQTT broker's server certificate with `ca` at `now` (Unix seconds), valid for
/// `validity`, for the public key of `request` (DER), with the DNS names `names`. See the module
/// documentation for the profile.
///
/// # Errors
///
/// As [`gateway_certificate`].
pub fn broker_certificate(
    ca: &IssuingCa,
    request: &[u8],
    names: &BrokerNames,
    validity: Duration,
    now: i64,
) -> Result<IssuedCertificate, IssueError> {
    sign(
        ca,
        request,
        Profile::Broker,
        BROKER_SERVER_NAME.to_owned(),
        names.as_slice(),
        validity,
        now,
    )
}

fn sign(
    ca: &IssuingCa,
    request: &[u8],
    profile: Profile,
    subject: String,
    names: &[String],
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
        Profile::Broker => {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        }
    }
    params.subject_alt_names = names
        .iter()
        .map(|name| {
            name.as_str()
                .try_into()
                .map(SanType::DnsName)
                .map_err(|_| IssueError::Sign(format!("{name:?} is not a DNS name")))
        })
        .collect::<Result<_, _>>()?;
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
        names: names.to_vec(),
        not_before,
        not_after,
        der: certificate.der().to_vec(),
        issuer_der: ca.certificate_der().to_vec(),
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

/// Why [`sign_and_record`] or [`sign_and_record_in`] issued nothing. The caller turns it into its
/// response; the log line is already written.
#[derive(Debug)]
pub enum Refusal {
    /// No issuing CA can sign now: none is loaded, none that is valid has a CRL yet, or the
    /// newest expires within [`MIN_REMAINING_SECS`]. Retrying later helps once the CA is set up or
    /// its first CRL is out.
    NotReady,
    /// The request was refused ([`requested_key`]'s reasons).
    Request(IssueError),
    /// Signing failed: no randomness, or rcgen refused.
    Failed,
    /// The database failed; nothing was recorded.
    Database,
}

/// Signs a certificate with `sign` and records it in one transaction on `pool`, with the newest
/// issuing CA of `ca` that is valid and already has a CRL, so nothing is handed out that the
/// broker could not check against one ([`crate::authority::Authority::signer`], [`record`]).
/// `what` names the certificate in the log lines, for example "a workstation certificate". The
/// log never carries the request or the certificate.
///
/// # Errors
///
/// [`Refusal`], after logging why.
pub async fn sign_and_record(
    pool: &PgPool,
    ca: &CaState,
    what: &str,
    sign: impl FnOnce(&IssuingCa, i64) -> Result<IssuedCertificate, IssueError>,
) -> Result<IssuedCertificate, Refusal> {
    let Some(authority) = ca.authority() else {
        tracing::warn!("refused {what}: no issuing CA is loaded");
        return Err(Refusal::NotReady);
    };
    let mut tx = pool.begin().await.map_err(|e| database(what, &e))?;
    let issued = sign_and_record_in(&mut tx, &authority, what, sign).await?;
    tx.commit().await.map_err(|e| database(what, &e))?;
    Ok(issued)
}

/// [`sign_and_record`] inside the caller's transaction, with the issuing CAs of `authority`:
/// reads which of them have a CRL, signs with the newest valid one that has, and records the
/// certificate, all on `connection`. The caller commits, so the certificate is recorded together
/// with whatever else the transaction writes (the factory registration, Laysi/trustiot-v2#79,
/// writes the registration and its event with it); on any refusal the caller rolls back.
///
/// # Errors
///
/// [`Refusal`], after logging why.
pub async fn sign_and_record_in(
    connection: &mut PgConnection,
    authority: &Authority,
    what: &str,
    sign: impl FnOnce(&IssuingCa, i64) -> Result<IssuedCertificate, IssueError>,
) -> Result<IssuedCertificate, Refusal> {
    let ids: Vec<String> = authority
        .issuing()
        .iter()
        .map(|ca| ca.key_id().to_owned())
        .collect();
    let with_crl: Vec<String> =
        sqlx::query_scalar("SELECT ca_key_id FROM certs.crls WHERE ca_key_id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *connection)
            .await
            .map_err(|e| database(what, &e))?;
    let now = unix_now();
    let Some(signer) = authority.signer(now, |id| with_crl.iter().any(|known| known == id)) else {
        tracing::warn!(
            "refused {what}: no valid issuing CA has a CRL yet (the CRL job gives a new one its \
             first CRL within crl.check_interval)"
        );
        return Err(Refusal::NotReady);
    };
    let issued = sign(signer, now).map_err(|failure| match failure {
        IssueError::IssuingCaExpiring => {
            tracing::warn!(ca_key_id = %signer.key_id(), "refused {what}: {failure}");
            Refusal::NotReady
        }
        IssueError::Random | IssueError::Sign(_) => {
            tracing::error!(problem = %failure, "could not sign {what}");
            Refusal::Failed
        }
        refused => Refusal::Request(refused),
    })?;
    match record(connection, &issued).await {
        Ok(()) => Ok(issued),
        Err(RecordError::NoCrl(ca_key_id)) => {
            // The CRL was read in this transaction and CRLs are never removed, so this means a
            // broken invariant; refuse rather than hand out an uncheckable certificate.
            tracing::error!(ca_key_id = %ca_key_id, "the signing CA has no CRL after all");
            Err(Refusal::NotReady)
        }
        Err(RecordError::Database(e)) => Err(database(what, &e)),
    }
}

/// Logs a database failure while issuing `what`; [`Refusal::Database`].
fn database(what: &str, error: &dyn std::fmt::Display) -> Refusal {
    tracing::error!(error = %error, "could not issue {what}");
    Refusal::Database
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
    fn broker_names_start_with_the_fixed_name_and_take_only_dns_names() {
        assert!(is_dns_name(BROKER_SERVER_NAME));
        assert_eq!(
            BrokerNames::new(None).expect("names").as_slice(),
            [BROKER_SERVER_NAME]
        );
        assert_eq!(
            BrokerNames::new(Some(BROKER_SERVER_NAME))
                .expect("names")
                .as_slice(),
            [BROKER_SERVER_NAME]
        );
        let service = "trustiot-mqtt-broker.trustiot.svc";
        assert_eq!(
            BrokerNames::new(Some(service)).expect("names").as_slice(),
            [BROKER_SERVER_NAME, service]
        );
        let longest = format!("{}.{}.svc", "a".repeat(63), "b".repeat(63));
        for name in ["a.b", "x-1.y2.svc", "a1.svc", longest.as_str()] {
            assert!(is_dns_name(name), "{name}");
        }
        let long_label = format!("{}.svc", "a".repeat(64));
        let too_long = format!("{}.svc", ["a".repeat(60).as_str(); 5].join("."));
        assert!(too_long.len() > MAX_DNS_NAME_LEN);
        for name in [
            "",
            "localhost",
            ".svc",
            "a..svc",
            "a.svc.",
            "*.svc.cluster.local",
            "-a.svc",
            "a-.svc",
            "A.svc",
            "a_b.svc",
            "127.0.0.1",
            "a.123",
            "a b.svc",
            "ä.svc",
            long_label.as_str(),
            too_long.as_str(),
        ] {
            assert!(!is_dns_name(name), "{name:?}");
            assert!(BrokerNames::new(Some(name)).is_err(), "{name:?}");
        }
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
