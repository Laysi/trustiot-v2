//! The MQTT broker's trust files and reloads (Laysi/trustiot-v2#84; deploy/chart/README.md,
//! "MQTT broker").
//!
//! The broker verifies gateway certificates against the CA certificates in `ca.pem` and the CRLs
//! in `crl.pem`, both published by the certs service (services/certs/README.md, "介面"), and
//! presents the server certificate of the Secret mounted at `tls_dir` (`tls.crt`, `tls.key`).
//! Mosquitto reads all of them at start and again on SIGHUP, which keeps every connection
//! (Mosquitto 2.1.2, `src/signals.c` and `src/security_default.c`). If a file it rereads cannot
//! be loaded, it stops, so nothing here signals it unless every file loads.
//!
//! The gateway listener (Laysi/trustiot-v2#535) is a file of its own, which the broker's
//! configuration includes from `listeners_dir` (`include_dir`, empty when the pod starts): it
//! names the server certificate, so the broker serves the gateway port only once the Secret holds
//! a usable certificate and key. Before the broker certificate CronJob (src/mqtt_certificate.rs)
//! first fills the Secret, for example before the CA ceremony, the broker serves the cloud
//! services on the service port and no gateway, and both containers log why.
//!
//! `migrate mqtt-broker-trust` runs twice in the broker pod:
//!
//! - as an init container with `--once` ([`once`]): installs the trust files before the broker
//!   starts, checks the server certificate and key, and writes the gateway listener's file when
//!   they are usable;
//! - as a sidecar ([`run`]): every `interval` fetches both files again and installs them when they
//!   changed, watches the server certificate Secret, writes the gateway listener's file once it is
//!   usable and restarts the broker once so that it opens the port (Mosquitto adds no listener on
//!   SIGHUP), and sends the broker SIGHUP after the trust files or the server certificate
//!   changed. The pod shares its process namespace, and the sidecar runs as the broker's user, so
//!   it can signal it and nothing else.
//!
//! Trust files are installed into a new directory under `dir`, and then the symlink
//! `dir/current` is replaced in one rename, so the broker always reads a CA file and a CRL file
//! of the same fetch. The last good pair stays on the broker's volume: while the certs service is
//! unreachable or not ready, the broker keeps it. With no pair at all (a fresh volume and no
//! answer, no certs service, or a certs service without a usable issuing CA, whose `crl.pem` is
//! empty), a placeholder pair is installed: a CA generated here, whose key is discarded, and its
//! CRL. The broker then starts and rejects every gateway certificate.

use std::fmt::Write as _;
use std::io::Cursor;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType, IsCa, Issuer,
    KeyIdMethod, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SerialNumber, date_time_ymd,
};
use rustls::pki_types::pem::{self, PemObject as _, SectionKind};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest as _, Sha256};

use crate::config::MqttTrustSettings;
use crate::http::{self, Auth, Client};
use crate::mqtt_accounts::write_atomically;
use crate::{Error, Report};

/// The certs service's CA certificates: the root, then every usable issuing CA.
pub const CA_PATH: &str = "/internal/certs/v1/ca.pem";
/// The certs service's CRLs: one per usable issuing CA; empty when there is none.
pub const CRL_PATH: &str = "/internal/certs/v1/crl.pem";

/// The CA file of a trust directory (Mosquitto's `cafile`).
pub const CA_FILE: &str = "ca.pem";
/// The CRL file of a trust directory (Mosquitto's `crlfile`).
pub const CRL_FILE: &str = "crl.pem";
/// The symlink the broker's configuration names: `<dir>/current/ca.pem` and `crl.pem`.
pub const CURRENT: &str = "current";
/// Present in a trust directory that holds the placeholder pair.
const PLACEHOLDER_MARKER: &str = "PLACEHOLDER";
const SET_PREFIX: &str = "set-";
const TEMPORARY_PREFIX: &str = ".new-";

/// The server certificate chain and key in the Secret's mount.
pub const SERVER_CERTIFICATE: &str = "tls.crt";
pub const SERVER_KEY: &str = "tls.key";

/// The broker's process name (`/proc/<pid>/comm`).
pub const BROKER_PROCESS: &str = "mosquitto";

/// SIGHUP's bit in `/proc/<pid>/status`'s `SigCgt` mask: set once the broker handles SIGHUP.
/// Before that, SIGHUP would end it.
const SIGHUP_CAUGHT: u64 = 1 << (libc::SIGHUP - 1);

/// Expiry warnings (a CRL or the server certificate about to expire) repeat at most this often.
const EXPIRY_LOG_EVERY: Duration = Duration::from_secs(3600);
/// A CRL that expires sooner than this is reported as an error: when it expires, the broker
/// rejects every gateway of its issuing CA.
const CRL_EXPIRY_WARNING_SECONDS: i64 = 24 * 3600;
/// A server certificate that expires sooner than this is reported as a warning.
const SERVER_EXPIRY_WARNING_SECONDS: i64 = 7 * 24 * 3600;

/// A CA file and a CRL file that the broker can load, as written to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustSet {
    /// The CA certificates, PEM, re-encoded from the certificates that passed [`validate`].
    pub ca_pem: String,
    /// The CRLs, PEM, re-encoded likewise.
    pub crl_pem: String,
    /// Whether it is the placeholder pair, which trusts no gateway.
    pub placeholder: bool,
    /// Each CRL's issuer and nextUpdate (seconds since the epoch), for expiry warnings.
    pub crl_expiry: Vec<(String, i64)>,
}

impl TrustSet {
    fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.ca_pem.as_bytes());
        hash.update([0]);
        hash.update(self.crl_pem.as_bytes());
        hash.update([u8::from(self.placeholder)]);
        hash.finalize().into()
    }

    fn same_files(&self, other: &Self) -> bool {
        self.ca_pem == other.ca_pem
            && self.crl_pem == other.crl_pem
            && self.placeholder == other.placeholder
    }
}

fn to_pem(label: &str, blocks: &[Vec<u8>]) -> String {
    let mut text = String::new();
    for der in blocks {
        let encoded = STANDARD.encode(der);
        let _ = writeln!(text, "-----BEGIN {label}-----");
        // Base64 is ASCII, so splitting at any byte keeps whole characters.
        for line in encoded.as_bytes().chunks(64) {
            text.push_str(&String::from_utf8_lossy(line));
            text.push('\n');
        }
        let _ = writeln!(text, "-----END {label}-----");
    }
    text
}

/// The PEM sections of `text` of `wanted` kind, refusing any other kind the parser knows (a
/// private key in a CA file, say). Sections of unknown kinds are skipped; they are not written
/// back.
fn sections(text: &str, wanted: SectionKind, file: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut reader = Cursor::new(text.as_bytes());
    let mut found = Vec::new();
    loop {
        match pem::from_buf(&mut reader) {
            Ok(None) => return Ok(found),
            Ok(Some((kind, der))) if kind == wanted => found.push(der),
            Ok(Some((kind, _))) => {
                return Err(format!(
                    "{file} holds a PEM section of another kind ({kind:?})"
                ));
            }
            Err(error) => return Err(format!("{file} is not valid PEM: {error:?}")),
        }
    }
}

