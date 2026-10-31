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
//! The fieldbus allowlist component (`src/bin/trustiot-agent-allowlist.rs`, Laysi/trustiot-v2#864)
//! is a separate executable of this package and starts one program, nft, so the agent's scan
//! leaves out exactly that file and [`the_allowlist_component_starts_only_nft_and_reads_nothing`]
//! checks it on its own terms.
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

/// The fieldbus allowlist component, relative to the package: not the agent, so not in
/// [`production_code`].
const ALLOWLIST_SOURCE: &str = "src/bin/trustiot-agent-allowlist.rs";
const ALLOWLIST_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-allowlist");

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

/// Functions that read the environment or start another program, by their own name (libc's,
/// and std's `vars`): denied wherever they appear, whatever path or import brought them in.
/// libc's `system` is denied as `libc::system` only: `system` is also the agent's own name for
/// the system clock.
const DENIED_NAMES: &[&str] = &[
    "getenv",
    "secure_getenv",
    "environ",
    "vars",
    "vars_os",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "execl",
    "execle",
    "execlp",
    "fexecve",
    "fork",
    "vfork",
    "posix_spawn",
    "posix_spawnp",
    "popen",
];

/// What the agent may name in `std::env` and `std::process`: reading one variable, its own
/// arguments, its exit status and its process ID. `std::process::Command` is not here.
const ENV_ITEMS: &[&str] = &["var", "var_os", "args_os"];
const PROCESS_ITEMS: &[&str] = &["ExitCode", "id"];

/// What the agent may name in libc: the file, account and interface calls it makes, with their
/// types and constants. Anything else (`execveat`, `syscall`, `clone`, ...) needs a reason here.
const LIBC_ITEMS: &[&str] = &[
    "AF_INET",
    "AF_INET6",
    "AT_SYMLINK_NOFOLLOW",
    "ELOOP",
    "ENODEV",
    "ENOTDIR",
    "ENXIO",
    "O_CLOEXEC",
    "O_CREAT",
    "O_DIRECTORY",
    "O_EXCL",
    "O_NOCTTY",
    "O_NOFOLLOW",
    "O_NONBLOCK",
    "O_RDONLY",
    "O_WRONLY",
    "S_IFMT",
    "S_IFREG",
    "c_char",
    "c_int",
    "c_uint",
    "freeifaddrs",
    "fstatat",
    "getifaddrs",
    "getpwnam_r",
    "ifaddrs",
    "openat",
    "passwd",
    "renameat",
    "sockaddr",
    "sockaddr_in",
    "sockaddr_in6",
    "stat",
    "unlinkat",
];

/// The code's tokens: identifiers, `::`, string literals (one token each, with their quotes, so
/// the words in them are not names) and single punctuation characters. Comments (line and nested
/// block comments) are left out, so they cannot split a path. A raw string becomes the token
/// `raw string`, which [`violations`] refuses, because a quote inside one would make the rest of
/// the file look like a string.
fn tokens(code: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            let mut word = String::from(c);
            while let Some(&next) = chars.peek() {
                if !(next.is_alphanumeric() || next == '_') {
                    break;
                }
                word.push(next);
                chars.next();
            }
            if matches!(word.as_str(), "r" | "br" | "cr") && matches!(chars.peek(), Some('#' | '"'))
            {
                tokens.push("raw string".to_owned());
            } else {
                tokens.push(word);
            }
        } else if c == '"' {
            // To the closing quote; a backslash escapes the character after it. The literal is one
            // token with its quotes, so the words in it are not names.
            let mut literal = String::from(c);
            while let Some(inside) = chars.next() {
                literal.push(inside);
                match inside {
                    '\\' => literal.extend(chars.next()),
                    '"' => break,
                    _ => {}
                }
            }
            tokens.push(literal);
        } else if c == '/' && chars.peek() == Some(&'/') {
            // A line comment, to the end of the line.
            for inside in chars.by_ref() {
                if inside == '\n' {
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            // A block comment, which Rust nests.
            chars.next();
            let mut depth = 1_usize;
            while depth > 0 {
                match (chars.next(), chars.peek()) {
                    (Some('/'), Some('*')) => {
                        chars.next();
                        depth += 1;
                    }
                    (Some('*'), Some('/')) => {
                        chars.next();
                        depth -= 1;
                    }
                    (None, _) => break,
                    _ => {}
                }
            }
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            tokens.push("::".to_owned());
        } else if !c.is_whitespace() {
            tokens.push(c.to_string());
        }
    }
    tokens
}

