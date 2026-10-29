//! The gateway's side of `register --gateway` (decision 12, Laysi/trustiot-v2#616):
//! `trustiot-agent provision` (Laysi/trustiot-v2#613) run on the gateway through the system's
//! `ssh`, its standard output and exit status brought back for [`crate::pipeline`] to check.
//!
//! # What crosses to the gateway
//!
//! Only this, and nothing of the workstation's identity (its key, its certificate, the session,
//! the `--headers-file` values) is ever read for or handed to ssh:
//!
//! - **The remote command**: `sh -c '<script>' trustiot-provision '<hardware ID>' '<handle>'
//!   '<mode>' '<run>'` and, with an upload, `'<SHA-256 of the agent>'`. The script is one of two
//!   fixed texts ([`INSTALLED_SCRIPT`], [`UPLOAD_SCRIPT`]); it reads the values only as positional
//!   parameters, always in double quotes. Every value is checked with [`is_safe_word`] (ASCII
//!   letters, digits, `.`, `_`, `-`) before anything starts, so no value can close the quotes or
//!   reach a shell as code: the hardware ID is a `protocol::GatewayId`, the handle is printed from
//!   a [`KeyHandle`], the rest are fixed words and lower-case hex.
//! - **Standard input**: the agent executable with `--upload-agent`, nothing otherwise.
//! - **ssh's environment**: only [`KEPT_ENVIRONMENT`] of the tool's, so a `SendEnv` in the
//!   operator's ssh configuration has nothing else to forward.
//!
//! ssh runs with [`SSH_OPTIONS`]: no password prompt (`BatchMode`), only the keys the ssh
//! configuration names with `IdentityFile` (`IdentitiesOnly`, so no other key loaded in the
//! operator's ssh agent is offered to a gateway), no terminal, no agent or X11
//! forwarding (a gateway must not reach the operator's ssh agent), no port forwardings, no local
//! command, no connection sharing (each attempt is its own connection, and no master process
//! outlives the tool holding its pipes), and keepalives so a dead connection ends instead of
//! hanging. The destination is checked with [`Destination::parse`] and given after `--`, so it is
//! never read as an option.
//!
//! # Host keys
//!
//! A gateway's host key is bound to its hardware ID, not to the bench address it is reached at:
//! every call names the host key `trustiot-<hardware ID>` (`HostKeyAlias`) and looks it up only
//! in the tool's own file, `known_hosts` in the workstation directory ([`KnownHosts`],
//! `UserKnownHostsFile`; [`HOST_KEY_OPTIONS`] empty the system-wide file and turn off host keys
//! from DNS). A known hardware ID that answers with another host key is refused by
//! ssh, wherever it is plugged in. Before each run the tool asks ssh for the settings it will use
//! (`ssh -G` with the same options) and refuses to go on when host key checking is off
//! (`StrictHostKeyChecking no`), when the known hosts file is not the tool's (`/dev/null`
//! included), or when a `KnownHostsCommand` could supply keys
//! ([`check_effective_configuration`]). When the configuration leaves `StrictHostKeyChecking` at
//! `ask`, which `BatchMode` turns into a refusal of every new gateway, the calls add
//! `StrictHostKeyChecking=accept-new`: the first connection to a hardware ID records its host
//! key, and every later one must match it. `yes` and `accept-new` are kept as they are.
//!
//! # Upload
//!
//! With `--upload-agent`, [`UPLOAD_SCRIPT`] makes a directory with `mktemp -d` (mode 0700), writes
//! standard input into it, compares its `sha256sum` with the one the workstation computed (exit
//! [`CHECKSUM_MISMATCH`] when they differ), runs it, and removes the directory on exit through a
//! `trap`, also when the shell is ended by SIGHUP, SIGINT, SIGPIPE or SIGTERM.
//!
//! # Retries
//!
//! ssh ends with 255 when it cannot connect or the connection breaks. Neither
//! `trustiot-agent provision` (0 to 7) nor the scripts ([`UPLOAD_FAILED`], [`CHECKSUM_MISMATCH`],
//! 128 + a signal) end with 255, so 255 is retried, with the backoff of [`SSH_BACKOFF`]; so is an
//! attempt that runs past its deadline, which is killed first. Each attempt has one deadline for
//! all of it (upload, output and exit): [`ATTEMPT_TIMEOUT`] after it starts, but never later than
//! [`RUN_TIMEOUT`] after the first attempt started, which also bounds the waits between attempts.
//! So one run on the gateway ends within [`RUN_TIMEOUT`]. That is safe because
//! `provision` is idempotent: it reuses the key at the handle, and two runs at once end with the
//! same key (edge/agent/README.md, "金鑰佈建"). Every other status is the gateway's answer and is
//! never retried.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use protocol::GatewayId;
use protocol::provisioning::{KeyHandle, MAX_REPORT_BYTES};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tracing::{debug, warn};

use crate::Error;
use crate::files;
use crate::http::printable;
use crate::retry::Backoff;

/// The ssh client, found on `PATH`.
pub const SSH_PROGRAM: &str = "ssh";

/// The options every ssh call has, before `--` and the destination.
pub const SSH_OPTIONS: &[&str] = &[
    "-T",
    "-a",
    "-x",
    "-o",
    "BatchMode=yes",
    "-o",
    "IdentitiesOnly=yes",
    "-o",
    "ConnectTimeout=20",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "LogLevel=ERROR",
];

/// The variables of the tool's environment ssh gets; every other one is removed.
pub const KEPT_ENVIRONMENT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SSH_AUTH_SOCK",
    "XDG_RUNTIME_DIR",
];

