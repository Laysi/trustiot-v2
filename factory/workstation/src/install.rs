//! Installing a registered gateway's certificates on the gateway (Laysi/trustiot-v2#617): after
//! [`crate::register`] recorded the registration, the gateway's
//! `trustiot-agent provision install` receives on standard input the certificate chain and the
//! root certificate of that record, and checks and installs them (edge/agent/README.md,
//! "安裝憑證鏈與根憑證"). Running it again is safe: the gateway leaves files that are already right
//! untouched, so the same command reinstalls the certificates after the gateway is reflashed.
//!
//! What crosses to the gateway is public data only, built here from the record ([`Payload`]):
//! the gateway's certificate and the issuing CA's, then the root's, each re-encoded as PEM from
//! its DER, so nothing else in the record (or anything that was around the PEM in it) is sent.
//! The session cookie, the workstation's key and certificate and the signature headers are not in
//! the record and never reach this module.
//!
//! Before anything is sent:
//!
//! 1. The record must be `registered`, of this hardware ID.
//! 2. The SHA-256 of its `root_certificate` (DER) must be the root fingerprint the operator gives
//!    this run (`--root-sha256`, the `pinned_root_sha256` argument), and the record's own
//!    `root_sha256` must be that same value; otherwise [`InstallStopKind::RootMismatch`]. The root
//!    becomes the gateway's only trust anchor for the broker, so it is checked again here, not
//!    only when the answer was recorded, and against the operator's value rather than the
//!    record's alone: whoever can rewrite the record can rewrite both of its fields together.
//! 3. The chain must be the gateway's certificate (common name the hardware ID) and the issuing
//!    CA that signed it, which the root signed. A chain of the certificate alone (the platform no
//!    longer has the issuing CA, services/certs/README.md, "輪替") stops with
//!    [`InstallStopKind::NoIssuingCa`]: the broker would refuse a gateway that cannot present its
//!    issuing CA.
//!
//! The gateway checks everything again with its TPM key ([`InstallStopKind::Refused`] when it
//! refuses), and [`InstallStopKind::GatewayNotReady`] says the agent's installation there cannot
//! be used yet. Each outcome is a line in the journal's log (`installed` or `install_stopped`).

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};
use x509_parser::oid_registry::OID_X509_COMMON_NAME;
use x509_parser::prelude::{FromDer as _, X509Certificate};

use crate::Error;
use crate::http::printable;
use crate::journal::{Event, Journal, Record, State};
use crate::register::is_hardware_id;

/// The agent's subcommand on the gateway, after the agent executable (and `sudo -n` where the
/// gateway needs it). The hardware ID follows as the value of `--hardware-id`.
pub const REMOTE_SUBCOMMAND: [&str; 3] = ["provision", "install", "--hardware-id"];

/// The gateway's exit statuses this tool tells apart: the agent's
/// (`agent::provision::install::Exit`), and the shell's for a command it cannot find.
mod remote_exit {
    pub const SUCCESS: i32 = 0;
    pub const USAGE: i32 = 2;
    pub const REFUSED: i32 = 8;
    pub const CONFIGURATION: i32 = 9;
    pub const NOT_FOUND: i32 = 127;
}

/// The largest standard error of the gateway kept for the operator.
const MAX_REMOTE_MESSAGE_CHARS: usize = 2000;

/// The argument vector of the remote command for `hardware_id`: [`REMOTE_SUBCOMMAND`] and the
/// hardware ID, which must follow the gateway ID rule ([`is_hardware_id`]), so it holds no
/// character a shell treats specially.
///
/// # Errors
///
/// A usage error for a hardware ID that breaks the rule.
pub fn remote_arguments(hardware_id: &str) -> Result<Vec<String>, Error> {
    if !is_hardware_id(hardware_id) {
        return Err(Error::Usage(format!(
            "--hardware-id {:?}: 1 to 64 ASCII letters, digits, '.', '_' or '-', starting with a \
             letter or digit",
            printable(hardware_id)
        )));
    }
    let mut arguments: Vec<String> = REMOTE_SUBCOMMAND
        .iter()
        .map(|&arg| arg.to_owned())
        .collect();
    arguments.push(hardware_id.to_owned());
    Ok(arguments)
}

