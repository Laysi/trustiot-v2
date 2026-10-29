//! `trustiot-agent provision install`, the last gateway step of the factory registration
//! (decision 12, Laysi/trustiot-v2#617): the production workstation runs it over ssh after the
//! platform registered the gateway, and sends on standard input the certificate chain and the
//! root certificate the registration answered with. They are public data; nothing else crosses.
//!
//! ```text
//! trustiot-agent provision install --hardware-id <id>
//! ```
//!
//! The command runs as root, and the configuration decides where it writes and which root the
//! gateway trusts. So whoever can run it, or name its configuration, controls the gateway's trust
//! anchor; the configuration is always [`crate::config::DEFAULT_PATH`] (no `--config`), and the
//! workstation's ssh access must allow this one command only (its README).
//!
//! Everything is checked before anything is written:
//!
//! 1. The agent's configuration ([`Config::parse`]) names where the files go
//!    (`identity.certificate` and `broker.ca`) and the TPM key (`identity.key`, which must be the
//!    TPM). The file and its directory must belong to root and be writable by no one else. Both
//!    target directories must exist (they belong to the agent's installation: its unit's
//!    `StateDirectory` and `ConfigurationDirectory`) and be writable by no one but their owner:
//!    the agent's account [`AGENT_ACCOUNT`] for `identity.certificate`, which its renewal
//!    rewrites, and root for `broker.ca`. Every path is opened from `/` one component at a time
//!    without following symbolic links ([`Directory`]), and every later read and write goes
//!    through the opened directory, never the path again.
//! 2. Standard input, at most [`MAX_INPUT_BYTES`], is PEM `CERTIFICATE` blocks and nothing else:
//!    exactly three, the gateway's certificate, the issuing CA that signed it, then the root.
//! 3. The chain, here ([`check_chain`]): the gateway certificate is not a CA and its common name
//!    is the hardware ID; the issuing CA is a CA, not self-issued, and signed the gateway
//!    certificate; the root is a self-issued CA, signed by its own key, and signed the issuing CA.
//!    Signatures are checked with the TLS handshake's algorithms only.
//! 4. The agent's own start-up checks, on the files' future contents: the root as `broker.ca`
//!    ([`BrokerTrust::from_certificates`]), the root CRL `broker.crl` against that root, the chain
//!    as `identity.certificate` with the TPM key ([`identity::from_chain`]: the leaf's public key
//!    must be the TPM key's, its usages must allow TLS client authentication), and webpki's client
//!    certificate check against the root and its CRL, so an issuing CA the root revoked is refused
//!    ([`tls::check_client_certificate_under`]). A certificate that
//!    is only outside its validity by this clock passes with a warning, as at start-up.
//! 5. A renewed certificate is never replaced by an older one ([`newer_installed`]): when
//!    `identity.certificate` holds a certificate for the same key with a later notBefore, the
//!    agent renewed it, and the registration's certificate (which the platform answers with on
//!    every resend of the registration) would only take the gateway back to an older one. Only
//!    a chain that passes under the root being installed counts, so a reinstall after the root
//!    changed is not held by a certificate under the old root. From this check until the files
//!    are renamed, the directory of `identity.certificate` is locked (`flock`), as the renewal
//!    locks it to write, so a renewal cannot land in between and be overwritten.
//!
//! Then each file whose content, owner or mode differ is replaced atomically: a new file in the
//! same open directory (`openat` with `O_EXCL`, `O_NOFOLLOW`), written, its owner and mode set,
//! flushed to disk, and only when both files are ready renamed over the old ones (`renameat`),
//! and the directories flushed. The chain (leaf, then the issuing CA) belongs to the agent's
//! account [`AGENT_ACCOUNT`], the root to root; both have mode [`FILE_MODE`]. A file that is
//! already right is not touched (same inode), so running it again changes nothing. A failure
//! before the renames removes the new files and leaves the old ones as they were; the two renames
//! themselves are not one atomic step, and a crash between them leaves one file new and one old,
//! which the next run completes.
//!
//! The command needs root (it sets owners). It does not restart the agent: the agent reads the
//! files when it starts.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use protocol::{GatewayId, UnixMillis};
use rustls_pki_types::{CertificateDer, UnixTime};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::config::Config;
use crate::directory::Directory;
use crate::identity::{self, IdentityError, KeySource};
use crate::link::printable;
use crate::tls::{self, BrokerTrust, RootCrl, Validity};
use crate::tpm::TpmTransport;

/// The subcommand's name, the argument after `provision`.
pub const SUBCOMMAND: &str = "install";

/// The account the agent runs as (`systemd/trustiot-agent.sysusers`): it owns the chain.
pub const AGENT_ACCOUNT: &str = "trustiot-agent";

/// The mode of both files: certificates are public.
pub const FILE_MODE: u32 = 0o644;

/// The largest standard input read: three PEM certificates are a few KiB.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// The subcommand's help.
pub const USAGE: &str = "usage: trustiot-agent provision install --hardware-id <id>

Installs the gateway's certificate chain and the root certificate the production workstation
sends on standard input (PEM: the gateway certificate, the issuing CA, then the root; nothing
else). Checks them first: the gateway certificate's public key is the TPM key's, its common name
is the hardware ID, the chain leads to that root, and the agent would accept them. Then writes the
chain to identity.certificate (owner trustiot-agent, mode 0644) and the root to broker.ca (owner
root, mode 0644), each replaced atomically, files that are already right untouched. Needs root.

The paths come from the agent's configuration /etc/trustiot/agent.toml (no other file is
accepted), which with its directory must belong to root and be writable by no one else. The
directory of broker.ca must belong to root, that of identity.certificate to trustiot-agent, both
writable by no one else, and no path may go through a symbolic link.

  --hardware-id <id>  the gateway's hardware ID: the certificate's common name (required)
  --help              print this help (only as the sole argument)

Exit status: 0 installed (or already installed); 1 other failure (nothing renamed, or see the
message); 2 invalid arguments; 8 the certificates are refused, nothing was written; 9 the
configuration or the installation cannot be used (no agent configuration, a key that is not the
TPM's, a missing directory or account, a wrong owner or mode, a symbolic link), nothing was
written.";

/// How the subcommand ends: its exit status. Distinct from `provision`'s own (3 to 7), so the
/// workstation can tell every status of both apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// 0: both files hold what was sent (written now or before).
    Success,
    /// 1: anything not listed below, such as a TPM error or a file that could not be written.
    Failure,
    /// 2: invalid arguments.
    Usage,
    /// 8: the certificates are refused; nothing was written.
    Refused,
    /// 9: the agent's configuration or installation cannot be used; nothing was written.
    Configuration,
}

impl Exit {
    /// The process exit status.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::Refused => 8,
            Self::Configuration => 9,
        }
    }
}

/// The checked arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The common name the gateway certificate must have.
    pub hardware_id: GatewayId,
    /// The agent's configuration file: always [`crate::config::DEFAULT_PATH`] from the command
    /// line, because whoever names it chooses where root writes and which root the gateway
    /// trusts; tests set their own.
    pub config: PathBuf,
}

/// What the arguments ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Install with these options.
    Install(Options),
    /// Print [`USAGE`].
    Help,
}

