//! The command line's options, the platform URL and where files live.

use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use url::Url;

use crate::Error;

/// How long a request may take, connecting included, unless `--timeout` says otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest `--timeout`, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// The directory under `XDG_RUNTIME_DIR` (session) and the data directory (workstation files).
pub const APP_DIRECTORY: &str = "trustiot-workstation";

fn usage(message: impl Into<String>) -> Error {
    Error::Usage(message.into())
}

/// The options after a command: each `--name value` at most once, only the names the command
/// takes.
#[derive(Debug, Default)]
pub struct Options(Vec<(String, String)>);

impl Options {
    /// Parses `arguments` (everything after the command).
    ///
    /// # Errors
    ///
    /// A usage error for a stray argument, an unknown or repeated option, or a missing value.
    pub fn parse(arguments: &[String], allowed: &[&str]) -> Result<Self, Error> {
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

    /// The value of `--name`, if given.
    #[must_use]
    pub fn optional(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value of `--name`.
    ///
    /// # Errors
    ///
    /// A usage error when it is missing.
    pub fn required(&self, name: &str) -> Result<&str, Error> {
        self.optional(name)
            .ok_or_else(|| usage(format!("--{name} is required")))
    }

    /// `--timeout <seconds>`, 1 to [`MAX_TIMEOUT_SECS`]; [`DEFAULT_TIMEOUT`] when absent.
    ///
    /// # Errors
    ///
    /// A usage error for any other value.
    pub fn timeout(&self) -> Result<Duration, Error> {
        let Some(text) = self.optional("timeout") else {
            return Ok(DEFAULT_TIMEOUT);
        };
        text.parse::<u64>()
            .ok()
            .filter(|seconds| (1..=MAX_TIMEOUT_SECS).contains(seconds))
            .map(Duration::from_secs)
            .ok_or_else(|| {
                usage(format!(
                    "--timeout must be a whole number of seconds from 1 to {MAX_TIMEOUT_SECS}"
                ))
            })
    }
}

/// The platform's origin: `https://<app host>[:port]`, or `http://` on loopback only (tests and
/// a local stand-in). Session cookies are only ever sent to the origin that set them.
#[derive(Clone, PartialEq, Eq)]
pub struct Origin {
    /// `scheme://host[:port]`, the port only when it is not the scheme's default.
    text: String,
    https: bool,
    /// The host as a TLS server name or a socket address: no IPv6 brackets.
    host: String,
    port: u16,
    /// The `Host` header: host (IPv6 in brackets) and the port when not the default.
    authority: String,
}

impl Origin {
    /// Parses `--url`: `https://` (plain `http://` only on a loopback IP address), a host, an
    /// optional port, and no credentials, path, query or fragment (a trailing `/` is fine).
    ///
    /// # Errors
    ///
    /// A usage error naming what is wrong.
    pub fn parse(text: &str) -> Result<Self, Error> {
        // A URL with user information may carry a password: it is not repeated back.
        let shown = if text.contains('@') {
            String::new()
        } else {
            format!(" {text:?}")
        };
        let invalid = |why: &str| usage(format!("--url{shown}: {why}"));
        let url = Url::parse(text).map_err(|error| invalid(&error.to_string()))?;
        let Some(host) = url.host_str() else {
            return Err(invalid("has no host"));
        };
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        // Only loopback IP literals: the name localhost is resolved, and a resolver or hosts file
        // could send it elsewhere.
        let loopback = bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        let https = match url.scheme() {
            "https" => true,
            "http" if loopback => false,
            _ => {
                return Err(invalid(
                    "must be https:// (plain http:// only on a loopback IP address such as 127.0.0.1)",
                ));
            }
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("must not contain credentials"));
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(invalid(
                "must be only the platform's address, without a path, query or fragment",
            ));
        }
        let Some(port) = url.port_or_known_default() else {
            return Err(invalid("has no port"));
        };
        let origin = url.origin().ascii_serialization();
        let authority = origin
            .split_once("://")
            .map(|(_, authority)| authority.to_owned())
            .ok_or_else(|| invalid("has no host"))?;
        Ok(Self {
            text: origin,
            https,
            host: bare.to_owned(),
            port,
            authority,
        })
    }

    /// `scheme://host[:port]`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether requests go over TLS.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.https
    }