/// Checks a CA file and a CRL file as the certs service serves them and returns them re-encoded.
/// Every block must parse; every certificate must be a CA; at least one must be self-signed (the
/// root, which the broker's chain of every gateway certificate ends at; without it the broker
/// would reject every gateway); every CRL must be signed by a CA of the file with its issuer's
/// name (a CRL the broker would find for that CA but could not verify would make it reject every
/// gateway of that CA) and must have a nextUpdate (without one the broker would trust its
/// revocations for ever, even after the certs service stopped renewing it); every CA that is not
/// self-signed (an issuing CA, the issuer of gateway certificates) must have a CRL, because the
/// broker rejects certificates of an issuer without one. A CRL file without any CRL is refused
/// too: Mosquitto cannot load it. The CRLs' validity periods are not checked here, because the
/// installed files are checked again later; [`outdated_crl`] checks them when they are fetched.
///
/// # Errors
///
/// What is wrong, without echoing the files.
pub fn validate(ca_text: &str, crl_text: &str) -> Result<TrustSet, String> {
    let certificates = sections(ca_text, SectionKind::Certificate, CA_FILE)?;
    if certificates.is_empty() {
        return Err(format!("{CA_FILE} holds no certificate"));
    }
    let crls = sections(crl_text, SectionKind::Crl, CRL_FILE)?;
    if crls.is_empty() {
        return Err(format!("{CRL_FILE} holds no CRL"));
    }
    let mut cas = Vec::new();
    // The parsed CA certificates, which borrow from `certificates`, to verify the CRLs with.
    let mut parsed = Vec::new();
    for (index, der) in certificates.iter().enumerate() {
        let certificate = match x509_parser::parse_x509_certificate(der) {
            Ok(([], certificate)) => certificate,
            _ => return Err(format!("certificate {index} of {CA_FILE} does not parse")),
        };
        let is_ca = certificate
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|constraints| constraints.value.ca);
        let subject = certificate.subject().to_string();
        if !is_ca {
            return Err(format!(
                "certificate {index} of {CA_FILE} ({subject}) is not a CA"
            ));
        }
        let self_signed = certificate.subject().to_string() == certificate.issuer().to_string();
        cas.push((subject, self_signed));
        parsed.push(certificate);
    }
    if !cas.iter().any(|(_, self_signed)| *self_signed) {
        return Err(format!(
            "{CA_FILE} holds no root CA (a self-signed CA certificate)"
        ));
    }
    let mut crl_expiry = Vec::new();
    for (index, der) in crls.iter().enumerate() {
        let crl = match x509_parser::parse_x509_crl(der) {
            Ok(([], crl)) => crl,
            _ => return Err(format!("CRL {index} of {CRL_FILE} does not parse")),
        };
        let issuer = crl.issuer().to_string();
        let Some(next_update) = crl.next_update().map(|time| time.timestamp()) else {
            return Err(format!(
                "CRL {index} of {CRL_FILE} ({issuer}) has no nextUpdate, so it would never expire"
            ));
        };
        let signers: Vec<_> = parsed
            .iter()
            .filter(|certificate| certificate.subject().to_string() == issuer)
            .collect();
        if signers.is_empty() {
            return Err(format!(
                "CRL {index} of {CRL_FILE} is issued by {issuer}, which is not in {CA_FILE}"
            ));
        }
        if !signers
            .iter()
            .any(|certificate| crl.verify_signature(certificate.public_key()).is_ok())
        {
            return Err(format!(
                "CRL {index} of {CRL_FILE} names {issuer} as its issuer but is not signed by it"
            ));
        }
        crl_expiry.push((issuer, next_update));
    }
    for (subject, self_signed) in &cas {
        if !self_signed && !crl_expiry.iter().any(|(issuer, _)| issuer == subject) {
            return Err(format!("the issuing CA {subject} has no CRL in {CRL_FILE}"));
        }
    }
    Ok(TrustSet {
        ca_pem: to_pem("CERTIFICATE", &certificates),
        crl_pem: to_pem("X509 CRL", &crls),
        placeholder: false,
        crl_expiry,
    })
}

/// Why the CRLs of `crl_pem` (a file that passed [`validate`]) cannot be installed at `now`
/// (Unix seconds), if they cannot: one is not valid yet or has expired. The broker would reject
/// every gateway certificate of that CRL's issuer, so a fetched pair with such a CRL is not
/// installed and the installed one stays; the sidecar runs on the broker's node, so it reads the
/// broker's clock.
#[must_use]
pub fn outdated_crl(crl_pem: &str, now: i64) -> Option<String> {
    let crls = match sections(crl_pem, SectionKind::Crl, CRL_FILE) {
        Ok(crls) => crls,
        Err(problem) => return Some(problem),
    };
    for (index, der) in crls.iter().enumerate() {
        let Ok(([], crl)) = x509_parser::parse_x509_crl(der) else {
            return Some(format!("CRL {index} of {CRL_FILE} does not parse"));
        };
        let issuer = crl.issuer().to_string();
        let this_update = crl.last_update().timestamp();
        if this_update > now {
            return Some(format!(
                "the CRL of {issuer} is not valid yet (thisUpdate {this_update}, now {now}): \
                 check the clocks of the certs service's and the broker's nodes"
            ));
        }
        let Some(next_update) = crl.next_update().map(|time| time.timestamp()) else {
            return Some(format!("the CRL of {issuer} has no nextUpdate"));
        };
        if next_update <= now {
            return Some(format!(
                "the CRL of {issuer} expired (nextUpdate {next_update}, now {now}): the certs \
                 service has not renewed it"
            ));
        }
    }
    None
}

/// A CA generated here, whose key is dropped at once, and its empty CRL: a pair the broker loads
/// and that no gateway certificate verifies against.
///
/// # Errors
///
/// When there is no randomness or rcgen fails.
pub fn placeholder() -> Result<TrustSet, String> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|error| format!("cannot generate a key: {error}"))?;
    let mut params = CertificateParams::default();
    params.distinguished_name.push(
        DnType::CommonName,
        "TrustIoT MQTT broker placeholder CA (trusts no gateway)",
    );
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let certificate = params
        .self_signed(&key)
        .map_err(|error| format!("cannot sign the placeholder CA: {error}"))?;
    let issuer = Issuer::new(params, key);
    let crl = CertificateRevocationListParams {
        this_update: date_time_ymd(2000, 1, 1),
        next_update: date_time_ymd(9999, 12, 31),
        crl_number: SerialNumber::from(1_u64),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&issuer)
    .map_err(|error| format!("cannot sign the placeholder CRL: {error}"))?;
    let mut set = validate(
        &to_pem("CERTIFICATE", &[certificate.der().to_vec()]),
        &to_pem("X509 CRL", &[crl.der().to_vec()]),
    )?;
    set.placeholder = true;
    Ok(set)
}

/// What the certs service answered.
#[derive(Debug)]
pub enum Fetched {
    /// A usable pair.
    Set(TrustSet),
    /// It has no usable issuing CA (an empty `crl.pem`): no gateway certificate can be valid.
    NoIssuingCa,
    /// It is not ready (503: no CA loaded yet, or an issuing CA without its first CRL); keep what
    /// is installed.
    NotReady(String),
    /// No answer, an unexpected status, files that fail [`validate`], or a CRL that is not valid
    /// now ([`outdated_crl`]); keep what is installed.
    Failed(String),
}

async fn get(client: &Client, base: &str, path: &str) -> Result<Option<String>, String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let response = client
        .get(&url, Auth::None)
        .await
        .map_err(|error| format!("GET {path}: {error}"))?;
    match response.status.as_u16() {
        200 => Ok(Some(response.body)),
        503 => Ok(None),
        status => Err(format!("GET {path}: status {status}")),
    }
}

