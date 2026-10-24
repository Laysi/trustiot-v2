//! The settings of a connection and the password file.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use zeroize::{Zeroize as _, Zeroizing};

/// The broker's service port (deploy/chart/README.md, "MQTT broker").
pub const DEFAULT_PORT: u16 = 1883;

/// The longest password the password file may hold. The chart's passwords are 256-bit values
/// written as text, far below this.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Every service account's username starts with this; the rest is the service's name.
pub const USERNAME_PREFIX: &str = "service:";

/// The longest service name in a username: a Kubernetes name label.
const MAX_SERVICE_NAME_BYTES: usize = 63;

/// The longest host name (RFC 1035).
const MAX_HOST_BYTES: usize = 253;

/// Where and as whom a service connects. Embed it in the service's configuration type, for
/// example as `mqtt`, so the keys are `mqtt.host`, `mqtt.port`, `mqtt.username` and
/// `mqtt.password_file`; call [`Config::validate`] from the service's own validation.
///
/// There is no TLS setting, and no setting that turns a check off: the client speaks plain MQTT
/// to the service port, which only the mesh (ztunnel's mTLS) reaches.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host`: the broker's host name or IP address, `<release>-mqtt-broker.<namespace>.svc`
    /// in the cluster.
    pub host: String,
    /// `port`: the service port, [`DEFAULT_PORT`] when not set.
    #[serde(default = "default_port")]
    pub port: u16,
    /// `username`: the service's broker account, `service:<service>`.
    pub username: String,
    /// `password_file`: the file that holds the account's password (the chart mounts the
    /// service's key of the broker's account Secret). Read again for every connection, so a
    /// rotated password takes effect at the next reconnect without a restart.
    pub password_file: PathBuf,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// A setting with an unacceptable value. Names the setting, never its value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `host` is empty, too long, or has characters other than letters, digits, `.`, `-` and
    /// `:` (an IPv6 address).
    #[error(
        "mqtt host must be a host name or an IP address (letters, digits, '.', '-', ':'), at most {MAX_HOST_BYTES} bytes"
    )]
    Host,
    /// `port` is 0.
    #[error("mqtt port must be between 1 and 65535")]
    Port,
    /// `username` is not `service:<service>` with a service name of lowercase letters, digits and
    /// `-`.
    #[error(
        "mqtt username must be 'service:' followed by a service name of 1 to {MAX_SERVICE_NAME_BYTES} lowercase letters, digits and '-'"
    )]
    Username,
    /// `password_file` is empty.
    #[error("mqtt password_file must name a file")]
    PasswordFile,
}

impl Config {
    /// Checks every setting.
    ///
    /// # Errors
    ///
    /// The first setting whose value is unacceptable.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let host_ok = !self.host.is_empty()
            && self.host.len() <= MAX_HOST_BYTES
            && self
                .host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'));
        if !host_ok {
            return Err(ConfigError::Host);
        }
        if self.port == 0 {
            return Err(ConfigError::Port);
        }
        let service = self
            .username
            .strip_prefix(USERNAME_PREFIX)
            .ok_or(ConfigError::Username)?;
        let service_ok = !service.is_empty()
            && service.len() <= MAX_SERVICE_NAME_BYTES
            && service
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !service_ok {
            return Err(ConfigError::Username);
        }
        if self.password_file.as_os_str().is_empty() {
            return Err(ConfigError::PasswordFile);
        }
        Ok(())
    }
}

/// A broker account's password. Wiped from memory when dropped; its `Debug` output is
/// `Password(<redacted>)`, and nothing else in this crate formats it.
pub struct Password(Zeroizing<String>);

/// Why the password file could not be used. Never contains the file's content.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// The file could not be opened or read.
    #[error("the mqtt password file cannot be read: {0}")]
    Unreadable(std::io::ErrorKind),
    /// The file is empty (a final line break does not count).
    #[error("the mqtt password file is empty")]
    Empty,
    /// The file holds more than [`MAX_PASSWORD_BYTES`].
    #[error("the mqtt password file holds more than {MAX_PASSWORD_BYTES} bytes")]
    TooLong,
    /// The password has a character other than printable ASCII: the chart's passwords never do.
    #[error("the mqtt password must be printable ASCII without spaces")]
    Characters,
}

