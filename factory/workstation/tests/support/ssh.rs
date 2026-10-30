//! A fake `ssh` for `register --gateway` (Laysi/trustiot-v2#616): a shell script put first on
//! the tool's `PATH`. Each call records its arguments, its environment and its standard input,
//! then does what sshd would: runs the remote command with `sh -c`, with every signal's default
//! disposition (a test runner may ignore SIGHUP, which a shell then cannot trap), in a clean
//! environment whose
//! `HOME`, `PATH` (the fake remote's `bin` first) and `TMPDIR` are directories of the fake
//! remote, and passes its output and exit status back. Files in the state directory make a call
//! fail as ssh does (status 255): `fail-before-<n>` before the remote command runs,
//! `fail-after-<n>` after it ran (its output is lost); `corrupt-<n>` adds a byte to what the
//! remote command reads on standard input.
//!
//! `ssh -G` is answered apart and not counted as a call: it records its arguments and prints
//! settings as ssh does, the first value of each winning: the `-o` options, then the state
//! directory's `ssh_config` (lines `<lower-case name> <value>`, the operator's configuration),
//! then ssh's defaults (`stricthostkeychecking ask`, the user's and the system's known hosts
//! files, `verifyhostkeydns false`).
//! `ssh_config_first` is read before the `-o` options, as a client that let the configuration
//! override the command line would; with `config-flood`, it prints 1 MiB of spaces instead.

use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, DnType, KeyPair};

use super::{Scratch, set_mode};

/// A stand-in for `trustiot-agent` on the fake remote: appends its arguments to
/// `$HOME/agent-calls`, prints `$HOME/report-<inspect|provision>.json` (when there is one) and
/// ends with the status in `$HOME/status-<inspect|provision>` (0 without one). With
/// `$HOME/hup`, it first sends SIGHUP to the shell that started it, as a dropped ssh session
/// does; with `$HOME/flood`, it prints 1 MiB of spaces instead of a report, which fills the pipe
/// long before the reader's limit is reached. As `provision install` (Laysi/trustiot-v2#617) it
/// appends its standard input to `$HOME/install-stdin`, prints `$HOME/report-install` (or that
/// both files were written) and ends with the status in `$HOME/status-install` (0 without one).
pub const CANNED_AGENT: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/agent-calls"
if [ "$1" = provision ] && [ "$2" = install ]; then
    cat >> "$HOME/install-stdin"
    if [ -f "$HOME/report-install" ]; then
        cat "$HOME/report-install"
    else
        printf '%s\n' '/var/lib/trustiot-agent/gateway.pem: written' \
            '/etc/trustiot/broker-ca.pem: written'
    fi
    exit "$(cat "$HOME/status-install" 2>/dev/null || echo 0)"
fi
mode=provision
for arg in "$@"; do
    if [ "$arg" = --inspect ]; then mode=inspect; fi
done
if [ -f "$HOME/hup" ]; then kill -HUP "$PPID"; fi
if [ -f "$HOME/flood" ]; then head -c 1048576 /dev/zero | tr '\0' ' '; exit 0; fi
if [ -f "$HOME/report-$mode.json" ]; then cat "$HOME/report-$mode.json"; fi
exit "$(cat "$HOME/status-$mode" 2>/dev/null || echo 0)"
"#;

/// A `sudo` for the fake remote: accepts only `-n`, records the command in `$HOME/sudo-calls`
/// and runs it.
pub const FAKE_SUDO: &str = r#"#!/bin/sh
if [ "$1" != -n ]; then echo "sudo: a password is required" >&2; exit 1; fi
shift
if [ "$1" = -l ]; then shift; command -v "$1" || exit 1; exit 0; fi
printf '%s\n' "$*" >> "$HOME/sudo-calls"
exec "$@"
"#;

/// A gateway's key and EK, and the provisioning reports `trustiot-agent provision` would print.
pub struct GatewayKeys {
    pub key: KeyPair,
    pub ek: KeyPair,
}