/// Fetches and checks both files from the certs service at `base` (`http://<host>:<port>`).
pub async fn fetch(client: &Client, base: &str) -> Fetched {
    let ca = match get(client, base, CA_PATH).await {
        Ok(Some(text)) => text,
        Ok(None) => return Fetched::NotReady("the certs service has not loaded its CA".to_owned()),
        Err(problem) => return Fetched::Failed(problem),
    };
    let crl = match get(client, base, CRL_PATH).await {
        Ok(Some(text)) => text,
        Ok(None) => {
            return Fetched::NotReady(
                "an issuing CA of the certs service has no CRL yet".to_owned(),
            );
        }
        Err(problem) => return Fetched::Failed(problem),
    };
    if crl.trim().is_empty() {
        return Fetched::NoIssuingCa;
    }
    match validate(&ca, &crl) {
        Ok(set) => match outdated_crl(&set.crl_pem, unix_now()) {
            None => Fetched::Set(set),
            Some(problem) => Fetched::Failed(format!(
                "the certs service served a CRL the broker would reject: {problem}"
            )),
        },
        Err(problem) => Fetched::Failed(format!(
            "the certs service served unusable files: {problem}"
        )),
    }
}

/// The pair `dir/current` points at, if it is there and still passes [`validate`].
pub fn installed(dir: &Path) -> Result<Option<TrustSet>, String> {
    let current = dir.join(CURRENT);
    let read = |name: &str| match std::fs::read_to_string(current.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read {}: {error}",
            current.join(name).display()
        )),
    };
    let (Some(ca), Some(crl)) = (read(CA_FILE)?, read(CRL_FILE)?) else {
        return Ok(None);
    };
    let mut set = validate(&ca, &crl)
        .map_err(|problem| format!("the installed trust files are unusable: {problem}"))?;
    set.placeholder = current.join(PLACEHOLDER_MARKER).exists();
    Ok(Some(set))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether `target` is a directory (not a link) that holds exactly the files of `set`.
fn holds(target: &Path, set: &TrustSet) -> bool {
    let same = |name: &str, content: &str| {
        std::fs::read(target.join(name)).is_ok_and(|bytes| bytes == content.as_bytes())
    };
    std::fs::symlink_metadata(target).is_ok_and(|meta| meta.is_dir())
        && same(CA_FILE, &set.ca_pem)
        && same(CRL_FILE, &set.crl_pem)
        && std::fs::symlink_metadata(target.join(PLACEHOLDER_MARKER)).is_ok() == set.placeholder
}

/// Writes `set` into `dir/set-<digest>` and points `dir/current` at it, then removes every other
/// trust directory. The files are readable by everyone: they are public certificates and CRLs.
///
/// A directory of that name that already holds exactly these files (an earlier install of the
/// same pair) is linked again as it is. One that holds anything else, such as files damaged on
/// disk that [`installed`] refused, is never linked again: the pair goes into a new directory
/// with a suffix, and the damaged one is removed with the others.
///
/// # Errors
///
/// When the directory cannot be written.
pub fn install(dir: &Path, set: &TrustSet) -> std::io::Result<()> {
    let base = format!("{SET_PREFIX}{}", hex(&set.digest()[..12]));
    let mut name = base.clone();
    let mut attempt = 0_u32;
    while std::fs::symlink_metadata(dir.join(&name)).is_ok() && !holds(&dir.join(&name), set) {
        attempt += 1;
        name = format!("{base}-{attempt}");
    }
    let target = dir.join(&name);
    if !holds(&target, set) {
        let temporary = dir.join(format!("{TEMPORARY_PREFIX}{name}"));
        remove_any(&temporary)?;
        std::fs::DirBuilder::new().mode(0o755).create(&temporary)?;
        write_atomically(&temporary.join(CA_FILE), set.ca_pem.as_bytes(), 0o644)?;
        write_atomically(&temporary.join(CRL_FILE), set.crl_pem.as_bytes(), 0o644)?;
        if set.placeholder {
            write_atomically(
                &temporary.join(PLACEHOLDER_MARKER),
                b"A CA generated by the trust sidecar, whose key was discarded: the broker \
                  trusts no gateway certificate.\n",
                0o644,
            )?;
        }
        std::fs::rename(&temporary, &target)?;
        sync_dir(dir)?;
    }
    let link = dir.join(format!("{TEMPORARY_PREFIX}{CURRENT}"));
    remove_any(&link)?;
    std::os::unix::fs::symlink(&name, &link)?;
    std::fs::rename(&link, dir.join(CURRENT))?;
    sync_dir(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let ours = file_name.starts_with(SET_PREFIX) || file_name.starts_with(TEMPORARY_PREFIX);
        if ours && file_name != name.as_str() {
            remove_any(&entry.path())?;
        }
    }
    Ok(())
}

/// What the trust files should be now: the certs service's answer, or what is installed, or the
/// placeholder. Returns the set to install when it differs from the installed one.
async fn wanted(
    client: &Client,
    settings: &MqttTrustSettings,
    current: Option<&TrustSet>,
) -> Result<Option<TrustSet>, String> {
    let keep_or_placeholder = |current: Option<&TrustSet>| match current {
        Some(_) => Ok(None),
        None => placeholder().map(Some),
    };
    let fresh = if settings.certs_url.is_empty() {
        if current.is_some_and(|set| set.placeholder) {
            return Ok(None);
        }
        tracing::warn!(
            "no certs service is deployed: the broker trusts no gateway certificate (placeholder \
             CA)"
        );
        return placeholder().map(Some);
    } else {
        match fetch(client, &settings.certs_url).await {
            Fetched::Set(set) => set,
            Fetched::NoIssuingCa => {
                if current.is_some_and(|set| set.placeholder) {
                    return Ok(None);
                }
                tracing::warn!(
                    "the certs service has no usable issuing CA: the broker trusts no gateway \
                     certificate (placeholder CA)"
                );
                return placeholder().map(Some);
            }
            Fetched::NotReady(why) => {
                tracing::info!(reason = %why, installed = current.is_some(), "keeping the installed trust files");
                return keep_or_placeholder(current);
            }
            Fetched::Failed(why) => {
                tracing::warn!(reason = %why, installed = current.is_some(), "keeping the installed trust files");
                return keep_or_placeholder(current);
            }
        }
    };
    Ok(match current {
        Some(set) if set.same_files(&fresh) => None,
        _ => Some(fresh),
    })
}

/// Brings `settings.dir` up to date; returns the installed pair.
async fn sync_trust(
    client: &Client,
    settings: &MqttTrustSettings,
    report: &mut Report,
) -> Result<TrustSet, Error> {
    let current = match installed(&settings.dir) {
        Ok(current) => current,
        Err(problem) => {
            tracing::warn!(problem = %problem, "replacing the installed trust files");
            None
        }
    };
    let Some(new) = wanted(client, settings, current.as_ref())
        .await
        .map_err(Error::MqttBroker)?
    else {
        return current.ok_or_else(|| {
            Error::MqttBroker("no trust files are installed and none were fetched".to_owned())
        });
    };
    install(&settings.dir, &new).map_err(|error| {
        Error::MqttBroker(format!(
            "cannot install the trust files in {}: {error}",
            settings.dir.display()
        ))
    })?;
    report.change(format!(
        "installed the broker's trust files ({})",
        if new.placeholder {
            "placeholder CA: no gateway is trusted"
        } else {
            "the certs service's CA certificates and CRLs"
        }
    ));
    Ok(new)
}

/// The server certificate and key, as the broker would load them.
#[derive(Debug)]
pub struct ServerFiles {
    /// SHA-256 of both files, to notice a change.
    pub digest: [u8; 32],
    /// The certificate's notAfter, seconds since the epoch.
    pub not_after: i64,
}