/// The file of the gateways' host keys, in the workstation directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// The start of every gateway's host key alias: `trustiot-<hardware ID>`.
pub const HOST_KEY_ALIAS_PREFIX: &str = "trustiot-";

/// The longest `ssh -G` may take: it reads the configuration, which may run `Match exec`
/// commands, and connects to nothing.
const CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of `ssh -G`'s output accepted: its settings are a few KiB.
const MAX_CONFIGURATION_BYTES: usize = 256 * 1024;

/// The host key alias of `hardware_id`.
#[must_use]
pub fn host_key_alias(hardware_id: &GatewayId) -> String {
    format!("{HOST_KEY_ALIAS_PREFIX}{hardware_id}")
}

/// The tool's own known hosts file: [`KNOWN_HOSTS_FILE`] in the workstation directory, as an
/// absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHosts(String);

impl KnownHosts {
    /// The file in `directory`. ssh splits the value of `UserKnownHostsFile` at white space,
    /// reads quotes and backslashes in it, and expands `%` and `~`, so the path may hold only
    /// ASCII letters, digits and `/`, `.`, `_`, `-`, `+`, `@`, `,`, `:`.
    ///
    /// # Errors
    ///
    /// A usage error when the path holds anything else, or cannot be made absolute.
    pub fn in_directory(directory: &Path) -> Result<Self, Error> {
        let path = std::path::absolute(directory.join(KNOWN_HOSTS_FILE)).map_err(|error| {
            Error::Usage(format!(
                "cannot make the path of {KNOWN_HOSTS_FILE} in {} absolute: {error}",
                directory.display()
            ))
        })?;
        let text = path.to_str().unwrap_or_default();
        let safe = !text.is_empty()
            && text.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+' | b'@' | b',' | b':')
            });
        if !safe {
            return Err(Error::Usage(format!(
                "{}: register --gateway keeps the gateways' host keys in this file, and its path \
                 cannot be given to ssh safely: use a workstation directory (--dir) whose path \
                 has only ASCII letters, digits and '/', '.', '_', '-', '+', '@', ',', ':'",
                printable(&path.to_string_lossy())
            )));
        }
        Ok(Self(text.to_owned()))
    }

    /// The absolute path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the effective configuration does with `StrictHostKeyChecking`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyChecking {
    /// `yes` or `accept-new`: kept.
    Kept,
    /// `ask`: the calls add `StrictHostKeyChecking=accept-new`.
    AcceptNew,
}

/// The options of every call that leave the tool's own file the only source of host keys: no
/// system-wide known hosts file, no host keys from DNS.
pub const HOST_KEY_OPTIONS: &[&str] = &[
    "-o",
    "GlobalKnownHostsFile=/dev/null",
    "-o",
    "VerifyHostKeyDNS=no",
];

/// Checks `ssh -G`'s output for one gateway: host key checking is on, the known hosts file is
/// exactly `known_hosts`, the system-wide one is empty (`/dev/null`), host keys from DNS are not
/// accepted, the host key alias is `alias`, and no `KnownHostsCommand` is set.
///
/// # Errors
///
/// What is wrong, as a sentence for the operator.
pub fn check_effective_configuration(
    output: &str,
    alias: &str,
    known_hosts: &KnownHosts,
) -> Result<HostKeyChecking, String> {
    let setting = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    let checking = match setting("stricthostkeychecking").map(str::to_ascii_lowercase) {
        Some(value) if matches!(value.as_str(), "false" | "no" | "off") => {
            return Err(
                "the ssh configuration turns host key checking off (StrictHostKeyChecking no); \
                 remove that setting for the gateways"
                    .to_owned(),
            );
        }
        Some(value) if value == "ask" => HostKeyChecking::AcceptNew,
        Some(value) if matches!(value.as_str(), "true" | "yes" | "accept-new") => {
            HostKeyChecking::Kept
        }
        other => {
            return Err(format!(
                "ssh -G reports StrictHostKeyChecking {}, which this tool does not know",
                other
                    .as_deref()
                    .map_or_else(|| "(nothing)".to_owned(), printable)
            ));
        }
    };
    let files: Vec<&str> = setting("userknownhostsfile")
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if files.contains(&"/dev/null") {
        return Err(
            "the ssh configuration keeps known host keys in /dev/null (UserKnownHostsFile \
             /dev/null), so no host key is ever checked; remove that setting for the gateways"
                .to_owned(),
        );
    }
    if files != [known_hosts.as_str()] {
        return Err(format!(
            "ssh would look up host keys in {}, not only in the tool's own {}",
            printable(&files.join(" ")),
            known_hosts.as_str()
        ));
    }
    if setting("globalknownhostsfile") != Some("/dev/null") {
        return Err(format!(
            "ssh would also accept host keys from the system's files ({}), not only from the \
             tool's own {}",
            printable(setting("globalknownhostsfile").unwrap_or("(nothing)")),
            known_hosts.as_str()
        ));
    }
    if !matches!(
        setting("verifyhostkeydns")
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("false" | "no")
    ) {
        return Err(
            "the ssh configuration accepts host keys published in DNS (VerifyHostKeyDNS); remove \
             that setting for the gateways"
                .to_owned(),
        );
    }
    if setting("hostkeyalias") != Some(alias) {
        return Err(format!(
            "ssh would not name the gateway's host key {alias} (HostKeyAlias)"
        ));
    }
    if let Some(command) = setting("knownhostscommand").filter(|value| *value != "none") {
        return Err(format!(
            "the ssh configuration sets KnownHostsCommand ({}), which could supply any host key; \
             remove that setting for the gateways",
            printable(command)
        ));
    }
    Ok(checking)
}

