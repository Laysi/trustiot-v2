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
//! ```
//!
//! Exit codes: 0 done, 1 failed, 2 usage. Messages never contain a private key.

use std::fs::OpenOptions;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use certs::ceremony::{self, DEFAULT_ISSUING_DAYS, DEFAULT_ROOT_DAYS};
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
The root private key goes to stdout (create) or comes from stdin (sign, check), never a terminal: \
pipe it into or from age (services/certs/README.md).";

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
        _ => Err(usage("expected `root create`, `root sign` or `root check`")),
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