/// Why `code` could read the environment or start a program past the checks of this file; empty
/// when it cannot. The rules are on tokens, so neither a path (`::std`, `std::`, `libc::`), a
/// grouped import nor an alias gets around them:
///
/// - a denied name anywhere;
/// - `std::env` and `std::process` only as a path to one of their allowed items: not imported
///   as a module (`use std::{env}`, `process as p`, `env::{...}`), not `process::Command`;
/// - `var` and `var_os` only right after `env::`, and every `env::` item called right there,
///   never imported or renamed (so every read is an `env::var(` the variable check sees);
/// - no alias of `std`, `libc` or `self`, no `extern` (crate or block), no inline assembly, and
///   from libc only the items of [`LIBC_ITEMS`].
///
/// It reads source text, not the compiler's view: a tripwire for an honest change, which review
/// and the unit file back up.
fn violations(code: &str) -> Vec<String> {
    let tokens = tokens(code);
    let at = |index: usize| tokens.get(index).map_or("", String::as_str);
    let mut found = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let (previous, next, item) = (
            index.checked_sub(1).map_or("", at),
            at(index + 1),
            at(index + 2),
        );
        let token = token.as_str();
        if DENIED_NAMES.contains(&token) {
            found.push(token.to_owned());
        }
        match (token, next) {
            ("env", "::") if !ENV_ITEMS.contains(&item) => found.push(format!("env::{item}")),
            // Called right there, not imported (and so perhaps renamed).
            ("env", "::") if at(index + 3) != "(" => found.push(format!("env::{item} uncalled")),
            (_, "as") if ENV_ITEMS.contains(&token) => found.push(format!("{token} as")),
            ("process", "::") if !PROCESS_ITEMS.contains(&item) => {
                found.push(format!("process::{item}"));
            }
            ("env" | "process", "," | "}" | ";" | "as") => found.push(format!("{token} {next}")),
            ("std" | "libc" | "self", "as") => found.push(format!("{token} as")),
            // `extern crate`, and `extern "C"` blocks that would declare libc functions directly.
            ("extern", _) => found.push("extern".to_owned()),
            ("asm" | "global_asm", "!") => found.push(format!("{token}!")),
            ("libc", "::") if !LIBC_ITEMS.contains(&item) => found.push(format!("libc::{item}")),
            ("raw string", _) => found.push("raw string".to_owned()),
            ("var" | "var_os", _)
                if !(previous == "::" && index.checked_sub(2).map_or("", at) == "env") =>
            {
                found.push(format!("bare {token}"));
            }
            _ => {}
        }
    }
    found
}

#[test]
fn the_scan_catches_the_ways_around_it_it_knows() {
    for evasion in [
        "use std::env::var; var(\"PIN\")",
        "use std::env::{var}; var(\"PIN\")",
        "use std::{env::{var as read}}; read(\"PIN\")",
        "use std::{env}; env::args()",
        "use ::std::env::var_os; var_os(\"PIN\")",
        "use std::env::var as read; read(\"PIN\")",
        "use std::env::{var_os as read}; read(\"PIN\")",
        "let read = std::env::var; read(\"PIN\")",
        "use std as s; s::env::vars()",
        "extern crate std as s;",
        "use std::process::{Command as C}; C::new(\"x\")",
        "use std::process::{self as p}; p::Command::new(\"x\")",
        "use std::process::*; Command::new(\"x\")",
        "std::process::Command::new(\"x\")",
        "use ::libc::{getenv}; getenv(name)",
        "use ::libc::*;",
        "unsafe { libc::system(command) }",
        "unsafe { libc::execveat(fd, path, argv, envp, 0) }",
        "unsafe { libc::syscall(libc::SYS_execve, path, argv, envp) }",
        "extern \"C\" { fn execve(); }",
        "std::os::unix::process::CommandExt::exec(command)",
        "core::arch::asm!(\"syscall\")",
        "std::process /* a comment */ :: Command::new(\"x\")",
        "std::process::/* a /* nested */ comment */Command::new(\"x\")",
        "std::env:: // a comment\n vars()",
        "unsafe { libc::execv(path, argv) }",
        "std::env::vars_os()",
        "let s = r#\"\"\"#; std::env::vars()",
    ] {
        assert!(
            !violations(evasion).is_empty(),
            "the scan misses {evasion:?}"
        );
    }
    // What the agent does use.
    assert_eq!(
        violations(
            "use std::process::ExitCode;\nstd::process::id();\nstd::env::var_os(store::NAME);\n\
             // fork and system in a comment\nasync fn process<S>() {}\nprocess(stream);\n\
             const PATH: &str = \"/var/lib/fork \\\" env, system\";"
        ),
        Vec::<String>::new()
    );
}

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

/// `text` before its unit tests (`#[cfg(test)] mod tests {`, which every file keeps at its end;
/// a file of tests only is scanned whole).
fn before_tests(text: &str) -> String {
    text.split_once("\n#[cfg(test)]\nmod tests {")
        .map_or(text, |(code, _)| code)
        .to_owned()
}

/// The source files of the agent with their code before the unit tests: every `.rs` file under
/// `src/` but the allowlist component's ([`ALLOWLIST_SOURCE`]), which is checked on its own.
fn production_code() -> Vec<(PathBuf, String)> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    sources(&manifest.join("src"), &mut paths);
    let component = manifest.join(ALLOWLIST_SOURCE);
    assert!(paths.contains(&component), "{paths:?}");
    paths.retain(|path| *path != component);
    assert!(paths.len() > 20, "the agent's sources: {paths:?}");
    paths
        .into_iter()
        .map(|path| {
            let code = before_tests(&std::fs::read_to_string(&path).unwrap());
            (path, code)
        })
        .collect()
}