/// What is sent to the gateway: the remote command's arguments and its standard input.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    /// [`remote_arguments`] of the record's hardware ID.
    pub arguments: Vec<String>,
    /// The gateway's certificate, the issuing CA, then the root, PEM, and nothing else.
    pub stdin: Vec<u8>,
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Payload")
            .field("arguments", &self.arguments)
            .field("stdin_bytes", &self.stdin.len())
            .finish()
    }
}

/// How an installation stopped without success, each with its own exit code
/// (factory/workstation/README.md, "結束碼").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStopKind {
    /// The record's root certificate, or its `root_sha256`, is not the root the operator pinned
    /// for this run; nothing was sent.
    RootMismatch,
    /// The record's chain is the gateway's certificate alone, without the issuing CA; nothing
    /// was sent.
    NoIssuingCa,
    /// The gateway refused the certificates (its status 8); it wrote nothing.
    Refused,
    /// The agent's installation on the gateway cannot be used yet (its status 9: no
    /// configuration, no account, a missing directory; 127: no `trustiot-agent` installed; 2: an
    /// agent without `provision install`); it wrote nothing.
    GatewayNotReady,
}

impl InstallStopKind {
    /// The process exit code.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::RootMismatch => 30,
            Self::NoIssuingCa => 31,
            Self::Refused => 32,
            Self::GatewayNotReady => 33,
        }
    }

    /// The name in log lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RootMismatch => "root_mismatch",
            Self::NoIssuingCa => "no_issuing_ca",
            Self::Refused => "refused_by_gateway",
            Self::GatewayNotReady => "gateway_not_ready",
        }
    }
}

/// An installation that stopped: the kind (and exit code) and the message for the operator.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct InstallStopped {
    pub kind: InstallStopKind,
    pub message: String,
}

fn stopped(kind: InstallStopKind, message: String) -> Error {
    Error::Install(Box::new(InstallStopped { kind, message }))
}

/// The standard input for `record`'s gateway, after checks 1 to 3 of the module documentation;
/// `pinned_root_sha256` is the SHA-256 of the root's DER that the operator gave.
///
/// # Errors
///
/// [`Error::Install`] with [`InstallStopKind::RootMismatch`] or [`InstallStopKind::NoIssuingCa`],
/// or a plain error for a record that is not registered or does not hold usable certificates.
pub fn payload(record: &Record, pinned_root_sha256: &[u8; 32]) -> Result<Payload, Error> {
    let id = record.hardware_id.as_str();
    let arguments = remote_arguments(id)?;
    let registration = match (&record.state, &record.registration) {
        (State::Registered, Some(registration)) => registration,
        _ => {
            return Err(Error::Failed(format!(
                "hardware ID {id} is not recorded as registered on this workstation: run \
                 register first; nothing was sent"
            )));
        }
    };
    if registration.hardware_id != id {
        return Err(Error::Failed(format!(
            "the record of hardware ID {id} holds the registration of another hardware ID; \
             nothing was sent"
        )));
    }
    let unusable = |what: &str| {
        Error::Failed(format!(
            "the record of hardware ID {id}: {what}; nothing was sent"
        ))
    };
    let roots = pem_certificates(&registration.root_certificate)
        .ok_or_else(|| unusable("its root_certificate is not PEM certificates"))?;
    let [root_der] = roots.as_slice() else {
        return Err(unusable("its root_certificate is not one certificate"));
    };
    let pinned = hex(pinned_root_sha256);
    let actual = hex(&Sha256::digest(root_der));
    if actual != pinned || record.root_sha256 != pinned {
        return Err(stopped(
            InstallStopKind::RootMismatch,
            format!(
                "the root certificate recorded for hardware ID {id} has SHA-256 {actual} and \
                 the record pins {}, but --root-sha256 is {pinned}: the record was changed \
                 since, or the fingerprint given is not the one it was registered with. \
                 Nothing was sent; check the platform's root fingerprint, then run register \
                 again with it",
                record.root_sha256
            ),
        ));
    }
    let chain = pem_certificates(&registration.certificate_chain)
        .ok_or_else(|| unusable("its certificate_chain is not PEM certificates"))?;
    let (leaf_der, issuing_der) = match chain.as_slice() {
        [leaf, issuing] => (leaf, issuing),
        [_] => {
            return Err(stopped(
                InstallStopKind::NoIssuingCa,
                format!(
                    "the registration of hardware ID {id} has only the gateway's certificate, \
                     not the issuing CA that signed it (the platform no longer has that issuing \
                     CA): the broker would refuse the gateway. Nothing was sent; the gateway \
                     needs a certificate from a current issuing CA"
                ),
            ));
        }
        _ => return Err(unusable("its certificate_chain is not two certificates")),
    };
    let leaf = whole(leaf_der).ok_or_else(|| unusable("its certificate cannot be parsed"))?;
    let issuing = whole(issuing_der).ok_or_else(|| unusable("its issuing CA cannot be parsed"))?;
    let root = whole(root_der).ok_or_else(|| unusable("its root certificate cannot be parsed"))?;
    let mut names = leaf
        .subject()
        .iter_attributes()
        .filter(|attribute| attribute.attr_type() == &OID_X509_COMMON_NAME);
    let common_name = match (names.next(), names.next()) {
        (Some(name), None) => name.as_str().ok(),
        _ => None,
    };
    if common_name != Some(id) {
        return Err(unusable(
            "its certificate's common name is not the hardware ID",
        ));
    }
    if !issuing.is_ca()
        || leaf.verify_signature(Some(issuing.public_key())).is_err()
        || issuing.verify_signature(Some(root.public_key())).is_err()
    {
        return Err(unusable(
            "its chain is not the certificate, then an issuing CA that signed it and that the \
             root signed",
        ));
    }
    let mut stdin = String::new();
    for der in [leaf_der, issuing_der, root_der] {
        stdin.push_str(&pem_encode(der));
    }
    Ok(Payload {
        arguments,
        stdin: stdin.into_bytes(),
    })
}

