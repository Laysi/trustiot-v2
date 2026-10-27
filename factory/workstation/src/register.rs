//! Factory registration of a gateway (services/certs/README.md, "出廠登錄"; Laysi/trustiot-v2#615):
//! `POST /api/certs/v1/gateway-registrations` with the platform administrator's session and the
//! workstation's signature ([`crate::sign`]), retried safely, its answer checked, the result
//! recorded ([`crate::journal`]).
//!
//! [`register`] takes a CSR and an endorsement key it is given ([`Gateway::new`]): the `register`
//! command reads them from files, and the ssh flow (Laysi/trustiot-v2#616) from the gateway.
//!
//! 1. The body is built once, from the CSR and the endorsement key as checked (re-encoded PEM).
//! 2. Before anything is sent: the workstation key and certificate are checked, and the journal
//!    must not hold this public key as registered under another hardware ID. A `pending` record
//!    is written, unless the hardware ID is already recorded as `registered`.
//! 3. Each attempt is signed anew (time, nonce, signature) and sent once on a new connection.
//! 4. A connection error, a timeout, 502, 503, 504 or Cloudflare's 520 to 524 is retried
//!    ([`Backoff`]). The platform's API is idempotent: when it committed but the answer was lost,
//!    the next attempt gets 200 and the same registration. When the limits are used up,
//!    [`StopKind::GaveUp`] says it is safe to run again.
//! 5. Every other answer stops with its own [`StopKind`] (and exit code), or as a plain error
//!    that says whether running again is safe. A log line that cannot be written is added to the
//!    message; it never replaces the stop.
//! 6. A 200 or 201 is checked before it counts: the size of its fields, the hardware ID, both
//!    hashes against the ones computed here, the certificate's common name and public key, the
//!    chain (the certificate, then an issuing CA that signed it and that the root certificate
//!    signed), and the root certificate's SHA-256 against the pinned one
//!    ([`Registrar::root_sha256`], required), which is what makes the chain authentic: the root
//!    certificate becomes the gateway's only trust anchor (Laysi/trustiot-v2#617).
//! 7. The record becomes `registered` with the pinned fingerprint, and a log line says so.

use std::fmt;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use contract::certs::{GatewayRegistration, RegisterGateway};
use http::{Method, StatusCode};
use rcgen::{CertificateSigningRequestParams, PKCS_ECDSA_P256_SHA256, PublicKeyData as _};
use rustls_pki_types::CertificateSigningRequestDer;
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::{OID_SIG_ECDSA_WITH_SHA256, OID_X509_COMMON_NAME};
use x509_parser::prelude::{FromDer as _, X509Certificate};
use x509_parser::x509::SubjectPublicKeyInfo;
use zeroize::Zeroizing;

use crate::Error;
use crate::files;
use crate::http::{ApiError, Call, Client, Response, printable};
use crate::identity::{self, CheckedCertificate, WorkstationDirectory, WorkstationKey};
use crate::journal::{Event, Journal, RECORD_VERSION, Record, State};
pub use crate::retry::Backoff;
use crate::secret::SessionCookie;
use crate::sign::{self, Nonce};

/// The registration endpoint, as the ingress forwards it.
pub const REGISTRATIONS_PATH: &str = "/api/certs/v1/gateway-registrations";

/// Longest hardware ID (`protocol::MAX_ID_LEN`).
pub const MAX_HARDWARE_ID_LEN: usize = 64;

/// Largest CSR accepted, DER (the certs service's limit).
pub const MAX_REQUEST_BYTES: usize = 4096;

/// Largest endorsement key accepted, DER (the certs service's limit).
pub const MAX_ENDORSEMENT_KEY_BYTES: usize = 2048;

/// Largest workstation certificate file read.
const MAX_CERTIFICATE_FILE_BYTES: usize = 64 * 1024;

/// Largest `certificate` and `root_certificate` of an answer: one PEM certificate is 1 to 2 KiB.
const MAX_ANSWER_CERTIFICATE_BYTES: usize = 8 * 1024;

/// Largest `certificate_chain` of an answer: two certificates.
const MAX_ANSWER_CHAIN_BYTES: usize = 2 * MAX_ANSWER_CERTIFICATE_BYTES;

/// Largest `workstation` (name) of an answer.
const MAX_ANSWER_NAME_BYTES: usize = 256;

/// The certs service's message keys this client tells apart.
mod keys {
    pub const PUBLIC_KEY_MISMATCH: &str = "gateway_registration.public_key_mismatch";
    pub const ENDORSEMENT_KEY_MISMATCH: &str = "gateway_registration.endorsement_key_mismatch";
    pub const PUBLIC_KEY_IN_USE: &str = "gateway_registration.public_key_in_use";
    pub const ENDORSEMENT_KEY_IN_USE: &str = "gateway_registration.endorsement_key_in_use";
    pub const DECOMMISSIONED: &str = "gateway_registration.decommissioned";
    pub const WORKSTATION_REFUSED: &str = "gateway_registration.workstation_refused";
}

/// Whether `id` follows the gateway ID rule (`protocol::GatewayId`): 1 to 64 ASCII letters,
/// digits, `.`, `_` or `-`, starting with a letter or digit. Such an ID is also a plain file name.
#[must_use]
pub fn is_hardware_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.len() <= MAX_HARDWARE_ID_LEN
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `der` as one PEM block labelled `label`: 64 base64 characters a line.
fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