/// Parses the arguments after `provision install`. Every option may be given once.
///
/// # Errors
///
/// A message naming the argument that is unknown, repeated, missing or invalid: then the status
/// is [`Exit::Usage`].
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        args.next();
        return match args.next() {
            None => Ok(Command::Help),
            Some(_) => Err(HELP_ALONE.to_owned()),
        };
    }
    let mut hardware_id = None;
    while let Some(arg) = args.next() {
        let name = arg
            .to_str()
            .ok_or_else(|| format!("unknown argument {arg:?}"))?;
        match name {
            "--hardware-id" => {
                let value = args
                    .next()
                    .ok_or("--hardware-id needs a value")?
                    .into_string()
                    .map_err(|value| format!("--hardware-id: {value:?} is not UTF-8"))?;
                let id =
                    GatewayId::new(value).map_err(|error| format!("--hardware-id: {error}"))?;
                if hardware_id.replace(id).is_some() {
                    return Err("--hardware-id is given twice".to_owned());
                }
            }
            "--config" => {
                return Err(format!(
                    "--config is not accepted: the configuration is always {}",
                    crate::config::DEFAULT_PATH
                ));
            }
            "--help" | "-h" => return Err(HELP_ALONE.to_owned()),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    Ok(Command::Install(Options {
        hardware_id: hardware_id.ok_or("--hardware-id is required")?,
        config: PathBuf::from(crate::config::DEFAULT_PATH),
    }))
}

const HELP_ALONE: &str = "--help is accepted only as the sole argument";

/// A file's owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The user ID.
    pub uid: u32,
    /// The group ID.
    pub gid: u32,
}

/// Who owns the installed files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owners {
    /// The owner of the chain (`identity.certificate`).
    pub chain: Owner,
    /// The owner of the root (`broker.ca`).
    pub root: Owner,
}

impl Owners {
    /// On the gateway: the chain belongs to [`AGENT_ACCOUNT`] and its primary group, the root to
    /// root (0:0).
    ///
    /// # Errors
    ///
    /// The account does not exist (the agent is not installed) or cannot be looked up.
    pub fn system() -> Result<Self, String> {
        let chain = account(AGENT_ACCOUNT)
            .map_err(|error| format!("cannot look up the account {AGENT_ACCOUNT}: {error}"))?
            .ok_or_else(|| {
                format!(
                    "there is no account {AGENT_ACCOUNT}: install the agent first (its sysusers.d file creates it)"
                )
            })?;
        Ok(Self {
            chain,
            root: Owner { uid: 0, gid: 0 },
        })
    }
}

/// The user ID and primary group of the account `name`, through the C library (so NSS), or
/// `None` when there is no such account.
#[allow(
    unsafe_code,
    reason = "getpwnam_r, the C library's account lookup, has no safe wrapper in std"
)]
fn account(name: &str) -> io::Result<Option<Owner>> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut buffer: Vec<libc::c_char> = vec![0; 16 * 1024];
    // SAFETY: `passwd` is plain C data (integers and pointers); all zeroes is a valid value, and
    // getpwnam_r overwrites it before `found` points at it.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `name` is a NUL-terminated string; `entry`, `buffer` (with its true length) and
    // `found` are valid for writes for the whole call and outlive it. The strings getpwnam_r
    // points `entry` at live in `buffer`, and only the integers are read below.
    let code = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if found.is_null() {
        return Ok(None);
    }
    Ok(Some(Owner {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    }))
}

/// Why nothing (or not everything) was installed.
#[derive(Debug, thiserror::Error)]
enum Failure {
    /// The configuration or the installation cannot be used.
    #[error("{0}")]
    Configuration(String),
    /// The certificates are refused.
    #[error("refused: {0}; nothing was written")]
    Refused(String),
    /// Anything else.
    #[error("{0}")]
    Failed(String),
}

impl Failure {
    fn exit(&self) -> Exit {
        match self {
            Self::Configuration(_) => Exit::Configuration,
            Self::Refused(_) => Exit::Refused,
            Self::Failed(_) => Exit::Failure,
        }
    }
}

/// What happened to one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It already held exactly this, with the right owner and mode; it was not touched.
    Unchanged,
    /// It was replaced (or created).
    Written,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Written => "written",
        }
    }
}