/// `$0` of the remote scripts: the name in their messages.
pub const SCRIPT_NAME: &str = "trustiot-provision";

/// The remote script without an upload: runs the agent installed on the gateway (the factory
/// image's, Laysi/trustiot-v2#95). Positional parameters: hardware ID, handle, `inspect` or
/// `provision`, `sudo` or `direct`. One line without `'`, `!` or `\`, so any login shell passes it
/// to `sh` unchanged.
pub const INSTALLED_SCRIPT: &str = concat!(
    "set -u; id=$1; handle=$2; mode=$3; run=$4; ",
    "set -- provision --hardware-id \"$id\" --handle \"$handle\"; ",
    "if [ \"$mode\" = inspect ]; then set -- \"$@\" --inspect; fi; ",
    "if [ \"$run\" = sudo ]; then exec sudo -n trustiot-agent \"$@\"; fi; ",
    "exec trustiot-agent \"$@\"",
);

/// The remote script with an upload: the agent arrives on standard input. Positional parameters
/// as [`INSTALLED_SCRIPT`]'s, then the agent's SHA-256 (lower-case hex).
pub const UPLOAD_SCRIPT: &str = concat!(
    "set -u; id=$1; handle=$2; mode=$3; run=$4; sum=$5; ",
    "set -- provision --hardware-id \"$id\" --handle \"$handle\"; ",
    "if [ \"$mode\" = inspect ]; then set -- \"$@\" --inspect; fi; ",
    "umask 077; ",
    "dir=$(mktemp -d) || exit 90; ",
    "cleanup() { rm -rf -- \"$dir\"; }; ",
    "trap cleanup EXIT; ",
    "trap \"exit 129\" HUP; trap \"exit 130\" INT; trap \"exit 141\" PIPE; trap \"exit 143\" TERM; ",
    "cat > \"$dir/trustiot-agent\" || exit 90; ",
    "actual=$(sha256sum \"$dir/trustiot-agent\") || exit 90; ",
    "actual=${actual%% *}; ",
    "if [ \"$actual\" = \"$sum\" ]; then :; else ",
    "echo \"trustiot-provision: the uploaded agent does not have the SHA-256 the workstation sent\" >&2; ",
    "exit 91; fi; ",
    "chmod 700 \"$dir/trustiot-agent\" || exit 90; ",
    "if [ \"$run\" = sudo ]; then sudo -n \"$dir/trustiot-agent\" \"$@\"; ",
    "else \"$dir/trustiot-agent\" \"$@\"; fi; ",
    "status=$?; exit \"$status\"",
);

/// [`UPLOAD_SCRIPT`]'s status when the temporary directory or the file in it cannot be made.
pub const UPLOAD_FAILED: i32 = 90;

/// [`UPLOAD_SCRIPT`]'s status when the uploaded agent's SHA-256 is not the one sent.
pub const CHECKSUM_MISMATCH: i32 = 91;

/// ssh's own status when it cannot connect or the connection breaks.
pub const SSH_FAILURE: i32 = 255;

/// The largest `--upload-agent` file: the release agent is a few MiB.
pub const MAX_AGENT_BYTES: usize = 64 * 1024 * 1024;

/// The longest destination accepted.
pub const MAX_DESTINATION_LEN: usize = 255;

/// The most of ssh's standard error kept for messages (the agent's diagnostics included); the
/// rest is read and dropped.
pub const MAX_STDERR_BYTES: usize = 8 * 1024;

/// The longest one attempt may take, upload and TPM work included: creating a key on the
/// gateway's TPM takes seconds, uploading the agent on the factory network about as long.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest one run on the gateway may take, all attempts and the waits between them
/// included: the retry budget of [`SSH_BACKOFF`] plus one whole attempt.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(420);

/// The retry limits of the ssh calls: 5 attempts within 2 minutes, 1 s doubling to at most 10 s.
pub const SSH_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    cap: Duration::from_secs(10),
    max_attempts: 5,
    max_total: Duration::from_secs(120),
};

/// The gateway key's default handle, as the agent's (`agent::tpm::DEFAULT_KEY_HANDLE`).
pub const DEFAULT_HANDLE: u32 = 0x8100_0100;

/// The storage root key's handle, which never holds the gateway key.
const SRK_HANDLE: u32 = 0x8100_0001;

/// The last owner persistent handle; the agent refuses the platform's range above it.
const LAST_OWNER_HANDLE: u32 = 0x817f_ffff;

/// `--handle`: `0x` and 1 to 8 hex digits, an owner persistent handle (`0x81000000` to
/// `0x817fffff`) other than the storage root key's, the agent's own rule
/// (edge/agent/README.md, "出廠佈建"); [`DEFAULT_HANDLE`] when absent.
///
/// # Errors
///
/// A usage error for anything else.
pub fn parse_handle(text: Option<&str>) -> Result<KeyHandle, Error> {
    let Some(text) = text else {
        return KeyHandle::new(DEFAULT_HANDLE).map_err(|error| Error::Failed(error.to_string()));
    };
    let handle: KeyHandle = text
        .parse()
        .map_err(|error| Error::Usage(format!("--handle: {error}")))?;
    if handle.get() > LAST_OWNER_HANDLE || handle.get() == SRK_HANDLE {
        return Err(Error::Usage(format!(
            "--handle: {handle} cannot hold the gateway key: it must be an owner persistent \
             handle (0x81000000 to 0x817fffff) other than the storage root key's ({SRK_HANDLE:#010x})"
        )));
    }
    Ok(handle)
}