/// Reads `tls_dir/tls.crt` (the chain, server certificate first) and `tls_dir/tls.key` through the
/// Secret mount's symlinks and checks that the key belongs to the certificate.
///
/// # Errors
///
/// What is wrong, without echoing the files.
pub fn check_server(tls_dir: &Path) -> Result<ServerFiles, String> {
    let certificate_path = tls_dir.join(SERVER_CERTIFICATE);
    let key_path = tls_dir.join(SERVER_KEY);
    let chain_bytes = std::fs::read(&certificate_path)
        .map_err(|error| format!("cannot read {}: {error}", certificate_path.display()))?;
    let key_bytes = zeroize::Zeroizing::new(
        std::fs::read(&key_path)
            .map_err(|error| format!("cannot read {}: {error}", key_path.display()))?,
    );
    check_pair(&chain_bytes, &key_bytes)
}

/// Checks that `chain_bytes` (`tls.crt`: PEM, the server certificate first) and `key_bytes`
/// (`tls.key`: a PEM private key) load and belong together, as the broker needs them; the
/// broker's certificate issuer checks the Secret with the same rules (src/mqtt_certificate.rs).
///
/// # Errors
///
/// What is wrong, without echoing either.
pub fn check_pair(chain_bytes: &[u8], key_bytes: &[u8]) -> Result<ServerFiles, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_bytes)
        .collect::<Result<_, _>>()
        .map_err(|error| format!("{SERVER_CERTIFICATE} is not valid PEM: {error:?}"))?;
    let leaf = chain
        .first()
        .ok_or_else(|| format!("{SERVER_CERTIFICATE} holds no certificate"))?;
    let not_after = match x509_parser::parse_x509_certificate(leaf) {
        Ok(([], certificate)) => certificate.validity().not_after.timestamp(),
        _ => {
            return Err(format!(
                "the first certificate of {SERVER_CERTIFICATE} does not parse"
            ));
        }
    };
    let key = PrivateKeyDer::from_pem_slice(key_bytes)
        .map_err(|_| format!("{SERVER_KEY} holds no private key in PEM"))?;
    let provider = rustls::crypto::ring::default_provider();
    let signing_key = provider
        .key_provider
        .load_private_key(key)
        .map_err(|error| format!("{SERVER_KEY} is not a usable key: {error}"))?;
    rustls::sign::CertifiedKey::new(chain, signing_key)
        .keys_match()
        .map_err(|error| {
            format!(
                "{SERVER_KEY} does not belong to the certificate in {SERVER_CERTIFICATE}: {error}"
            )
        })?;
    let mut hash = Sha256::new();
    hash.update(chain_bytes);
    hash.update([0]);
    hash.update(key_bytes);
    Ok(ServerFiles {
        digest: hash.finalize().into(),
        not_after,
    })
}

/// The broker's process ID: the one process under `proc_root` named [`BROKER_PROCESS`] that runs
/// as `uid` and already handles SIGHUP.
///
/// # Errors
///
/// When there is none (the broker is not running, or still starting) or more than one.
pub fn find_broker(proc_root: &Path, uid: u32) -> Result<i32, String> {
    let entries = std::fs::read_dir(proc_root)
        .map_err(|error| format!("cannot list {}: {error}", proc_root.display()))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 1)
        else {
            continue;
        };
        let path = entry.path();
        // A process that ended meanwhile is simply skipped.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.uid() != uid {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim_end_matches('\n') != BROKER_PROCESS {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(path.join("status")) else {
            continue;
        };
        let caught = status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .is_some_and(|mask| mask & SIGHUP_CAUGHT != 0);
        found.push((pid, caught));
    }
    match found.as_slice() {
        [] => Err(format!("no {BROKER_PROCESS} process runs as user {uid}")),
        [(pid, true)] => Ok(*pid),
        [(pid, false)] => Err(format!(
            "{BROKER_PROCESS} (pid {pid}) does not handle SIGHUP yet; it is still starting"
        )),
        _ => Err(format!(
            "{} {BROKER_PROCESS} processes run as user {uid}; signalling none",
            found.len()
        )),
    }
}

/// Sends SIGHUP to `pid`.
///
/// # Errors
///
/// As [`send_signal`].
pub fn send_sighup(pid: i32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGHUP)
}

/// Sends `signal` to `pid`.
///
/// # Errors
///
/// When `pid` is not a single process (1 or below) or the kernel refuses.
#[allow(
    unsafe_code,
    reason = "the standard library cannot signal another process; kill(2) has no safe binding \
              among the workspace's dependencies"
)]
pub fn send_signal(pid: i32, signal: libc::c_int) -> std::io::Result<()> {
    // 0 and negative values signal process groups, 1 is the pod's pause process.
    if pid <= 1 {
        return Err(std::io::Error::other(format!("{pid} is not a broker pid")));
    }
    // SAFETY: kill(2) takes two integers and touches no memory of this process; `pid` is a single
    // process above 1, so it cannot address a process group or every process.
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The port of the gateway listener's configuration: the number after its only `listener` line.
///
/// # Errors
///
/// When the text holds no `listener <port>` line, or more than one.
pub fn listener_port(configuration: &str) -> Result<u16, String> {
    let mut ports = configuration.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("listener"))
            .then(|| words.next().and_then(|port| port.parse::<u16>().ok()))
    });
    match (ports.next(), ports.next()) {
        (Some(Some(port)), None) if port != 0 => Ok(port),
        (None, _) => Err("it has no listener line".to_owned()),
        (Some(_), None) => Err("its listener line names no TCP port".to_owned()),
        (Some(_), Some(_)) => Err("it has more than one listener line".to_owned()),
    }
}

/// The gateway listener's configuration and port, read from `settings.gateway_listener`.
fn gateway_listener(settings: &MqttTrustSettings) -> Result<(String, u16), Error> {
    let path = &settings.gateway_listener;
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::MqttBroker(format!(
            "cannot read the gateway listener's configuration {}: {error}",
            path.display()
        ))
    })?;
    let port = listener_port(&text).map_err(|problem| {
        Error::MqttBroker(format!(
            "the gateway listener's configuration {}: {problem}",
            path.display()
        ))
    })?;
    Ok((text, port))
}

/// Whether a TCP socket of this network namespace listens on `port`, from `proc_root`'s
/// `net/tcp` and `net/tcp6`. The containers of a pod share one network namespace, so the sidecar
/// sees the broker's sockets.
///
/// # Errors
///
/// When neither file can be read.
pub fn listens_on(proc_root: &Path, port: u16) -> Result<bool, String> {
    let wanted = format!("{port:04X}");
    let mut read_any = false;
    for table in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(proc_root.join("net").join(table)) else {
            continue;
        };
        read_any = true;
        // `sl local_address rem_address st ...`: the local address is `<address>:<port>` in hex,
        // and state 0A is LISTEN.
        let listening = text.lines().skip(1).any(|line| {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1);
            let state = fields.nth(1);
            state == Some("0A")
                && local
                    .and_then(|local| local.rsplit_once(':'))
                    .is_some_and(|(_, local_port)| local_port.eq_ignore_ascii_case(&wanted))
        });
        if listening {
            return Ok(true);
        }
    }
    if read_any {
        Ok(false)
    } else {
        Err(format!(
            "cannot read {}/net/tcp or tcp6",
            proc_root.display()
        ))
    }
}

/// The file in the listeners directory that holds the gateway listener.
pub const GATEWAY_LISTENER_FILE: &str = "gateway.conf";

