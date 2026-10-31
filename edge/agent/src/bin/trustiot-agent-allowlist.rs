//! `trustiot-agent-allowlist`: the component that keeps the fieldbus allowlist in nftables
//! (Laysi/trustiot-v2#864). The format and the sets are in [`agent::allowlist`]; the interface,
//! the permission boundary and the threat model in `README.md`, section "fieldbus 放行集合".
//!
//! systemd starts one for each connection to `trustiot-agent-allowlist.socket` (`Accept=yes`),
//! with the connection as standard input and output and `CAP_NET_ADMIN` as its only capability
//! (`systemd/trustiot-agent-allowlist@.service`). It reads one request, at most
//! [`MAX_REQUEST_BYTES`], until the client closes its side; refuses it whole unless it is a
//! complete list ([`Allowlist::parse`]); has nft replace the contents of the two sets in one
//! transaction ([`Allowlist::nft_script`]); writes one answer line ([`Reply`]); logs what it did
//! on standard error, which the unit sends to the journal (the instance's name there carries the
//! connecting process's PID and UID); and exits: 0 applied, 2 refused, 1 failed.
//!
//! It takes no arguments and reads no environment variable, no configuration and no name service.
//! The only program it starts is nft, by absolute path, with an empty environment; nft reads the
//! script from an unnamed temporary file ([`apply`]), so it always gets the whole script, never a
//! part of it, and never a byte of the request.

#![allow(
    clippy::print_stderr,
    reason = "the log goes to standard error, which the unit sends to the journal"
)]

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;

use agent::allowlist::{Allowlist, MAX_REQUEST_BYTES, Reply};

/// nft, where Debian's nftables package installs it (`trustiot-agent-egress.service` runs the
/// same one).
const NFT: &str = "/usr/sbin/nft";

/// Where the script file is created, unnamed: the unit's private `/tmp` (`PrivateTmp=`).
const SCRIPT_DIRECTORY: &str = "/tmp";

/// The most bytes of nft's error output that are logged.
const MAX_LOGGED_BYTES: usize = 4096;

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "trustiot-agent-allowlist takes no arguments: systemd starts it for each connection to \
             trustiot-agent-allowlist.socket, with the request on standard input"
        );
        return ExitCode::from(2);
    }
    let reply = handle();
    let mut stdout = io::stdout().lock();
    if let Err(error) = writeln!(stdout, "{reply}").and_then(|()| stdout.flush()) {
        eprintln!("the answer did not reach the client: {error}");
    }
    match reply {
        Reply::Applied(_) => ExitCode::SUCCESS,
        Reply::Refused(_) => ExitCode::from(2),
        Reply::Failed(_) => ExitCode::FAILURE,
    }
}

/// Reads the request, checks it and applies it; logs the outcome.
fn handle() -> Reply {
    let mut request = Vec::new();
    // One byte past the limit tells a request of the largest size from a larger one; the rest of
    // a larger one is never read.
    let limit = u64::try_from(MAX_REQUEST_BYTES).map_or(u64::MAX, |limit| limit.saturating_add(1));
    if let Err(error) = io::stdin().lock().take(limit).read_to_end(&mut request) {
        eprintln!("reading the request failed; the sets are unchanged: {error}");
        return Reply::Failed(format!("reading the request failed: {}", error.kind()));
    }
    let list = match Allowlist::parse(&request) {
        Ok(list) => list,
        Err(error) => {
            eprintln!(
                "refused a request of {} bytes; the sets are unchanged: {error}",
                request.len()
            );
            return Reply::Refused(error.to_string());
        }
    };
    match apply(&list.nft_script()) {
        Ok(()) => {
            let (v4, v6) = list.families();
            eprintln!("applied {} destinations ({v4} IPv4, {v6} IPv6)", list.len());
            Reply::Applied(list.len())
        }
        Err(failure) => {
            eprintln!(
                "did not apply a list of {} destinations; the sets are unchanged: {}",
                list.len(),
                failure.detail
            );
            Reply::Failed(failure.reason)
        }
    }
}

/// Why nft did not apply a script: a short reason for the client and the detail for the log.
struct Failure {
    reason: String,
    detail: String,
}

impl Failure {
    fn io(what: &str, error: &io::Error) -> Self {
        Self {
            reason: format!("{what}: {}", error.kind()),
            detail: format!("{what}: {error}"),
        }
    }
}

/// Has nft apply `script` as one transaction.
///
/// The script goes into an unnamed file (`O_TMPFILE`) that is complete before nft starts, and
/// nft reads it as its standard input. Through a pipe, a client that kept the component reading
/// until systemd stopped it could have nft see the end of its input in the middle of the script
/// and apply the commands before it, the flush of the sets among them; a complete file has no
/// such middle. The file has no name, so nothing else can open or replace it, and it is gone
/// when nft and this process close it.
fn apply(script: &str) -> Result<(), Failure> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_TMPFILE)
        .open(SCRIPT_DIRECTORY)
        .map_err(|error| Failure::io("creating the script file failed", &error))?;
    file.write_all(script.as_bytes())
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .map_err(|error| Failure::io("writing the script file failed", &error))?;
    let output = Command::new(NFT)
        .args(["-f", "-"])
        .env_clear()
        .stdin(Stdio::from(file))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| Failure::io("starting nft failed", &error))?;
    if output.status.success() {
        return Ok(());
    }
    let mut errors = String::from_utf8_lossy(&output.stderr)
        .trim()
        .replace('\n', " | ");
    if errors.len() > MAX_LOGGED_BYTES {
        let mut end = MAX_LOGGED_BYTES;
        while !errors.is_char_boundary(end) {
            end -= 1;
        }
        errors.truncate(end);
        errors.push_str(" ...");
    }
    Err(Failure {
        reason: format!("nft {}", output.status),
        detail: format!("nft {}: {errors}", output.status),
    })
}