/// Runs the subcommand: reads the certificates from `stdin`, checks them with the TPM key
/// reached through `open_tpm(identity.key.device)` and installs them with `owners`, printing one
/// line per file (`<path>: written` or `<path>: unchanged`) on `stdout`, or why not on `stderr`.
/// On the gateway `open_tpm` opens the device and `owners` is [`Owners::system`]; tests pass a
/// software TPM and their own user.
pub fn run(
    options: &Options,
    owners: &Owners,
    open_tpm: &mut dyn FnMut(&Path) -> TpmTransport,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Exit {
    match install(options, owners, open_tpm, stdin, stderr) {
        Ok(files) => {
            let mut text = String::new();
            for (path, outcome) in files {
                text.push_str(&format!("{}: {}\n", path.display(), outcome.as_str()));
            }
            match stdout
                .write_all(text.as_bytes())
                .and_then(|()| stdout.flush())
            {
                Ok(()) => Exit::Success,
                Err(error) => {
                    let _ = writeln!(
                        stderr,
                        "trustiot-agent provision install: installed, but cannot say so on standard output: {error}"
                    );
                    Exit::Failure
                }
            }
        }
        Err(failure) => {
            let _ = writeln!(stderr, "trustiot-agent provision install: {failure}");
            failure.exit()
        }
    }
}

/// The steps of the module documentation; the files and what happened to each.
fn install(
    options: &Options,
    owners: &Owners,
    open_tpm: &mut dyn FnMut(&Path) -> TpmTransport,
    stdin: &mut dyn Read,
    stderr: &mut dyn Write,
) -> Result<Vec<(PathBuf, Outcome)>, Failure> {
    // Whoever controls the configuration chooses where root writes and which root the gateway
    // trusts, so it is read only when root owns it and its directory (step 1).
    let config = load_config(&options.config, owners.root)?;
    if !matches!(config.key, KeySource::Tpm { .. }) {
        return Err(Failure::Configuration(format!(
            "{}: identity.key.source is not \"tpm\": the certificate is installed only for the TPM key",
            options.config.display()
        )));
    }
    let chain_target = Target::open(
        "identity.certificate",
        &config.certificate,
        owners.chain,
        AGENT_ACCOUNT,
    )?;
    let root_target = Target::open("broker.ca", &config.broker_ca, owners.root, "root")?;
    // Two configured paths may name one file (the directories are compared by device and inode,
    // so a bind mount does not hide it): the second rename would then replace the first file.
    if chain_target.is(&root_target.directory, &root_target.name)? {
        return Err(Failure::Configuration(format!(
            "{}: identity.certificate and broker.ca are the same file ({})",
            options.config.display(),
            config.broker_ca.display()
        )));
    }
    let crl_source = match config.broker_crl.as_deref() {
        Some(crl) => Some(CrlSource::open(crl, owners)?),
        None => None,
    };
    if let Some(CrlSource::Directory {
        path,
        directory,
        name,
    }) = &crl_source
    {
        for (key, target) in [
            ("identity.certificate", &chain_target),
            ("broker.ca", &root_target),
        ] {
            if target.is(directory, name)? {
                return Err(Failure::Configuration(format!(
                    "{}: broker.crl and {key} are the same file ({})",
                    options.config.display(),
                    path.display()
                )));
            }
        }
    }

    let input = read_input(stdin)?;
    let certificates = certificates(&input)
        .map_err(|reason| Failure::Refused(format!("standard input {reason}")))?;
    let chain = check_chain(&certificates, &options.hardware_id).map_err(Failure::Refused)?;

    // The agent's own checks, on what the files will hold.
    let root = CertificateDer::from(chain.root.clone());
    let trust = BrokerTrust::from_certificates(&config.broker_ca, vec![root])
        .map_err(|error| Failure::Refused(format!("broker.ca: {error}")))?;
    let now = crate::setup::clock_now();
    let root_crl = root_crl(crl_source.as_ref(), &trust, now)?;
    let identity = identity::from_chain(
        &config.certificate,
        vec![
            CertificateDer::from(chain.leaf.clone()),
            CertificateDer::from(chain.issuing.clone()),
        ],
        &config.key,
        open_tpm,
    )
    .map_err(|error| match error {
        IdentityError::KeyMismatch => {
            Failure::Refused("the gateway certificate's public key is not the TPM key's".to_owned())
        }
        IdentityError::Tpm(error) => Failure::Failed(format!("the TPM key: {error}")),
        other => Failure::Refused(other.to_string()),
    })?;
    if identity.gateway != options.hardware_id {
        return Err(Failure::Refused(
            "the gateway certificate's common name is not the hardware ID".to_owned(),
        ));
    }
    let validity = tls::check_client_certificate_under(&trust, &root_crl, &identity, now)
        .map_err(|error| Failure::Refused(error.to_string()))?;
    if validity == Validity::OutsideNow {
        let _ = writeln!(
            stderr,
            "trustiot-agent provision install: by this clock the gateway certificate is not valid (from {} to {}); installing anyway, because the clock may be wrong",
            identity::utc(identity.not_before),
            identity::utc(identity.not_after)
        );
    }
    drop(identity);
    // The agent's renewal may write a newer certificate at any time: with the directory locked
    // until the renames are done, it cannot do so between the check below and the rename, and
    // a renewal that waited for the lock writes over the installed file afterwards.
    chain_target.directory.lock().map_err(|error| {
        Failure::Failed(format!(
            "cannot lock {}: {error}",
            chain_target.directory.path().display()
        ))
    })?;
    if let Some(installed) = newer_installed(&chain_target, &chain.leaf, &trust, &root_crl, now) {
        return Err(Failure::Refused(format!(
            "{} holds a newer certificate for this TPM key (valid from {installed}): the agent \
             renewed it after the registration. To install the registration's certificate \
             anyway, remove that file first",
            config.certificate.display()
        )));
    }

    let chain_text = format!(
        "{}{}",
        pem("CERTIFICATE", &chain.leaf),
        pem("CERTIFICATE", &chain.issuing)
    );
    let root_text = pem("CERTIFICATE", &chain.root);
    let files = [
        (&chain_target, chain_text, owners.chain),
        (&root_target, root_text, owners.root),
    ];
    replace_all(&files, &mut |file: &mut File, bytes: &[u8]| {
        file.write_all(bytes)
    })
    .map(|outcomes| {
        [config.certificate.clone(), config.broker_ca.clone()]
            .into_iter()
            .zip(outcomes)
            .collect()
    })
    .map_err(Failure::Failed)
}

/// The largest configuration file read: the agent's is well under 4 KiB.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// The configuration at `path`, read only when the file and its directory belong to `owner`
/// (root on the gateway), are writable by no one else, and are reached without a symbolic link.
fn load_config(path: &Path, owner: Owner) -> Result<Config, Failure> {
    let unusable = |reason: String| Failure::Configuration(format!("{}: {reason}", path.display()));
    let (directory, name) = open_directory(path, owner, "root")?;
    match directory.is_file(&name) {
        Ok(Some(true)) => {}
        Ok(Some(false)) => {
            return Err(unusable(
                "is not a regular file (a symbolic link is not followed)".to_owned(),
            ));
        }
        Ok(None) => {
            return Err(unusable(
                "does not exist: install the agent first".to_owned(),
            ));
        }
        Err(error) => return Err(unusable(format!("cannot be read: {error}"))),
    }
    let mut file = directory
        .open_read(&name)
        .map_err(|error| unusable(format!("cannot be read: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| unusable(format!("cannot be read: {error}")))?;
    if !metadata.is_file() {
        return Err(unusable("is not a regular file".to_owned()));
    }
    check_owner(&metadata, owner, "root").map_err(unusable)?;
    let mut bytes = Vec::new();
    Read::take(&mut file, MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unusable(format!("cannot be read: {error}")))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_CONFIG_BYTES) {
        return Err(unusable(format!("is larger than {MAX_CONFIG_BYTES} bytes")));
    }
    let text = String::from_utf8(bytes).map_err(|_| unusable("is not UTF-8".to_owned()))?;
    Config::parse(&text).map_err(|error| unusable(error.to_string()))
}

/// The directory of `path`, opened without following links ([`Directory::open`]), and the
/// file's name in it. The directory must exist, belong to `owner` (`who`, for the message) and
/// be writable by no one else.
fn open_directory(path: &Path, owner: Owner, who: &str) -> Result<(Directory, OsString), Failure> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Failure::Configuration(format!(
            "{} is not a file's path",
            path.display()
        )));
    };
    let directory = Directory::open(parent).map_err(|error| {
        Failure::Configuration(if error.kind() == io::ErrorKind::NotFound {
            format!(
                "the directory {} of {} does not exist: install and start the agent first",
                parent.display(),
                path.display()
            )
        } else {
            format!(
                "the directory {} of {} cannot be opened without following symbolic links: {error}",
                parent.display(),
                path.display()
            )
        })
    })?;
    let metadata = directory
        .metadata()
        .map_err(|error| Failure::Failed(format!("cannot read {}: {error}", parent.display())))?;
    check_owner(&metadata, owner, who).map_err(|reason| {
        Failure::Configuration(format!(
            "the directory {} of {}: {reason}",
            parent.display(),
            path.display()
        ))
    })?;
    Ok((directory, name.to_owned()))
}

/// Whether `metadata` belongs to `owner` (`who`, for the message) and is writable by no one
/// else (no group or other write bit).
fn check_owner(metadata: &fs::Metadata, owner: Owner, who: &str) -> Result<(), String> {
    if metadata.uid() == owner.uid && metadata.mode() & 0o022 == 0 {
        return Ok(());
    }
    Err(format!(
        "must belong to {who} (uid {}) and be writable by no one else, but belongs to uid {} \
         with mode {:o}",
        owner.uid,
        metadata.uid(),
        metadata.mode() & 0o7777
    ))
}

/// A file `provision install` writes: its directory, held open, and its name in it.
struct Target {
    directory: Directory,
    name: OsString,
}

impl Target {
    /// The target `path` of the configuration key `key` ([`open_directory`]).
    fn open(key: &str, path: &Path, owner: Owner, who: &str) -> Result<Self, Failure> {
        let (directory, name) =
            open_directory(path, owner, who).map_err(|failure| match failure {
                Failure::Configuration(reason) => {
                    Failure::Configuration(format!("{key}: {reason}"))
                }
                other => other,
            })?;
        Ok(Self { directory, name })
    }

    /// The path, for messages.
    fn path(&self) -> PathBuf {
        self.directory.path().join(&self.name)
    }

    /// Whether `name` in `directory` is this file: the same name in the same directory (device
    /// and inode).
    fn is(&self, directory: &Directory, name: &OsStr) -> Result<bool, Failure> {
        let identity = |directory: &Directory| {
            directory
                .metadata()
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|error| {
                    Failure::Failed(format!(
                        "cannot read {}: {error}",
                        directory.path().display()
                    ))
                })
        };
        Ok(self.name == name && identity(&self.directory)? == identity(directory)?)
    }
}

/// Where the root CRL is read from.
enum CrlSource {
    /// Its directory does not exist: no root CRL yet, as at start-up.
    Missing(PathBuf),
    /// Its directory, held open, and its name in it.
    Directory {
        path: PathBuf,
        directory: Directory,
        name: OsString,
    },
}