impl GatewayKeys {
    pub fn new() -> Self {
        Self {
            key: KeyPair::generate().expect("a P-256 key"),
            ek: KeyPair::generate().expect("a P-256 EK"),
        }
    }

    /// A CSR of the key with the subject `CN=<common_name>`.
    pub fn request(&self, common_name: &str) -> String {
        let mut params = CertificateParams::new(Vec::new()).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params
            .serialize_request(&self.key)
            .expect("a CSR")
            .pem()
            .expect("PEM")
    }

    /// The report of `hardware_id` at handle `0x81000100`: the key absent, or present with
    /// `created`.
    pub fn report(&self, hardware_id: &str, present: Option<bool>) -> serde_json::Value {
        let key = match present {
            None => serde_json::json!({"state": "absent"}),
            Some(created) => serde_json::json!({
                "state": "present",
                "created": created,
                "public_key": self.key.public_key_pem(),
                "certificate_request": self.request(hardware_id),
            }),
        };
        serde_json::json!({
            "version": 1,
            "hardware_id": hardware_id,
            "key_handle": "0x81000100",
            "key": key,
            "endorsement_key": self.ek.public_key_pem(),
            "ek_certificate": {"state": "absent"},
        })
    }

    /// The SHA-256 of the key's SubjectPublicKeyInfo, lower-case hex.
    pub fn public_key_sha256(&self) -> String {
        use rcgen::PublicKeyData as _;
        use sha2::Digest as _;
        sha2::Sha256::digest(self.key.subject_public_key_info())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The fake ssh script; `@STATE@` and `@REMOTE@` are replaced with directories.
const SCRIPT: &str = r#"#!/bin/sh
state='@STATE@'
remote='@REMOTE@'
for arg in "$@"; do
    if [ "$arg" = -G ]; then
        k=$(cat "$state/config-count" 2>/dev/null || echo 0)
        k=$((k + 1))
        echo "$k" > "$state/config-count"
        for a in "$@"; do printf '%s\0' "$a"; done > "$state/config-$k"
        if [ -f "$state/config-flood" ]; then
            head -c 1048576 /dev/zero | tr '\0' ' '
            exit 0
        fi
        seen=' '
        emit() {
            case "$seen" in *" $1 "*) return ;; esac
            seen="$seen$1 "
            printf '%s %s\n' "$1" "$2"
        }
        if [ -f "$state/ssh_config_first" ]; then
            while read -r key value; do emit "$key" "$value"; done < "$state/ssh_config_first"
        fi
        prev=
        for a in "$@"; do
            if [ "$prev" = -o ]; then
                emit "$(printf '%s' "${a%%=*}" | tr 'A-Z' 'a-z')" "${a#*=}"
            fi
            prev=$a
        done
        if [ -f "$state/ssh_config" ]; then
            while read -r key value; do emit "$key" "$value"; done < "$state/ssh_config"
        fi
        emit stricthostkeychecking ask
        emit userknownhostsfile '~/.ssh/known_hosts ~/.ssh/known_hosts2'
        emit globalknownhostsfile '/etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2'
        emit verifyhostkeydns false
        exit 0
    fi
done
count=$(cat "$state/count" 2>/dev/null || echo 0)
n=$((count + 1))
echo "$n" > "$state/count"
call="$state/call-$n"
mkdir "$call"
for arg in "$@"; do printf '%s\0' "$arg"; done > "$call/argv"
env -0 > "$call/env"
while [ $# -gt 0 ]; do
    case "$1" in
        --) shift; break ;;
        -o) shift 2 ;;
        -*) shift ;;
        *) break ;;
    esac
done
printf '%s' "$1" > "$call/destination"
shift
printf '%s' "$*" > "$call/command"
cat > "$call/stdin"
if [ -f "$state/corrupt-$n" ]; then printf 'x' >> "$call/stdin"; fi
if [ -f "$state/fail-before-$n" ]; then
    echo "ssh: connect to host gateway port 22: Connection refused" >&2
    exit 255
