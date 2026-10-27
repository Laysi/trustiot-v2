//! The workstation's own identity: an ECDSA P-256 key that never leaves the workstation, its CSR,
//! and the workstation certificate the certs service issues for it (services/certs/README.md,
//! "工作站憑證").
//!
//! Files, in the workstation directory ([`crate::config::workstation_directory`], mode 0700):
//!
//! | File | Mode | Content |
//! | --- | --- | --- |
//! | [`KEY_FILE`] | 0600, never overwritten | the private key, PKCS #8 PEM |
//! | [`CSR_FILE`] | 0644 | the certificate signing request, PEM (public) |
//! | [`CERTIFICATE_FILE`] | 0644 | the workstation certificate, PEM (public), once it passed [`check_certificate`]; re-encoded from the checked DER, never the input as given |

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use contract::certs::{
    IssueWorkstationCertificate, WORKSTATION_PURPOSE_DER, WORKSTATION_SUBJECT_PREFIX,
    WorkstationCertificate,
};
use http::{Method, StatusCode};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
};
use rustls_pki_types::CertificateSigningRequestDer;
use rustls_pki_types::pem::PemObject as _;
use tracing::{debug, info};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::OID_X509_COMMON_NAME;
use x509_parser::prelude::{FromDer as _, X509Certificate};
use zeroize::{Zeroize as _, Zeroizing};

use crate::Error;
use crate::files::{self, Created};
use crate::http::{ApiError, Call, Client};
use crate::secret::{REDACTED, SessionCookie};

/// The private key file.
pub const KEY_FILE: &str = "workstation-key.pem";

/// The certificate signing request file.
pub const CSR_FILE: &str = "workstation.csr";

/// The workstation certificate file.
pub const CERTIFICATE_FILE: &str = "workstation.pem";

/// The certs service's endpoint that issues workstation certificates.
pub const ISSUE_PATH: &str = "/api/certs/v1/workstation-certificates";

/// Largest key file read; a P-256 PKCS #8 PEM key is about 250 bytes.
const MAX_KEY_BYTES: usize = 16 * 1024;

/// Largest CSR or certificate file read.
const MAX_PUBLIC_BYTES: usize = 64 * 1024;

/// Longest workstation name (services/certs/README.md, "欄位").
pub const MAX_NAME_LEN: usize = 48;

/// The common name of the CSR. The CA ignores the CSR's subject; this only labels the file.
const CSR_COMMON_NAME: &str = "trustiot-workstation";

// Assembled with concat! so secret scanners do not read the two markers as a key block.
const PEM_BEGIN: &str = concat!("-----BEGIN ", "PRIVATE KEY", "-----");
const PEM_END: &str = concat!("-----END ", "PRIVATE KEY", "-----");

/// The workstation directory and the paths of its files.
#[derive(Clone, Debug)]
pub struct WorkstationDirectory(PathBuf);

impl WorkstationDirectory {
    /// The directory at `path`; nothing is created yet.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The private key file.
    #[must_use]
    pub fn key(&self) -> PathBuf {
        self.0.join(KEY_FILE)
    }

    /// The CSR file.
    #[must_use]
    pub fn csr(&self) -> PathBuf {
        self.0.join(CSR_FILE)
    }

    /// The certificate file.
    #[must_use]
    pub fn certificate(&self) -> PathBuf {
        self.0.join(CERTIFICATE_FILE)
    }
}

/// The workstation's ECDSA P-256 key pair. Its PKCS #8 copy is wiped from memory when dropped
/// (ring's own key object is not); `Debug` shows nothing of it.
pub struct WorkstationKey(KeyPair);

impl Drop for WorkstationKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for WorkstationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorkstationKey({REDACTED})")
    }
}

impl WorkstationKey {
    /// A new key from the operating system's CSPRNG.
    ///
    /// # Errors
    ///
    /// When the crypto library cannot generate one.
    pub fn generate() -> Result<Self, Error> {
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map(Self)
            .map_err(|_| Error::Failed("cannot generate an ECDSA P-256 key".to_owned()))
    }

