//! `trustiot-agent-nameservers`: puts the nameservers glibc uses in the DNS allowance of the
//! agent's default deny (Laysi/trustiot-v2#879). The parsing and the sets are in
//! [`agent::nameservers`]; who runs it and when in `README.md`, section "連線位址的限制".
//!
//! It reads `/etc/resolv.conf` ([`RESOLV_CONF`], root's file, which NetworkManager writes), takes
//! the nameservers glibc would use ([`Nameservers::parse`]), has nft replace the contents of the
//! two sets in one transaction ([`Nameservers::nft_script`]), logs what it did on standard error
//! and exits: 0 applied, 1 failed. It then reads the file once more and applies again while it
//! changed, so a change that came while it ran is not lost (a path unit that triggers it during a
//! run does not start a second run). A missing file means no nameserver, as for glibc (which then
//! asks 127.0.0.1, which Laysi/trustiot-v2#813 refuses anyway); a file it cannot read or that is
//! larger than [`MAX_FILE_BYTES`] empties the sets (fail closed) and fails.
//!
//! It takes no arguments and reads no environment variable and nothing the agent can write. The
//! only program it starts is nft, by absolute path, with an empty environment; nft reads the
//! script from an anonymous file in memory ([`apply`]), so it always gets the whole script and
//! needs no writable file system.

#![allow(
    clippy::print_stderr,
    reason = "the log goes to standard error, which systemd sends to the journal"
)]

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::FromRawFd;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;

use agent::nameservers::{MAX_FILE_BYTES, Nameservers, RESOLV_CONF};

/// nft, where Debian's nftables package installs it.
const NFT: &str = "/usr/sbin/nft";

/// How many times the file is read and applied before a file that keeps changing is given up on.
const ROUNDS: usize = 10;

/// The most bytes of nft's error output that are logged.
const MAX_LOGGED_BYTES: usize = 4096;

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "trustiot-agent-nameservers takes no arguments: it reads {RESOLV_CONF} and puts its \
             nameservers in the default deny's DNS allowance"
        );
        return ExitCode::from(2);
    }
    match update() {
        Ok(nameservers) => {
            eprintln!("the agent may reach the nameservers {nameservers} on port 53");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::FAILURE
        }
    }
}

/// Reads the file and applies its nameservers until a read finds what the last one applied.
fn update() -> Result<Nameservers, String> {
    let mut applied: Option<Nameservers> = None;
    for _ in 0..ROUNDS {
        let nameservers = match read() {
            Ok(contents) => Nameservers::parse(&contents),
            Err(error) => {
                // Unknown nameservers: none, rather than the ones of an older file.
                let emptied = match apply(&Nameservers::default().nft_script()) {
                    Ok(()) => "the sets are now empty".to_owned(),
                    Err(failure) => format!("emptying the sets failed too: {failure}"),
                };
                return Err(format!("reading {RESOLV_CONF} failed: {error}; {emptied}"));
            }
        };
        if applied.as_ref() == Some(&nameservers) {
            return Ok(nameservers);
        }
        apply(&nameservers.nft_script()).map_err(|failure| {
            format!(
                "did not apply the nameservers {nameservers}; the sets are unchanged: {failure}"
            )
        })?;
        applied = Some(nameservers);
    }
    Err(format!(
        "{RESOLV_CONF} changed on each of {ROUNDS} reads; the sets hold the last nameservers read"
    ))
}

/// The file's contents, empty when it does not exist.
fn read() -> io::Result<Vec<u8>> {
    let file = match File::open(RESOLV_CONF) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut contents = Vec::new();
    // One byte past the limit tells a file of the largest size from a larger one.
    let limit = u64::try_from(MAX_FILE_BYTES).map_or(u64::MAX, |limit| limit.saturating_add(1));
    file.take(limit).read_to_end(&mut contents)?;
    if contents.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {MAX_FILE_BYTES} bytes"),
        ));
    }
    Ok(contents)
}

/// Has nft apply `script` as one transaction. The script goes into an anonymous file in memory
/// ([`memory_file`]) that is complete before nft starts, so nft never sees a part of it (for
/// example only the flush of the sets) when this process is stopped while it writes. No file
/// system is needed: the agent's `ExecStartPre=+` and nftables.service run it with the gateway's
/// own `/tmp`, which may be read-only or lack `O_TMPFILE`.
fn apply(script: &str) -> Result<(), String> {
    let mut file =
        memory_file().map_err(|error| format!("creating the script file failed: {error}"))?;
    file.write_all(script.as_bytes())
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .map_err(|error| format!("writing the script file failed: {error}"))?;
    let output = Command::new(NFT)
        .args(["-f", "-"])
        .env_clear()
        .stdin(Stdio::from(file))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("starting nft failed: {error}"))?;
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
    Err(format!("nft {}: {errors}", output.status))
}

/// An anonymous file in memory (`memfd_create(2)`), closed on exec: nft gets a duplicate of it as
/// standard input.
#[allow(unsafe_code, reason = "memfd_create(2) has no safe wrapper in std")]
fn memory_file() -> io::Result<File> {
    // SAFETY: the name is a NUL-terminated literal that outlives the call; there is no other
    // input.
    let fd =
        unsafe { libc::memfd_create(c"trustiot-agent-nameservers".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new descriptor that nothing else owns or closes.
    Ok(unsafe { File::from_raw_fd(fd) })
}