/// The DER of the one PEM block in `bytes`, which must be labelled `label`; any other block
/// refuses the input. Text outside the block is ignored: only what is decoded is sent.
fn one_pem_block(bytes: &[u8], label: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.matches("-----BEGIN").count() != 1 || text.matches("-----END").count() != 1 {
        return None;
    }
    let (_, after_begin) = text.split_once(&format!("-----BEGIN {label}-----"))?;
    let (body, _) = after_begin.split_once(&format!("-----END {label}-----"))?;
    let compact: String = body
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    STANDARD.decode(compact.as_bytes()).ok()
}

/// Why a gateway's input is refused before anything is sent.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    #[error(
        "--hardware-id {0}: 1 to 64 ASCII letters, digits, '.', '_' or '-', starting with a letter \
         or digit"
    )]
    HardwareId(String),
    #[error("the certificate request {0}")]
    CertificateRequest(&'static str),
    #[error("the endorsement key {0}")]
    EndorsementKey(&'static str),
}

impl From<InputError> for Error {
    fn from(error: InputError) -> Self {
        match error {
            InputError::HardwareId(_) => Self::Usage(error.to_string()),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// A gateway to register, once its input holds.
#[derive(Clone)]
pub struct Gateway {
    hardware_id: String,
    /// The CSR's public key, DER SubjectPublicKeyInfo as the certs service encodes it.
    public_key: Vec<u8>,
    public_key_sha256: String,
    endorsement_key_sha256: String,
    /// The request body, fixed once.
    body: Vec<u8>,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gateway")
            .field("hardware_id", &self.hardware_id)
            .field("public_key_sha256", &self.public_key_sha256)
            .field("endorsement_key_sha256", &self.endorsement_key_sha256)
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// The gateway `hardware_id` with the CSR in `request_pem` (one PEM `CERTIFICATE REQUEST`,
    /// at most [`MAX_REQUEST_BYTES`] as DER, ECDSA P-256 signed with ecdsa-with-SHA256 by its own
    /// key) and the endorsement key in `endorsement_key_pem` (one PEM `PUBLIC KEY`, a
    /// SubjectPublicKeyInfo of at most [`MAX_ENDORSEMENT_KEY_BYTES`]; which algorithms the
    /// platform accepts is its decision).
    ///
    /// # Errors
    ///
    /// The first input that does not hold.
    pub fn new(
        hardware_id: &str,
        request_pem: &[u8],
        endorsement_key_pem: &[u8],
    ) -> Result<Self, InputError> {
        if !is_hardware_id(hardware_id) {
            return Err(InputError::HardwareId(format!(
                "{:?}",
                printable(hardware_id)
            )));
        }
        let request = one_pem_block(request_pem, "CERTIFICATE REQUEST").ok_or(
            InputError::CertificateRequest("is not one PEM CERTIFICATE REQUEST block"),
        )?;
        if request.len() > MAX_REQUEST_BYTES {
            return Err(InputError::CertificateRequest(
                "is larger than 4096 bytes as DER",
            ));
        }
        let (rest, parsed) = X509CertificationRequest::from_der(&request).map_err(|_| {
            InputError::CertificateRequest("is not a PKCS #10 certificate signing request")
        })?;
        if !rest.is_empty() {
            return Err(InputError::CertificateRequest(
                "has bytes after the certificate signing request",
            ));
        }
        if parsed.signature_algorithm.algorithm != OID_SIG_ECDSA_WITH_SHA256
            || parsed.signature_algorithm.parameters.is_some()
        {
            return Err(InputError::CertificateRequest(
                "is not signed with ecdsa-with-SHA256",
            ));
        }
        // rcgen checks the signature with the request's own key, and re-encodes that key as the
        // certs service does before it hashes it.
        let params = CertificateSigningRequestParams::from_der(
            &CertificateSigningRequestDer::from(request.as_slice()),
        )
        .map_err(|_| {
            InputError::CertificateRequest("is not signed by its own key (or cannot be read)")
        })?;
        if params.public_key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            return Err(InputError::CertificateRequest(
                "is not for an ECDSA P-256 key",
            ));
        }
        let public_key = params.public_key.subject_public_key_info();

        let endorsement_key = one_pem_block(endorsement_key_pem, "PUBLIC KEY").ok_or(
            InputError::EndorsementKey("is not one PEM PUBLIC KEY block"),
        )?;
        if endorsement_key.len() > MAX_ENDORSEMENT_KEY_BYTES {
            return Err(InputError::EndorsementKey(
                "is larger than 2048 bytes as DER",
            ));
        }
        match SubjectPublicKeyInfo::from_der(&endorsement_key) {
            Ok(([], _)) => {}
            _ => {
                return Err(InputError::EndorsementKey("is not a SubjectPublicKeyInfo"));
            }
        }

        let body = serde_json::to_vec(&RegisterGateway {
            hardware_id: hardware_id.to_owned(),
            certificate_request: pem_encode("CERTIFICATE REQUEST", &request),
            endorsement_key: pem_encode("PUBLIC KEY", &endorsement_key),
        })
        .map_err(|_| InputError::CertificateRequest("cannot be encoded"))?;
        Ok(Self {
            hardware_id: hardware_id.to_owned(),
            public_key_sha256: sha256_hex(&public_key),
            public_key,
            endorsement_key_sha256: sha256_hex(&endorsement_key),
            body,
        })
    }

    /// The hardware ID.
    #[must_use]
    pub fn hardware_id(&self) -> &str {
        &self.hardware_id
    }

    /// The SHA-256 of the public key, lower-case hex, as the platform computes it.
    #[must_use]
    pub fn public_key_sha256(&self) -> &str {
        &self.public_key_sha256
    }

    /// The SHA-256 of the endorsement key, lower-case hex, as the platform computes it.
    #[must_use]
    pub fn endorsement_key_sha256(&self) -> &str {
        &self.endorsement_key_sha256
    }

    /// The request body, exactly as it is sent and signed.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// The workstation's key and its checked certificate, the identity requests are signed with.
pub struct Workstation {
    key: WorkstationKey,
    certificate: CheckedCertificate,
}

impl fmt::Debug for Workstation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workstation")
            .field("key", &self.key)
            .field("subject", &self.certificate.subject)
            .field("serial", &self.certificate.serial)
            .finish()
    }
}

impl Workstation {
    /// Reads the key and the certificate of `directory` and checks the certificate at `now`
    /// (Unix seconds), as `install-certificate` does.
    ///
    /// # Errors
    ///
    /// When the key or the certificate is missing or refused (an expired certificate included).
    pub fn load(directory: &WorkstationDirectory, now: i64) -> Result<Self, Error> {
        let key = WorkstationKey::load(&directory.key())?;
        let pem = files::read_public(&directory.certificate(), MAX_CERTIFICATE_FILE_BYTES)?;
        let certificate = identity::check_certificate(&pem, &key, now)?;
        Ok(Self { key, certificate })
    }

    /// The certificate's common name, `workstation:<name>`.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.certificate.subject
    }

    /// The certificate's serial, lower-case hex.
    #[must_use]
    pub fn serial(&self) -> &str {
        &self.certificate.serial
    }
}

/// How a registration stopped without success, each with its own exit code
/// (factory/workstation/README.md, "結束碼").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {
    /// 409 `public_key_mismatch` or `endorsement_key_mismatch`: the hardware ID is registered
    /// with another key. A wrong label, or a cleared TPM, which makes a new device (decision 12).
    KeyMismatch,
    /// 409 `public_key_in_use` or `endorsement_key_in_use`, or this workstation's records hold the
    /// public key under another hardware ID: this TPM is registered as another gateway.
    KeyInUse,
    /// 409 `decommissioned`.
    Decommissioned,
    /// 401: the session ended; log in again.
    LoginRequired,
    /// 403 `workstation_refused` with reason `stale`: the clocks differ by more than 300 s.
    ClockSkew,
    /// 403 `workstation_refused` with reason `revoked`, `expired`, `untrusted` or
    /// `not_workstation`: the workstation certificate itself.
    WorkstationRefused,
    /// 403 `workstation_refused` with reason `missing`, `malformed`, `signature_invalid` or
    /// `replayed`, or a reason this tool does not know: the signature headers did not arrive as
    /// sent (a proxy in front of the platform drops or rewrites them, or changes the body), or a
    /// bug of this tool (`replayed`: it never sends a nonce twice). Not the certificate.
    SignatureRejected,
    /// 403 `permission_denied` without `workstation_refused`: not a platform administrator.
    NotPlatformAdministrator,
    /// 422: a field of the request breaks a rule.
    Invalid,
    /// 200 or 201, but the answer is not this gateway's registration; nothing is recorded as
    /// registered.
    ResponseMismatch,
    /// No answer that settles it within the retry limits; running the same command again is
    /// safe.
    GaveUp,
}

