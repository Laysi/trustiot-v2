//! The production workstation tool (decision 12, Laysi/trustiot-v2#80): `trustiot-workstation`.
//!
//! A production workstation registers gateways with two identities at once: its own
//! workstation certificate, whose private key never leaves the workstation, and a platform
//! administrator's session. This crate holds both:
//!
//! - [`identity`]: the workstation's ECDSA P-256 key and CSR (`init`), the certificate a
//!   platform administrator has the certs service issue for it (`enroll`), and the checks before
//!   a certificate is kept (`install-certificate`).
//! - [`session`]: the platform administrator's email-code login (`login`), kept as a 0600 file
//!   under `XDG_RUNTIME_DIR` until `logout` or until the session expires.
//! - [`http`]: the HTTP client every command uses: one connection per request, never a retry,
//!   the extra headers of `--headers-file`.
//! - [`secret`]: wrappers whose `Debug` and `Display` never show a secret.
//! - [`register`]: the factory registration of a gateway (`register`), signed by the workstation
//!   ([`sign`]), retried with backoff ([`retry`]), its answer checked and recorded
//!   ([`journal`]).
//! - [`install`]: the certificate chain and the root certificate of a recorded registration,
//!   checked against the pinned root and installed on the gateway.
//! - [`pipeline`]: `register --gateway` (Laysi/trustiot-v2#616): `trustiot-agent provision` run
//!   on the gateway over ssh ([`gateway`]), its report checked, then [`register`].
//! - [`verify`]: the gateway's `certs.gateway_registered` event in the platform's event log.
//!
//! The operator's guide is factory/workstation/README.md.

pub mod config;
pub mod files;
pub mod gateway;
pub mod http;
pub mod identity;
pub mod install;
pub mod journal;
pub mod pipeline;
pub mod register;
pub mod retry;
pub mod secret;
pub mod session;
pub mod sign;
pub mod verify;

use std::io;
use std::path::PathBuf;

/// Why a command failed. Messages name files, endpoints, statuses and M1 message keys; they
/// never contain a secret, a file's contents or a response body.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A mistake on the command line (exit code 2).
    #[error("{0}")]
    Usage(String),
    /// A file that holds a secret allows more than its owner, or allows execution.
    #[error(
        "{} has mode {mode:04o}: a file holding a secret must be 0600 or narrower \
         (chmod 600 {})",
        path.display(),
        path.display()
    )]
    TooOpen { path: PathBuf, mode: u32 },
    /// A directory the tool keeps files in is writable by the group or others.
    #[error(
        "{} has mode {mode:04o}: the group and others must not be able to write to it \
         (chmod go-w {})",
        path.display(),
        path.display()
    )]
    DirectoryTooOpen { path: PathBuf, mode: u32 },
    /// A file the tool uploads to run on a gateway (`--upload-agent`) is writable by the group
    /// or others, who could replace what runs there.
    #[error(
        "{} has mode {mode:04o}: a file run on the gateway must not be writable by the group or \
         others (chmod go-w {})",
        path.display(),
        path.display()
    )]
    WritableByOthers { path: PathBuf, mode: u32 },
    /// A secret file, or a directory the tool keeps files in, belongs to another user, who could
    /// read or replace it.
    #[error(
        "{} belongs to user id {owner}, not to the user running the tool ({expected}): each \
         operator keeps their own files",
        path.display()
    )]
    NotOwned {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },
    /// A file system operation failed.
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// A file does not hold what it should; `reason` is fixed text.
    #[error("{} {reason}", path.display())]
    BadFile { path: PathBuf, reason: &'static str },
    /// A certificate fails the checks before it is kept.
    #[error("the certificate is refused: {0}")]
    Certificate(#[from] identity::CertificateProblem),
    /// No response, or an unusable one (see [`http::RequestError`]).
    #[error(transparent)]
    Request(#[from] http::RequestError),
    /// The platform answered with an error status (boxed: it is the largest variant).
    #[error(transparent)]
    Api(Box<http::ApiError>),
    /// Anything else, as fixed text.
    #[error("{0}")]
    Failed(String),
    /// A registration stopped with a known refusal, a failed check or used-up retries; each has
    /// its own exit code ([`register::StopKind`]).
    #[error(transparent)]
    Registration(Box<register::Stopped>),
    /// Installing the certificates on the gateway stopped: a failed check, the gateway's refusal
    /// or any other failure; each has its own exit code ([`install::InstallStopKind`]).
    #[error(transparent)]
    Install(Box<install::InstallStopped>),
    /// `register --gateway` stopped before the registration: the gateway could not be reached,
    /// its provisioning failed, its report was refused, or its TPM has no key for a known
    /// hardware ID; each has its own exit code ([`pipeline::StopKind`]).
    #[error(transparent)]
    Provisioning(Box<pipeline::Stopped>),
    /// `verify` found no registration event of the hardware ID (exit code [`NO_EVENT_EXIT_CODE`]).
    #[error("{0}")]
    NoEvent(String),
    /// `verify` found a registration event of the hardware ID whose serial or public key is not
    /// this workstation's record (exit code [`EVENT_MISMATCH_EXIT_CODE`]).
    #[error("{0}")]
    EventMismatch(String),
    /// `verify` found registration events of the hardware ID, but this workstation has no
    /// `registered` record to compare them with (exit code [`UNRECORDED_EXIT_CODE`]).
    #[error("{0}")]
    Unrecorded(String),
}

/// The exit code of [`Error::NoEvent`].
pub const NO_EVENT_EXIT_CODE: u8 = 24;

/// The exit code of [`Error::EventMismatch`].
pub const EVENT_MISMATCH_EXIT_CODE: u8 = 25;

/// The exit code of [`Error::Unrecorded`].
pub const UNRECORDED_EXIT_CODE: u8 = 26;

impl From<http::ApiError> for Error {
    fn from(error: http::ApiError) -> Self {
        Self::Api(Box::new(error))
    }
}

impl Error {
    /// The process exit code for this failure: 2 for usage mistakes, a registration's own code
    /// ([`register::StopKind::exit_code`]), a gateway run's ([`pipeline::StopKind::exit_code`]),
    /// an installation's ([`install::InstallStopKind::exit_code`]), `verify`'s
    /// [`NO_EVENT_EXIT_CODE`], [`EVENT_MISMATCH_EXIT_CODE`] and [`UNRECORDED_EXIT_CODE`], 1
    /// otherwise.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Registration(stopped) => stopped.kind.exit_code(),
            Self::Install(stopped) => stopped.kind.exit_code(),
            Self::Provisioning(stopped) => stopped.kind.exit_code(),
            Self::NoEvent(_) => NO_EVENT_EXIT_CODE,
            Self::EventMismatch(_) => EVENT_MISMATCH_EXIT_CODE,
            Self::Unrecorded(_) => UNRECORDED_EXIT_CODE,
            _ => 1,
        }
    }
}
