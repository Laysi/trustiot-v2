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
//!
//! The operator's guide is factory/workstation/README.md.

pub mod config;
pub mod files;
pub mod http;
pub mod identity;
pub mod secret;
pub mod session;

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
    /// The platform answered with an error status.
    #[error(transparent)]
    Api(#[from] http::ApiError),
    /// Anything else, as fixed text.
    #[error("{0}")]
    Failed(String),
}

impl Error {
    /// The process exit code for this failure: 2 for usage mistakes, 1 otherwise.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            _ => 1,
        }
    }
}