impl StopKind {
    /// The process exit code.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::KeyMismatch => 10,
            Self::KeyInUse => 11,
            Self::Decommissioned => 12,
            Self::LoginRequired => 13,
            Self::ClockSkew => 14,
            Self::WorkstationRefused => 15,
            Self::NotPlatformAdministrator => 16,
            Self::Invalid => 17,
            Self::ResponseMismatch => 18,
            Self::SignatureRejected => 19,
            // EX_TEMPFAIL of sysexits.h: a temporary failure, try again.
            Self::GaveUp => 75,
        }
    }

    /// The name in log lines and records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KeyMismatch => "key_mismatch",
            Self::KeyInUse => "key_in_use",
            Self::Decommissioned => "decommissioned",
            Self::LoginRequired => "login_required",
            Self::ClockSkew => "clock_skew",
            Self::WorkstationRefused => "workstation_refused",
            Self::NotPlatformAdministrator => "not_platform_administrator",
            Self::Invalid => "invalid",
            Self::ResponseMismatch => "response_mismatch",
            Self::SignatureRejected => "signature_rejected",
            Self::GaveUp => "gave_up",
        }
    }
}

/// A registration that stopped: the kind (and exit code), the message for the operator, and the
/// platform's error when there was one.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct Stopped {
    pub kind: StopKind,
    pub message: String,
    pub api: Option<ApiError>,
}

/// Why a 200 or 201 is not taken as this gateway's registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResponseProblem {
    #[error("the body is not a gateway registration")]
    NotARegistration,
    #[error(
        "a field is too large to be a registration (workstation over 256 bytes, certificate or \
         root_certificate over 8 KiB, certificate_chain over 16 KiB) or its ca_key_id is not 64 \
         lower-case hex digits"
    )]
    Size,
    #[error("its hardware_id is not the one sent")]
    HardwareId,
    #[error("its public_key_sha256 is not the SHA-256 of the CSR's public key")]
    PublicKeySha256,
    #[error("its endorsement_key_sha256 is not the SHA-256 of the endorsement key sent")]
    EndorsementKeySha256,
    #[error("its serial is not 2 to 40 lower-case hex digits")]
    Serial,
    #[error("its certificate is not one PEM certificate that can be parsed")]
    Certificate,
    #[error("its certificate's common name is not the hardware ID")]
    CommonName,
    #[error("its certificate's public key is not the CSR's public key")]
    CertificateKey,
    #[error(
        "its certificate_chain is not the certificate followed by an issuing CA that signed it \
         and that its root_certificate signed"
    )]
    Chain,
    #[error("its root_certificate is not one PEM certificate")]
    RootCertificate,
    #[error("the SHA-256 of its root_certificate is not the one given with --root-sha256")]
    RootFingerprint,
}