    /// Reads the key at `path`: a file of mode 0600 or narrower holding one PKCS #8 PEM ECDSA
    /// P-256 key. The file's bytes and the decoded key are wiped once parsed.
    ///
    /// # Errors
    ///
    /// When the file is refused or does not hold such a key; never with the file's contents.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let bytes = files::read_secret(path, MAX_KEY_BYTES)?;
        let not_a_key = || Error::BadFile {
            path: path.to_owned(),
            reason: "does not hold one PKCS #8 PEM ECDSA P-256 private key \
                     (a PEM block labelled PRIVATE KEY)",
        };
        let der = pkcs8_der(&bytes).ok_or_else(not_a_key)?;
        let key = KeyPair::try_from(der.as_slice())
            .map(Self)
            .map_err(|_| not_a_key())?;
        if key.0.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            return Err(not_a_key());
        }
        Ok(key)
    }

    /// The private key as PKCS #8 PEM, wiped when dropped.
    #[must_use]
    pub fn pem(&self) -> Zeroizing<String> {
        let mut der = Zeroizing::new(self.0.serialize_der());
        let mut body = Zeroizing::new(STANDARD.encode(der.as_slice()));
        der.zeroize();
        let mut pem = Zeroizing::new(String::with_capacity(body.len() + 128));
        pem.push_str(PEM_BEGIN);
        pem.push('\n');
        let mut rest: &str = &body;
        while !rest.is_empty() {
            let (line, tail) = rest.split_at(rest.len().min(64));
            pem.push_str(line);
            pem.push('\n');
            rest = tail;
        }
        pem.push_str(PEM_END);
        pem.push('\n');
        body.zeroize();
        pem
    }

    /// The public key, DER SubjectPublicKeyInfo.
    #[must_use]
    pub fn public_key_der(&self) -> Vec<u8> {
        self.0.subject_public_key_info()
    }

    /// The key pair, to sign with (Laysi/trustiot-v2#615).
    #[must_use]
    pub fn key_pair(&self) -> &KeyPair {
        &self.0
    }

    /// A CSR for this key, PEM, signed with it.
    ///
    /// # Errors
    ///
    /// When the request cannot be built.
    pub fn certificate_request_pem(&self) -> Result<String, Error> {
        let mut params = CertificateParams::new(Vec::new())
            .map_err(|_| Error::Failed("cannot build the CSR".to_owned()))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, CSR_COMMON_NAME);
        params.distinguished_name = name;
        params
            .serialize_request(&self.0)
            .and_then(|request| request.pem())
            .map_err(|_| Error::Failed("cannot build the CSR".to_owned()))
    }
}

const CERTIFICATE_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const CERTIFICATE_END: &str = "-----END CERTIFICATE-----";

/// The DER of the one certificate in `pem`: the text must hold exactly one PEM block, labelled
/// `CERTIFICATE`. Any other block (a private key, a second certificate, a chain) refuses the
/// whole input; text outside the block is ignored, because only the certificate re-encoded from
/// its DER is ever stored ([`certificate_pem`]).
pub(crate) fn certificate_der(pem: &[u8]) -> Result<Vec<u8>, CertificateProblem> {
    let text = std::str::from_utf8(pem).map_err(|_| CertificateProblem::NotOneCertificate)?;
    if text.matches("-----BEGIN").count() != 1 || text.matches("-----END").count() != 1 {
        return Err(CertificateProblem::NotOneCertificate);
    }
    let (_, after_begin) = text
        .split_once(CERTIFICATE_BEGIN)
        .ok_or(CertificateProblem::NotOneCertificate)?;
    let (body, _) = after_begin
        .split_once(CERTIFICATE_END)
        .ok_or(CertificateProblem::NotOneCertificate)?;
    let compact: String = body
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    STANDARD
        .decode(compact.as_bytes())
        .map_err(|_| CertificateProblem::Malformed)
}

/// `der` as a PEM `CERTIFICATE` block: 64 base64 characters a line, LF line ends.
#[must_use]
pub fn certificate_pem(der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut pem = String::with_capacity(body.len() + body.len() / 64 + 64);
    pem.push_str(CERTIFICATE_BEGIN);
    pem.push('\n');
    for line in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str(CERTIFICATE_END);
    pem.push('\n');
    pem
}

