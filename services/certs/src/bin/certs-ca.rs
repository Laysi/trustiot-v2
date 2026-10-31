//! `certs-ca`: the offline root ceremony (services/certs/README.md, "CA 階層與私鑰保管"). Run by
//! an operator on an offline machine, never deployed.
//!
//! ```text
//! certs-ca root create --common-name <name> [--days <days>] --certificate <file>
//!     Makes a root key (ECDSA P-384) and its self-signed certificate. Writes the certificate to
//!     <file>, which must not exist, and the private key (PKCS#8 PEM) to stdout, which must not
//!     be a terminal: pipe it into age.
//! certs-ca root sign --root-certificate <file> --request <file> --common-name <name>
//!                    [--days <days>] --certificate <file>
//!     Reads the root private key (PKCS#8 PEM) from stdin, which must not be a terminal (pipe it
//!     from age), and signs the issuing CA certificate for the CSR in --request. Writes it to
//!     <file>, which must not exist.
//! certs-ca root check --root-certificate <file>
//!     Reads a root private key (PKCS#8 PEM) from stdin, which must not be a terminal (pipe it
//!     from age), and checks that it is the key of the root certificate in <file>: the check of
//!     a backup copy. Prints the root's key id; writes nothing.
//! certs-ca root crl --root-certificate <file> --previous-crl <file|none>
//!                   --previous-certificates <file|none> [--revoke <file> --reason <reason>]
//!                   --crl <file>
//!     Reads the root private key (PKCS#8 PEM) from stdin, which must not be a terminal (pipe it
//!     from age), and signs the next root CRL (services/certs/README.md, 「根 CRL」): the
//!     entries of the previous root CRL (`none` for the first one) whose issuing CA certificates
//!     in --previous-certificates expired less than 400 days ago or not at all, plus the issuing
//!     CA certificates in --revoke, revoked now for <reason> (ca-compromise, superseded,
//!     cessation-of-operation or unspecified); the CRL number is the previous one's plus one,
//!     or 1. Writes it to <file>, which must not exist, and lists on stderr the SHA-256 of the
//!     previous and the new CRL and every entry with its key id.
//! ```
//!
//! Exit codes: 0 done, 1 failed, 2 usage. Messages never contain a private key.

use std::fs::OpenOptions;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use certs::ceremony::{
    self, DEFAULT_ISSUING_DAYS, DEFAULT_ROOT_DAYS, RootCrlInputs, RootRevocationReason,
};
use rcgen::KeyPair;
use zeroize::{Zeroize as _, Zeroizing};

/// Largest root key read from stdin; a P-384 PKCS#8 PEM key is about 300 bytes.
const MAX_KEY_BYTES: u64 = 16 * 1024;

/// Largest certificate or request file read.
const MAX_FILE_BYTES: u64 = 64 * 1024;

const USAGE: &str = "usage:
  certs-ca root create --common-name <name> [--days <days>] --certificate <file>
  certs-ca root sign --root-certificate <file> --request <file> --common-name <name> \
[--days <days>] --certificate <file>
  certs-ca root check --root-certificate <file>
  certs-ca root crl --root-certificate <file> --previous-crl <file|none> \
--previous-certificates <file|none> [--revoke <file> --reason <reason>] --crl <file>
The root private key goes to stdout (create) or comes from stdin (sign, check, crl), never a \
terminal: pipe it into or from age (services/certs/README.md).";

/// A failure: the message, and whether it was a usage mistake (exit code 2).
struct Failure {
    message: String,
    usage: bool,
}

fn usage(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        usage: true,
    }
}