/// Whether `word` may stand in the remote command: 1 to 128 ASCII letters, digits, `.`, `_` or
/// `-`, not starting with `-`. Such a word means the same inside single quotes, double quotes or
/// none, and is never an option.
#[must_use]
pub fn is_safe_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 128
        && !word.starts_with('-')
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// An ssh destination as the operator gives it: `[user@]host`, a host name, an address or a name
/// from the ssh configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct Destination(String);

impl fmt::Debug for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Destination {
    /// Checks `--gateway`: 1 to [`MAX_DESTINATION_LEN`] ASCII letters, digits, `.`, `_`, `-`, `:`,
    /// `%`, `[`, `]` and at most one `@`, with something on both sides of it; never starting with
    /// `-`, which ssh would read as an option.
    ///
    /// # Errors
    ///
    /// A usage error naming the rule.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::Usage(format!(
                "--gateway {:?}: an ssh destination is [user@]host: 1 to {MAX_DESTINATION_LEN} \
                 ASCII letters, digits, '.', '_', '-', ':', '%', '[', ']' and at most one '@', not \
                 starting with '-' (put other settings, such as the port, in the ssh \
                 configuration)",
                printable(text)
            ))
        };
        if text.is_empty() || text.len() > MAX_DESTINATION_LEN || text.starts_with('-') {
            return Err(invalid());
        }
        if !text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b':' | b'%' | b'[' | b']' | b'@')
        }) {
            return Err(invalid());
        }
        match text.split_once('@') {
            Some((user, host)) if user.is_empty() || host.is_empty() || host.contains('@') => {
                Err(invalid())
            }
            _ => Ok(Self(text.to_owned())),
        }
    }

    /// The destination as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An agent executable to upload (`--upload-agent`).
pub struct UploadedAgent {
    bytes: Vec<u8>,
    sha256: String,
}

impl fmt::Debug for UploadedAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadedAgent")
            .field("bytes", &self.bytes.len())
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl UploadedAgent {
    /// Reads the executable at `path`: a regular file of 1 byte to [`MAX_AGENT_BYTES`] that
    /// belongs to the user running the tool and that the group and others cannot write to
    /// ([`files::read_executable`]): it runs as root on the gateway.
    ///
    /// # Errors
    ///
    /// When it cannot be read, is not a regular file, belongs to another user, is writable by
    /// the group or others, is empty or is too large.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let bytes = files::read_executable(path, MAX_AGENT_BYTES)?;
        if bytes.is_empty() {
            return Err(Error::BadFile {
                path: path.to_owned(),
                reason: "is empty: --upload-agent takes the aarch64 trustiot-agent executable",
            });
        }
        Ok(Self::from_bytes(bytes))
    }

    /// The executable `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self { bytes, sha256 }
    }

    /// Its SHA-256, lower-case hex.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Whether `provision` may create the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `provision --inspect`: only look.
    Inspect,
    /// `provision`: reuse or create the key.
    Provision,
}

impl Mode {
    fn word(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Provision => "provision",
        }
    }
}

/// One `trustiot-agent provision` run.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// `--hardware-id`.
    pub hardware_id: &'a GatewayId,
    /// `--handle`.
    pub handle: KeyHandle,
    /// With or without `--inspect`.
    pub mode: Mode,
}

/// The remote command for `request`: see the module documentation, "What crosses to the
/// gateway". `upload_sha256` is the uploaded agent's SHA-256, or `None` to run the installed
/// agent.
///
/// # Errors
///
/// A usage error when a value is not a [safe word](is_safe_word); nothing is sent then.
pub fn remote_command(
    request: &Request<'_>,
    sudo: bool,
    upload_sha256: Option<&str>,
) -> Result<String, Error> {
    let mut words = vec![
        request.hardware_id.as_str().to_owned(),
        request.handle.to_string(),
        request.mode.word().to_owned(),
        if sudo { "sudo" } else { "direct" }.to_owned(),
    ];
    if let Some(sum) = upload_sha256 {
        words.push(sum.to_owned());
    }
    if let Some(unsafe_word) = words.iter().find(|word| !is_safe_word(word)) {
        return Err(Error::Usage(format!(
            "{:?} cannot be sent to the gateway: only ASCII letters, digits, '.', '_' and '-'",
            printable(unsafe_word)
        )));
    }
    let script = if upload_sha256.is_some() {
        UPLOAD_SCRIPT
    } else {
        INSTALLED_SCRIPT
    };
    let mut command = format!("sh -c '{script}' {SCRIPT_NAME}");
    for word in &words {
        command.push_str(" '");
        command.push_str(word);
        command.push('\'');
    }
    Ok(command)
}

/// What one run on the gateway gave back.
#[derive(Clone, Debug)]
pub struct Ran {
    /// The remote command's exit status as ssh passed it on; `None` when ssh was ended by a
    /// signal.
    pub status: Option<i32>,
    /// Standard output, at most [`MAX_REPORT_BYTES`].
    pub stdout: Vec<u8>,
    /// More than [`MAX_REPORT_BYTES`] arrived on standard output; the rest was not read.
    pub too_long: bool,
    /// The start of standard error (ssh's and the remote command's), at most
    /// [`MAX_STDERR_BYTES`], as UTF-8 (lossy).
    pub stderr: String,
    /// How many ssh calls it took.
    pub attempts: u32,
}