    /// The host to connect to and verify, without IPv6 brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port to connect to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `Host` header's value.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Origin({})", self.text)
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// An absolute directory from the environment variable `name`; `None` when it is unset, empty or
/// relative (the XDG base directory specification says to ignore a relative one).
fn absolute(env: &dyn Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    env(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The directory of the workstation's key, CSR and certificate: `--dir` when given, otherwise
/// `$XDG_DATA_HOME/trustiot-workstation`, or `$HOME/.local/share/trustiot-workstation`.
///
/// # Errors
///
/// A usage error when there is no `--dir`, no usable `XDG_DATA_HOME` and no absolute `HOME`.
pub fn workstation_directory(
    option: Option<&str>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, Error> {
    if let Some(directory) = option {
        if directory.is_empty() {
            return Err(usage("--dir must not be empty"));
        }
        return Ok(PathBuf::from(directory));
    }
    if let Some(data) = absolute(env, "XDG_DATA_HOME") {
        return Ok(data.join(APP_DIRECTORY));
    }
    absolute(env, "HOME")
        .map(|home| home.join(".local/share").join(APP_DIRECTORY))
        .ok_or_else(|| usage("set --dir: neither XDG_DATA_HOME nor HOME is an absolute path"))
}

/// The directory of the session file: `$XDG_RUNTIME_DIR/trustiot-workstation`. There is no
/// fallback: the session must not outlive the user's login on this machine or land in a
/// directory that is backed up.
///
/// # Errors
///
/// When `XDG_RUNTIME_DIR` is unset, empty or relative.
pub fn session_directory(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, Error> {
    absolute(env, "XDG_RUNTIME_DIR")
        .map(|runtime| runtime.join(APP_DIRECTORY))
        .ok_or_else(|| {
            Error::Failed(
                "XDG_RUNTIME_DIR is not set to an absolute path: the session is kept only in \
                 the user's runtime directory (log in to the workstation with a normal user \
                 session, which sets it)"
                    .to_owned(),
            )
        })
}

/// A path option, refused when empty.
///
/// # Errors
///
/// A usage error when the value is empty.
pub fn path_option<'a>(options: &'a Options, name: &str) -> Result<Option<&'a Path>, Error> {
    match options.optional(name) {
        None => Ok(None),
        Some("") => Err(usage(format!("--{name} must not be empty"))),
        Some(value) => Ok(Some(Path::new(value))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &[&str]) -> Vec<String> {
        text.iter().map(|&value| value.to_owned()).collect()
    }

    #[test]
    fn options_are_named_once_with_a_value() {
        let options =
            Options::parse(&args(&["--url", "https://a.example"]), &["url"]).expect("valid");
        assert_eq!(options.optional("url"), Some("https://a.example"));
        for wrong in [
            &["--url"][..],
            &["--url", "a", "--url", "b"],
            &["--other", "x"],
            &["stray"],
        ] {
            assert!(
                matches!(Options::parse(&args(wrong), &["url"]), Err(Error::Usage(_))),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn timeouts_are_whole_seconds_within_bounds() {
        let parse = |value: &str| {
            Options::parse(&args(&["--timeout", value]), &["timeout"])
                .expect("parsed")
                .timeout()
        };
        assert_eq!(parse("5").expect("valid"), Duration::from_secs(5));
        for wrong in ["0", "601", "1.5", "-1", ""] {
            assert!(parse(wrong).is_err(), "{wrong}");
        }
        assert_eq!(
            Options::default().timeout().expect("default"),
            DEFAULT_TIMEOUT
        );
    }

    #[test]
    fn origins_are_https_or_loopback_http_without_anything_else() {
        let origin = Origin::parse("https://app.example/").expect("valid");
        assert_eq!(origin.as_str(), "https://app.example");
        assert_eq!(origin.port(), 443);
        assert_eq!(origin.authority(), "app.example");
        assert!(origin.is_https());
        let origin = Origin::parse("https://app.example:8443").expect("valid");
        assert_eq!(origin.authority(), "app.example:8443");
        assert_eq!(origin.port(), 8443);
        let origin = Origin::parse("http://127.0.0.1:4000").expect("loopback http");
        assert!(!origin.is_https());
        assert_eq!(origin.host(), "127.0.0.1");
        let origin = Origin::parse("http://[::1]:4000").expect("loopback http");
        assert_eq!(origin.host(), "::1");
        assert_eq!(origin.authority(), "[::1]:4000");
        for wrong in [
            "http://app.example",
            "http://localhost:4000",
            "http://LocalHost",
            "https://user:pw@app.example",
            "https://app.example/api",
            "https://app.example/?a=1",
            "https://app.example/#x",
            "ftp://app.example",
            "app.example",
        ] {
            let error = Origin::parse(wrong).expect_err(wrong);
            assert!(matches!(error, Error::Usage(_)), "{wrong}");
            assert!(!error.to_string().contains("pw"), "{error}");
        }
    }

    #[test]
    fn the_session_lives_only_under_an_absolute_runtime_directory() {
        let with = |value: Option<&'static str>| {
            move |name: &str| {
                (name == "XDG_RUNTIME_DIR")
                    .then_some(value)
                    .flatten()
                    .map(OsString::from)
            }
        };
        assert_eq!(
            session_directory(&with(Some("/run/user/1000"))).expect("set"),
            PathBuf::from("/run/user/1000/trustiot-workstation")
        );
        for wrong in [None, Some(""), Some("relative/dir")] {
            assert!(session_directory(&with(wrong)).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn the_workstation_directory_follows_xdg() {
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/users/operator")),
            _ => None,
        };
        assert_eq!(
            workstation_directory(None, &env).expect("from HOME"),
            PathBuf::from("/users/operator/.local/share/trustiot-workstation")
        );
        let env = |name: &str| match name {
            "XDG_DATA_HOME" => Some(OsString::from("/data")),
            "HOME" => Some(OsString::from("/users/operator")),
            _ => None,
        };
        assert_eq!(
            workstation_directory(None, &env).expect("from XDG_DATA_HOME"),
            PathBuf::from("/data/trustiot-workstation")
        );
        assert_eq!(
            workstation_directory(Some("/srv/ws"), &env).expect("from --dir"),
            PathBuf::from("/srv/ws")
        );
        assert!(workstation_directory(None, &|_| None).is_err());
    }
}