fi
mkdir -p "$remote/tmp" "$remote/bin" "$remote/home"
cd "$remote/home" || exit 255
env -i --default-signal HOME="$remote/home" PATH="$remote/bin:/usr/bin:/bin" TMPDIR="$remote/tmp" \
    sh -c "$*" < "$call/stdin" > "$call/stdout" 2> "$call/stderr"
status=$?
echo "$status" > "$call/status"
cat "$call/stderr" >&2
if [ -f "$state/fail-after-$n" ]; then
    echo "Connection to gateway closed by remote host." >&2
    exit 255
fi
cat "$call/stdout"
exit "$status"
"#;

/// One recorded ssh call.
#[derive(Debug)]
pub struct Call {
    /// Every argument, in order.
    pub argv: Vec<String>,
    /// The environment, `NAME=value` each.
    pub env: Vec<String>,
    pub destination: String,
    /// The remote command as ssh would send it.
    pub command: String,
    /// What the remote command got on standard input.
    pub stdin: Vec<u8>,
    /// What the remote command printed, even when the call then failed.
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// The remote command's status; `None` when it never ran.
    pub status: Option<i32>,
}

impl Call {
    /// The names of the environment's variables.
    pub fn env_names(&self) -> Vec<String> {
        self.env
            .iter()
            .filter_map(|entry| entry.split_once('=').map(|(name, _)| name.to_owned()))
            .collect()
    }
}

/// The fake ssh, its state and the fake remote.
pub struct FakeSsh {
    bin: PathBuf,
    state: PathBuf,
    remote: PathBuf,
}