/// Why no run gave an answer.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// ssh could not be started (it is not installed, for example).
    #[error("cannot run {SSH_PROGRAM}: {0}")]
    Spawn(#[source] io::Error),
    /// Every attempt ended with ssh's own failure (255) or ran out of time.
    #[error(
        "could not reach the gateway through ssh after {attempts} attempt(s); the last one: \
         {last}"
    )]
    Unreachable {
        /// How many ssh calls were made.
        attempts: u32,
        /// What the last one ended with, and the start of its standard error (printable).
        last: String,
    },
    /// The effective ssh configuration would not check the gateway's host key against the
    /// tool's own file, or `ssh -G` failed; nothing ran on the gateway.
    #[error("{0}")]
    Configuration(String),
    /// The operating system gave no randomness for the retry jitter.
    #[error(transparent)]
    Local(#[from] Error),
}

/// One attempt's outcome.
enum Attempt {
    Ended(Ran),
    TimedOut { stderr: String },
}

/// Runs `trustiot-agent provision` on one gateway through ssh.
#[derive(Debug)]
pub struct SshGateway {
    destination: Destination,
    known_hosts: KnownHosts,
    upload: Option<UploadedAgent>,
    sudo: bool,
    retry: Backoff,
    attempt_timeout: Duration,
    run_timeout: Duration,
}

impl SshGateway {
    /// Runs the gateway's installed agent, or uploads `upload` for each run; with `sudo`,
    /// through `sudo -n`. Host keys are looked up in `known_hosts` only.
    #[must_use]
    pub fn new(
        destination: Destination,
        known_hosts: KnownHosts,
        upload: Option<UploadedAgent>,
        sudo: bool,
    ) -> Self {
        Self {
            destination,
            known_hosts,
            upload,
            sudo,
            retry: SSH_BACKOFF,
            attempt_timeout: ATTEMPT_TIMEOUT,
            run_timeout: RUN_TIMEOUT,
        }
    }

    /// The tool's known hosts file.
    #[must_use]
    pub fn known_hosts(&self) -> &KnownHosts {
        &self.known_hosts
    }

    /// The environment ssh runs with: only [`KEPT_ENVIRONMENT`] of the tool's.
    fn environment() -> Vec<(&'static str, OsString)> {
        KEPT_ENVIRONMENT
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
            .collect()
    }