/// Writes the gateway listener into `settings.listeners_dir`, so the broker opens the gateway
/// port the next time it starts.
fn enable_gateway_listener(settings: &MqttTrustSettings, text: &str) -> Result<(), Error> {
    let path = settings.listeners_dir.join(GATEWAY_LISTENER_FILE);
    write_atomically(&path, text.as_bytes(), 0o644)
        .map_err(|error| Error::MqttBroker(format!("cannot write {}: {error}", path.display())))
}

/// Why the broker serves no gateway port, for the logs: an empty Secret (before the first
/// certificate) reads better than a missing file.
fn no_gateway_port(tls_dir: &Path, problem: &str) -> String {
    if !tls_dir.join(SERVER_CERTIFICATE).exists() && !tls_dir.join(SERVER_KEY).exists() {
        "the server certificate Secret holds no certificate yet; the broker certificate CronJob \
         fills it once the certs service can sign (deploy/chart/README.md, \"MQTT broker\")"
            .to_owned()
    } else {
        format!("the server certificate Secret is unusable: {problem}")
    }
}

/// The init container: installs the trust files (one attempt at the certs service, so the broker
/// starts without it), checks the server certificate and key, and enables the gateway listener
/// when they are usable. Without a usable pair the broker starts without its gateway port, so it
/// never serves gateways a certificate they cannot verify, and still serves the cloud services on
/// the service port; the sidecar opens the gateway port once the Secret is filled.
///
/// # Errors
///
/// When the trust or listeners directory cannot be written, or the gateway listener's
/// configuration cannot be read.
pub async fn once(settings: &MqttTrustSettings, report: &mut Report) -> Result<(), Error> {
    create_dir(&settings.dir)?;
    let set = sync_trust(&http::plain(), settings, report).await?;
    let (listener, port) = gateway_listener(settings)?;
    match check_server(&settings.tls_dir) {
        Ok(server) => {
            log_expiry(&set, &server, unix_now());
            enable_gateway_listener(settings, &listener)?;
            tracing::info!(
                placeholder = set.placeholder,
                gateway_port = port,
                "the broker's trust files and server certificate are in place; it serves the \
                 gateway port"
            );
        }
        Err(problem) => {
            tracing::warn!(
                placeholder = set.placeholder,
                gateway_port = port,
                problem = %no_gateway_port(&settings.tls_dir, &problem),
                "the broker starts without its gateway port; it opens once the server \
                 certificate Secret holds a usable certificate and key"
            );
        }
    }
    Ok(())
}

fn create_dir(dir: &Path) -> Result<(), Error> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(|error| Error::MqttBroker(format!("cannot create {}: {error}", dir.display())))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn log_expiry(set: &TrustSet, server: &ServerFiles, now: i64) {
    if !set.placeholder {
        for (issuer, next_update) in &set.crl_expiry {
            if *next_update - now < CRL_EXPIRY_WARNING_SECONDS {
                tracing::error!(
                    issuer = %issuer,
                    next_update,
                    "the broker's CRL of this issuing CA expires within a day or has expired; \
                     then every gateway of the CA is rejected. Check that the certs service \
                     renews it and that this sidecar reaches the certs service"
                );
            }
        }
    }
    if server.not_after <= now {
        tracing::error!(
            not_after = server.not_after,
            "the broker's server certificate has expired: gateways refuse the broker. Check the \
             runs of the broker certificate CronJob"
        );
    } else if server.not_after - now < SERVER_EXPIRY_WARNING_SECONDS {
        tracing::warn!(
            not_after = server.not_after,
            "the broker's server certificate expires within 7 days; the broker certificate \
             CronJob renews it at half of its validity, so check its runs"
        );
    }
}

/// What the sidecar knows between its rounds.
#[derive(Default)]
struct Loaded {
    /// What it last had the broker load.
    trust: Option<[u8; 32]>,
    server: Option<[u8; 32]>,
    /// The broker it asked to stop so that it starts again with the gateway listener.
    restarted: Option<i32>,
    /// When it last logged that the broker serves no gateway port.
    no_gateway_logged: Option<Instant>,
}

/// The broker as the sidecar finds it: its processes under `proc_root` that run as `uid`, and the
/// gateway listener it should open.
struct Broker<'a> {
    proc_root: &'a Path,
    uid: u32,
    listener: &'a str,
    port: u16,
}

/// The sidecar: keeps the trust files current, opens the gateway port once the server certificate
/// Secret is usable, and has the broker reload the trust files and the server certificate
/// whenever either changed (and once at its own start, so a change it could not see before it
/// started is loaded too). Runs until the pod stops.
///
/// # Errors
///
/// When the trust directory cannot be created, the gateway listener's configuration cannot be
/// read, or this process's user cannot be read.
pub async fn run(settings: &MqttTrustSettings, proc_root: &Path) -> Result<(), Error> {
    create_dir(&settings.dir)?;
    let (listener, port) = gateway_listener(settings)?;
    let uid = std::fs::metadata(proc_root.join("self"))
        .map_err(|error| Error::MqttBroker(format!("cannot read this process's user: {error}")))?
        .uid();
    let client = http::plain();
    let mut loaded = Loaded::default();
    let mut last_expiry_log = None::<Instant>;
    let broker = Broker {
        proc_root,
        uid,
        listener: &listener,
        port,
    };
    loop {
        let mut report = Report::default();
        match sync_trust(&client, settings, &mut report).await {
            Ok(set) => {
                reload_if_changed(settings, &broker, &set, &mut loaded, &mut last_expiry_log);
            }
            Err(error) => {
                tracing::error!(error = %error, "cannot bring the trust files up to date");
            }
        }
        tokio::time::sleep(settings.interval).await;
    }
}

/// One round of the sidecar after the trust files are current: the gateway listener (written
/// once the server certificate is usable, and the broker restarted once when it started without
/// it), then a reload when the trust files or the server certificate changed.
///
/// Mosquitto opens its listeners at start only (2.1 refuses to add one on SIGHUP), and it opens
/// them before it handles SIGHUP; so a broker that handles SIGHUP but does not listen on the
/// gateway port started before the listener's file existed, and SIGTERM (which Mosquitto takes
/// as a clean shutdown, saving its state) has the kubelet start it again with the file. The
/// sidecar never sends SIGHUP to such a broker: the reload would fail on the listener it cannot
/// add.
fn reload_if_changed(
    settings: &MqttTrustSettings,
    broker: &Broker<'_>,
    set: &TrustSet,
    loaded: &mut Loaded,
    last_expiry_log: &mut Option<Instant>,
) {
    let server = check_server(&settings.tls_dir);
    if !settings.listeners_dir.join(GATEWAY_LISTENER_FILE).exists() {
        match &server {
            Ok(_) => match enable_gateway_listener(settings, broker.listener) {
                Ok(()) => tracing::info!(
                    gateway_port = broker.port,
                    "the server certificate Secret holds a usable certificate and key: enabled \
                     the gateway listener"
                ),
                Err(error) => {
                    tracing::error!(error = %error, "cannot enable the gateway listener");
                    return;
                }
            },
            Err(problem) => {
                if loaded
                    .no_gateway_logged
                    .is_none_or(|at| at.elapsed() >= EXPIRY_LOG_EVERY)
                {
                    tracing::warn!(
                        gateway_port = broker.port,
                        problem = %no_gateway_port(&settings.tls_dir, problem),
                        "the broker serves no gateway port; it opens once the server certificate \
                         Secret holds a usable certificate and key"
                    );
                    loaded.no_gateway_logged = Some(Instant::now());
                }
                // Nothing the broker loaded names the trust files or the Secret yet.
                return;
            }
        }
    }
    let pid = match find_broker(broker.proc_root, broker.uid) {
        Ok(pid) => pid,
        Err(why) => {
            tracing::info!(reason = %why, "not reloading the broker yet");
            return;
        }
    };
    match listens_on(broker.proc_root, broker.port) {
        Ok(true) => {}
        Ok(false) => {
            restart_for_listener(broker, pid, loaded);
            return;
        }
        Err(problem) => {
            tracing::error!(
                problem = %problem,
                "cannot tell whether the broker serves the gateway port"
            );
            return;
        }
    }
    let server = match server {
        Ok(server) => server,
        Err(problem) => {
            // Reloading now would stop the broker; it keeps what it loaded before.
            tracing::error!(
                problem = %problem,
                "the server certificate Secret is unusable: not reloading the broker until it is \
                 fixed"
            );
            return;
        }
    };
    if last_expiry_log.is_none_or(|at| at.elapsed() >= EXPIRY_LOG_EVERY) {
        log_expiry(set, &server, unix_now());
        *last_expiry_log = Some(Instant::now());
    }
    let trust = set.digest();
    if loaded.trust == Some(trust) && loaded.server == Some(server.digest) {
        return;
    }
    match send_sighup(pid) {
        Ok(()) => {
            tracing::info!(
                pid,
                trust_changed = loaded.trust != Some(trust),
                server_certificate_changed = loaded.server != Some(server.digest),
                placeholder = set.placeholder,
                "asked the broker to reload its trust files and server certificate (SIGHUP)"
            );
            loaded.trust = Some(trust);
            loaded.server = Some(server.digest);
        }
        Err(error) => tracing::error!(pid, error = %error, "cannot signal the broker"),
    }
}