impl Password {
    /// Reads the password from `path`. One final line break (`\n` or `\r\n`) is not part of
    /// the password: the chart's passwords have none, but a file written by hand often does.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is empty, is too long, or has other characters than
    /// printable ASCII.
    pub fn read(path: &Path) -> Result<Self, PasswordError> {
        let file =
            std::fs::File::open(path).map_err(|error| PasswordError::Unreadable(error.kind()))?;
        // Room for the longest password, its line break and one more byte to detect a longer
        // file, allocated once: a growing Vec would leave copies of the password behind.
        let limit = MAX_PASSWORD_BYTES + 3;
        let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 1));
        file.take(u64::try_from(limit).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .map_err(|error| PasswordError::Unreadable(error.kind()))?;
        if bytes.ends_with(b"\n") {
            bytes.pop();
            if bytes.ends_with(b"\r") {
                bytes.pop();
            }
        }
        if bytes.is_empty() {
            return Err(PasswordError::Empty);
        }
        if bytes.len() > MAX_PASSWORD_BYTES {
            return Err(PasswordError::TooLong);
        }
        if !bytes.iter().all(u8::is_ascii_graphic) {
            return Err(PasswordError::Characters);
        }
        match String::from_utf8(std::mem::take(&mut *bytes)) {
            Ok(text) => Ok(Self(Zeroizing::new(text))),
            Err(error) => {
                // Unreachable: printable ASCII is UTF-8. Wipe the bytes the error hands back.
                error.into_bytes().zeroize();
                Err(PasswordError::Characters)
            }
        }
    }

    /// The password, for the CONNECT packet only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            host: "trustiot-mqtt-broker.trustiot.svc".to_owned(),
            port: DEFAULT_PORT,
            username: "service:certs".to_owned(),
            password_file: PathBuf::from("/run/secrets/mqtt/password"),
        }
    }

    #[test]
    fn a_service_account_on_a_cluster_host_is_valid() {
        assert_eq!(config().validate(), Ok(()));
        let ipv6 = Config {
            host: "fd00::1".to_owned(),
            ..config()
        };
        assert_eq!(ipv6.validate(), Ok(()));
    }

    #[test]
    fn settings_are_checked_by_name() {
        for host in ["", "mqtt://broker", "broker/x", "user@broker", "bro ker"] {
            let bad = Config {
                host: host.to_owned(),
                ..config()
            };
            assert_eq!(bad.validate(), Err(ConfigError::Host), "{host:?}");
        }
        let bad = Config {
            port: 0,
            ..config()
        };
        assert_eq!(bad.validate(), Err(ConfigError::Port));
        for username in [
            "certs",
            "service:",
            "service:Certs",
            "service:certs:1",
            "gw-a",
            "service:a/b",
        ] {
            let bad = Config {
                username: username.to_owned(),
                ..config()
            };
            assert_eq!(bad.validate(), Err(ConfigError::Username), "{username:?}");
        }
        let bad = Config {
            password_file: PathBuf::new(),
            ..config()
        };
        assert_eq!(bad.validate(), Err(ConfigError::PasswordFile));
    }

    #[test]
    fn the_configuration_deserializes_with_a_default_port_and_rejects_unknown_keys() {
        let parsed: Config = serde_json::from_value(serde_json::json!({
            "host": "broker",
            "username": "service:ingest",
            "password_file": "/run/secrets/mqtt/password",
        }))
        .unwrap();
        assert_eq!(parsed.port, DEFAULT_PORT);
        let unknown = serde_json::from_value::<Config>(serde_json::json!({
            "host": "broker",
            "username": "service:ingest",
            "password_file": "/p",
            "tls": false,
        }));
        assert!(unknown.is_err());
    }
}