/// How the remote command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteOutcome {
    /// Its exit status; `None` when it was ended by a signal.
    pub status: Option<i32>,
    /// Its standard output.
    pub stdout: Vec<u8>,
    /// Its standard error, shown to the operator when it failed.
    pub stderr: Vec<u8>,
}

/// Runs the agent on the gateway with the given arguments and standard input: over ssh in the
/// tool, a stand-in in tests.
pub trait Remote {
    /// Runs the agent with `arguments` (after the agent executable) and `stdin`, and returns
    /// how it ended.
    ///
    /// # Errors
    ///
    /// When the command could not be run at all (no connection, for example).
    fn run_agent(
        &mut self,
        arguments: &[String],
        stdin: &[u8],
    ) -> impl Future<Output = Result<RemoteOutcome, Error>> + Send;
}

/// The certificates are on the gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// What the gateway printed: one line per file, `<path>: written` or `<path>: unchanged`.
    pub report: String,
}

impl Installed {
    /// Whether any file was written (false when both were already right).
    #[must_use]
    pub fn changed(&self) -> bool {
        self.report.lines().any(|line| line.ends_with(": written"))
    }
}

/// Installs the certificates of `hardware_id`'s record in `journal` on its gateway through
/// `remote`: see the module documentation. `pinned_root_sha256` is the root fingerprint the
/// operator gave this run (`--root-sha256`, never read from the record); `now` gives the time
/// of the log lines.
///
/// # Errors
///
/// [`Error::Install`] with the [`InstallStopKind`] of a stop; any other error for a record that
/// cannot be used, a remote command that could not be run or failed otherwise, or a log line that
/// cannot be written.
pub async fn install(
    journal: &Journal,
    hardware_id: &str,
    pinned_root_sha256: &[u8; 32],
    remote: &mut impl Remote,
    now: DateTime<Utc>,
) -> Result<Installed, Error> {
    // The hardware ID names the record file: check it before it is used as a path.
    remote_arguments(hardware_id)?;
    let record = journal.load(hardware_id)?.ok_or_else(|| {
        Error::Failed(format!(
            "this workstation has no record of hardware ID {hardware_id}: run register first; \
             nothing was sent"
        ))
    })?;
    let payload = match payload(&record, pinned_root_sha256) {
        Ok(payload) => payload,
        Err(error) => return Err(logged_stop(journal, hardware_id, now, None, error)),
    };
    let outcome = match remote.run_agent(&payload.arguments, &payload.stdin).await {
        Ok(outcome) => outcome,
        Err(error) => return Err(logged_stop(journal, hardware_id, now, None, error)),
    };
    let message = printable_output(&outcome.stderr);
    let error = match outcome.status {
        Some(remote_exit::SUCCESS) => {
            let report = String::from_utf8_lossy(&outcome.stdout).into_owned();
            let installed = Installed {
                report: printable_report(&report),
            };
            journal.append(
                &Event::new(now, hardware_id, "installed").with("changed", installed.changed()),
            )?;
            return Ok(installed);
        }
        Some(remote_exit::REFUSED) => stopped(
            InstallStopKind::Refused,
            format!(
                "the gateway refused the certificates of hardware ID {hardware_id} and wrote \
                 nothing: {message}"
            ),
        ),
        Some(remote_exit::CONFIGURATION) => stopped(
            InstallStopKind::GatewayNotReady,
            format!(
                "the agent's installation on the gateway cannot be used yet and nothing was \
                 written: {message}. Install the agent and its configuration on the gateway, \
                 then run the same command again"
            ),
        ),
        Some(remote_exit::NOT_FOUND) => stopped(
            InstallStopKind::GatewayNotReady,
            format!(
                "the gateway has no trustiot-agent installed, so there is nothing the \
                 certificates could be installed for (an agent given with --upload-agent is \
                 only used for provisioning): {message}. Install the agent and its \
                 configuration on the gateway, then run the same command again"
            ),
        ),
        Some(remote_exit::USAGE) => stopped(
            InstallStopKind::GatewayNotReady,
            format!(
                "the trustiot-agent installed on the gateway does not know provision install \
                 (an agent too old for this tool?) and wrote nothing: {message}. Update the \
                 agent on the gateway, then run the same command again"
            ),
        ),
        status => Error::Failed(format!(
            "the gateway's trustiot-agent provision install ended with {}: {message}. Running \
             the same command again is safe",
            status.map_or_else(|| "a signal".to_owned(), |code| format!("status {code}"))
        )),
    };
    Err(logged_stop(
        journal,
        hardware_id,
        now,
        outcome.status,
        error,
    ))
}