    /// Runs `ssh <options> -G -- <destination>`: its exit status, at most
    /// [`MAX_CONFIGURATION_BYTES`] of its standard output and [`MAX_STDERR_BYTES`] of its standard
    /// error, within [`CONFIGURATION_TIMEOUT`]. More output than that ends the call.
    async fn effective_configuration(
        &self,
        options: &[String],
    ) -> Result<(Option<i32>, Vec<u8>, String), SshError> {
        let deadline = tokio::time::Instant::now() + CONFIGURATION_TIMEOUT;
        let mut child = tokio::process::Command::new(SSH_PROGRAM)
            .args(options)
            .arg("-G")
            .arg("--")
            .arg(self.destination.as_str())
            .env_clear()
            .envs(Self::environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(SshError::Spawn)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let exchange = async {
            let out = read_bounded(stdout, MAX_CONFIGURATION_BYTES);
            let err = read_head(stderr);
            tokio::pin!(out, err);
            tokio::select! {
                (out, too_long) = &mut out => {
                    if too_long {
                        return (out, true, String::new());
                    }
                    (out, false, err.await)
                }
                err = &mut err => {
                    let (out, too_long) = out.await;
                    (out, too_long, err)
                }
            }
        };
        let stopped = |problem: String| {
            SshError::Configuration(format!(
                "ssh -G (the settings ssh would use for {}) {problem}. Nothing was run on the \
                 gateway; check the ssh configuration (Match exec, Include)",
                self.destination
            ))
        };
        let Ok((stdout, too_long, stderr)) = tokio::time::timeout_at(deadline, exchange).await
        else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(stopped(format!(
                "did not end within {} s",
                CONFIGURATION_TIMEOUT.as_secs()
            )));
        };
        if too_long {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(stopped(format!(
                "printed more than {MAX_CONFIGURATION_BYTES} bytes"
            )));
        }
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(status) => Ok((status.map_err(SshError::Spawn)?.code(), stdout, stderr)),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(stopped(format!(
                    "did not end within {} s",
                    CONFIGURATION_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Every option of the calls for `hardware_id`, before `--`: [`SSH_OPTIONS`], the options
    /// that bind the host key to the hardware ID, and `StrictHostKeyChecking=accept-new` when
    /// the configuration leaves it at `ask`; after `ssh -G` with the same options showed that
    /// the calls check the host key against the tool's own file (module documentation, "Host
    /// keys").
    async fn checked_options(&self, hardware_id: &GatewayId) -> Result<Vec<String>, SshError> {
        let alias = host_key_alias(hardware_id);
        let mut options: Vec<String> = SSH_OPTIONS
            .iter()
            .map(|&option| option.to_owned())
            .collect();
        options.extend([
            "-o".to_owned(),
            format!("HostKeyAlias={alias}"),
            "-o".to_owned(),
            format!("UserKnownHostsFile={}", self.known_hosts.as_str()),
        ]);
        options.extend(HOST_KEY_OPTIONS.iter().map(|&option| option.to_owned()));
        let (status, stdout, stderr) = self.effective_configuration(&options).await?;
        if status != Some(0) {
            return Err(SshError::Configuration(format!(
                "ssh -G (the settings ssh would use for {}) failed ({}): {}. Nothing was run on \
                 the gateway; check the ssh configuration",
                self.destination,
                status.map_or_else(
                    || "ended by a signal".to_owned(),
                    |code| format!("status {code}")
                ),
                excerpt(&stderr)
            )));
        }
        let checking = check_effective_configuration(
            &String::from_utf8_lossy(&stdout),
            &alias,
            &self.known_hosts,
        )
        .map_err(|problem| {
            SshError::Configuration(format!(
                "ssh to {} would not check the gateway's host key: {problem}. Nothing was run on \
                 the gateway",
                self.destination
            ))
        })?;
        if checking == HostKeyChecking::AcceptNew {
            options.extend([
                "-o".to_owned(),
                "StrictHostKeyChecking=accept-new".to_owned(),
            ]);
        }
        Ok(options)
    }

    /// The destination.
    #[must_use]
    pub fn destination(&self) -> &Destination {
        &self.destination
    }

    /// The uploaded agent's SHA-256, if one is uploaded.
    #[must_use]
    pub fn uploaded_sha256(&self) -> Option<&str> {
        self.upload.as_ref().map(UploadedAgent::sha256)
    }

    /// Runs `request` on the gateway, retrying ssh's own failures: see the module documentation.
    ///
    /// # Errors
    ///
    /// [`SshError::Spawn`] when ssh cannot be started, [`SshError::Unreachable`] when no attempt
    /// within [`SSH_BACKOFF`] got an answer from the gateway, [`SshError::Configuration`] when
    /// the calls would not check the gateway's host key against the tool's own file (nothing
    /// runs then), and a usage error (inside [`SshError::Local`]) when a value cannot be sent.
    pub async fn provision(&self, request: &Request<'_>) -> Result<Ran, SshError> {
        let command = remote_command(request, self.sudo, self.uploaded_sha256())?;
        let options = self.checked_options(request.hardware_id).await?;
        let started = Instant::now();
        let run_deadline = started + self.run_timeout;
        let mut attempt: u32 = 1;
        loop {
            debug!(
                destination = %self.destination,
                hardware_id = request.hardware_id.as_str(),
                mode = request.mode.word(),
                attempt,
                "running trustiot-agent provision over ssh"
            );
            let deadline = (Instant::now() + self.attempt_timeout).min(run_deadline);
            let last = match self
                .attempt(&options, &command, deadline)
                .await
                .map_err(SshError::Spawn)?
            {
                Attempt::Ended(mut ran) if ran.status != Some(SSH_FAILURE) => {
                    ran.attempts = attempt;
                    return Ok(ran);
                }
                Attempt::Ended(ran) => {
                    format!("ssh ended with {SSH_FAILURE}: {}", excerpt(&ran.stderr))
                }
                Attempt::TimedOut { stderr } => format!(
                    "no end within its time limit ({} s an attempt, {} s in all): {}",
                    self.attempt_timeout.as_secs(),
                    self.run_timeout.as_secs(),
                    excerpt(&stderr)
                ),
            };
            let delay = self.retry.delay(attempt + 1, started.elapsed())?;
            let Some(delay) = delay.filter(|delay| Instant::now() + *delay < run_deadline) else {
                return Err(SshError::Unreachable {
                    attempts: attempt,
                    last,
                });
            };
            warn!(
                destination = %self.destination,
                attempt,
                problem = %last,
                delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                "the ssh call did not reach the gateway; trying again"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// One ssh call: writes the upload (if any) to its standard input, reads at most
    /// [`MAX_REPORT_BYTES`] + 1 of standard output and [`MAX_STDERR_BYTES`] of standard error,
    /// all before `deadline`, when the call is killed.
    async fn attempt(
        &self,
        options: &[String],
        command: &str,
        deadline: Instant,
    ) -> io::Result<Attempt> {
        let deadline = tokio::time::Instant::from_std(deadline);
        let mut child = tokio::process::Command::new(SSH_PROGRAM)
            .args(options)
            .arg("--")
            .arg(self.destination.as_str())
            .arg(command)
            .env_clear()
            .envs(Self::environment())
            .stdin(if self.upload.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let upload = self.upload.as_ref().map(|agent| agent.bytes.as_slice());
        let write = async move {
            if let (Some(mut stdin), Some(bytes)) = (stdin, upload) {
                // The remote side may end before it reads everything (a failed mktemp): a broken
                // pipe here is its answer, which the exit status reports.
                let _ = stdin.write_all(bytes).await;
                let _ = stdin.shutdown().await;
            }
        };
        // Standard output is read alongside the upload and standard error. Once it passes the
        // limit, the other two are dropped at once and the call is killed below: a gateway that
        // keeps writing must get its report refused, not hold the call until its deadline.
        let exchange = async {
            let report = read_report(stdout);
            let rest = async { tokio::join!(write, read_head(stderr)) };
            tokio::pin!(report, rest);
            tokio::select! {
                (out, too_long) = &mut report => {
                    if too_long {
                        return (out, true, String::new());
                    }
                    let ((), err) = rest.await;
                    (out, false, err)
                }
                ((), err) = &mut rest => {
                    let (out, too_long) = report.await;
                    (out, too_long, err)
                }
            }
        };
        let Ok((stdout, too_long, stderr)) = tokio::time::timeout_at(deadline, exchange).await
        else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(Attempt::TimedOut {
                stderr: String::new(),
            });
        };
        if too_long {
            // Nothing more is read: end the call rather than wait for the rest.
            let _ = child.start_kill();
        }
        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(Attempt::TimedOut { stderr });
            }
        };
        Ok(Attempt::Ended(Ran {
            status: status.code(),
            stdout,
            too_long,
            stderr,
            attempts: 0,
        }))
    }
}

/// At most [`MAX_REPORT_BYTES`] of `stdout`, and whether more arrived.
async fn read_report(stdout: Option<impl AsyncRead + Unpin>) -> (Vec<u8>, bool) {
    read_bounded(stdout, MAX_REPORT_BYTES).await
}

/// At most `max_bytes` of `reader`, and whether more arrived; nothing past `max_bytes` + 1 is
/// read.
async fn read_bounded(reader: Option<impl AsyncRead + Unpin>, max_bytes: usize) -> (Vec<u8>, bool) {
    let Some(reader) = reader else {
        return (Vec::new(), false);
    };
    let mut buffer = Vec::with_capacity(4096);
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    // A read error ends the output; the exit status tells what happened.
    let _ = reader.take(limit).read_to_end(&mut buffer).await;
    if buffer.len() > max_bytes {
        buffer.truncate(max_bytes);
        return (buffer, true);
    }
    (buffer, false)
}

/// The first [`MAX_STDERR_BYTES`] of `stderr`; the rest is read and dropped, so the remote side
/// never blocks on it.
async fn read_head(stderr: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buffer = Vec::new();
    let limit = u64::try_from(MAX_STDERR_BYTES).unwrap_or(u64::MAX);
    let _ = (&mut stderr).take(limit).read_to_end(&mut buffer).await;
    let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The last lines of `stderr` as one printable line, for a message.
#[must_use]
pub fn excerpt(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "nothing on standard error".to_owned();
    }
    let start = lines.len().saturating_sub(3);
    let joined = lines.get(start..).unwrap_or_default().join(" | ");
    printable(&joined)
}

/// What an exit status of the remote command means, for messages.
#[must_use]
pub fn describe_status(status: Option<i32>) -> String {
    let Some(status) = status else {
        return "ssh was ended by a signal".to_owned();
    };
    let meaning = match status {
        0 => "success",
        1 => {
            "a failure on the gateway: the TPM refused a command or did not answer, the device \
             could not be opened, the CSR did not check out, or sudo -n needs a password"
        }
        2 => "trustiot-agent refused its arguments (an agent too old for this tool?)",
        3 => "the gateway has no TPM device",
        4 => "the TPM device is not the kernel's resource manager",
        5 => "another object is at the key handle; it was left untouched",
        6 => "the TPM's EK certificate is not for its EK, or cannot be read",
        7 => {
            "an EK setup that is not supported (EK template or nonce in NV, endorsement authorization)"
        }
        UPLOAD_FAILED => {
            "the upload failed on the gateway: the temporary directory or the file in it could \
             not be made (disk full?)"
        }
        CHECKSUM_MISMATCH => "the uploaded agent arrived with another SHA-256",
        126 => {
            "the agent cannot be executed: not an aarch64 executable, or the temporary directory \
             is mounted noexec"
        }
        127 => {
            "trustiot-agent (or sudo) is not installed on the gateway: give --upload-agent with \
             the aarch64 agent"
        }
        129 | 130 | 141 | 143 => "the remote command was interrupted by a signal",
        _ => "an unexpected status",
    };
    format!("status {status}: {meaning}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> GatewayId {
        GatewayId::new(text).expect("a gateway ID")
    }

    fn handle() -> KeyHandle {
        KeyHandle::new(0x8100_0100).expect("a handle")
    }

    #[test]
    fn scripts_cannot_be_broken_out_of() {
        for script in [INSTALLED_SCRIPT, UPLOAD_SCRIPT] {
            for forbidden in ['\'', '!', '\\', '\n', '`'] {
                assert!(!script.contains(forbidden), "{forbidden:?} in {script}");
            }
        }
        assert!(is_safe_word(SCRIPT_NAME));
    }

    #[test]
    fn the_remote_command_is_the_script_and_checked_words() {
        let hardware_id = id("tg-0001");
        let request = Request {
            hardware_id: &hardware_id,
            handle: handle(),
            mode: Mode::Inspect,
        };
        assert_eq!(
            remote_command(&request, false, None).expect("a command"),
            format!(
                "sh -c '{INSTALLED_SCRIPT}' trustiot-provision 'tg-0001' '0x81000100' 'inspect' \
                 'direct'"
            )
        );
        let sum = "ab".repeat(32);
        let request = Request {
            mode: Mode::Provision,
            ..request
        };
        assert_eq!(
            remote_command(&request, true, Some(&sum)).expect("a command"),
            format!(
                "sh -c '{UPLOAD_SCRIPT}' trustiot-provision 'tg-0001' '0x81000100' 'provision' \
                 'sudo' '{sum}'"
            )
        );
        assert!(matches!(
            remote_command(&request, true, Some("ab;cd")),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn only_safe_words_pass() {
        for good in ["tg-0001", "0x81000100", "A.b_c-9"] {
            assert!(is_safe_word(good), "{good}");
        }
        for bad in [
            "", "-o", "a b", "a;b", "a'b", "a\"b", "$(id)", "`id`", "a\nb", "a|b", "a&b", "a/b",
            "a*", "é",
        ] {
            assert!(!is_safe_word(bad), "{bad:?}");
        }
        assert!(!is_safe_word(&"a".repeat(129)));
    }

    #[test]
    fn destinations_are_never_options() {
        for good in [
            "gateway-01",
            "admin@192.0.2.10",
            "admin@gw.factory.example",
            "fe80::1%eth0",
            "user@[2001:db8::1]",
            "gw_7",
        ] {
            assert_eq!(
                Destination::parse(good).expect("valid").as_str(),
                good,
                "{good}"
            );
        }
        for bad in [
            "",
            "-oProxyCommand=touch x",
            "-p",
            "a b",
            "a;b",
            "@host",
            "user@",
            "a@b@c",
            "ssh://host:22",
            "host\n",
            "$(id)",
            "a'b",
        ] {
            assert!(
                matches!(Destination::parse(bad), Err(Error::Usage(_))),
                "{bad:?}"
            );
        }
        assert!(Destination::parse(&"a".repeat(256)).is_err());
    }

    #[test]
    fn statuses_are_described_and_stderr_is_made_printable() {
        assert!(describe_status(Some(5)).contains("left untouched"));
        assert!(describe_status(Some(CHECKSUM_MISMATCH)).contains("SHA-256"));
        assert!(describe_status(None).contains("signal"));
        assert_eq!(excerpt(""), "nothing on standard error");
        let shown = excerpt("one\n\ntwo\nthree\nfour \u{1b}[31mred\n");
        assert!(shown.starts_with("two | three | four"), "{shown}");
        assert!(!shown.contains('\u{1b}'), "{shown}");
    }

    #[test]
    fn handles_follow_the_agents_rule() {
        assert_eq!(
            parse_handle(None).expect("the default").get(),
            DEFAULT_HANDLE
        );
        assert_eq!(
            parse_handle(Some("0x81000101")).expect("valid").to_string(),
            "0x81000101"
        );
        for bad in [
            "0x81000001",
            "0x81800000",
            "0x80000000",
            "81000100",
            "0x81000100;id",
            "$(id)",
            "0x 81000100",
            "",
        ] {
            assert!(
                matches!(parse_handle(Some(bad)), Err(Error::Usage(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_known_hosts_file_is_absolute_and_safe_for_ssh() {
        let known = KnownHosts::in_directory(Path::new("/srv/station-1/trustiot")).expect("safe");
        assert_eq!(known.as_str(), "/srv/station-1/trustiot/known_hosts");
        let relative = KnownHosts::in_directory(Path::new("station")).expect("made absolute");
        assert!(relative.as_str().starts_with('/'), "{}", relative.as_str());
        assert!(relative.as_str().ends_with("/station/known_hosts"));
        for bad in [
            "/srv/a b",
            "/srv/%h",
            "/srv/~x",
            "/srv/a\"b",
            "/srv/a'b",
            "/srv/a\\b",
            "/srv/a#b",
            "/srv/a=b",
            "/srv/é",
        ] {
            assert!(
                matches!(
                    KnownHosts::in_directory(Path::new(bad)),
                    Err(Error::Usage(_))
                ),
                "{bad:?}"
            );
        }
        assert_eq!(host_key_alias(&id("tg-0001")), "trustiot-tg-0001");
    }

    /// `ssh -G`'s output with `stricthostkeychecking`, `userknownhostsfile` and the rest as
    /// given.
    fn effective(checking: &str, files: &str, extra: &str) -> String {
        format!(
            "host gateway\nuser admin\nbatchmode yes\nstricthostkeychecking {checking}\n\
             hostkeyalias trustiot-tg-0001\nverifyhostkeydns false\n\
             globalknownhostsfile /dev/null\nuserknownhostsfile {files}\n{extra}"
        )
    }

    #[test]
    fn only_settings_that_check_the_host_key_against_the_tools_file_pass() {
        let known = KnownHosts::in_directory(Path::new("/srv/ws")).expect("safe");
        let file = known.as_str();
        let check =
            |output: &str| check_effective_configuration(output, "trustiot-tg-0001", &known);
        assert_eq!(
            check(&effective("ask", file, "")),
            Ok(HostKeyChecking::AcceptNew)
        );
        for kept in ["true", "yes", "accept-new"] {
            assert_eq!(
                check(&effective(kept, file, "knownhostscommand none\n")),
                Ok(HostKeyChecking::Kept),
                "{kept}"
            );
        }
        for off in ["false", "no", "off"] {
            let problem = check(&effective(off, file, "")).expect_err("refused");
            assert!(
                problem.contains("StrictHostKeyChecking no"),
                "{off}: {problem}"
            );
        }
        let problem = check(&effective("ask", "/dev/null", "")).expect_err("refused");
        assert!(problem.contains("/dev/null"), "{problem}");
        let problem = check(&effective("ask", &format!("{file} /dev/null"), "")).expect_err("x");
        assert!(problem.contains("/dev/null"), "{problem}");
        let problem = check(&effective(
            "ask",
            "~/.ssh/known_hosts ~/.ssh/known_hosts2",
            "",
        ))
        .expect_err("x");
        assert!(problem.contains("not only in the tool's own"), "{problem}");
        let problem = check(&effective(
            "yes",
            file,
            "knownhostscommand /usr/local/bin/keys %H\n",
        ))
        .expect_err("refused");
        assert!(problem.contains("KnownHostsCommand"), "{problem}");
        let other_alias = effective("ask", file, "").replace("trustiot-tg-0001", "gateway");
        assert!(check(&other_alias).expect_err("x").contains("HostKeyAlias"));
        let unknown = effective("ask", file, "").replace("stricthostkeychecking ask\n", "");
        assert!(check(&unknown).expect_err("x").contains("does not know"));
        let global = effective("ask", file, "").replace(
            "globalknownhostsfile /dev/null",
            "globalknownhostsfile /etc/ssh/ssh_known_hosts",
        );
        assert!(check(&global).expect_err("x").contains("system's files"));
        let dns =
            effective("ask", file, "").replace("verifyhostkeydns false", "verifyhostkeydns true");
        assert!(check(&dns).expect_err("x").contains("VerifyHostKeyDNS"));
    }

    #[test]
    fn uploads_carry_their_digest() {
        let agent = UploadedAgent::from_bytes(b"abc".to_vec());
        assert_eq!(
            agent.sha256(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