/// The arguments of every `env::var(...)` and `env::var_os(...)` call in `code`, as their
/// tokens written together (comments and spacing gone).
fn variables_read(code: &str) -> Vec<String> {
    let tokens = tokens(code);
    let mut read = Vec::new();
    for (index, window) in tokens.windows(4).enumerate() {
        if window[0] == "env"
            && window[1] == "::"
            && matches!(window[2].as_str(), "var" | "var_os")
            && window[3] == "("
        {
            let mut argument: Vec<&str> = tokens[index + 4..]
                .iter()
                .map(String::as_str)
                .take_while(|&token| token != ")")
                .collect();
            if argument.last() == Some(&",") {
                argument.pop();
            }
            read.push(argument.concat());
        }
    }
    read
}

#[test]
fn the_scan_finds_variable_reads() {
    assert_eq!(
        variables_read(
            "let a = std::env::var(\"A\");\nlet b = env::var_os(\n    some::NAME,\n);\nvars()\n\
             let c = env::var /* a comment */ (\"B\");"
        ),
        ["\"A\"", "some::NAME", "\"B\""]
    );
}

#[test]
fn the_agent_reads_only_the_listed_environment_variables_and_starts_no_process() {
    let mut read = BTreeSet::new();
    for (path, code) in production_code() {
        for variable in variables_read(&code) {
            read.insert(variable);
        }
        assert_eq!(
            violations(&code),
            Vec::<String>::new(),
            "{}",
            path.display()
        );
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

/// The allowlist component holds `CAP_NET_ADMIN` and reads its request from the agent, which may
/// be compromised. It reads no environment variable, and the one program it starts is nft, by
/// absolute path, with an empty environment, fixed arguments and the script as standard input:
/// its only items outside the agent's lists are `process::Command` and `process::Stdio` for that,
/// and of libc only `O_TMPFILE`, for the script file. It opens no file by name but the directory
/// of that unnamed file, and its code is this one file (README.md, "fieldbus 放行集合"). Like the
/// agent's scan, this reads source text: a tripwire for an honest change, which review backs up.
#[test]
fn the_allowlist_component_starts_only_nft_and_reads_nothing() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(ALLOWLIST_SOURCE);
    let code = before_tests(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(
        violations(&code),
        ["process::Command", "process::Stdio", "libc::O_TMPFILE"],
        "a new item needs a reason here"
    );
    assert_eq!(variables_read(&code), Vec::<String>::new());
    let tokens = tokens(&code);
    let count = |sequence: &[&str]| {
        tokens
            .windows(sequence.len())
            .filter(|window| {
                window
                    .iter()
                    .zip(sequence)
                    .all(|(token, item)| token == item)
            })
            .count()
    };
    // libc: its own list, narrower than the agent's.
    let libc_items: BTreeSet<&str> = tokens
        .windows(3)
        .filter(|window| window[0] == "libc" && window[1] == "::")
        .map(|window| window[2].as_str())
        .collect();
    assert_eq!(libc_items, BTreeSet::from(["O_TMPFILE"]));
    // One program, with fixed arguments: no other `.arg(` or `.args(`.
    assert_eq!(
        count(&["Command", "::", "new"]),
        1,
        "one place starts a program"
    );
    assert_eq!(count(&[".", "arg", "("]), 0);
    assert_eq!(count(&[".", "args", "("]), 1);
    for expected in [
        "const NFT: &str = \"/usr/sbin/nft\";",
        "Command::new(NFT)",
        ".args([\"-f\", \"-\"])",
        ".env_clear()",
        "const SCRIPT_DIRECTORY: &str = \"/tmp\";",
        ".custom_flags(libc::O_TMPFILE)",
    ] {
        assert!(code.contains(expected), "{expected}");
    }
    // Files: one `open`, the unnamed script file in the private /tmp; nothing read or written by
    // name, and no other source pulled in.
    assert_eq!(count(&[".", "open", "("]), 1);
    assert_eq!(count(&["OpenOptions", "::", "new"]), 1);
    for denied in [
        "File",
        "read_to_string",
        "read_dir",
        "read_link",
        "create",
        "create_new",
        "remove_file",
        "rename",
        "include",
        "include_str",
        "include_bytes",
        "mod",
        "path",
    ] {
        assert!(
            !tokens.iter().any(|token| token == denied),
            "the component names {denied}"
        );
    }
    assert_eq!(count(&["fs", "::"]), 1, "std::fs only for OpenOptions");
    assert_eq!(count(&["fs", "::", "OpenOptions"]), 1);

    // It takes no argument, and with one it reads nothing and applies nothing.
    for arguments in [
        &["--help"][..],
        &["--file", "/etc/nftables.conf"],
        &["1234"],
    ] {
        let output = Command::new(ALLOWLIST_EXECUTABLE)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("takes no arguments"),
            "{arguments:?}: {output:?}"
        );
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