/// Checks that `registration` is `gateway`'s and that its root certificate is the one pinned by
/// `root_sha256` (the SHA-256 of its DER): see the module documentation, step 6. The size check
/// keeps a recorded registration well under the journal's read limit, so one answer cannot make
/// the records unreadable.
///
/// # Errors
///
/// The first check that fails.
pub fn check_registration(
    gateway: &Gateway,
    registration: &GatewayRegistration,
    root_sha256: &[u8; 32],
) -> Result<(), ResponseProblem> {
    if registration.workstation.len() > MAX_ANSWER_NAME_BYTES
        || registration.certificate.len() > MAX_ANSWER_CERTIFICATE_BYTES
        || registration.root_certificate.len() > MAX_ANSWER_CERTIFICATE_BYTES
        || registration.certificate_chain.len() > MAX_ANSWER_CHAIN_BYTES
        || !is_sha256_hex(&registration.ca_key_id)
    {
        return Err(ResponseProblem::Size);
    }
    if registration.hardware_id != gateway.hardware_id {
        return Err(ResponseProblem::HardwareId);
    }
    if registration.public_key_sha256 != gateway.public_key_sha256 {
        return Err(ResponseProblem::PublicKeySha256);
    }
    if registration.endorsement_key_sha256 != gateway.endorsement_key_sha256 {
        return Err(ResponseProblem::EndorsementKeySha256);
    }
    if !identity::is_serial(&registration.serial) {
        return Err(ResponseProblem::Serial);
    }
    let der = identity::certificate_der(registration.certificate.as_bytes())
        .map_err(|_| ResponseProblem::Certificate)?;
    let (rest, certificate) =
        X509Certificate::from_der(&der).map_err(|_| ResponseProblem::Certificate)?;
    if !rest.is_empty() {
        return Err(ResponseProblem::Certificate);
    }
    let mut names = certificate
        .subject()
        .iter_attributes()
        .filter(|attribute| attribute.attr_type() == &OID_X509_COMMON_NAME);
    let common_name = match (names.next(), names.next()) {
        (Some(name), None) => name.as_str().ok(),
        _ => None,
    };
    if common_name != Some(gateway.hardware_id.as_str()) {
        return Err(ResponseProblem::CommonName);
    }
    let certificate_key = rcgen::SubjectPublicKeyInfo::from_der(certificate.public_key().raw)
        .map(|key| key.subject_public_key_info())
        .map_err(|_| ResponseProblem::CertificateKey)?;
    if certificate_key != gateway.public_key {
        return Err(ResponseProblem::CertificateKey);
    }
    // The chain: the certificate, then an issuing CA whose key signed it and which the root
    // certificate's key signed. The pinned root (below) makes that chain authentic.
    let chain = pem_certificates(&registration.certificate_chain).ok_or(ResponseProblem::Chain)?;
    let [leaf, issuing] = chain.as_slice() else {
        return Err(ResponseProblem::Chain);
    };
    if *leaf != der {
        return Err(ResponseProblem::Chain);
    }
    let issuing = whole(issuing).ok_or(ResponseProblem::Chain)?;
    let root_der = identity::certificate_der(registration.root_certificate.as_bytes())
        .map_err(|_| ResponseProblem::RootCertificate)?;
    let root = whole(&root_der).ok_or(ResponseProblem::RootCertificate)?;
    if !issuing.is_ca()
        || certificate
            .verify_signature(Some(issuing.public_key()))
            .is_err()
        || issuing.verify_signature(Some(root.public_key())).is_err()
    {
        return Err(ResponseProblem::Chain);
    }
    let actual: [u8; 32] = Sha256::digest(&root_der).into();
    if &actual != root_sha256 {
        return Err(ResponseProblem::RootFingerprint);
    }
    Ok(())
}

/// Whether `text` is 64 lower-case hex digits.
fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The certificate `der` is, with nothing after it.
fn whole(der: &[u8]) -> Option<X509Certificate<'_>> {
    X509Certificate::from_der(der)
        .ok()
        .and_then(|(rest, parsed)| rest.is_empty().then_some(parsed))
}

/// The DER of each PEM `CERTIFICATE` block in `text`, in order; `None` when there is a block of
/// another kind, a block without its end, or base64 that does not decode.
fn pem_certificates(text: &str) -> Option<Vec<Vec<u8>>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let begins = text.matches("-----BEGIN").count();
    if begins != text.matches(BEGIN).count()
        || text.matches("-----END").count() != text.matches(END).count()
        || begins != text.matches(END).count()
    {
        return None;
    }
    let mut certificates = Vec::with_capacity(begins);
    let mut rest = text;
    while let Some((_, after_begin)) = rest.split_once(BEGIN) {
        let (body, after_end) = after_begin.split_once(END)?;
        if body.contains("-----") {
            return None;
        }
        let compact: String = body
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        certificates.push(STANDARD.decode(compact.as_bytes()).ok()?);
        rest = after_end;
    }
    Some(certificates)
}

/// `--root-sha256`: 64 hex digits, either case, optionally in pairs separated by `:` (as
/// `openssl x509 -noout -fingerprint -sha256` prints it).
///
/// # Errors
///
/// A usage error for anything else.
pub fn parse_root_sha256(text: &str) -> Result<[u8; 32], Error> {
    let compact: String = text
        .trim()
        .trim_start_matches("sha256 Fingerprint=")
        .trim_start_matches("SHA256 Fingerprint=")
        .chars()
        .filter(|character| *character != ':')
        .collect();
    let invalid = || {
        Error::Usage(
            "--root-sha256 must be 64 hex digits (pairs may be separated by ':')".to_owned(),
        )
    };
    if compact.len() != 64 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut digest = [0_u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let pair = compact.get(index * 2..index * 2 + 2).ok_or_else(invalid)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| invalid())?;
    }
    Ok(digest)
}