impl CrlSource {
    /// Opens the directory of `path` without following links. It must belong to root or to the
    /// agent's account (`owners`; the example keeps the CRL in the agent's `StateDirectory`) and
    /// be writable by no one else, so only root or the agent can put anything there, and the
    /// agent cannot create device nodes: reading it as root opens no device another user
    /// planted. Its content is used only when the new root signed it.
    fn open(path: &Path, owners: &Owners) -> Result<Self, Failure> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(Failure::Configuration(format!(
                "broker.crl: {} is not a file's path",
                path.display()
            )));
        };
        match Directory::open(parent) {
            Ok(directory) => {
                let metadata = directory.metadata().map_err(|error| {
                    Failure::Failed(format!("cannot read {}: {error}", parent.display()))
                })?;
                let owner = if metadata.uid() == owners.chain.uid {
                    owners.chain
                } else {
                    owners.root
                };
                let who = if owner == owners.chain {
                    AGENT_ACCOUNT
                } else {
                    "root"
                };
                check_owner(&metadata, owner, who).map_err(|reason| {
                    Failure::Configuration(format!(
                        "broker.crl: the directory {} of {}: {reason}",
                        parent.display(),
                        path.display()
                    ))
                })?;
                Ok(Self::Directory {
                    path: path.to_owned(),
                    directory,
                    name: name.to_owned(),
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Self::Missing(path.to_owned()))
            }
            Err(error) => Err(Failure::Configuration(format!(
                "broker.crl: the directory {} cannot be opened without following symbolic links: {error}",
                parent.display()
            ))),
        }
    }
}

/// The notBefore (RFC 2822) of the certificate installed at `path`, when it is for the same key
/// as `leaf` (DER) and valid from a later time: the agent renewed it ([`crate::renewal`]), and
/// the registration's certificate, which the platform answers with on every resend of the
/// registration, is the older one. Only a chain that passes as a client certificate under the
/// root being installed (`trust`, with `root_crl`) counts, whatever the clock: one under
/// another root, as after the root changed, would not connect anyway. `None` when there is no
/// such file, it cannot be read or parsed, or it is for another key, not newer or not under the
/// new root: the file is then replaced as usual.
fn newer_installed(
    target: &Target,
    leaf: &[u8],
    trust: &BrokerTrust,
    root_crl: &RootCrl,
    now: UnixTime,
) -> Option<String> {
    let limit = u64::try_from(MAX_INPUT_BYTES).unwrap_or(u64::MAX);
    if target.directory.is_file(&target.name).ok()? != Some(true) {
        return None;
    }
    let mut file = target.directory.open_read(&target.name).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut text = Vec::new();
    Read::take(&mut file, limit).read_to_end(&mut text).ok()?;
    let chain: Vec<CertificateDer<'static>> = certificates(&text)
        .ok()?
        .into_iter()
        .map(CertificateDer::from)
        .collect();
    let (_, installed) = X509Certificate::from_der(chain.first()?).ok()?;
    let (_, new) = X509Certificate::from_der(leaf).ok()?;
    let same_key = installed.public_key().raw == new.public_key().raw;
    let installed_from = installed.validity().not_before;
    if !same_key || installed_from.timestamp() <= new.validity().not_before.timestamp() {
        return None;
    }
    let millis = |time: x509_parser::time::ASN1Time| {
        UnixMillis::new(
            u64::try_from(time.timestamp())
                .unwrap_or(0)
                .saturating_mul(1000),
        )
    };
    let validity = (
        millis(installed_from),
        millis(installed.validity().not_after),
    );
    tls::check_chain_under(trust, root_crl, &chain, validity, now).ok()?;
    Some(
        installed_from
            .to_rfc2822()
            .unwrap_or_else(|_| installed_from.to_string()),
    )
}

/// The root CRL (`None`: not configured) checked against the new root, as the agent will load it
/// ([`RootCrl::from_bytes`]). Running as root, it is read only as a regular file reached without
/// a symbolic link (its directory by [`Directory::open`], then `O_NOFOLLOW`, `O_NONBLOCK` and
/// `fstat`), so a link or a FIFO planted on the configured path cannot make it read another file
/// or hang.
fn root_crl(
    source: Option<&CrlSource>,
    trust: &BrokerTrust,
    now: UnixTime,
) -> Result<RootCrl, Failure> {
    let (path, directory, name) = match source {
        None => return Ok(RootCrl::NotConfigured),
        Some(CrlSource::Missing(path)) => return Ok(RootCrl::Missing(path.clone())),
        Some(CrlSource::Directory {
            path,
            directory,
            name,
        }) => (path, directory, name),
    };
    let Some(bytes) = read_regular(path, directory, name, tls::MAX_ROOT_CRL_BYTES)? else {
        return Ok(RootCrl::Missing(path.clone()));
    };
    RootCrl::from_bytes(path, &bytes, trust, now).map_err(|error| {
        Failure::Refused(format!(
            "the agent would not start with this root: broker.crl: {error}"
        ))
    })
}

/// The contents of `name` in `directory` (`path`, for messages), at most `limit` bytes and one
/// more (so the caller sees a file that is too large), when it is a regular file; `None` when
/// there is nothing there. A symbolic link, a directory, a FIFO or a device is a configuration
/// error, found without following or blocking on it.
fn read_regular(
    path: &Path,
    directory: &Directory,
    name: &OsStr,
    limit: u64,
) -> Result<Option<Vec<u8>>, Failure> {
    let failed =
        |error: io::Error| Failure::Failed(format!("cannot read {}: {error}", path.display()));
    let not_regular = || {
        Failure::Configuration(format!(
            "{} is not a regular file (a symbolic link is not followed)",
            path.display()
        ))
    };
    // Checked before opening, so a device node is not opened; and again on the opened file, in
    // case it was replaced in between (only root or the agent's account can write the directory,
    // and neither of them plants device nodes: `CrlSource::open`).
    match directory.is_file(name).map_err(failed)? {
        None => return Ok(None),
        Some(false) => return Err(not_regular()),
        Some(true) => {}
    }
    let mut file = match directory.open_read(name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(not_regular()),
        Err(error) => return Err(failed(error)),
    };
    if !file.metadata().map_err(failed)?.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    Read::take(&mut file, limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    Ok(Some(bytes))
}

/// Standard input, at most [`MAX_INPUT_BYTES`].
fn read_input(stdin: &mut dyn Read) -> Result<Vec<u8>, Failure> {
    let mut input = Vec::new();
    stdin
        .take(u64::try_from(MAX_INPUT_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut input)
        .map_err(|error| Failure::Failed(format!("cannot read standard input: {error}")))?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(Failure::Refused(format!(
            "standard input is larger than {MAX_INPUT_BYTES} bytes"
        )));
    }
    Ok(input)
}

const BEGIN_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----";
const END_CERTIFICATE: &str = "-----END CERTIFICATE-----";

/// The DER of each PEM `CERTIFICATE` block of `input`, in order. Only such blocks and blank lines
/// are accepted: a block of another kind (a private key, say) or any other text refuses the
/// input. The reason never quotes the input.
fn certificates(input: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    use base64::Engine as _;
    let text = std::str::from_utf8(input).map_err(|_| "is not text".to_owned())?;
    let mut certificates = Vec::new();
    let mut body: Option<String> = None;
    for (index, line) in text.split('\n').enumerate() {
        let number = index + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        match body.as_mut() {
            None if line.trim().is_empty() => {}
            None if line == BEGIN_CERTIFICATE => body = Some(String::new()),
            None if line.starts_with("-----BEGIN") => {
                return Err(format!(
                    "line {number}: a PEM block that is not a CERTIFICATE; only certificates are accepted"
                ));
            }
            None => return Err(format!("line {number}: text outside a PEM block")),
            Some(base64) if line == END_CERTIFICATE => {
                let der = base64::engine::general_purpose::STANDARD
                    .decode(base64.as_bytes())
                    .map_err(|_| format!("line {number}: a certificate that is not base64"))?;
                certificates.push(der);
                body = None;
            }
            Some(_) if line.starts_with("-----") => {
                return Err(format!(
                    "line {number}: a PEM CERTIFICATE block that does not end with {END_CERTIFICATE}"
                ));
            }
            Some(base64) => base64.push_str(line.trim()),
        }
    }
    if body.is_some() {
        return Err(format!(
            "ends inside a PEM CERTIFICATE block, without {END_CERTIFICATE}"
        ));
    }
    if certificates.is_empty() {
        return Err("holds no certificate".to_owned());
    }
    Ok(certificates)
}

/// The three certificates, in DER, after [`check_chain`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chain {
    leaf: Vec<u8>,
    issuing: Vec<u8>,
    root: Vec<u8>,
}

