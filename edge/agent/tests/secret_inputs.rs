//! No PIN or password reaches the agent through a command line or an environment variable
//! (Laysi/trustiot-v2#812, rebuild checklist D5; the legacy agent passed the TPM key's PIN on a
//! command line, Laysi/trustiot#121). The TPM key has no password at all (empty auth, decision
//! 12; a key with one fails at start, `tests/tpm_swtpm.rs`), so there is nothing to pass:
//!
//! - the executable's arguments: every subcommand refuses an argument it does not know, and none
//!   of them knows a PIN, password or authorization value;
//! - the configuration file: the key's table has no such field, and an unknown field refuses the
//!   start;
//! - the environment: the agent reads only the variables listed in [`READ_VARIABLES`], none of
//!   them a secret, and never the whole environment;
//! - other programs: the agent starts no process, so it hands no command line to anything.
//!
//! The unit file and the sysusers line are checked in `tests/systemd_unit.rs`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent::config::Config;

const EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent");
const EXAMPLE: &str = include_str!("../agent.example.toml");

/// Every environment variable the agent's code reads, as the argument appears in the source:
/// systemd's notification socket and watchdog, the state directory systemd passes, the log
/// level, and whether stderr goes to the journal.
const READ_VARIABLES: &[&str] = &[
    "\"JOURNAL_STREAM\"",
    "\"NOTIFY_SOCKET\"",
    "\"TRUSTIOT_AGENT_LOG\"",
    "\"WATCHDOG_PID\"",
    "\"WATCHDOG_USEC\"",
    "store::STATE_DIRECTORY_VARIABLE",
];

/// Arguments a PIN or password could come in by.
const SECRET_ARGUMENTS: &[&str] = &[
    "--pin",
    "--password",
    "--passphrase",
    "--auth",
    "--key-auth",
    "--tpm-pin",
    "--secret",
];

/// The command lines that take arguments: the service and the two production subcommands.
const COMMANDS: &[&[&str]] = &[&[], &["provision"], &["provision", "install"]];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// The source files of the agent with their code before the unit tests (`#[cfg(test)] mod
/// tests`, which every file keeps at its end).
fn production_code() -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut paths,
    );
    assert!(paths.len() > 20, "the agent's sources: {paths:?}");
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text
                .split_once("\n#[cfg(test)]\nmod tests")
                .map_or(text.as_str(), |(code, _)| code)
                .to_owned();
            (path, code)
        })
        .collect()
}

/// The arguments of every `env::var(...)` and `env::var_os(...)` call in `code`.
fn variables_read(code: &str) -> Vec<String> {
    let mut read = Vec::new();
    for call in ["env::var(", "env::var_os("] {
        let mut rest = code;
        while let Some((_, after)) = rest.split_once(call) {
            let (argument, remaining) = after.split_once(')').unwrap();
            read.push(argument.trim().trim_end_matches(',').trim_end().to_owned());
            rest = remaining;
        }
    }
    read
}

#[test]
fn the_scan_finds_variable_reads() {
    assert_eq!(
        variables_read(
            "let a = std::env::var(\"A\");\nlet b = env::var_os(\n    some::NAME,\n);\nvars()"
        ),
        ["\"A\"", "some::NAME"]
    );
}

#[test]
fn the_agent_reads_only_the_listed_environment_variables_and_starts_no_process() {
    let mut read = BTreeSet::new();
    for (path, code) in production_code() {
        for variable in variables_read(&code) {
            read.insert(variable);
        }
        for forbidden in [
            // The whole environment, where a secret could be picked up by name.
            "env::vars(",
            "env::vars_os(",
            // Another program, whose command line or environment could carry a secret.
            "process::Command",
            "Command::new(",
        ] {
            assert!(
                !code.contains(forbidden),
                "{} uses {forbidden}",
                path.display()
            );
        }
    }
    let expected: BTreeSet<String> = READ_VARIABLES.iter().map(|&name| name.to_owned()).collect();
    assert_eq!(read, expected, "a new variable needs a reason here");
    for name in READ_VARIABLES {
        let upper = name.to_uppercase();
        for secret in ["PIN", "PASS", "AUTH", "SECRET", "KEY"] {
            assert!(!upper.contains(secret), "{name} looks like a secret");
        }
    }
}

#[test]
fn no_command_line_takes_a_pin_or_password() {
    for command in COMMANDS {
        let help = Command::new(EXECUTABLE)
            .args(*command)
            .arg("--help")
            .output()
            .unwrap();
        assert!(help.status.success(), "{command:?} --help: {help:?}");
        // The options the help lists (the help's prose may say "authorization": provision
        // reports an endorsement hierarchy with an authorization value as unsupported).
        let usage = String::from_utf8_lossy(&help.stdout).to_lowercase();
        let options: BTreeSet<&str> = usage
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .filter(|word| word.starts_with("--"))
            .collect();
        assert!(options.contains("--help"), "{command:?}: {usage}");
        for option in &options {
            for secret in ["pin", "pass", "auth", "secret"] {
                assert!(
                    !option.contains(secret),
                    "{command:?} --help lists {option}: {usage}"
                );
            }
        }
        for argument in SECRET_ARGUMENTS {
            let output = Command::new(EXECUTABLE)
                .args(*command)
                .args([*argument, "1234"])
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{command:?} {argument}: {stderr}"
            );
            assert!(
                stderr.contains("unknown argument"),
                "{command:?} {argument}: {stderr}"
            );
        }
    }
}

#[test]
fn the_configuration_has_no_field_for_a_key_password() {
    let tpm_key = "key = { source = \"tpm\", device = \"/dev/tpmrm0\", handle = 0x81000100 }";
    assert!(EXAMPLE.contains(tpm_key));
    Config::parse(EXAMPLE).unwrap();
    for field in ["pin", "password", "auth", "auth_value", "passphrase"] {
        let with_secret = EXAMPLE.replace(
            tpm_key,
            &format!(
                "key = {{ source = \"tpm\", device = \"/dev/tpmrm0\", handle = 0x81000100, {field} = \"1234\" }}"
            ),
        );
        let error = Config::parse(&with_secret).unwrap_err().to_string();
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{field}: {error}"
        );
        // Nor anywhere else in the identity table.
        let with_secret = EXAMPLE.replace(tpm_key, &format!("{tpm_key}\n{field} = \"1234\""));
        let error = Config::parse(&with_secret).unwrap_err().to_string();
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{field}: {error}"
        );
    }
}