/// What [`register`] needs besides the gateway.
pub struct Registrar<'a> {
    /// The client for the platform; each attempt opens a new connection.
    pub client: &'a Client,
    /// The platform administrator's session.
    pub cookie: &'a SessionCookie,
    /// The identity requests are signed with.
    pub workstation: &'a Workstation,
    /// Where the result is recorded.
    pub journal: &'a Journal,
    /// The retry limits.
    pub backoff: Backoff,
    /// The SHA-256 the answer's root certificate (DER) must have: the platform's root CA, pinned
    /// by the operator. Required: the root certificate becomes the gateway's only trust anchor.
    pub root_sha256: [u8; 32],
}

/// A registration that passed its checks and is recorded.
#[derive(Clone, Debug)]
pub struct Registered {
    /// 201 for a new registration, 200 when the platform already had it (a rerun, or a lost
    /// answer before).
    pub status: StatusCode,
    pub registration: GatewayRegistration,
    /// How many attempts it took.
    pub attempts: u32,
}

fn time(now: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(now, 0).unwrap_or_default()
}

/// Registers `gateway`: see the module documentation. `now` gives Unix seconds, for the
/// signatures and the records.
///
/// # Errors
///
/// [`Error::Registration`] with the [`StopKind`] of a known refusal, a failed check or used-up
/// retries; any other error for a local problem (a file refused, the journal not writable) or an
/// unexpected answer.
pub async fn register(
    registrar: &Registrar<'_>,
    gateway: &Gateway,
    now: impl Fn() -> i64,
) -> Result<Registered, Error> {
    let journal = registrar.journal;
    let id = gateway.hardware_id();
    journal.prepare()?;
    if let Some(other) = recorded_elsewhere(journal, gateway)? {
        let stopped = Stopped {
            kind: StopKind::KeyInUse,
            message: format!(
                "this workstation recorded the gateway's public key as registered under hardware \
                 ID {other} ({}): this TPM is registered as another gateway. Check the label; \
                 nothing was sent",
                journal.record_path(&other).display()
            ),
            api: None,
        };
        return Err(logged(
            journal,
            &Event::new(time(now()), id, "stopped")
                .with("stop", stopped.kind.as_str())
                .with("recorded_under", other),
            Error::Registration(Box::new(stopped)),
        ));
    }
    let root_sha256 = hex(&registrar.root_sha256);
    let previous = journal.load(id)?;
    let already = previous
        .as_ref()
        .is_some_and(|record| record.state == State::Registered);
    if !already {
        journal.write(&Record {
            version: RECORD_VERSION,
            hardware_id: id.to_owned(),
            state: State::Pending,
            platform: registrar.client.origin().to_string(),
            root_sha256: root_sha256.clone(),
            public_key_sha256: gateway.public_key_sha256.clone(),
            endorsement_key_sha256: gateway.endorsement_key_sha256.clone(),
            workstation_serial: registrar.workstation.serial().to_owned(),
            updated_at: time(now()),
            registration: None,
        })?;
    }
    journal.append(
        &Event::new(time(now()), id, "started")
            .with("platform", registrar.client.origin().to_string())
            .with("root_sha256", root_sha256)
            .with("public_key_sha256", gateway.public_key_sha256.clone())
            .with(
                "endorsement_key_sha256",
                gateway.endorsement_key_sha256.clone(),
            )
            .with("workstation_serial", registrar.workstation.serial())
            .with("recorded_before", already),
    )?;
    info!(hardware_id = id, origin = %registrar.client.origin(), "registering a gateway");

    let started = Instant::now();
    let mut attempt: u32 = 1;
    loop {
        let problem = match attempt_once(registrar, gateway, &now).await {
            Ok(response) if is_retryable(response.status) => {
                format!("status {}", response.status.as_u16())
            }
            Ok(response) => {
                return conclude(registrar, gateway, response, attempt, &now);
            }
            Err(error) if error.is_transient() => error.problem_kind().to_owned(),
            Err(error) => {
                let event = Event::new(time(now()), id, "stopped")
                    .with("attempt", attempt)
                    .with("problem", error.problem_kind());
                return Err(logged(journal, &event, error.into()));
            }
        };
        journal.append(
            &Event::new(time(now()), id, "attempt_failed")
                .with("attempt", attempt)
                .with("problem", problem.clone()),
        )?;
        let Some(delay) = registrar.backoff.delay(attempt + 1, started.elapsed())? else {
            let event = Event::new(time(now()), id, "gave_up")
                .with("attempts", attempt)
                .with("problem", problem.clone());
            return Err(logged(
                journal,
                &event,
                Error::Registration(Box::new(Stopped {
                    kind: StopKind::GaveUp,
                    message: format!(
                        "no settled answer after {attempt} attempt(s) (last: {problem}). Nothing \
                         is lost: run the same register command again; the platform answers a \
                         registration it already has with that registration"
                    ),
                    api: None,
                })),
            ));
        };
        warn!(
            hardware_id = id,
            attempt,
            problem = %problem,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "the registration attempt got no settled answer; trying again"
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// 502, 503, 504, and Cloudflare's 520 to 524 (the origin's answer was unusable, refused, timed
/// out or unreachable; after 524 the platform may have committed, which a retry settles).
fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 502..=504 | 520..=524)
}

/// Logs `event`, then gives back `error`. A log that cannot be written never replaces the error
/// (a stop keeps its kind and exit code): it is added to the message.
fn logged(journal: &Journal, event: &Event, error: Error) -> Error {
    let Err(log) = journal.append(event) else {
        return error;
    };
    let also = format!("also, the log line could not be written: {log}");
    match error {
        Error::Registration(mut stopped) => {
            stopped.message = format!("{}; {also}", stopped.message);
            Error::Registration(stopped)
        }
        other => Error::Failed(format!("{other}; {also}")),
    }
}

/// The hardware ID another `registered` record of this public key belongs to.
fn recorded_elsewhere(journal: &Journal, gateway: &Gateway) -> Result<Option<String>, Error> {
    Ok(journal
        .records()?
        .into_iter()
        .find(|record| {
            record.state == State::Registered
                && record.hardware_id != gateway.hardware_id
                && record.public_key_sha256 == gateway.public_key_sha256
        })
        .map(|record| record.hardware_id))
}

/// One attempt: a new time, nonce and signature over the fixed body, sent once.
async fn attempt_once(
    registrar: &Registrar<'_>,
    gateway: &Gateway,
    now: &impl Fn() -> i64,
) -> Result<Response, AttemptError> {
    let nonce = Nonce::generate().map_err(AttemptError::Local)?;
    let headers = sign::sign(
        &registrar.workstation.key,
        &registrar.workstation.certificate.der,
        &Method::POST,
        REGISTRATIONS_PATH,
        &gateway.body,
        now(),
        &nonce,
    )
    .map_err(AttemptError::Local)?;
    registrar
        .client
        .send(Call {
            method: Method::POST,
            path: REGISTRATIONS_PATH,
            cookie: Some(registrar.cookie),
            headers: headers.into_headers(),
            json: Some(Zeroizing::new(gateway.body.clone())),
        })
        .await
        .map_err(AttemptError::Request)
}

/// Why an attempt got no response.
#[derive(Debug)]
enum AttemptError {
    Local(Error),
    Request(crate::http::RequestError),
}

impl AttemptError {
    fn is_transient(&self) -> bool {
        matches!(self, Self::Request(error) if error.is_transient())
    }

    fn problem_kind(&self) -> &'static str {
        match self {
            Self::Local(_) => "local",
            Self::Request(error) => error.problem_kind(),
        }
    }
}