/// Logs that the installation of `hardware_id` stopped with `error`, then gives back `error`; a
/// log line that cannot be written is added to its message, never replaces it.
fn logged_stop(
    journal: &Journal,
    hardware_id: &str,
    now: DateTime<Utc>,
    status: Option<i32>,
    error: Error,
) -> Error {
    let stop = match &error {
        Error::Install(stopped) => stopped.kind.as_str(),
        _ => "failed",
    };
    let mut event = Event::new(now, hardware_id, "install_stopped").with("stop", stop);
    if let Some(status) = status {
        event = event.with("remote_status", status);
    }
    let Err(log) = journal.append(&event) else {
        return error;
    };
    let also = format!("also, the log line could not be written: {log}");
    match error {
        Error::Install(mut stopped) => {
            stopped.message = format!("{}; {also}", stopped.message);
            Error::Install(stopped)
        }
        other => Error::Failed(format!("{other}; {also}")),
    }
}

/// The gateway's standard error for a message: printable, on one line, bounded.
fn printable_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let joined: String = text.lines().map(str::trim).collect::<Vec<_>>().join(" / ");
    let shown: String = joined
        .chars()
        .take(MAX_REMOTE_MESSAGE_CHARS)
        .map(shown_character)
        .collect();
    if shown.is_empty() {
        "(no message)".to_owned()
    } else {
        shown
    }
}