/// Checks step 3 of the module documentation on `certificates` (DER, in the order sent).
fn check_chain(certificates: &[Vec<u8>], hardware_id: &GatewayId) -> Result<Chain, String> {
    let [leaf_der, issuing_der, root_der] = certificates else {
        return Err(format!(
            "{} certificate(s) were sent; exactly 3 are needed: the gateway certificate, the issuing CA that signed it, then the root{}",
            certificates.len(),
            if certificates.len() < 3 {
                " (without the issuing CA the gateway cannot present a chain the broker accepts)"
            } else {
                ""
            }
        ));
    };
    let leaf = whole("first certificate", leaf_der)?;
    let issuing = whole("second certificate", issuing_der)?;
    let root = whole("third certificate", root_der)?;

    if is_ca(&leaf)? {
        return Err(
            "the first certificate is a CA certificate, not the gateway's: the certificates are out of order (the gateway certificate comes first, the root last)".to_owned(),
        );
    }
    let names = leaf
        .subject()
        .iter_common_name()
        .map(|name| name.as_str().map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "the gateway certificate's common name is not a string".to_owned())?;
    match names.as_slice() {
        [name] if name == hardware_id.as_str() => {}
        [name] => {
            return Err(format!(
                "the gateway certificate's common name is {:?}, not the hardware ID {hardware_id}",
                printable(name, 64)
            ));
        }
        _ => {
            return Err(format!(
                "the gateway certificate's subject has {} common names, not one",
                names.len()
            ));
        }
    }

    if !is_ca(&issuing)? {
        return Err(
            "the second certificate is not a CA certificate: it must be the issuing CA that signed the gateway certificate".to_owned(),
        );
    }
    if issuing.subject().as_raw() == issuing.issuer().as_raw() {
        return Err(
            "the second certificate is a root (self-issued), not the issuing CA: the issuing CA is missing".to_owned(),
        );
    }
    if leaf.issuer().as_raw() != issuing.subject().as_raw()
        || tls::verify_as_the_handshake_does(leaf_der, issuing.public_key().raw).is_err()
    {
        return Err(
            "the gateway certificate is not signed by the second certificate (the issuing CA)"
                .to_owned(),
        );
    }

    if root.subject().as_raw() != root.issuer().as_raw() || !is_ca(&root)? {
        return Err(
            "the third certificate is not a root CA certificate (self-issued, with basicConstraints cA)".to_owned(),
        );
    }
    tls::verify_as_the_handshake_does(root_der, root.public_key().raw)
        .map_err(|reason| format!("the root certificate is not signed by its own key: {reason}"))?;
    if issuing.issuer().as_raw() != root.subject().as_raw()
        || tls::verify_as_the_handshake_does(issuing_der, root.public_key().raw).is_err()
    {
        return Err(
            "the issuing CA is not signed by the root certificate sent: the chain does not lead to that root".to_owned(),
        );
    }
    Ok(Chain {
        leaf: leaf_der.clone(),
        issuing: issuing_der.clone(),
        root: root_der.clone(),
    })
}

/// The certificate `der` is, with nothing after it; `position` names it in the error.
fn whole<'a>(position: &str, der: &'a [u8]) -> Result<X509Certificate<'a>, String> {
    match X509Certificate::from_der(der) {
        Ok(([], certificate)) => Ok(certificate),
        Ok(_) => Err(format!("the {position} has bytes after the certificate")),
        Err(_) => Err(format!("the {position} cannot be parsed")),
    }
}

/// Whether `certificate` has basicConstraints with cA set.
fn is_ca(certificate: &X509Certificate<'_>) -> Result<bool, String> {
    match certificate.basic_constraints() {
        Ok(constraints) => Ok(constraints.is_some_and(|constraints| constraints.value.ca)),
        Err(_) => Err("a certificate has unreadable basicConstraints".to_owned()),
    }
}

/// `der` as PEM with `label`: base64 in lines of 64 characters, each line ending in `\n`. The
/// files hold this canonical form, not the text as sent, so a rerun compares equal.
fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).unwrap_or_default());
        text.push('\n');
    }
    text.push_str(&format!("-----END {label}-----\n"));
    text
}

/// Writes the contents of a new file; tests pass one that fails half way.
type WriteFn<'a> = dyn FnMut(&mut File, &[u8]) -> io::Result<()> + 'a;

/// A new file, ready to be renamed over `target`.
struct Staged<'a> {
    temporary: OsString,
    target: &'a Target,
}

/// Makes each of `files` (target, contents, owner) hold its contents with that owner and
/// [`FILE_MODE`]: the files that already do are left alone; the others are staged first, all of
/// them, and only then renamed into place. A failure while staging removes every staged file and
/// renames nothing. Every step works on the targets' open directories ([`Directory`]).
fn replace_all(
    files: &[(&Target, String, Owner)],
    write: &mut WriteFn<'_>,
) -> Result<Vec<Outcome>, String> {
    let mut outcomes = Vec::with_capacity(files.len());
    let mut staged: Vec<Staged<'_>> = Vec::new();
    for (target, contents, owner) in files {
        let current = is_current(target, contents.as_bytes(), *owner)
            .map_err(|error| format!("cannot read {}: {error}", target.path().display()));
        let current = match current {
            Ok(current) => current,
            Err(error) => {
                remove(&staged);
                return Err(error);
            }
        };
        if current {
            outcomes.push(Outcome::Unchanged);
            continue;
        }
        match stage(target, contents.as_bytes(), *owner, write) {
            Ok(temporary) => staged.push(Staged { temporary, target }),
            Err(error) => {
                remove(&staged);
                return Err(error);
            }
        }
        outcomes.push(Outcome::Written);
    }
    for (index, file) in staged.iter().enumerate() {
        let target = file.target;
        if let Err(error) = target.directory.rename(&file.temporary, &target.name) {
            remove(staged.get(index..).unwrap_or_default());
            return Err(format!(
                "cannot replace {}: {error}{}",
                target.path().display(),
                if index > 0 {
                    "; the files before it were replaced, so run the command again"
                } else {
                    ""
                }
            ));
        }
    }
    for file in &staged {
        file.target.directory.sync().map_err(|error| {
            format!(
                "{} was replaced, but its directory cannot be flushed: {error}",
                file.target.path().display()
            )
        })?;
    }
    Ok(outcomes)
}