impl From<AttemptError> for Error {
    fn from(error: AttemptError) -> Self {
        match error {
            AttemptError::Local(error) => error,
            AttemptError::Request(error) => Self::Request(error),
        }
    }
}

/// The answer that settles the registration: success after the checks, or a stop.
fn conclude(
    registrar: &Registrar<'_>,
    gateway: &Gateway,
    response: Response,
    attempts: u32,
    now: &impl Fn() -> i64,
) -> Result<Registered, Error> {
    let journal = registrar.journal;
    let id = gateway.hardware_id();
    let status = response.status;
    if status == StatusCode::OK || status == StatusCode::CREATED {
        let checked = serde_json::from_slice::<GatewayRegistration>(&response.body)
            .map_err(|_| ResponseProblem::NotARegistration)
            .and_then(|registration| {
                check_registration(gateway, &registration, &registrar.root_sha256)
                    .map(|()| registration)
            });
        let registration = match checked {
            Ok(registration) => registration,
            Err(problem) => {
                let event = Event::new(time(now()), id, "stopped")
                    .with("attempt", attempts)
                    .with("status", status.as_u16())
                    .with("stop", StopKind::ResponseMismatch.as_str())
                    .with("problem", problem.to_string());
                return Err(logged(
                    journal,
                    &event,
                    Error::Registration(Box::new(Stopped {
                        kind: StopKind::ResponseMismatch,
                        message: format!(
                            "POST {REGISTRATIONS_PATH} answered {} but {problem}: it is not the \
                             registration of hardware ID {id}. Nothing was recorded as \
                             registered; install nothing on the gateway and report this to a \
                             platform administrator",
                            status.as_u16()
                        ),
                        api: None,
                    })),
                ));
            }
        };
        // Checked again right before recording: another run may have recorded the key since.
        if let Some(other) = recorded_elsewhere(journal, gateway)? {
            let event = Event::new(time(now()), id, "stopped")
                .with("attempt", attempts)
                .with("status", status.as_u16())
                .with("stop", StopKind::KeyInUse.as_str())
                .with("recorded_under", other.clone());
            return Err(logged(
                journal,
                &event,
                Error::Registration(Box::new(Stopped {
                    kind: StopKind::KeyInUse,
                    message: format!(
                        "the platform registered hardware ID {id} (serial {}), but this \
                         workstation recorded the same public key as registered under hardware \
                         ID {other}; it is not recorded. Report this to a platform administrator",
                        registration.serial
                    ),
                    api: None,
                })),
            ));
        }
        let record = Record {
            version: RECORD_VERSION,
            hardware_id: id.to_owned(),
            state: State::Registered,
            platform: registrar.client.origin().to_string(),
            root_sha256: hex(&registrar.root_sha256),
            public_key_sha256: gateway.public_key_sha256.clone(),
            endorsement_key_sha256: gateway.endorsement_key_sha256.clone(),
            workstation_serial: registrar.workstation.serial().to_owned(),
            updated_at: time(now()),
            registration: Some(registration.clone()),
        };
        journal.write(&record).map_err(|error| {
            Error::Failed(format!(
                "the platform registered hardware ID {id} (serial {}), but the record could not \
                 be written: {error}. Fix that and run the same register command again; the \
                 platform answers with the same registration",
                registration.serial
            ))
        })?;
        journal.append(
            &Event::new(time(now()), id, "registered")
                .with("attempt", attempts)
                .with("status", status.as_u16())
                .with("serial", registration.serial.clone())
                .with("ca_key_id", registration.ca_key_id.clone())
                .with("not_after", registration.not_after.to_rfc3339()),
        )?;
        info!(
            hardware_id = id,
            status = status.as_u16(),
            serial = %registration.serial,
            attempts,
            "registered the gateway"
        );
        return Ok(Registered {
            status,
            registration,
            attempts,
        });
    }

    let api = ApiError::new(&Method::POST, REGISTRATIONS_PATH, &response);
    let stopped = stop_for(id, &api, &response, now());
    let mut event = Event::new(time(now()), id, "stopped")
        .with("attempt", attempts)
        .with("status", status.as_u16());
    if let Some(key) = &api.message_key {
        event = event.with("message_key", key.clone());
    }
    if let Some(reason) = &api.reason {
        event = event.with("reason", reason.clone());
    }
    if let Some(stopped) = &stopped {
        event = event.with("stop", stopped.kind.as_str());
    }
    let error = match stopped {
        Some(stopped) => Error::Registration(Box::new(stopped)),
        // A server error, or an answer that is not the platform's own (no M1 body: a proxy or a
        // load balancer in between): the platform may have committed before it.
        None if status.is_server_error() || api.code.is_none() => Error::Failed(format!(
            "{api}: the platform may or may not have registered hardware ID {id}. Running the \
             same register command again is safe: the platform answers a registration it already \
             has with that registration"
        )),
        None => api.into(),
    };
    Err(logged(journal, &event, error))
}