/// The gateway's report: its lines without control characters, bounded.
fn printable_report(text: &str) -> String {
    text.lines()
        .take(8)
        .map(|line| {
            line.chars()
                .take(512)
                .map(shown_character)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `character`, or a space for one that would change how the terminal shows the rest: a control
/// character (escape sequences, line breaks) or a Unicode format character (category Cf, such as
/// U+202E RIGHT-TO-LEFT OVERRIDE, which can make the text read differently than it is).
fn shown_character(character: char) -> char {
    if character.is_control() || is_format(character) {
        ' '
    } else {
        character
    }
}

/// Whether `character` is in the Unicode general category Cf (format), Unicode 15.1. The standard
/// library has no category lookup and the list is short, so it is spelled out here.
fn is_format(character: char) -> bool {
    matches!(
        u32::from(character),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The certificate `der` is, with nothing after it.
fn whole(der: &[u8]) -> Option<X509Certificate<'_>> {
    X509Certificate::from_der(der)
        .ok()
        .and_then(|(rest, parsed)| rest.is_empty().then_some(parsed))
}

/// `der` as one PEM `CERTIFICATE` block: 64 base64 characters a line.
fn pem_encode(der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut pem = "-----BEGIN CERTIFICATE-----\n".to_owned();
    for line in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use contract::certs::GatewayRegistration;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };

    use super::*;
    use crate::files;
    use crate::journal::RECORD_VERSION;

    const ID: &str = "tg-0001";

    /// A new directory for one test. The counter keeps tests that run in parallel apart where
    /// the clock's resolution is coarser than nanoseconds (macOS gives microseconds).
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "workstation-install-{name}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_nanos()
        ));
        files::ensure_directory(&path).expect("a scratch directory");
        path
    }

    fn ca(name: &str, constraints: BasicConstraints) -> CertificateParams {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(constraints);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    }

    /// DER of a root, an issuing CA under it and a gateway certificate for `ID` under that.
    fn pki() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let root_key = KeyPair::generate().expect("a key");
        let root_params = ca("Test Root", BasicConstraints::Unconstrained);
        let root = root_params
            .self_signed(&root_key)
            .expect("root")
            .der()
            .to_vec();
        let root_issuer = Issuer::new(root_params, root_key);
        let issuing_key = KeyPair::generate().expect("a key");
        let issuing_params = ca("Test Issuing", BasicConstraints::Constrained(0));
        let issuing = issuing_params
            .signed_by(&issuing_key, &root_issuer)
            .expect("issuing")
            .der()
            .to_vec();
        let issuer = Issuer::new(issuing_params, issuing_key);
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, ID);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = params
            .signed_by(&KeyPair::generate().expect("a key"), &issuer)
            .expect("leaf")
            .der()
            .to_vec();
        (leaf, issuing, root)
    }

    fn record(leaf: &[u8], issuing: &[u8], root: &[u8]) -> Record {
        let time = DateTime::from_timestamp(1_793_000_000, 0).expect("a time");
        Record {
            version: RECORD_VERSION,
            hardware_id: ID.to_owned(),
            state: State::Registered,
            platform: "https://app.example".to_owned(),
            root_sha256: hex(&Sha256::digest(root)),
            public_key_sha256: "a".repeat(64),
            endorsement_key_sha256: "b".repeat(64),
            workstation_serial: "4f3a".to_owned(),
            updated_at: time,
            registration: Some(GatewayRegistration {
                hardware_id: ID.to_owned(),
                public_key_sha256: "a".repeat(64),
                endorsement_key_sha256: "b".repeat(64),
                workstation: "workstation:line-1".to_owned(),
                registered_at: time,
                serial: "0a1b".to_owned(),
                ca_key_id: "d".repeat(64),
                not_before: time,
                not_after: time,
                certificate: pem_encode(leaf),
                // As the platform answers: comments around the blocks are ignored, never sent.
                certificate_chain: format!("{}{}", pem_encode(leaf), pem_encode(issuing)),
                root_certificate: format!("root CA\n{}", pem_encode(root)),
            }),
        }
    }

    /// A gateway stand-in: records what it got and answers with `status`.
    struct Recorder {
        calls: Vec<(Vec<String>, Vec<u8>)>,
        status: Option<i32>,
        stdout: &'static str,
        stderr: &'static str,
    }

    impl Recorder {
        fn answering(status: Option<i32>, stdout: &'static str, stderr: &'static str) -> Self {
            Self {
                calls: Vec::new(),
                status,
                stdout,
                stderr,
            }
        }
    }

    impl Remote for Recorder {
        async fn run_agent(
            &mut self,
            arguments: &[String],
            stdin: &[u8],
        ) -> Result<RemoteOutcome, Error> {
            self.calls.push((arguments.to_vec(), stdin.to_vec()));
            Ok(RemoteOutcome {
                status: self.status,
                stdout: self.stdout.as_bytes().to_vec(),
                stderr: self.stderr.as_bytes().to_vec(),
            })
        }
    }

    /// The fingerprint an operator gives for `root`.
    fn pin(root: &[u8]) -> [u8; 32] {
        Sha256::digest(root).into()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_793_000_100, 0).expect("a time")
    }

    fn journal_with(record: &Record) -> Journal {
        let journal = Journal::new(&scratch("journal"));
        journal.prepare().expect("prepared");
        journal.write(record).expect("written");
        journal
    }

    fn log(journal: &Journal) -> String {
        std::fs::read_to_string(journal.log_path()).unwrap_or_default()
    }

    #[tokio::test]
    async fn only_the_chain_and_the_root_are_sent_with_a_fixed_command() {
        let (leaf, issuing, root) = pki();
        let journal = journal_with(&record(&leaf, &issuing, &root));
        let mut gateway = Recorder::answering(
            Some(0),
            "/var/lib/trustiot-agent/gateway.pem: written\n/etc/trustiot/broker-ca.pem: unchanged\n",
            "",
        );
        let installed = install(&journal, ID, &pin(&root), &mut gateway, now())
            .await
            .expect("installed");
        assert!(installed.changed());
        assert_eq!(gateway.calls.len(), 1);
        let (arguments, stdin) = &gateway.calls[0];
        assert_eq!(
            arguments,
            &["provision", "install", "--hardware-id", "tg-0001"]
        );
        let expected = format!(
            "{}{}{}",
            pem_encode(&leaf),
            pem_encode(&issuing),
            pem_encode(&root)
        );
        assert_eq!(String::from_utf8(stdin.clone()).expect("text"), expected);
        assert!(!String::from_utf8_lossy(stdin).contains("root CA"));
        assert!(
            log(&journal).contains("\"event\":\"installed\""),
            "{}",
            log(&journal)
        );

        let mut gateway = Recorder::answering(
            Some(0),
            "/var/lib/trustiot-agent/gateway.pem: unchanged\n/etc/trustiot/broker-ca.pem: unchanged\n",
            "",
        );
        assert!(
            !install(&journal, ID, &pin(&root), &mut gateway, now())
                .await
                .expect("installed")
                .changed()
        );
    }

    #[tokio::test]
    async fn a_root_that_is_not_the_pinned_one_is_never_sent() {
        let (leaf, issuing, root) = pki();
        let (_, _, other_root) = pki();
        let mut changed = record(&leaf, &issuing, &root);
        if let Some(registration) = changed.registration.as_mut() {
            registration.root_certificate = pem_encode(&other_root);
        }
        let journal = journal_with(&changed);
        let mut gateway = Recorder::answering(Some(0), "", "");
        let error = install(&journal, ID, &pin(&root), &mut gateway, now())
            .await
            .expect_err("stopped");
        assert!(gateway.calls.is_empty(), "something was sent");
        assert_eq!(error.exit_code(), 30, "{error}");
        assert!(error.to_string().contains("Nothing was sent"), "{error}");
        assert!(log(&journal).contains("root_mismatch"), "{}", log(&journal));

        // The record's pin alone changed: the same.
        let mut repinned = record(&leaf, &issuing, &root);
        repinned.root_sha256 = hex(&Sha256::digest(&other_root));
        let journal = journal_with(&repinned);
        let error = install(&journal, ID, &pin(&root), &mut gateway, now())
            .await
            .expect_err("stopped");
        assert_eq!(error.exit_code(), 30, "{error}");
        assert!(gateway.calls.is_empty());

        // The whole record rewritten for another root, consistent in itself: only the
        // operator's fingerprint stops it.
        let (other_leaf, other_issuing, other_root) = pki();
        let journal = journal_with(&record(&other_leaf, &other_issuing, &other_root));
        let error = install(&journal, ID, &pin(&root), &mut gateway, now())
            .await
            .expect_err("stopped");
        assert_eq!(error.exit_code(), 30, "{error}");
        assert!(gateway.calls.is_empty());
        assert!(log(&journal).contains("root_mismatch"), "{}", log(&journal));
    }

    #[tokio::test]
    async fn a_chain_without_its_issuing_ca_or_that_does_not_hold_is_never_sent() {
        let (leaf, issuing, root) = pki();
        let (other_leaf, other_issuing, _) = pki();
        let mut gateway = Recorder::answering(Some(0), "", "");

        let mut leaf_only = record(&leaf, &issuing, &root);
        if let Some(registration) = leaf_only.registration.as_mut() {
            registration.certificate_chain = pem_encode(&leaf);
        }
        let error = install(
            &journal_with(&leaf_only),
            ID,
            &pin(&root),
            &mut gateway,
            now(),
        )
        .await
        .expect_err("stopped");
        assert_eq!(error.exit_code(), 31, "{error}");

        for (chain, expected) in [
            (
                format!("{}{}", pem_encode(&leaf), pem_encode(&other_issuing)),
                "chain",
            ),
            (
                format!("{}{}", pem_encode(&other_leaf), pem_encode(&issuing)),
                "chain",
            ),
            (
                format!("{}{}", pem_encode(&issuing), pem_encode(&leaf)),
                "common name",
            ),
            (
                format!(
                    "{}{}{}",
                    pem_encode(&leaf),
                    pem_encode(&issuing),
                    pem_encode(&root)
                ),
                "two certificates",
            ),
            (
                format!(
                    "{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
                    pem_encode(&leaf)
                ),
                "not PEM certificates",
            ),
        ] {
            let mut bad = record(&leaf, &issuing, &root);
            if let Some(registration) = bad.registration.as_mut() {
                registration.certificate_chain = chain;
            }
            let error = install(&journal_with(&bad), ID, &pin(&root), &mut gateway, now())
                .await
                .expect_err("stopped");
            assert_eq!(error.exit_code(), 1, "{error}");
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }

        let mut pending = record(&leaf, &issuing, &root);
        pending.state = State::Pending;
        pending.registration = None;
        let error = install(
            &journal_with(&pending),
            ID,
            &pin(&root),
            &mut gateway,
            now(),
        )
        .await
        .expect_err("stopped");
        assert!(error.to_string().contains("run register first"), "{error}");
        let empty = Journal::new(&scratch("empty"));
        let error = install(&empty, ID, &pin(&root), &mut gateway, now())
            .await
            .expect_err("stopped");
        assert!(error.to_string().contains("no record"), "{error}");
        let error = install(&empty, "tg 0001; reboot", &pin(&root), &mut gateway, now())
            .await
            .expect_err("usage");
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(gateway.calls.is_empty(), "something was sent");
    }

    #[tokio::test]
    async fn the_gateways_answers_have_their_exit_codes() {
        let (leaf, issuing, root) = pki();
        let journal = journal_with(&record(&leaf, &issuing, &root));
        for (status, code, expected) in [
            (Some(8), 32, "refused"),
            (Some(9), 33, "cannot be used yet"),
            (Some(1), 1, "status 1"),
            (Some(127), 33, "no trustiot-agent installed"),
            (Some(2), 33, "does not know provision install"),
            (Some(3), 1, "status 3"),
            (None, 1, "a signal"),
        ] {
            let mut gateway = Recorder::answering(
                status,
                "",
                "trustiot-agent provision install: refused: the reason\x1b[31m\n",
            );
            let error = install(&journal, ID, &pin(&root), &mut gateway, now())
                .await
                .expect_err("stopped");
            assert_eq!(error.exit_code(), code, "{error}");
            let message = error.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("the reason"), "{message}");
            assert!(!message.contains('\x1b'), "{message}");
        }
        let log = log(&journal);
        assert!(
            log.contains("refused_by_gateway") && log.contains("\"remote_status\":9"),
            "{log}"
        );
    }

    #[test]
    fn the_gateways_text_is_shown_without_control_or_format_characters() {
        let hostile = "a\u{1b}[31mb\u{202e}c\u{200b}d\u{feff}e\u{e0041}f\u{ad}g\r\nnext";
        let message = printable_output(hostile.as_bytes());
        assert_eq!(message, "a [31mb c d e f g / next");
        let report = printable_report(hostile);
        assert_eq!(report, "a [31mb c d e f g\nnext");
        // Ordinary text, including other scripts, is kept.
        assert_eq!(printable_output("閘道器 é ok".as_bytes()), "閘道器 é ok");
    }

    #[test]
    fn the_payload_does_not_show_its_contents() {
        let (leaf, issuing, root) = pki();
        let payload = payload(&record(&leaf, &issuing, &root), &pin(&root)).expect("a payload");
        let shown = format!("{payload:?}");
        assert!(!shown.contains("BEGIN"), "{shown}");
        assert!(shown.contains("stdin_bytes"), "{shown}");
    }
}