/// The broker `pid` handles SIGHUP but does not listen on the gateway port although the
/// listener's file exists: it started before the file did. A broker still starting is never
/// taken for one: Mosquitto 2.1.2's `main` (src/mosquitto.c) calls `listeners__start` before
/// `signal__setup`, so its listeners are open by the time `find_broker` accepts it. Asks it to
/// stop once, so the kubelet starts it again with the listener; a broker started after that which
/// still does not listen is reported, never stopped again.
fn restart_for_listener(broker: &Broker<'_>, pid: i32, loaded: &mut Loaded) {
    match loaded.restarted {
        // Still shutting down.
        Some(restarted) if restarted == pid => {}
        Some(_) => {
            if loaded
                .no_gateway_logged
                .is_none_or(|at| at.elapsed() >= EXPIRY_LOG_EVERY)
            {
                tracing::error!(
                    pid,
                    gateway_port = broker.port,
                    "the broker started again with the gateway listener but does not listen on \
                     its port; see the mosquitto container's log"
                );
                loaded.no_gateway_logged = Some(Instant::now());
            }
        }
        None => match send_signal(pid, libc::SIGTERM) {
            Ok(()) => {
                tracing::info!(
                    pid,
                    gateway_port = broker.port,
                    "the broker started without the gateway listener: asked it to stop (SIGTERM) \
                     so that it starts again with it"
                );
                loaded.restarted = Some(pid);
                loaded.trust = None;
                loaded.server = None;
            }
            Err(error) => tracing::error!(pid, error = %error, "cannot signal the broker"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::path::PathBuf;

    use rcgen::{CertifiedIssuer, ExtendedKeyUsagePurpose, RevokedCertParams};

    use super::*;

    fn temporary_dir(label: &str) -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).expect("randomness");
        let dir = std::env::temp_dir().join(format!(
            "mqtt-trust-{label}-{}-{}",
            std::process::id(),
            hex(&random)
        ));
        std::fs::create_dir(&dir).expect("temporary directory");
        dir
    }

    struct Pki {
        root: String,
        issuing: String,
        crl: String,
        /// Another issuing CA of the same root, without a CRL.
        second: String,
    }

    fn ca(name: &str) -> (CertificateParams, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        (params, key)
    }

    fn crl_of(issuer: &Issuer<'_, KeyPair>, revoked: &[u64]) -> String {
        crl_dated(issuer, revoked, (2026, 1, 1), (2099, 1, 1))
    }

    fn crl_dated(
        issuer: &Issuer<'_, KeyPair>,
        revoked: &[u64],
        this_update: (i32, u8, u8),
        next_update: (i32, u8, u8),
    ) -> String {
        let crl = CertificateRevocationListParams {
            this_update: date_time_ymd(this_update.0, this_update.1, this_update.2),
            next_update: date_time_ymd(next_update.0, next_update.1, next_update.2),
            crl_number: SerialNumber::from(7_u64),
            issuing_distribution_point: None,
            revoked_certs: revoked
                .iter()
                .map(|serial| RevokedCertParams {
                    serial_number: SerialNumber::from(*serial),
                    revocation_time: date_time_ymd(2026, 1, 2),
                    reason_code: None,
                    invalidity_date: None,
                })
                .collect(),
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(issuer)
        .expect("CRL");
        to_pem("X509 CRL", &[crl.der().to_vec()])
    }

    fn pki() -> Pki {
        let (root_params, root_key) = ca("Test root");
        let root = CertifiedIssuer::self_signed(root_params, root_key).expect("root");
        let (issuing_params, issuing_key) = ca("Test issuing CA");
        let issuing =
            CertifiedIssuer::signed_by(issuing_params, issuing_key, &root).expect("issuing CA");
        let crl = crl_of(&issuing, &[42]);
        let (second_params, second_key) = ca("Second issuing CA");
        let second =
            CertifiedIssuer::signed_by(second_params, second_key, &root).expect("second CA");
        Pki {
            root: root.pem(),
            issuing: issuing.pem(),
            crl,
            second: second.pem(),
        }
    }

    /// The length of the DER header at the start of `bytes` and the length of its content.
    fn der_header(bytes: &[u8]) -> (usize, usize) {
        if bytes[1] < 0x80 {
            (2, usize::from(bytes[1]))
        } else {
            let count = usize::from(bytes[1] & 0x7f);
            let length = bytes[2..2 + count]
                .iter()
                .fold(0, |length, byte| (length << 8) | usize::from(*byte));
            (2 + count, length)
        }
    }

    /// A DER SEQUENCE holding `content`.
    fn der_sequence(content: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        if content.len() < 0x80 {
            out.push(u8::try_from(content.len()).expect("short"));
        } else {
            let length: Vec<u8> = content
                .len()
                .to_be_bytes()
                .into_iter()
                .skip_while(|byte| *byte == 0)
                .collect();
            out.push(0x80 | u8::try_from(length.len()).expect("few bytes"));
            out.extend(length);
        }
        out.extend(content);
        out
    }

    /// The CRL `der` without its nextUpdate (the signature no longer matches; nothing here
    /// reaches it).
    fn without_next_update(der: &[u8]) -> Vec<u8> {
        let (header, length) = der_header(der);
        let list = &der[header..header + length];
        let (tbs_header, tbs_length) = der_header(list);
        let tbs = &list[tbs_header..tbs_header + tbs_length];
        let mut fields = Vec::new();
        let mut at = 0;
        while at < tbs.len() {
            let (field_header, field_length) = der_header(&tbs[at..]);
            fields.push(&tbs[at..at + field_header + field_length]);
            at += field_header + field_length;
        }
        // version, signature, issuer, thisUpdate, nextUpdate (UTCTime or GeneralizedTime), ...
        assert!(
            matches!(fields[4][0], 0x17 | 0x18),
            "nextUpdate is the fifth field"
        );
        fields.remove(4);
        let tbs = der_sequence(&fields.concat());
        der_sequence(&[tbs.as_slice(), &list[tbs_header + tbs_length..]].concat())
    }

    #[test]
    fn a_crl_without_next_update_is_refused() {
        let pki = pki();
        let der = sections(&pki.crl, SectionKind::Crl, CRL_FILE)
            .expect("PEM")
            .remove(0);
        let stripped = without_next_update(&der);
        let ([], parsed) = x509_parser::parse_x509_crl(&stripped).expect("a CRL") else {
            panic!("trailing bytes");
        };
        assert!(parsed.next_update().is_none());
        let crl = to_pem("X509 CRL", &[stripped]);
        let problem = validate(&format!("{}{}", pki.root, pki.issuing), &crl).expect_err("refused");
        assert!(problem.contains("has no nextUpdate"), "{problem}");
        let problem = outdated_crl(&crl, 1_780_272_000).expect("refused");
        assert!(problem.contains("has no nextUpdate"), "{problem}");
    }

    #[test]
    fn a_complete_pair_passes_and_is_re_encoded() {
        let pki = pki();
        // Text around the blocks and an unknown section are dropped.
        let ca = format!(
            "root:\n{}\n-----BEGIN OPAQUE THING-----\nAAAA\n-----END OPAQUE THING-----\n{}",
            pki.root, pki.issuing
        );
        let set = validate(&ca, &pki.crl).expect("valid");
        assert_eq!(set.ca_pem.matches("BEGIN CERTIFICATE").count(), 2);
        assert!(!set.ca_pem.contains("root:") && !set.ca_pem.contains("OPAQUE"));
        assert_eq!(set.crl_pem.matches("BEGIN X509 CRL").count(), 1);
        assert_eq!(set.crl_expiry.len(), 1);
        assert!(set.crl_expiry[0].0.contains("Test issuing CA"));
        assert!(!set.placeholder);
        // Re-encoding is stable.
        assert_eq!(validate(&set.ca_pem, &set.crl_pem).expect("valid"), set);
    }

    #[test]
    fn incomplete_or_foreign_pairs_are_refused() {
        let pki = pki();
        let both = format!("{}{}", pki.root, pki.issuing);
        let (other_params, other_key) = ca("Other issuing CA");
        let other = Issuer::new(other_params, other_key);
        let foreign_crl = crl_of(&other, &[]);
        // The issuing CA's name, another key: the broker would find this CRL for the issuing CA
        // and fail to verify it.
        let (forged_params, forged_key) = ca("Test issuing CA");
        let forged_crl = crl_of(&Issuer::new(forged_params, forged_key), &[]);
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let mut leaf_params = CertificateParams::default();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "gw-1");
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = leaf_params.self_signed(&key).expect("leaf").pem();
        let cases = [
            ("", pki.crl.as_str(), "holds no certificate"),
            (both.as_str(), "", "holds no CRL"),
            (
                &format!("{both}{}", pki.second),
                pki.crl.as_str(),
                "Second issuing CA has no CRL",
            ),
            (pki.issuing.as_str(), pki.crl.as_str(), "holds no root CA"),
            (both.as_str(), foreign_crl.as_str(), "not in ca.pem"),
            (both.as_str(), forged_crl.as_str(), "is not signed by it"),
            (pki.root.as_str(), pki.crl.as_str(), "not in ca.pem"),
            (leaf.as_str(), pki.crl.as_str(), "is not a CA"),
            (pki.crl.as_str(), pki.crl.as_str(), "another kind"),
            (
                &format!("{both}{}", key.serialize_pem()),
                pki.crl.as_str(),
                "another kind",
            ),
            (
                both.as_str(),
                "-----BEGIN X509 CRL-----\n!!!!\n-----END X509 CRL-----\n",
                "not valid PEM",
            ),
            (
                both.as_str(),
                "-----BEGIN X509 CRL-----\nAAAA\n-----END X509 CRL-----\n",
                "does not parse",
            ),
        ];
        for (ca, crl, expected) in cases {
            let problem = validate(ca, crl).expect_err(expected);
            assert!(problem.contains(expected), "{problem:?} lacks {expected:?}");
        }
    }

    #[test]
    fn only_crls_valid_now_are_fetched() {
        let (root_params, root_key) = ca("Dated root");
        let root = CertifiedIssuer::self_signed(root_params, root_key).expect("root");
        let (params, key) = ca("Dated issuing CA");
        let issuer = CertifiedIssuer::signed_by(params, key, &root).expect("issuing CA");
        // 2026-06-01T00:00:00Z.
        let now = 1_780_272_000;
        let current = crl_dated(&issuer, &[], (2026, 5, 1), (2026, 7, 1));
        assert_eq!(outdated_crl(&current, now), None);
        let expired = crl_dated(&issuer, &[], (2026, 4, 1), (2026, 5, 1));
        let problem = outdated_crl(&expired, now).expect("expired");
        assert!(problem.contains("expired"), "{problem}");
        let early = crl_dated(&issuer, &[], (2026, 6, 2), (2026, 7, 1));
        let problem = outdated_crl(&early, now).expect("not valid yet");
        assert!(problem.contains("not valid yet"), "{problem}");
        // One outdated CRL among current ones is enough.
        let problem = outdated_crl(&format!("{current}{expired}"), now).expect("expired");
        assert!(problem.contains("Dated issuing CA"), "{problem}");
        // validate does not look at the clock: an installed pair whose CRL has since expired
        // stays readable, so it is kept rather than replaced by the placeholder.
        let chain = format!("{}{}", root.pem(), issuer.pem());
        assert!(validate(&chain, &expired).is_ok());
        assert!(validate(&chain, &early).is_ok());
    }

    #[test]
    fn the_placeholder_trusts_a_ca_nobody_holds() {
        let first = placeholder().expect("placeholder");
        let second = placeholder().expect("placeholder");
        assert!(first.placeholder);
        assert_ne!(first.ca_pem, second.ca_pem, "a new key every time");
        assert!(first.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(first.crl_pem.contains("BEGIN X509 CRL"));
    }

    #[test]
    fn installing_swaps_the_link_and_leaves_one_directory() {
        let dir = temporary_dir("install");
        assert_eq!(installed(&dir).expect("readable"), None);
        let first = placeholder().expect("placeholder");
        install(&dir, &first).expect("installed");
        assert_eq!(installed(&dir).expect("readable"), Some(first.clone()));
        let pki = pki();
        let second = validate(&format!("{}{}", pki.root, pki.issuing), &pki.crl).expect("valid");
        install(&dir, &second).expect("installed");
        // Installing the same pair again changes nothing.
        install(&dir, &second).expect("installed");
        assert_eq!(installed(&dir).expect("readable"), Some(second.clone()));
        let link = std::fs::read_link(dir.join(CURRENT)).expect("a symlink");
        assert!(link.is_relative(), "{link:?}");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("listing")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(!dir.join(CURRENT).join(PLACEHOLDER_MARKER).exists());
        // A damaged installation reads as unusable rather than as a pair.
        std::fs::write(dir.join(CURRENT).join(CRL_FILE), "garbage").expect("damaged");
        assert!(installed(&dir).is_err());
        // Installing the same pair again writes it afresh instead of linking the damaged files.
        let damaged = std::fs::read_link(dir.join(CURRENT)).expect("a symlink");
        install(&dir, &second).expect("installed");
        assert_eq!(installed(&dir).expect("readable"), Some(second.clone()));
        let relinked = std::fs::read_link(dir.join(CURRENT)).expect("a symlink");
        assert_ne!(relinked, damaged);
        assert!(
            !dir.join(&damaged).exists(),
            "the damaged directory is removed"
        );
        // A placeholder marker the pair does not have also counts as damage.
        std::fs::write(dir.join(CURRENT).join(PLACEHOLDER_MARKER), "").expect("marker");
        install(&dir, &second).expect("installed");
        assert_eq!(installed(&dir).expect("readable"), Some(second.clone()));
        let names = std::fs::read_dir(&dir).expect("listing").count();
        assert_eq!(names, 2, "one trust directory and the link");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn server_files(dir: &Path, matching: bool) {
        let (params, key) = ca("Broker server CA");
        let issuer = CertifiedIssuer::self_signed(params, key).expect("server CA");
        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let other_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let leaf = CertificateParams::new(vec!["mqtt-broker".to_owned()])
            .expect("params")
            .signed_by(&leaf_key, &issuer)
            .expect("leaf");
        // A Secret volume: the files are symlinks into a timestamped directory.
        let data = dir.join("..2026_10_22_00_00_00.000000001");
        std::fs::create_dir_all(&data).expect("data directory");
        std::fs::write(
            data.join(SERVER_CERTIFICATE),
            format!("{}{}", leaf.pem(), issuer.pem()),
        )
        .expect("certificate");
        let key = if matching { leaf_key } else { other_key };
        std::fs::write(data.join(SERVER_KEY), key.serialize_pem()).expect("key");
        let _ = std::fs::remove_file(dir.join("..data"));
        std::os::unix::fs::symlink(data.file_name().expect("name"), dir.join("..data"))
            .expect("..data");
        for name in [SERVER_CERTIFICATE, SERVER_KEY] {
            let _ = std::fs::remove_file(dir.join(name));
            std::os::unix::fs::symlink(Path::new("..data").join(name), dir.join(name))
                .expect("link");
        }
    }

    #[test]
    fn the_server_certificate_is_read_through_the_secret_links() {
        let dir = temporary_dir("server");
        assert!(check_server(&dir).is_err(), "nothing mounted");
        server_files(&dir, true);
        let first = check_server(&dir).expect("usable");
        assert!(first.not_after > unix_now());
        let problem = {
            let other = temporary_dir("server-mismatch");
            server_files(&other, false);
            let result = check_server(&other);
            std::fs::remove_dir_all(&other).expect("cleanup");
            result.expect_err("a key of another certificate")
        };
        assert!(problem.contains("does not belong"), "{problem}");
        std::fs::write(dir.join("..data").join(SERVER_KEY), "not a key").expect("damaged");
        assert!(
            check_server(&dir)
                .expect_err("no key")
                .contains("no private key")
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn fake_process(root: &Path, pid: i32, comm: &str, sig_caught: &str) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir(&dir).expect("process directory");
        std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("comm");
        std::fs::write(
            dir.join("status"),
            format!("Name:\t{comm}\nSigBlk:\t0000000000000000\nSigCgt:\t{sig_caught}\n"),
        )
        .expect("status");
    }

    #[test]
    fn the_broker_is_found_only_when_it_is_alone_and_ready() {
        let root = temporary_dir("proc");
        let uid = std::fs::metadata(&root).expect("metadata").uid();
        assert!(
            find_broker(&root, uid)
                .expect_err("empty")
                .contains("no mosquitto")
        );
        fake_process(&root, 1, "pause", "0000000000000000");
        fake_process(&root, 40, "mosquitto_pub", "0000000000004a03");
        std::fs::create_dir(root.join("self")).expect("not a pid");
        fake_process(&root, 17, "mosquitto", "0000000000000000");
        assert!(
            find_broker(&root, uid)
                .expect_err("starting")
                .contains("still starting")
        );
        std::fs::write(
            root.join("17").join("status"),
            "SigCgt:\t0000000000004a03\n",
        )
        .expect("status");
        assert_eq!(find_broker(&root, uid), Ok(17));
        assert!(
            find_broker(&root, uid + 1).is_err(),
            "a broker of another user is never signalled"
        );
        fake_process(&root, 99, "mosquitto", "0000000000004a03");
        assert!(
            find_broker(&root, uid)
                .expect_err("two")
                .contains("2 mosquitto")
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    #[allow(
        unsafe_code,
        reason = "a test runner started with SIGHUP ignored passes that on through exec; only \
                  signal(2) between fork and exec restores the default"
    )]
    fn sighup_reaches_the_process() {
        assert!(send_sighup(0).is_err());
        assert!(send_sighup(1).is_err());
        assert!(send_sighup(-1).is_err());
        let mut command = std::process::Command::new("sleep");
        command.arg("600");
        // SAFETY: the closure runs in the child between fork and exec and calls only signal(2),
        // which is async-signal-safe and touches no memory of the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGHUP, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("sleep runs");
        let pid = i32::try_from(child.id()).expect("a pid fits");
        send_sighup(pid).expect("signalled");
        // Generous for a loaded machine; sleep would otherwise run for 10 minutes.
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child's status") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for sleep to end on SIGHUP");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.signal(), Some(libc::SIGHUP));
    }

    #[test]
    fn the_gateway_port_is_the_listener_files_only_listener_line() {
        let file = "# Gateways\nlistener 8883\nprotocol mqtt\ncertfile /x/tls.crt\n";
        assert_eq!(listener_port(file), Ok(8883));
        assert_eq!(listener_port("  listener   1884  \n"), Ok(1884));
        for broken in [
            "",
            "# listener 8883\n",
            "listener\n",
            "listener 0 /run/mosquitto/probe.sock\n",
            "listener 70000\n",
            "listener 8883\nlistener 8884\n",
        ] {
            assert!(listener_port(broken).is_err(), "{broken:?}");
        }
    }

    const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when \
                              retrnsmt   uid  timeout inode\n";
    const TCP6_HEADER: &str = "  sl  local_address                         remote_address              \
                               st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    #[test]
    fn listening_sockets_are_read_from_proc_net() {
        let root = temporary_dir("mqtt-trust-net");
        assert!(listens_on(&root, 8883).is_err(), "no table to read");
        std::fs::create_dir(root.join("net")).expect("net");
        // 1883 listens; 8883 only has an established connection (state 01).
        std::fs::write(
            root.join("net").join("tcp"),
            format!(
                "{TCP_HEADER}   0: 00000000:075B 00000000:0000 0A 00000000:00000000 00:00000000 \
                 00000000  1883        0 1 1 0000000000000000 100 0 0 10 0\n   1: \
                 0A2A0005:22B3 0A2A0009:D431 01 00000000:00000000 00:00000000 00000000  1883 \
                 0 2 1 0000000000000000 20 4 30 10 -1\n"
            ),
        )
        .expect("tcp");
        assert_eq!(listens_on(&root, 1883), Ok(true));
        assert_eq!(listens_on(&root, 8883), Ok(false));
        // Mosquitto's `listener 8883` without an address binds IPv6 too.
        std::fs::write(
            root.join("net").join("tcp6"),
            format!(
                "{TCP6_HEADER}   0: 00000000000000000000000000000000:22B3 \
                 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  \
                 1883        0 3 1 0000000000000000 100 0 0 10 0\n"
            ),
        )
        .expect("tcp6");
        assert_eq!(listens_on(&root, 8883), Ok(true));
        assert_eq!(listens_on(&root, 8884), Ok(false));
    }
}