/// Whether `serial` is how the certs service writes a serial number: 2 to 40 lower-case hex
/// digits.
pub(crate) fn is_serial(serial: &str) -> bool {
    (2..=40).contains(&serial.len())
        && serial
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `serial` without leading zeros (`0` stays `0`), to compare two spellings of one number.
fn serial_value(serial: &str) -> &str {
    let trimmed = serial.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// The DER bytes of the one `PRIVATE KEY` block in `pem`, wiped when dropped. `None` unless the
/// text is exactly one such block (surrounding white space aside).
fn pkcs8_der(pem: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let text = std::str::from_utf8(pem).ok()?.trim();
    let body = text.strip_prefix(PEM_BEGIN)?.strip_suffix(PEM_END)?;
    if body.contains("-----") {
        return None;
    }
    let mut compact = Zeroizing::new(String::with_capacity(body.len()));
    compact.extend(
        body.chars()
            .filter(|character| !character.is_ascii_whitespace()),
    );
    STANDARD.decode(compact.as_bytes()).ok().map(Zeroizing::new)
}

/// Whether `init` made a key or kept the one already there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOrigin {
    /// A new key was generated and written.
    Created,
    /// The key file existed; it was read (and checked), never overwritten.
    Existing,
}

/// Creates the workstation directory (0700), the key (0600, only when there is none) and writes
/// a CSR for the key. Running it again keeps the key and writes a new CSR for it.
///
/// # Errors
///
/// When a file or the directory is refused or cannot be written.
pub fn init(directory: &WorkstationDirectory) -> Result<KeyOrigin, Error> {
    files::ensure_directory(directory.path())?;
    let key_path = directory.key();
    let generated = WorkstationKey::generate()?;
    let (key, origin) = match files::create_secret(&key_path, generated.pem().as_bytes())? {
        Created::Yes => {
            info!(path = %key_path.display(), "created the workstation key");
            (generated, KeyOrigin::Created)
        }
        Created::AlreadyExists => {
            drop(generated);
            let existing = WorkstationKey::load(&key_path)?;
            info!(path = %key_path.display(), "kept the existing workstation key");
            (existing, KeyOrigin::Existing)
        }
    };
    let csr_path = directory.csr();
    files::replace_public(&csr_path, key.certificate_request_pem()?.as_bytes())?;
    info!(path = %csr_path.display(), "wrote the certificate signing request");
    Ok(origin)
}

/// Whether `name` follows the certs service's rule for workstation names: 1 to 48 ASCII
/// letters, digits, `.`, `_` or `-`, starting with a letter or digit.
#[must_use]
pub fn is_workstation_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.len() <= MAX_NAME_LEN
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Why a certificate is not kept as the workstation certificate.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CertificateProblem {
    #[error("the input does not hold exactly one PEM certificate and no other PEM block")]
    NotOneCertificate,
    #[error("the certificate cannot be parsed")]
    Malformed,
    #[error("it is a CA certificate")]
    Ca,
    #[error("its key usage does not include digitalSignature")]
    NoDigitalSignature,
    #[error("its extended key usage does not include the TrustIoT workstation purpose")]
    NotWorkstationPurpose,
    #[error(
        "its extended key usage includes a TLS purpose (anyExtendedKeyUsage, clientAuth or serverAuth)"
    )]
    TlsPurpose,
    #[error("its subject is not one common name workstation:<name>")]
    Subject,
    #[error("it is not valid before {0}")]
    NotYetValid(String),
    #[error("it expired at {0}")]
    Expired(String),
    #[error("its public key is not the workstation's key")]
    KeyMismatch,
}

/// A certificate that passed [`check_certificate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedCertificate {
    /// The common name, `workstation:<name>`.
    pub subject: String,
    /// The serial number, lower-case hex without leading zeros.
    pub serial: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// The certificate as stored: one PEM block re-encoded from the DER that was checked.
    pub pem: String,
    /// The DER that was checked: what a registration request carries
    /// ([`contract::certs::WORKSTATION_CERTIFICATE_HEADER`]).
    pub der: Vec<u8>,
}

fn time_text(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map_or_else(|| timestamp.to_string(), |time| time.to_rfc3339())
}