/// The stop for an error answer; `None` for one this client has no particular advice for.
fn stop_for(id: &str, api: &ApiError, response: &Response, now: i64) -> Option<Stopped> {
    let key = api.message_key.as_deref().unwrap_or_default();
    let stop = |kind: StopKind, advice: String| Stopped {
        kind,
        message: format!("{api}: {advice}"),
        api: Some(api.clone()),
    };
    match response.status {
        StatusCode::CONFLICT => match key {
            keys::PUBLIC_KEY_MISMATCH | keys::ENDORSEMENT_KEY_MISMATCH => Some(stop(
                StopKind::KeyMismatch,
                format!(
                    "hardware ID {id} is registered with another TPM key. Check the label; if it \
                     is right, the gateway's TPM was cleared, and that makes a new device that \
                     needs a new hardware ID (decision 12)"
                ),
            )),
            keys::PUBLIC_KEY_IN_USE | keys::ENDORSEMENT_KEY_IN_USE => Some(stop(
                StopKind::KeyInUse,
                "this TPM is registered under another hardware ID. Check the label".to_owned(),
            )),
            keys::DECOMMISSIONED => Some(stop(
                StopKind::Decommissioned,
                format!("hardware ID {id} was decommissioned and is not registered again"),
            )),
            _ => None,
        },
        StatusCode::UNAUTHORIZED => Some(stop(
            StopKind::LoginRequired,
            "log in again (trustiot-workstation login), then run the same register command"
                .to_owned(),
        )),
        StatusCode::FORBIDDEN if key == keys::WORKSTATION_REFUSED => match api.reason.as_deref() {
            Some("stale") => Some(stop(
                StopKind::ClockSkew,
                format!(
                    "this workstation's clock is more than 300 seconds from the platform's. \
                         {} Set the clock (NTP), then run the same register command",
                    clock_hint(response.date.as_deref(), now)
                ),
            )),
            Some("revoked" | "expired" | "untrusted" | "not_workstation") => Some(stop(
                StopKind::WorkstationRefused,
                "the platform refuses the workstation certificate (revoked, expired, not \
                     issued by this platform's CA, or not a workstation certificate, as the \
                     reason says): have a platform administrator issue a new one \
                     (trustiot-workstation enroll)"
                    .to_owned(),
            )),
            Some("replayed") => Some(stop(
                StopKind::SignatureRejected,
                "the platform saw this request's nonce before. This tool sends a new nonce \
                     on every attempt, so something in between sent the request again, or this \
                     is a bug of the tool. Do not enroll again; report it to a platform \
                     administrator with the log (registrations.jsonl)"
                    .to_owned(),
            )),
            _ => Some(stop(
                StopKind::SignatureRejected,
                "the workstation signature did not reach the platform as it was sent (a \
                     header missing or malformed, or a signature that does not verify): a proxy \
                     or gateway in front of the platform drops or rewrites the \
                     trustiot-workstation-* headers or the body. The workstation certificate is \
                     not the problem; do not enroll again. Report it to a platform \
                     administrator with the log (registrations.jsonl)"
                    .to_owned(),
            )),
        },
        StatusCode::FORBIDDEN if api.code.as_deref() == Some("permission_denied") => Some(stop(
            StopKind::NotPlatformAdministrator,
            "the session is not a platform administrator's; log in as one".to_owned(),
        )),
        StatusCode::UNPROCESSABLE_ENTITY => Some(stop(
            StopKind::Invalid,
            "the platform refuses these fields; nothing was registered".to_owned(),
        )),
        _ => None,
    }
}