/// Removes the staged files that were not renamed; best effort, as it runs on a failure path.
fn remove(staged: &[Staged<'_>]) {
    for file in staged {
        let _ = file.target.directory.remove(&file.temporary);
    }
}

/// Whether `target` is a regular file (not a link) that holds exactly `contents`, belongs to
/// `owner` and has exactly [`FILE_MODE`]. A missing file, or anything else at the path, is not;
/// only a regular file is opened.
fn is_current(target: &Target, contents: &[u8], owner: Owner) -> io::Result<bool> {
    if target.directory.is_file(&target.name)? != Some(true) {
        return Ok(false);
    }
    let mut file = match target.directory.open_read(&target.name) {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner.uid
        || metadata.gid() != owner.gid
        || metadata.mode() & 0o7777 != FILE_MODE
        || u64::try_from(contents.len()).ok() != Some(metadata.len())
    {
        return Ok(false);
    }
    let mut existing = Vec::with_capacity(contents.len());
    Read::take(
        &mut file,
        u64::try_from(contents.len()).unwrap_or(u64::MAX) + 1,
    )
    .read_to_end(&mut existing)?;
    Ok(existing == contents)
}

/// A new file next to `target` holding `contents`, with `owner` and [`FILE_MODE`], flushed to
/// disk; removed again when any step fails. Its name in the target's directory is returned.
fn stage(
    target: &Target,
    contents: &[u8],
    owner: Owner,
    write: &mut WriteFn<'_>,
) -> Result<OsString, String> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let mut temporary = OsString::from(".");
    temporary.push(&target.name);
    temporary.push(format!(
        ".install-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let shown = || target.directory.path().join(&temporary);
    let mut file = target
        .directory
        .create_new(&temporary, 0o600)
        .map_err(|error| format!("cannot create {}: {error}", shown().display()))?;
    let written = write(&mut file, contents)
        .and_then(|()| std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid)))
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(FILE_MODE)))
        .and_then(|()| file.sync_all());
    drop(file);
    match written {
        Ok(()) => Ok(temporary),
        Err(error) => {
            let _ = target.directory.remove(&temporary);
            let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                " (the command needs root: it sets the files' owners)"
            } else {
                ""
            };
            Err(format!(
                "cannot write the new {}: {error}{hint}; nothing was replaced",
                target.path().display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use testkit::mqtt::ScratchDir;

    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    fn gateway() -> GatewayId {
        GatewayId::new("tg-0001").unwrap()
    }

    #[test]
    fn arguments_are_checked() {
        assert_eq!(
            parse(&["--hardware-id", "tg-0001"]),
            Ok(Command::Install(Options {
                hardware_id: gateway(),
                config: PathBuf::from("/etc/trustiot/agent.toml"),
            }))
        );
        // Whoever names the configuration decides where root writes: there is no option for it.
        let error = parse(&["--config", "/tmp/a.toml", "--hardware-id", "tg-0001"]).unwrap_err();
        assert!(error.contains("--config is not accepted"), "{error}");
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        for refused in [
            &[][..],
            &["--hardware-id"],
            &["--hardware-id", "tg 0001"],
            &["--hardware-id", "tg;reboot"],
            &["--hardware-id", "a", "--hardware-id", "a"],
            &["--hardware-id", "a", "--config"],
            &["--hardware-id", "a", "--config", "/a", "--config", "/a"],
            &["--hardware-id", "a", "--help"],
            &["--help", "--hardware-id", "a"],
            &["--hardware-id", "a", "--inspect"],
            &["tg-0001"],
        ] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn exit_codes_do_not_overlap_provision_s() {
        let codes: Vec<u8> = [
            Exit::Success,
            Exit::Failure,
            Exit::Usage,
            Exit::Refused,
            Exit::Configuration,
        ]
        .iter()
        .map(|exit| exit.code())
        .collect();
        assert_eq!(codes, [0, 1, 2, 8, 9]);
    }

    #[test]
    fn accounts_are_looked_up() {
        assert_eq!(account("root").unwrap(), Some(Owner { uid: 0, gid: 0 }));
        assert_eq!(account("no-such-account-617").unwrap(), None);
        assert!(account("bad\0name").is_err());
    }

    /// A root, an issuing CA under it and a gateway certificate (CN `cn`) under that.
    struct Pki {
        root: Vec<u8>,
        issuing: Vec<u8>,
        leaf: Vec<u8>,
    }

    fn ca_params(name: &str, constraints: BasicConstraints) -> CertificateParams {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(constraints);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    }

    fn pki(cn: &str) -> Pki {
        let root_key = KeyPair::generate().unwrap();
        let root_params = ca_params("Test Root", BasicConstraints::Unconstrained);
        let root = root_params.self_signed(&root_key).unwrap().der().to_vec();
        let root_issuer = Issuer::new(root_params, root_key);
        let issuing_key = KeyPair::generate().unwrap();
        let issuing_params = ca_params("Test Issuing", BasicConstraints::Constrained(0));
        let issuing = issuing_params
            .signed_by(&issuing_key, &root_issuer)
            .unwrap()
            .der()
            .to_vec();
        let issuer = Issuer::new(issuing_params, issuing_key);
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = params
            .signed_by(&KeyPair::generate().unwrap(), &issuer)
            .unwrap()
            .der()
            .to_vec();
        Pki {
            root,
            issuing,
            leaf,
        }
    }

    #[test]
    fn only_certificate_blocks_are_read() {
        let pki = pki("tg-0001");
        let text = format!(
            "{}\n{}\r\n{}",
            pem("CERTIFICATE", &pki.leaf),
            pem("CERTIFICATE", &pki.issuing).replace('\n', "\r\n"),
            pem("CERTIFICATE", &pki.root)
        );
        assert_eq!(
            certificates(text.as_bytes()).unwrap(),
            [pki.leaf.clone(), pki.issuing.clone(), pki.root.clone()]
        );
        let key = KeyPair::generate().unwrap().serialize_pem();
        for (input, expected) in [
            (format!("{text}{key}"), "not a CERTIFICATE"),
            (format!("{key}{text}"), "not a CERTIFICATE"),
            (format!("cookie=x\n{text}"), "text outside"),
            (format!("{text}trailing"), "text outside"),
            (
                pem("CERTIFICATE", &pki.leaf).replace("-----END CERTIFICATE-----\n", ""),
                "ends inside",
            ),
            (
                pem("CERTIFICATE", &pki.leaf)
                    .replace("-----END CERTIFICATE-----", "-----END PRIVATE KEY-----"),
                "does not end",
            ),
            (
                "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_owned(),
                "not base64",
            ),
            (String::new(), "no certificate"),
            ("\n \n".to_owned(), "no certificate"),
        ] {
            let error = certificates(input.as_bytes()).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
            assert!(!error.contains("cookie"), "{error}");
        }
        assert!(
            certificates(&[0xff, 0xfe])
                .unwrap_err()
                .contains("not text")
        );
    }

    #[test]
    fn only_a_chain_from_the_gateway_to_its_root_passes() {
        let pki = pki("tg-0001");
        // Another PKI whose CAs have the same names, so only the signatures tell them apart.
        let other = self::pki("tg-0001");
        let good = [pki.leaf.clone(), pki.issuing.clone(), pki.root.clone()];
        assert_eq!(
            check_chain(&good, &gateway()).unwrap(),
            Chain {
                leaf: pki.leaf.clone(),
                issuing: pki.issuing.clone(),
                root: pki.root.clone(),
            }
        );
        let cases: Vec<(Vec<Vec<u8>>, &str)> = vec![
            (vec![pki.leaf.clone()], "exactly 3"),
            (
                vec![pki.leaf.clone(), pki.root.clone()],
                "without the issuing CA",
            ),
            (
                vec![
                    pki.leaf.clone(),
                    pki.issuing.clone(),
                    pki.root.clone(),
                    pki.root.clone(),
                ],
                "4 certificate",
            ),
            (
                vec![pki.root.clone(), pki.issuing.clone(), pki.leaf.clone()],
                "out of order",
            ),
            (
                vec![pki.issuing.clone(), pki.leaf.clone(), pki.root.clone()],
                "out of order",
            ),
            (
                vec![pki.leaf.clone(), pki.root.clone(), pki.issuing.clone()],
                "a root (self-issued)",
            ),
            (
                vec![pki.leaf.clone(), pki.root.clone(), pki.root.clone()],
                "a root (self-issued)",
            ),
            (
                vec![pki.leaf.clone(), pki.leaf.clone(), pki.root.clone()],
                "not a CA",
            ),
            (
                vec![pki.leaf.clone(), pki.issuing.clone(), pki.issuing.clone()],
                "not a root CA",
            ),
            (
                vec![pki.leaf.clone(), pki.issuing.clone(), other.root.clone()],
                "does not lead to that root",
            ),
            (
                vec![pki.leaf.clone(), other.issuing.clone(), other.root.clone()],
                "not signed by the second",
            ),
            (
                vec![other.leaf.clone(), pki.issuing.clone(), pki.root.clone()],
                "not signed by the second",
            ),
            (
                vec![
                    pki.leaf[1..].to_vec(),
                    pki.issuing.clone(),
                    pki.root.clone(),
                ],
                "cannot be parsed",
            ),
            (
                vec![
                    [pki.leaf.clone(), vec![0]].concat(),
                    pki.issuing.clone(),
                    pki.root.clone(),
                ],
                "bytes after",
            ),
        ];
        for (index, (certificates, expected)) in cases.into_iter().enumerate() {
            let error = check_chain(&certificates, &gateway()).unwrap_err();
            assert!(error.contains(expected), "case {index}: {error}");
        }
        let error = check_chain(&good, &GatewayId::new("tg-0002").unwrap()).unwrap_err();
        assert!(error.contains("not the hardware ID tg-0002"), "{error}");
        // A forged signature: one bit of the issuing CA's signature flipped.
        let mut forged = pki.issuing.clone();
        let last = forged.len() - 1;
        forged[last] ^= 0x01;
        let error =
            check_chain(&[pki.leaf.clone(), forged, pki.root.clone()], &gateway()).unwrap_err();
        assert!(error.contains("does not lead"), "{error}");
        // A root that is self-issued but signed by another key.
        let error = check_chain(
            &[
                pki.leaf.clone(),
                pki.issuing.clone(),
                self_issued_not_self_signed(),
            ],
            &gateway(),
        )
        .unwrap_err();
        assert!(error.contains("not signed by its own key"), "{error}");
    }

    /// A certificate whose subject and issuer are both "Test Root", with cA set, signed by a key
    /// other than its own.
    fn self_issued_not_self_signed() -> Vec<u8> {
        let signer_key = KeyPair::generate().unwrap();
        let signer = Issuer::new(
            ca_params("Test Root", BasicConstraints::Unconstrained),
            signer_key,
        );
        ca_params("Test Root", BasicConstraints::Unconstrained)
            .signed_by(&KeyPair::generate().unwrap(), &signer)
            .unwrap()
            .der()
            .to_vec()
    }

    /// The owner this process gives a new file.
    fn me() -> Owner {
        let probe = ScratchDir::new("install-owner");
        let file = probe.write("probe", "x");
        let metadata = fs::metadata(file).unwrap();
        Owner {
            uid: metadata.uid(),
            gid: metadata.gid(),
        }
    }

    fn listing(directory: &Path) -> Vec<(String, u64, Vec<u8>)> {
        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = fs::symlink_metadata(entry.path()).unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    metadata.ino(),
                    fs::read(entry.path()).unwrap_or_default(),
                )
            })
            .collect();
        entries.sort();
        entries
    }

    /// A scratch directory and its path without links (the system's temporary directory may be
    /// reached through one, as on macOS), with mode 0755 whatever the umask.
    fn scratch(kind: &str) -> (ScratchDir, PathBuf) {
        let dir = ScratchDir::new(kind);
        let path = dir.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    /// The target `path`, its directory opened as `install` opens it.
    fn target(path: &Path) -> Target {
        Target {
            directory: Directory::open(path.parent().unwrap()).unwrap(),
            name: path.file_name().unwrap().to_owned(),
        }
    }

    #[test]
    fn a_write_that_fails_half_way_leaves_the_old_files_whole_and_nothing_else() {
        let (_dir, path) = scratch("install-interrupted");
        let first = path.join("gateway.pem");
        let second = path.join("broker-ca.pem");
        fs::write(&first, "old chain\n").unwrap();
        fs::write(&second, "old root\n").unwrap();
        fs::set_permissions(&first, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        fs::set_permissions(&second, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        let before = listing(&path);
        let owner = me();
        let (first_target, second_target) = (target(&first), target(&second));
        let files = [
            (&first_target, "new chain\n".repeat(100), owner),
            (&second_target, "new root\n".repeat(100), owner),
        ];
        // The first file is staged whole, the second fails after half of it: neither is renamed.
        let mut calls = 0;
        let error = replace_all(&files, &mut |file: &mut File, bytes: &[u8]| {
            calls += 1;
            if calls == 2 {
                file.write_all(&bytes[..bytes.len() / 2])?;
                return Err(io::Error::other("interrupted"));
            }
            file.write_all(bytes)
        })
        .unwrap_err();
        assert!(
            error.contains("interrupted") && error.contains("nothing was replaced"),
            "{error}"
        );
        assert_eq!(listing(&path), before);

        // Then the whole write: both replaced, new inodes, nothing else left.
        let outcomes = replace_all(&files, &mut |file: &mut File, bytes: &[u8]| {
            file.write_all(bytes)
        })
        .unwrap();
        assert_eq!(outcomes, [Outcome::Written, Outcome::Written]);
        let after = listing(&path);
        assert_eq!(after.len(), 2, "{after:?}");
        for ((name, inode, contents), (old_name, old_inode, _)) in after.iter().zip(&before) {
            assert_eq!(name, old_name);
            assert_ne!(inode, old_inode, "{name} was not replaced by a new file");
            assert!(contents.starts_with(b"new "), "{name}");
        }
        for path in [&first, &second] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, FILE_MODE);
        }
    }

    #[test]
    fn a_file_with_the_right_content_but_not_the_right_mode_owner_or_kind_is_replaced() {
        let (_dir, dir) = scratch("install-current");
        let owner = me();
        let path = dir.join("gateway.pem");
        fs::write(&path, "chain\n").unwrap();
        let file = target(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!is_current(&file, b"chain\n", owner).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        assert!(is_current(&file, b"chain\n", owner).unwrap());
        assert!(!is_current(&file, b"chain!\n", owner).unwrap());
        assert!(!is_current(&file, b"chain\n\n", owner).unwrap());
        let other = Owner {
            uid: owner.uid.wrapping_add(1),
            gid: owner.gid,
        };
        assert!(!is_current(&file, b"chain\n", other).unwrap());
        // A link to a file with the right content is replaced by a file.
        let link = dir.join("link.pem");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let link_target = target(&link);
        assert!(!is_current(&link_target, b"chain\n", owner).unwrap());
        let outcomes = replace_all(
            &[(&link_target, "chain\n".to_owned(), owner)],
            &mut |file: &mut File, bytes: &[u8]| file.write_all(bytes),
        )
        .unwrap();
        assert_eq!(outcomes, [Outcome::Written]);
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(fs::read(&path).unwrap(), b"chain\n");
        assert!(!is_current(&target(&dir.join("missing")), b"", owner).unwrap());
    }

    #[test]
    fn a_staged_file_is_removed_when_its_owner_cannot_be_set() {
        let owner = me();
        if owner.uid == 0 {
            // As root every owner can be set; the half-way failure above covers the clean-up.
            return;
        }
        let (_dir, path) = scratch("install-chown");
        let error = replace_all(
            &[(
                &target(&path.join("broker-ca.pem")),
                "root\n".to_owned(),
                Owner { uid: 0, gid: 0 },
            )],
            &mut |file: &mut File, bytes: &[u8]| file.write_all(bytes),
        )
        .unwrap_err();
        assert!(error.contains("needs root"), "{error}");
        assert!(listing(&path).is_empty());
    }

    #[test]
    fn writes_go_to_the_directory_that_was_checked_even_if_it_is_moved() {
        let (_dir, path) = scratch("install-moved");
        let checked = path.join("state");
        fs::create_dir(&checked).unwrap();
        let file = target(&checked.join("gateway.pem"));
        // After the checks, the directory is moved away and a link put in its place.
        let moved = path.join("moved");
        fs::rename(&checked, &moved).unwrap();
        let elsewhere = path.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &checked).unwrap();
        replace_all(
            &[(&file, "chain\n".to_owned(), me())],
            &mut |file: &mut File, bytes: &[u8]| file.write_all(bytes),
        )
        .unwrap();
        assert_eq!(fs::read(moved.join("gateway.pem")).unwrap(), b"chain\n");
        assert!(listing(&elsewhere).is_empty());
    }

    const CONFIG: &str = "[cloud]\nurl = \"wss://gw.trustiot.example/mqtt\"\n\
        trust = [\"/etc/ssl/certs/ca-certificates.crt\"]\n\n[broker]\n\
        ca = \"/etc/trustiot/broker-ca.pem\"\n\n[identity]\n\
        certificate = \"/var/lib/trustiot-agent/gateway.pem\"\nkey = { source = \"tpm\" }\n";

    #[test]
    fn the_configuration_is_read_only_when_its_owner_alone_can_write_it() {
        let (_dir, path) = scratch("install-config");
        let owner = me();
        let etc = path.join("etc");
        fs::create_dir(&etc).unwrap();
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)).unwrap();
        let config = etc.join("agent.toml");
        fs::write(&config, CONFIG).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = load_config(&config, owner).unwrap();
        assert_eq!(loaded.broker_ca, Path::new("/etc/trustiot/broker-ca.pem"));

        let refused = |path: &Path, owner: Owner, expected: &str| {
            let failure = load_config(path, owner).unwrap_err();
            assert!(
                matches!(failure, Failure::Configuration(_)),
                "{}: {failure:?}",
                path.display()
            );
            assert!(
                failure.to_string().contains(expected),
                "{}: expected {expected:?} in {failure}",
                path.display()
            );
        };
        let stranger = Owner {
            uid: owner.uid.wrapping_add(1),
            gid: owner.gid,
        };
        // Another owner, of the file and its directory.
        refused(&config, stranger, "must belong to root");
        // Writable by the group, then by others: the file, then the directory.
        for (path, mode, restored) in [
            (&config, 0o664, 0o644),
            (&config, 0o646, 0o644),
            (&etc, 0o775, 0o755),
            (&etc, 0o757, 0o755),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            refused(&config, owner, "writable by no one else");
            fs::set_permissions(path, fs::Permissions::from_mode(restored)).unwrap();
        }
        load_config(&config, owner).unwrap();
        // Through a symbolic link: to the file, or in the directory's path.
        let linked_file = etc.join("linked.toml");
        std::os::unix::fs::symlink(&config, &linked_file).unwrap();
        refused(&linked_file, owner, "not a regular file");
        let linked_etc = path.join("linked-etc");
        std::os::unix::fs::symlink(&etc, &linked_etc).unwrap();
        refused(&linked_etc.join("agent.toml"), owner, "symbolic link");
        refused(&etc.join("..").join("etc").join("agent.toml"), owner, "..");
        refused(&etc.join("missing.toml"), owner, "does not exist");
        refused(
            &path.join("missing").join("agent.toml"),
            owner,
            "does not exist",
        );
        fs::create_dir(etc.join("dir.toml")).unwrap();
        refused(&etc.join("dir.toml"), owner, "not a regular file");
        let large = etc.join("large.toml");
        fs::write(&large, " ".repeat(70 * 1024)).unwrap();
        fs::set_permissions(&large, fs::Permissions::from_mode(0o644)).unwrap();
        refused(&large, owner, "larger than");
    }

    #[test]
    fn a_target_directory_must_belong_to_its_owner_alone() {
        let (_dir, path) = scratch("install-target");
        let owner = me();
        let state = path.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let file = state.join("gateway.pem");
        Target::open("identity.certificate", &file, owner, AGENT_ACCOUNT).unwrap();
        let stranger = Owner {
            uid: owner.uid.wrapping_add(1),
            gid: owner.gid,
        };
        let refused = |path: &Path, owner: Owner, expected: &str| {
            let Err(failure) = Target::open("identity.certificate", path, owner, AGENT_ACCOUNT)
            else {
                panic!("{} was accepted", path.display());
            };
            assert!(matches!(failure, Failure::Configuration(_)), "{failure:?}");
            let message = failure.to_string();
            assert!(
                message.starts_with("identity.certificate: ") && message.contains(expected),
                "expected {expected:?} in {message}"
            );
        };
        refused(&file, stranger, "must belong to trustiot-agent");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o770)).unwrap();
        refused(&file, owner, "writable by no one else");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let link = path.join("link");
        std::os::unix::fs::symlink(&state, &link).unwrap();
        refused(&link.join("gateway.pem"), owner, "symbolic link");
        refused(
            &path.join("missing").join("gateway.pem"),
            owner,
            "does not exist",
        );
        // One file named two ways, here through a bind-like second name for the directory, is
        // the same file to `Target::is`; another name in it is not.
        let first = Target::open("a", &file, owner, "x").unwrap();
        let second = Target::open("b", &state.join(".").join("gateway.pem"), owner, "x").unwrap();
        assert!(first.is(&second.directory, &second.name).unwrap());
        assert!(
            !first
                .is(&second.directory, OsStr::new("other.pem"))
                .unwrap()
        );
    }

    #[test]
    fn the_root_crl_is_read_only_from_a_directory_only_root_or_the_agent_can_write() {
        let (_dir, path) = scratch("install-crl-directory");
        let owner = me();
        let owners = Owners {
            chain: owner,
            root: owner,
        };
        let crl = path.join("root-crl.pem");
        assert!(matches!(
            CrlSource::open(&crl, &owners).unwrap(),
            CrlSource::Directory { .. }
        ));
        assert!(matches!(
            CrlSource::open(&path.join("missing").join("root-crl.pem"), &owners).unwrap(),
            CrlSource::Missing(_)
        ));
        let refused = |owners: &Owners, expected: &str| {
            let Err(failure) = CrlSource::open(&crl, owners) else {
                panic!("the directory was accepted");
            };
            assert!(matches!(failure, Failure::Configuration(_)), "{failure:?}");
            assert!(failure.to_string().contains(expected), "{failure}");
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        refused(&owners, "writable by no one else");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let stranger = Owner {
            uid: owner.uid.wrapping_add(1),
            gid: owner.gid,
        };
        refused(
            &Owners {
                chain: stranger,
                root: stranger,
            },
            "must belong to root",
        );
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let linked = CrlSource::open(&link.join("root-crl.pem"), &owners);
        let _ = fs::remove_file(&link);
        let Err(failure) = linked else {
            panic!("a directory through a symbolic link was accepted");
        };
        assert!(failure.to_string().contains("symbolic link"), "{failure}");
    }
}