/// Checks `pem` as the certificate of `key` at `now` (Unix seconds): exactly one PEM
/// certificate and no other PEM block (see [`certificate_der`]); not a CA; key usage digitalSignature; the TrustIoT workstation purpose in its
/// extended key usage (compared as DER) and no TLS purpose; a subject of one common name
/// `workstation:<name>`; valid at `now`; the public key of `key`. The issuing CA is not checked
/// here: the workstation has no copy of it, and the certs service checks it on every request.
///
/// # Errors
///
/// The first check that fails.
pub fn check_certificate(
    pem: &[u8],
    key: &WorkstationKey,
    now: i64,
) -> Result<CheckedCertificate, CertificateProblem> {
    let der = certificate_der(pem)?;
    let (rest, certificate) =
        X509Certificate::from_der(&der).map_err(|_| CertificateProblem::Malformed)?;
    if !rest.is_empty() {
        return Err(CertificateProblem::Malformed);
    }
    if certificate.is_ca() {
        return Err(CertificateProblem::Ca);
    }
    let digital_signature = certificate
        .key_usage()
        .map_err(|_| CertificateProblem::Malformed)?
        .is_some_and(|usage| usage.value.digital_signature());
    if !digital_signature {
        return Err(CertificateProblem::NoDigitalSignature);
    }
    let usage = certificate
        .extended_key_usage()
        .map_err(|_| CertificateProblem::Malformed)?
        .ok_or(CertificateProblem::NotWorkstationPurpose)?
        .value;
    if !usage
        .other
        .iter()
        .any(|oid| oid.as_bytes() == WORKSTATION_PURPOSE_DER.as_slice())
    {
        return Err(CertificateProblem::NotWorkstationPurpose);
    }
    if usage.any || usage.client_auth || usage.server_auth {
        return Err(CertificateProblem::TlsPurpose);
    }
    let subject = {
        let mut attributes = certificate.subject().iter_attributes();
        let (Some(attribute), None) = (attributes.next(), attributes.next()) else {
            return Err(CertificateProblem::Subject);
        };
        if attribute.attr_type() != &OID_X509_COMMON_NAME {
            return Err(CertificateProblem::Subject);
        }
        let subject = attribute
            .as_str()
            .map_err(|_| CertificateProblem::Subject)?;
        if !subject
            .strip_prefix(WORKSTATION_SUBJECT_PREFIX)
            .is_some_and(is_workstation_name)
        {
            return Err(CertificateProblem::Subject);
        }
        subject.to_owned()
    };
    let validity = certificate.validity();
    let not_before = validity.not_before.timestamp();
    let not_after = validity.not_after.timestamp();
    if now < not_before {
        return Err(CertificateProblem::NotYetValid(time_text(not_before)));
    }
    if now > not_after {
        return Err(CertificateProblem::Expired(time_text(not_after)));
    }
    if certificate.public_key().raw != key.public_key_der().as_slice() {
        return Err(CertificateProblem::KeyMismatch);
    }
    let serial = certificate
        .raw_serial()
        .iter()
        .skip_while(|byte| **byte == 0)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let time = |timestamp: i64| {
        DateTime::<Utc>::from_timestamp(timestamp, 0).ok_or(CertificateProblem::Malformed)
    };
    let (not_before, not_after) = (time(not_before)?, time(not_after)?);
    let pem = certificate_pem(&der);
    drop(certificate);
    Ok(CheckedCertificate {
        subject,
        serial,
        not_before,
        not_after,
        pem,
        der,
    })
}

/// Checks the certificate in `pem` against the workstation's key at `now` and, when it passes,
/// stores it as [`CERTIFICATE_FILE`] (replacing the previous one).
///
/// # Errors
///
/// When the key is refused, the certificate fails a check, or it cannot be written.
pub fn install_certificate(
    directory: &WorkstationDirectory,
    pem: &[u8],
    now: i64,
) -> Result<CheckedCertificate, Error> {
    let key = WorkstationKey::load(&directory.key())?;
    let checked = check_certificate(pem, &key, now)?;
    store_certificate(directory, &checked)?;
    Ok(checked)
}