fn failed(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        usage: false,
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "certs-ca: {}", failure.message);
            if failure.usage {
                let _ = writeln!(stderr, "{USAGE}");
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// The options after the command: each `--name value` once.
struct Options(Vec<(String, String)>);

impl Options {
    fn parse(arguments: &[String], allowed: &[&str]) -> Result<Self, Failure> {
        let mut options: Vec<(String, String)> = Vec::new();
        let mut rest = arguments.iter();
        while let Some(flag) = rest.next() {
            let Some(name) = flag.strip_prefix("--") else {
                return Err(usage(format!("unexpected argument {flag:?}")));
            };
            if !allowed.contains(&name) {
                return Err(usage(format!("unknown option --{name}")));
            }
            if options.iter().any(|(known, _)| known == name) {
                return Err(usage(format!("--{name} is given twice")));
            }
            let Some(value) = rest.next() else {
                return Err(usage(format!("--{name} needs a value")));
            };
            options.push((name.to_owned(), value.clone()));
        }
        Ok(Self(options))
    }

    fn optional(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, Failure> {
        self.optional(name)
            .ok_or_else(|| usage(format!("--{name} is required")))
    }

    fn days(&self, default: u32) -> Result<u32, Failure> {
        self.optional("days").map_or(Ok(default), |days| {
            days.parse()
                .map_err(|_| usage("--days must be a whole number of days"))
        })
    }
}

fn run(arguments: &[String]) -> Result<(), Failure> {
    match arguments {
        [scope, command, rest @ ..] if scope == "root" && command == "create" => create(
            &Options::parse(rest, &["common-name", "days", "certificate"])?,
        ),
        [scope, command, rest @ ..] if scope == "root" && command == "sign" => {
            sign(&Options::parse(
                rest,
                &[
                    "root-certificate",
                    "request",
                    "common-name",
                    "days",
                    "certificate",
                ],
            )?)
        }
        [scope, command, rest @ ..] if scope == "root" && command == "check" => {
            check(&Options::parse(rest, &["root-certificate"])?)
        }
        [scope, command, rest @ ..] if scope == "root" && command == "crl" => crl(&Options::parse(
            rest,
            &[
                "root-certificate",
                "previous-crl",
                "previous-certificates",
                "revoke",
                "reason",
                "crl",
            ],
        )?),
        _ => Err(usage(
            "expected `root create`, `root sign`, `root check` or `root crl`",
        )),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn create(options: &Options) -> Result<(), Failure> {
    let common_name = options.required("common-name")?;
    let days = options.days(DEFAULT_ROOT_DAYS)?;
    let output = PathBuf::from(options.required("certificate")?);
    if std::io::stdout().is_terminal() {
        return Err(usage(
            "stdout is a terminal; the root private key goes to stdout, so pipe it into age",
        ));
    }
    let mut key = ceremony::new_root_key().map_err(|error| failed(error.to_string()))?;
    let certificate = ceremony::root_certificate(&key, common_name, days, now())
        .map_err(|error| failed(error.to_string()))?;
    write_new(&output, &certificate)?;
    let pem = Zeroizing::new(key.serialize_pem());
    key.zeroize();
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(pem.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| {
            failed(format!(
                "cannot write the root private key to stdout ({error}); remove {} and start over",
                output.display()
            ))
        })?;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "certs-ca: wrote the root certificate to {}; the root private key went to stdout",
        output.display()
    );
    Ok(())
}

fn sign(options: &Options) -> Result<(), Failure> {
    let root_file = PathBuf::from(options.required("root-certificate")?);
    let request_file = PathBuf::from(options.required("request")?);
    let common_name = options.required("common-name")?;
    let days = options.days(DEFAULT_ISSUING_DAYS)?;
    let output = PathBuf::from(options.required("certificate")?);
    if std::io::stdin().is_terminal() {
        return Err(usage(
            "stdin is a terminal; pipe the root private key from age instead of typing it",
        ));
    }
    let root_pem = read_file(&root_file)?;
    let request_pem = read_file(&request_file)?;
    let mut key = read_root_key()?;
    let certificate =
        ceremony::issuing_certificate(&key, &root_pem, &request_pem, common_name, days, now())
            .map_err(|error| failed(error.to_string()));
    key.zeroize();
    let certificate = certificate?;
    let key_id =
        ceremony::certificate_key_id(&certificate).map_err(|error| failed(error.to_string()))?;
    write_new(&output, &certificate)?;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "certs-ca: wrote the issuing CA certificate to {} for key {key_id}; set it only if the \
         certs service reports this key as waiting (services/certs/README.md)",
        output.display()
    );
    Ok(())
}

fn check(options: &Options) -> Result<(), Failure> {
    let root_file = PathBuf::from(options.required("root-certificate")?);
    if std::io::stdin().is_terminal() {
        return Err(usage(
            "stdin is a terminal; pipe the root private key from age instead of typing it",
        ));
    }
    let root_pem = read_file(&root_file)?;
    let mut key = read_root_key()?;
    let key_id =
        ceremony::check_root_key(&key, &root_pem).map_err(|error| failed(error.to_string()));
    key.zeroize();
    let key_id = key_id?;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "certs-ca: the root private key belongs to the root certificate in {} (key {key_id})",
        root_file.display()
    );
    Ok(())
}

fn crl(options: &Options) -> Result<(), Failure> {
    let root_file = PathBuf::from(options.required("root-certificate")?);
    // Required, with `none` for the first: leaving the previous root CRL out by mistake would
    // restart the numbers and drop every revocation.
    let previous_crl_file = match options.required("previous-crl")? {
        "none" => None,
        file => Some(PathBuf::from(file)),
    };
    // Required too: with the previous CRL's certificates left out, every entry is refused, and
    // `none` says on the command line that there are none.
    let previous_certificates_file = match options.required("previous-certificates")? {
        "none" => None,
        file => Some(PathBuf::from(file)),
    };
    let revoke_file = options.optional("revoke").map(PathBuf::from);
    let reason = match (options.optional("reason"), &revoke_file) {
        (Some(name), Some(_)) => RootRevocationReason::from_name(name)
            .ok_or_else(|| usage(format!("--reason must be {}", RootRevocationReason::NAMES)))?,
        (None, Some(_)) => {
            return Err(usage(format!(
                "--revoke needs --reason ({})",
                RootRevocationReason::NAMES
            )));
        }
        (Some(_), None) => return Err(usage("--reason goes only with --revoke")),
        (None, None) => RootRevocationReason::Unspecified,
    };
    let output = PathBuf::from(options.required("crl")?);
    if std::io::stdin().is_terminal() {
        return Err(usage(
            "stdin is a terminal; pipe the root private key from age instead of typing it",
        ));
    }
    // Checked again when the file is created; this only spares reading the key for nothing.
    if output.symlink_metadata().is_ok() {
        return Err(failed(format!(
            "{} already exists; name a new file",
            output.display()
        )));
    }
    let root_pem = read_file(&root_file)?;
    let previous_crl_pem = previous_crl_file.as_deref().map(read_file).transpose()?;
    let previous_certificates_pem = previous_certificates_file
        .as_deref()
        .map(read_file)
        .transpose()?
        .unwrap_or_default();
    let revoke_pem = revoke_file.as_deref().map(read_file).transpose()?;
    let inputs = RootCrlInputs {
        root_pem: &root_pem,
        previous_crl_pem: previous_crl_pem.as_deref(),
        previous_certificates_pem: &previous_certificates_pem,
        revoke_pem: revoke_pem.as_deref(),
        reason,
    };
    let mut key = read_root_key()?;
    let signed =
        ceremony::root_crl(&key, &inputs, now()).map_err(|error| failed(error.to_string()));
    key.zeroize();
    let signed = signed?;
    write_new(&output, &signed.pem)?;
    let mut stderr = std::io::stderr().lock();
    match &signed.previous_sha256 {
        Some(sha256) => {
            let _ = writeln!(
                stderr,
                "certs-ca: previous root CRL number {}, SHA-256 {sha256}: check that the record \
                 of the last ceremony names this one; a number signed once is never signed again",
                signed.number - 1
            );
        }
        None => {
            let _ = writeln!(stderr, "certs-ca: no previous root CRL: this is number 1");
        }
    }
    let _ = writeln!(
        stderr,
        "certs-ca: wrote root CRL number {} to {}: SHA-256 {}, thisUpdate {}, nextUpdate {}, \
         entries: {}",
        signed.number,
        output.display(),
        signed.sha256,
        rfc3339(signed.this_update),
        rfc3339(signed.next_update),
        signed.listed.len()
    );
    for entry in &signed.listed {
        let since = if entry.new {
            "revoked now".to_owned()
        } else {
            format!("revoked {}", rfc3339(entry.revoked_at))
        };
        let _ = writeln!(
            stderr,
            "certs-ca:   lists {} {} key {} ({since}, {}, expires {})",
            entry.serial,
            entry.subject,
            entry.key_id,
            entry.reason.name(),
            rfc3339(entry.not_after)
        );
    }
    for entry in &signed.removed {
        let _ = writeln!(
            stderr,
            "certs-ca:   removed {} {} key {} (expired {})",
            entry.serial,
            entry.subject,
            entry.key_id,
            rfc3339(entry.not_after)
        );
    }
    if signed.listed.iter().any(|entry| entry.new) {
        let _ = writeln!(
            stderr,
            "certs-ca: gateways that get this CRL refuse the newly revoked issuing CAs: set it \
             only after the broker's server certificate comes from another generation \
             (services/certs/README.md, 「根 CRL」)"
        );
    }
    Ok(())
}

/// Unix seconds as RFC 3339 UTC, for the operator.
fn rfc3339(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || format!("@{seconds}"),
        |time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

fn read_root_key() -> Result<KeyPair, Failure> {
    let mut text = Zeroizing::new(String::with_capacity(
        usize::try_from(MAX_KEY_BYTES + 1).unwrap_or(0),
    ));
    std::io::stdin()
        .lock()
        .take(MAX_KEY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| failed("stdin is not a PEM private key"))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_KEY_BYTES {
        return Err(failed("stdin is too large to be a private key"));
    }
    KeyPair::from_pem(&text).map_err(|_| failed("stdin is not a PKCS#8 PEM private key"))
}

fn read_file(path: &Path) -> Result<String, Failure> {
    let file = std::fs::File::open(path)
        .map_err(|error| failed(format!("cannot open {}: {error}", path.display())))?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| failed(format!("{} is not a text file", path.display())))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(failed(format!("{} is too large", path.display())));
    }
    Ok(text)
}

/// Writes `text` to a new file at `path`; never replaces one.
fn write_new(path: &Path, text: &str) -> Result<(), Failure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| failed(format!("cannot create {}: {error}", path.display())))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| failed(format!("cannot write {}: {error}", path.display())))
}