impl FakeSsh {
    /// Writes the fake ssh under `scratch`.
    pub fn install(scratch: &Scratch) -> Self {
        let bin = scratch.join("ssh-bin");
        let state = scratch.join("ssh-state");
        let remote = scratch.join("remote");
        for directory in [&bin, &state, &remote] {
            std::fs::create_dir_all(directory).expect("a directory");
        }
        let script = SCRIPT
            .replace("@STATE@", state.to_str().expect("a UTF-8 path"))
            .replace("@REMOTE@", remote.to_str().expect("a UTF-8 path"));
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, script).expect("the fake ssh");
        set_mode(&ssh, 0o755);
        let fake = Self { bin, state, remote };
        fake.reset_remote();
        fake
    }

    /// `PATH` for the tool: the fake ssh first, then the system's.
    pub fn path(&self) -> std::ffi::OsString {
        format!("{}:/usr/bin:/bin", self.bin.display()).into()
    }

    /// The fake remote's home directory, where its commands run.
    pub fn remote_home(&self) -> PathBuf {
        self.remote.join("home")
    }

    /// The fake remote's `TMPDIR`.
    pub fn remote_tmp(&self) -> PathBuf {
        self.remote.join("tmp")
    }

    /// Everything in the fake remote's `TMPDIR`: empty once every upload was cleaned up.
    pub fn remote_tmp_entries(&self) -> Vec<String> {
        std::fs::read_dir(self.remote_tmp())
            .map(|entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("an entry")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Empties the fake remote, as a reflashed SD card: no files, no installed agent.
    pub fn reset_remote(&self) {
        let _ = std::fs::remove_dir_all(&self.remote);
        for directory in ["tmp", "bin", "home"] {
            std::fs::create_dir_all(self.remote.join(directory)).expect("a remote directory");
        }
    }

    /// Puts `script` on the fake remote's `PATH` as `name`.
    pub fn install_remote(&self, name: &str, script: &str) {
        let path = self.remote.join("bin").join(name);
        std::fs::write(&path, script).expect("a remote program");
        set_mode(&path, 0o755);
    }

    /// Writes `contents` to `name` in the fake remote's home.
    pub fn write_remote_file(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.remote_home().join(name), contents).expect("a remote file");
    }

    /// Reads `name` from the fake remote's home, empty when it does not exist.
    pub fn read_remote_file(&self, name: &str) -> String {
        std::fs::read_to_string(self.remote_home().join(name)).unwrap_or_default()
    }

    /// Makes call `n` (counted from the first call ever) fail as ssh does, before the remote
    /// command runs.
    pub fn fail_before(&self, n: usize) {
        std::fs::write(self.state.join(format!("fail-before-{n}")), "").expect("a flag");
    }

    /// Makes call `n` fail as ssh does after the remote command ran: its output is lost.
    pub fn fail_after(&self, n: usize) {
        std::fs::write(self.state.join(format!("fail-after-{n}")), "").expect("a flag");
    }

    /// Makes call `n` add a byte to the remote command's standard input.
    pub fn corrupt(&self, n: usize) {
        std::fs::write(self.state.join(format!("corrupt-{n}")), "").expect("a flag");
    }

    /// Sets the operator's ssh configuration `ssh -G` reports (`None` removes it); with `first`,
    /// it overrides even the command line's options.
    pub fn set_config(&self, lines: Option<&str>, first: bool) {
        let path = self.state.join(if first {
            "ssh_config_first"
        } else {
            "ssh_config"
        });
        match lines {
            Some(lines) => std::fs::write(path, lines).expect("a configuration"),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Makes `ssh -G` print 1 MiB of spaces (`on`) or its settings.
    pub fn flood_config(&self, on: bool) {
        let path = self.state.join("config-flood");
        if on {
            std::fs::write(path, "").expect("a flag");
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The arguments of every `ssh -G` call so far.
    pub fn config_calls(&self) -> Vec<Vec<String>> {
        let count: usize = std::fs::read_to_string(self.state.join("config-count"))
            .map(|text| text.trim().parse().expect("a count"))
            .unwrap_or(0);
        (1..=count)
            .map(|k| {
                std::fs::read(self.state.join(format!("config-{k}")))
                    .unwrap_or_default()
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect()
            })
            .collect()
    }

    /// How many calls were made.
    pub fn count(&self) -> usize {
        std::fs::read_to_string(self.state.join("count"))
            .map(|text| text.trim().parse().expect("a count"))
            .unwrap_or(0)
    }

    /// Call `n` (from 1).
    pub fn call(&self, n: usize) -> Call {
        let directory = self.state.join(format!("call-{n}"));
        let read = |name: &str| std::fs::read(directory.join(name)).unwrap_or_default();
        let split = |bytes: Vec<u8>| -> Vec<String> {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        };
        Call {
            argv: split(read("argv")),
            env: split(read("env")),
            destination: String::from_utf8_lossy(&read("destination")).into_owned(),
            command: String::from_utf8_lossy(&read("command")).into_owned(),
            stdin: read("stdin"),
            stdout: read("stdout"),
            stderr: String::from_utf8_lossy(&read("stderr")).into_owned(),
            status: String::from_utf8_lossy(&read("status")).trim().parse().ok(),
        }
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<Call> {
        (1..=self.count()).map(|n| self.call(n)).collect()
    }

    /// The fake remote's root.
    pub fn remote(&self) -> &Path {
        &self.remote
    }

    /// Leaves the reports `trustiot-agent` prints on the fake remote: for `--inspect`, then
    /// without it.
    pub fn leave_reports(&self, inspect: &serde_json::Value, provision: &serde_json::Value) {
        self.write_remote_file(
            "report-inspect.json",
            &serde_json::to_vec_pretty(inspect).expect("JSON"),
        );
        self.write_remote_file(
            "report-provision.json",
            &serde_json::to_vec_pretty(provision).expect("JSON"),
        );
    }

    /// The arguments the remote `trustiot-agent` got, one run per line.
    pub fn agent_calls(&self) -> Vec<String> {
        self.read_remote_file("agent-calls")
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