/// Stores `checked` (its re-encoded PEM, never the input it came from) as [`CERTIFICATE_FILE`].
fn store_certificate(
    directory: &WorkstationDirectory,
    checked: &CheckedCertificate,
) -> Result<(), Error> {
    let path = directory.certificate();
    files::replace_public(&path, checked.pem.as_bytes())?;
    info!(
        path = %path.display(),
        subject = %checked.subject,
        serial = %checked.serial,
        not_after = %checked.not_after.to_rfc3339(),
        "installed the workstation certificate"
    );
    Ok(())
}

/// What `enroll` got: the certs service's record and the certificate as checked and stored.
#[derive(Clone, Debug)]
pub struct Enrolled {
    pub issued: WorkstationCertificate,
    pub installed: CheckedCertificate,
}

/// Has the certs service issue a certificate for the workstation's CSR under `name`, with the
/// platform administrator's session, then installs it (see [`install_certificate`]).
///
/// The key is read and the CSR is checked against it before anything is sent, so the service
/// never issues a certificate this workstation could not use.
///
/// # Errors
///
/// When the key or CSR is refused, the request fails or is answered with an error (a 401 means
/// the session is no longer valid), or the issued certificate fails a check: then the message
/// gives its serial number so a platform administrator can revoke it.
pub async fn enroll(
    client: &Client,
    cookie: &SessionCookie,
    directory: &WorkstationDirectory,
    name: &str,
    now: impl Fn() -> i64,
) -> Result<Enrolled, Error> {
    if !is_workstation_name(name) {
        return Err(Error::Usage(format!(
            "--name {name:?}: 1 to {MAX_NAME_LEN} ASCII letters, digits, '.', '_' or '-', \
             starting with a letter or digit"
        )));
    }
    let key = WorkstationKey::load(&directory.key())?;
    let csr_path = directory.csr();
    let csr = files::read_public(&csr_path, MAX_PUBLIC_BYTES)?;
    let request_key = CertificateSigningRequestDer::from_pem_slice(&csr)
        .ok()
        .and_then(|der| {
            X509CertificationRequest::from_der(der.as_ref())
                .ok()
                .map(|(_, request)| request.certification_request_info.subject_pki.raw.to_vec())
        })
        .ok_or_else(|| Error::BadFile {
            path: csr_path.clone(),
            reason: "is not a PEM certificate signing request",
        })?;
    if request_key != key.public_key_der() {
        return Err(Error::BadFile {
            path: csr_path,
            reason: "is not a request for the workstation key: run init again",
        });
    }
    let csr = String::from_utf8(csr).map_err(|_| Error::BadFile {
        path: csr_path.clone(),
        reason: "is not text",
    })?;
    let body = serde_json::to_vec(&IssueWorkstationCertificate {
        workstation: name.to_owned(),
        certificate_request: csr,
    })
    .map_err(|_| Error::Failed("cannot encode the request".to_owned()))?;
    let response = client
        .send(Call {
            method: Method::POST,
            path: ISSUE_PATH,
            cookie: Some(cookie),
            headers: Vec::new(),
            json: Some(Zeroizing::new(body)),
        })
        .await?;
    if response.status != StatusCode::CREATED {
        return Err(ApiError::new(&Method::POST, ISSUE_PATH, &response).into());
    }
    let issued: WorkstationCertificate = serde_json::from_slice(&response.body).map_err(|_| {
        Error::Failed(format!(
            "POST {ISSUE_PATH} answered 201 with a body that is not a workstation certificate"
        ))
    })?;
    // The serial is server text that ends up on the terminal: only the documented form is shown.
    if !is_serial(&issued.serial) {
        return Err(Error::Failed(format!(
            "POST {ISSUE_PATH} answered 201 with a serial number that is not 2 to 40 lower-case \
             hex digits; nothing was installed. Have a platform administrator revoke the \
             certificates issued for workstation {name} that it does not use."
        )));
    }
    debug!(serial = %issued.serial, "the certs service issued a workstation certificate");
    let not_installed = |serial: &str, error: &dyn fmt::Display| {
        Error::Failed(format!(
            "the certs service issued the certificate with serial {serial}, but it was not \
             installed: {error}. Have a platform administrator revoke that serial."
        ))
    };
    let installed = check_certificate(issued.certificate.as_bytes(), &key, now())
        .map_err(|problem| not_installed(&issued.serial, &Error::Certificate(problem)))?;
    if serial_value(&issued.serial) != serial_value(&installed.serial) {
        return Err(not_installed(
            &installed.serial,
            &format!(
                "the answer gives serial {} but the certificate's serial is {}",
                issued.serial, installed.serial
            ),
        ));
    }
    store_certificate(directory, &installed)
        .map_err(|error| not_installed(&installed.serial, &error))?;
    Ok(Enrolled { issued, installed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_pkcs8_pem_and_never_show_in_debug() {
        let key = WorkstationKey::generate().expect("a key");
        let pem = key.pem();
        assert!(pem.starts_with(PEM_BEGIN), "{}", pem.len());
        let der = pkcs8_der(pem.as_bytes()).expect("one block");
        let again = WorkstationKey(KeyPair::try_from(der.as_slice()).expect("parsed"));
        assert_eq!(again.public_key_der(), key.public_key_der());
        assert_eq!(format!("{key:?}"), "WorkstationKey(<redacted>)");
        let body: String = pem.lines().nth(1).expect("a body line").to_owned();
        assert!(!format!("{key:?}").contains(&body));
    }

    #[test]
    fn only_one_private_key_block_is_accepted() {
        let key = WorkstationKey::generate().expect("a key");
        let pem = key.pem();
        assert!(pkcs8_der(format!("\n{}\n", pem.as_str()).as_bytes()).is_some());
        assert!(pkcs8_der(format!("{}{}", pem.as_str(), pem.as_str()).as_bytes()).is_none());
        let sec1 = pem.replace("PRIVATE KEY", "EC PRIVATE KEY");
        assert!(pkcs8_der(sec1.as_bytes()).is_none());
    }

    #[test]
    fn serials_are_lower_case_hex_compared_without_leading_zeros() {
        let longest = "f".repeat(40);
        let too_long = "f".repeat(41);
        for good in ["00", "4f3a", "0f", longest.as_str()] {
            assert!(is_serial(good), "{good}");
        }
        for bad in [
            "",
            "0",
            "4F3A",
            "4f3g",
            "4f 3a",
            "\u{1b}[31m",
            too_long.as_str(),
        ] {
            assert!(!is_serial(bad), "{bad:?}");
        }
        assert_eq!(serial_value("004f3a"), serial_value("4f3a"));
        assert_eq!(serial_value("00"), "0");
        assert_ne!(serial_value("4f3a"), serial_value("4f3b"));
    }

    #[test]
    fn certificates_are_re_encoded_and_other_blocks_refused() {
        let der: Vec<u8> = (0..=255u8).cycle().take(700).collect();
        let pem = certificate_pem(&der);
        assert!(pem.lines().all(|line| line.len() <= 64), "{pem}");
        assert_eq!(certificate_der(pem.as_bytes()).expect("one block"), der);
        let noisy = format!("Certificate:\n  \u{1b}[2Jtext\n{pem}trailing text\n");
        assert_eq!(
            certificate_der(noisy.as_bytes()).expect("text is ignored"),
            der
        );
        let key = WorkstationKey::generate().expect("a key");
        for refused in [
            format!("{pem}{}", key.pem().as_str()),
            format!("{}{pem}", key.pem().as_str()),
            format!("{pem}{pem}"),
            pem.replace("CERTIFICATE", "TRUSTED CERTIFICATE"),
            String::new(),
        ] {
            assert_eq!(
                certificate_der(refused.as_bytes()),
                Err(CertificateProblem::NotOneCertificate),
                "{refused}"
            );
        }
    }

    #[test]
    fn workstation_names_follow_the_certs_rule() {
        let longest = "x".repeat(MAX_NAME_LEN);
        let too_long = "x".repeat(MAX_NAME_LEN + 1);
        for good in ["line-a-ws-01", "a", "A.b_c-9", longest.as_str()] {
            assert!(is_workstation_name(good), "{good}");
        }
        for bad in ["", "-a", ".a", "a b", "a:b", "工作站", too_long.as_str()] {
            assert!(!is_workstation_name(bad), "{bad}");
        }
    }
}
