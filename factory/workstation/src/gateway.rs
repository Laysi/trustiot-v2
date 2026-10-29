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
//! ssh runs with [`SSH_OPTIONS`]: no password prompt (`BatchMode`), no terminal, no agent or X11
//! forwarding (a gateway must not reach the operator's ssh agent), no port forwardings, no local
//! command, no connection sharing (each attempt is its own connection, and no master process
//! outlives the tool holding its pipes), and keepalives so a dead connection ends instead of
//! hanging. The destination is checked with [`Destination::parse`] and given after `--`, so it is
//! never read as an option. Host keys follow the operator's ssh configuration.
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
    /// Reads the executable at `path`: a regular file of 1 byte to [`MAX_AGENT_BYTES`].
    ///
    /// # Errors
    ///
    /// When it cannot be read, is not a regular file, is empty or is too large.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let bytes = files::read_public(path, MAX_AGENT_BYTES)?;
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
    upload: Option<UploadedAgent>,
    sudo: bool,
    retry: Backoff,
    attempt_timeout: Duration,
    run_timeout: Duration,
}

impl SshGateway {
    /// Runs the gateway's installed agent, or uploads `upload` for each run; with `sudo`,
    /// through `sudo -n`.
    #[must_use]
    pub fn new(destination: Destination, upload: Option<UploadedAgent>, sudo: bool) -> Self {
        Self {
            destination,
            upload,
            sudo,
            retry: SSH_BACKOFF,
            attempt_timeout: ATTEMPT_TIMEOUT,
            run_timeout: RUN_TIMEOUT,
        }
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
    /// within [`SSH_BACKOFF`] got an answer from the gateway, and a usage error (inside
    /// [`SshError::Local`]) when a value cannot be sent.
    pub async fn provision(&self, request: &Request<'_>) -> Result<Ran, SshError> {
        let command = remote_command(request, self.sudo, self.uploaded_sha256())?;
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
                .attempt(&command, deadline)
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
    async fn attempt(&self, command: &str, deadline: Instant) -> io::Result<Attempt> {
        let deadline = tokio::time::Instant::from_std(deadline);
        let environment: Vec<(&str, OsString)> = KEPT_ENVIRONMENT
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
            .collect();
        let mut child = tokio::process::Command::new(SSH_PROGRAM)
            .args(SSH_OPTIONS)
            .arg("--")
            .arg(self.destination.as_str())
            .arg(command)
            .env_clear()
            .envs(environment)
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
    let Some(stdout) = stdout else {
        return (Vec::new(), false);
    };
    let mut buffer = Vec::with_capacity(4096);
    let limit = u64::try_from(MAX_REPORT_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    // A read error ends the output; the exit status tells what happened.
    let _ = stdout.take(limit).read_to_end(&mut buffer).await;
    if buffer.len() > MAX_REPORT_BYTES {
        buffer.truncate(MAX_REPORT_BYTES);
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
            "pi@10.0.0.5",
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
    fn uploads_carry_their_digest() {
        let agent = UploadedAgent::from_bytes(b"abc".to_vec());
        assert_eq!(
            agent.sha256(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