/// How far the platform's clock (its `Date` header) is from `now`.
fn clock_hint(date: Option<&str>, now: i64) -> String {
    let Some(server) = date.and_then(|text| DateTime::parse_from_rfc2822(text).ok()) else {
        return "The answer carried no readable Date header.".to_owned();
    };
    let offset = server.timestamp().saturating_sub(now);
    let direction = if offset >= 0 { "ahead of" } else { "behind" };
    format!(
        "The platform's clock read {} (its Date header), {} seconds {direction} this \
         workstation's.",
        server.with_timezone(&Utc).to_rfc3339(),
        offset.unsigned_abs()
    )
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384};

    use super::*;

    fn csr(key: &KeyPair) -> String {
        CertificateParams::new(Vec::new())
            .expect("params")
            .serialize_request(key)
            .expect("a CSR")
            .pem()
            .expect("PEM")
    }

    fn endorsement_key() -> String {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("a key");
        pem_encode("PUBLIC KEY", &key.subject_public_key_info())
    }

    #[test]
    fn hardware_ids_follow_the_gateway_id_rule() {
        let longest = "g".repeat(64);
        let too_long = "g".repeat(65);
        for good in ["gw-0042", "A", "a.b_c-9", longest.as_str()] {
            assert!(is_hardware_id(good), "{good}");
        }
        for bad in [
            "",
            "-gw",
            ".gw",
            "..",
            "gw/1",
            "gw 1",
            "閘道器",
            too_long.as_str(),
        ] {
            assert!(!is_hardware_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_body_is_built_once_from_the_checked_input() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let ek = endorsement_key();
        let noisy = format!("Certificate request:\n{}trailing\n", csr(&key));
        let gateway = Gateway::new("gw-1", noisy.as_bytes(), ek.as_bytes()).expect("valid");
        let body: RegisterGateway = serde_json::from_slice(gateway.body()).expect("a body");
        assert_eq!(body.hardware_id, "gw-1");
        assert!(
            body.certificate_request
                .starts_with("-----BEGIN CERTIFICATE REQUEST-----\n")
        );
        assert!(!body.certificate_request.contains("trailing"));
        assert_eq!(
            gateway.public_key_sha256(),
            sha256_hex(&key.subject_public_key_info())
        );
        let ek_der = one_pem_block(ek.as_bytes(), "PUBLIC KEY").expect("one block");
        assert_eq!(gateway.endorsement_key_sha256(), sha256_hex(&ek_der));
    }

    #[test]
    fn inputs_that_do_not_hold_are_refused_before_anything_is_sent() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let request = csr(&key);
        let ek = endorsement_key();
        let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("a key");
        let tampered = {
            let der = one_pem_block(request.as_bytes(), "CERTIFICATE REQUEST").expect("DER");
            let mut der = der;
            let last = der.len() - 1;
            der[last] ^= 0x01;
            pem_encode("CERTIFICATE REQUEST", &der)
        };
        let cases: [(&str, &str, &str); 7] = [
            ("gw/1", &request, &ek),
            ("gw-1", "", &ek),
            ("gw-1", &format!("{request}{request}"), &ek),
            ("gw-1", &csr(&p384), &ek),
            ("gw-1", &tampered, &ek),
            ("gw-1", &request, &request),
            ("gw-1", &request, &format!("{ek}{ek}")),
        ];
        for (id, request, ek) in cases {
            assert!(
                Gateway::new(id, request.as_bytes(), ek.as_bytes()).is_err(),
                "{id} {request} {ek}"
            );
        }
        assert!(matches!(
            Error::from(Gateway::new("gw/1", request.as_bytes(), ek.as_bytes()).unwrap_err()),
            Error::Usage(_)
        ));
    }

    #[test]
    fn root_fingerprints_take_openssls_form() {
        let digest: Vec<u8> = (0..32).collect();
        let plain = hex(&digest);
        let colons = digest
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(parse_root_sha256(&plain).expect("plain").to_vec(), digest);
        assert_eq!(
            parse_root_sha256(&format!("sha256 Fingerprint={colons}"))
                .expect("openssl")
                .to_vec(),
            digest
        );
        for bad in [
            "",
            "abc",
            plain.get(1..).expect("ASCII"),
            &format!("{plain}00"),
            &plain.replace('0', "g"),
        ] {
            assert!(parse_root_sha256(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_clock_hint_reads_the_date_header() {
        let hint = clock_hint(Some("Mon, 26 Oct 2026 07:33:20 GMT"), 1_793_000_000 - 400);
        assert!(hint.contains("2026-10-26T07:33:20+00:00"), "{hint}");
        assert!(hint.contains("400 seconds ahead of"), "{hint}");
        let hint = clock_hint(Some("Mon, 26 Oct 2026 07:33:20 GMT"), 1_793_000_000 + 900);
        assert!(hint.contains("900 seconds behind"), "{hint}");
        assert!(clock_hint(None, 0).contains("no readable Date"));
        assert!(clock_hint(Some("\u{1b}[2J"), 0).contains("no readable Date"));
    }

    #[test]
    fn a_chain_is_certificate_blocks_only() {
        let one = identity::certificate_pem(b"one");
        let two = identity::certificate_pem(b"two");
        assert_eq!(
            pem_certificates(&format!("{one}{two}")),
            Some(vec![b"one".to_vec(), b"two".to_vec()])
        );
        assert_eq!(pem_certificates(""), Some(Vec::new()));
        let key = pem_encode("PUBLIC KEY", b"key");
        for bad in [
            format!("{one}{key}"),
            one.replace("-----END CERTIFICATE-----", ""),
            format!("{}{two}", one.replace("-----END CERTIFICATE-----\n", "")),
            one.replace(
                "-----BEGIN CERTIFICATE-----",
                "-----BEGIN CERTIFICATE-----\n!",
            ),
        ] {
            assert_eq!(pem_certificates(&bad), None, "{bad}");
        }
    }
}
