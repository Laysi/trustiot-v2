//! The agent's configuration file (TOML, default [`DEFAULT_PATH`]).
//!
//! It holds how the gateway reaches the cloud and who it is; the device configuration
//! (`DesiredConfig`) arrives over MQTT later (Laysi/trustiot-v2#94) and is not in this file.
//! Every security-relevant field (the URL, both trust lists, the certificate and the key) must be
//! given: there are no defaults for them. Unknown keys are errors, so a misspelled key is never
//! silently ignored; that includes `broker.name`, which earlier versions had: the broker's name
//! is now the fixed [`crate::tls::BROKER_NAME`]. The other fields have defaults and bounds.
//!
//! ```toml
//! [cloud]
//! url = "wss://gw.trustiot.example/mqtt"
//! trust = ["/etc/ssl/certs/ca-certificates.crt"]
//!
//! [broker]
//! ca = "/etc/trustiot/broker-ca.pem"
//! crl = "/var/lib/trustiot-agent/root-crl.pem"
//!
//! [identity]
//! certificate = "/var/lib/trustiot-agent/gateway.pem"
//! key = { source = "tpm" }
//! ```
//!
//! The certificate file must be in a directory the agent may write to (its state directory in
//! production): certificate renewal ([`crate::renewal`]) replaces the file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::backoff::BackoffSettings;
use crate::identity::{DEFAULT_TPM_DEVICE, DEFAULT_TPM_HANDLE, KeySource};
use crate::link::DEFAULT_INFLIGHT_LIMIT;
use crate::renewal::RenewalSettings;
use crate::tls::BROKER_NAME;
use crate::transport::CloudUrl;

/// Where the agent reads its configuration unless told otherwise.
pub const DEFAULT_PATH: &str = "/etc/trustiot/agent.toml";
/// Bounds of `connection.heartbeat_interval_s`.
pub const HEARTBEAT_INTERVAL_S: std::ops::RangeInclusive<u32> = 10..=3600;
/// Bounds of `connection.reconnect_base_ms`.
pub const RECONNECT_BASE_MS: std::ops::RangeInclusive<u64> = 100..=60_000;
/// The largest `connection.reconnect_cap_ms` (one hour); the smallest is the base.
pub const RECONNECT_CAP_MAX_MS: u64 = 3_600_000;
/// Bounds of `connection.inflight_limit`.
pub const INFLIGHT_LIMIT: std::ops::RangeInclusive<u16> = 1..=256;
/// Bounds of `renewal.remaining_percent`. With less than a tenth of the validity left (9 days of
/// a 90-day certificate) a failing endpoint leaves too little time to recover before the
/// certificate expires; with more than four fifths left, a 90-day certificate would be renewed
/// every 18 days or more often.
pub const RENEWAL_REMAINING_PERCENT: std::ops::RangeInclusive<u8> = 10..=80;
/// Bounds of `renewal.retry_base_s`.
pub const RENEWAL_RETRY_BASE_S: std::ops::RangeInclusive<u64> = 1..=3600;
/// The largest `renewal.retry_cap_s` (one day); the smallest is the base.
pub const RENEWAL_RETRY_CAP_MAX_S: u64 = 86_400;
/// The persistent handle range of a TPM 2.0 (TPM_HT_PERSISTENT).
const PERSISTENT_HANDLES: std::ops::RangeInclusive<u32> = 0x8100_0000..=0x81FF_FFFF;

/// The validated configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The cloud's gateway endpoint.
    pub url: CloudUrl,
    /// PEM files whose certificates the outer TLS trusts.
    pub outer_trust: Vec<PathBuf>,
    /// Our root CA certificates, the only trust anchors of the inner TLS.
    pub broker_ca: PathBuf,
    /// Where the root CRL is kept, if set; the file may not exist yet.
    pub broker_crl: Option<PathBuf>,
    /// The gateway's certificate chain (PEM, leaf first).
    pub certificate: PathBuf,
    /// The gateway's private key.
    pub key: KeySource,
    /// How often to send a heartbeat.
    pub heartbeat_interval: Duration,
    /// Reconnect delays.
    pub reconnect: BackoffSettings,
    /// QoS 1 publishes that may wait for a PUBACK at once.
    pub inflight_limit: u16,
    /// When to renew the client certificate, and how to retry a failed renewal.
    pub renewal: RenewalSettings,
}

/// Why the configuration cannot be used. The message names the file or the key.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file cannot be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not have the expected keys and types.
    #[error("{0}")]
    Parse(String),
    /// A value breaks a rule.
    #[error("{key}: {reason}")]
    Invalid {
        /// The key, such as `cloud.url`.
        key: &'static str,
        /// The rule it breaks.
        reason: String,
    },
}

fn invalid(key: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key,
        reason: reason.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    cloud: RawCloud,
    broker: RawBroker,
    identity: RawIdentity,
    #[serde(default)]
    connection: RawConnection,
    #[serde(default)]
    renewal: RawRenewal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCloud {
    url: String,
    trust: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBroker {
    ca: PathBuf,
    #[serde(default)]
    crl: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIdentity {
    certificate: PathBuf,
    key: RawKey,
}

#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
enum RawKey {
    Tpm {
        #[serde(default = "default_tpm_device")]
        device: PathBuf,
        #[serde(default = "default_tpm_handle")]
        handle: u32,
    },
    File {
        path: PathBuf,
    },
}

fn default_tpm_device() -> PathBuf {
    PathBuf::from(DEFAULT_TPM_DEVICE)
}

const fn default_tpm_handle() -> u32 {
    DEFAULT_TPM_HANDLE
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConnection {
    heartbeat_interval_s: u32,
    reconnect_base_ms: u64,
    reconnect_cap_ms: u64,
    inflight_limit: u16,
}

impl Default for RawConnection {
    fn default() -> Self {
        Self {
            heartbeat_interval_s: 60,
            reconnect_base_ms: 1000,
            reconnect_cap_ms: 60_000,
            inflight_limit: DEFAULT_INFLIGHT_LIMIT,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawRenewal {
    remaining_percent: u8,
    retry_base_s: u64,
    retry_cap_s: u64,
}

impl Default for RawRenewal {
    fn default() -> Self {
        let defaults = RenewalSettings::default();
        Self {
            remaining_percent: defaults.remaining_percent,
            retry_base_s: defaults.retry.base.as_secs(),
            retry_cap_s: defaults.retry.cap.as_secs(),
        }
    }
}

fn absolute(key: &'static str, path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid(
            key,
            format!("{} must be an absolute path", path.display()),
        ))
    }
}

impl Config {
    /// Reads and validates the file at `path`. Only reads the file itself; the certificates
    /// and keys it names are loaded by [`crate::setup::Setup::new`].
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|error| match error {
            ConfigError::Parse(message) => {
                ConfigError::Parse(format!("{}: {message}", path.display()))
            }
            other => other,
        })
    }

    /// Parses and validates configuration text.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: Raw =
            toml::from_str(text).map_err(|error| ConfigError::Parse(error.to_string()))?;
        let url = CloudUrl::parse(&raw.cloud.url).map_err(|reason| invalid("cloud.url", reason))?;
        if raw.cloud.trust.is_empty() {
            return Err(invalid(
                "cloud.trust",
                "must list at least one CA file (for SaaS the OS bundle, for example /etc/ssl/certs/ca-certificates.crt)",
            ));
        }
        let outer_trust = raw
            .cloud
            .trust
            .into_iter()
            .map(|path| absolute("cloud.trust", path))
            .collect::<Result<Vec<_>, _>>()?;
        // The outer TLS and the inner mTLS authenticate different servers, each against its own
        // trust anchors. One name for both would blur that line (Laysi/trustiot-v2#86 review).
        if url
            .host()
            .trim_end_matches('.')
            .eq_ignore_ascii_case(BROKER_NAME)
        {
            return Err(invalid(
                "cloud.url",
                format!(
                    "must not be the broker's inner name {BROKER_NAME} (the outer TLS and the inner mTLS name different servers)"
                ),
            ));
        }
        let broker_ca = absolute("broker.ca", raw.broker.ca)?;
        let broker_crl = raw
            .broker
            .crl
            .map(|path| absolute("broker.crl", path))
            .transpose()?;
        let certificate = absolute("identity.certificate", raw.identity.certificate)?;
        let key = match raw.identity.key {
            RawKey::Tpm { device, handle } => {
                if !PERSISTENT_HANDLES.contains(&handle) {
                    return Err(invalid(
                        "identity.key.handle",
                        format!("{handle:#x} is not a persistent handle (0x81000000-0x81ffffff)"),
                    ));
                }
                KeySource::Tpm {
                    device: absolute("identity.key.device", device)?,
                    handle,
                }
            }
            RawKey::File { path } => KeySource::File {
                path: absolute("identity.key.path", path)?,
            },
        };
        let connection = raw.connection;
        if !HEARTBEAT_INTERVAL_S.contains(&connection.heartbeat_interval_s) {
            return Err(invalid(
                "connection.heartbeat_interval_s",
                format!(
                    "must be within {}..={}",
                    HEARTBEAT_INTERVAL_S.start(),
                    HEARTBEAT_INTERVAL_S.end()
                ),
            ));
        }
        if !RECONNECT_BASE_MS.contains(&connection.reconnect_base_ms) {
            return Err(invalid(
                "connection.reconnect_base_ms",
                format!(
                    "must be within {}..={}",
                    RECONNECT_BASE_MS.start(),
                    RECONNECT_BASE_MS.end()
                ),
            ));
        }
        if !(connection.reconnect_base_ms..=RECONNECT_CAP_MAX_MS)
            .contains(&connection.reconnect_cap_ms)
        {
            return Err(invalid(
                "connection.reconnect_cap_ms",
                format!("must be within reconnect_base_ms..={RECONNECT_CAP_MAX_MS}"),
            ));
        }
        if !INFLIGHT_LIMIT.contains(&connection.inflight_limit) {
            return Err(invalid(
                "connection.inflight_limit",
                format!(
                    "must be within {}..={}",
                    INFLIGHT_LIMIT.start(),
                    INFLIGHT_LIMIT.end()
                ),
            ));
        }
        let renewal = raw.renewal;
        if !RENEWAL_REMAINING_PERCENT.contains(&renewal.remaining_percent) {
            return Err(invalid(
                "renewal.remaining_percent",
                format!(
                    "must be within {}..={}",
                    RENEWAL_REMAINING_PERCENT.start(),
                    RENEWAL_REMAINING_PERCENT.end()
                ),
            ));
        }
        if !RENEWAL_RETRY_BASE_S.contains(&renewal.retry_base_s) {
            return Err(invalid(
                "renewal.retry_base_s",
                format!(
                    "must be within {}..={}",
                    RENEWAL_RETRY_BASE_S.start(),
                    RENEWAL_RETRY_BASE_S.end()
                ),
            ));
        }
        if !(renewal.retry_base_s..=RENEWAL_RETRY_CAP_MAX_S).contains(&renewal.retry_cap_s) {
            return Err(invalid(
                "renewal.retry_cap_s",
                format!("must be within retry_base_s..={RENEWAL_RETRY_CAP_MAX_S}"),
            ));
        }
        Ok(Self {
            url,
            outer_trust,
            broker_ca,
            broker_crl,
            certificate,
            key,
            heartbeat_interval: Duration::from_secs(u64::from(connection.heartbeat_interval_s)),
            reconnect: BackoffSettings {
                base: Duration::from_millis(connection.reconnect_base_ms),
                cap: Duration::from_millis(connection.reconnect_cap_ms),
            },
            inflight_limit: connection.inflight_limit,
            renewal: RenewalSettings {
                remaining_percent: renewal.remaining_percent,
                retry: BackoffSettings {
                    base: Duration::from_secs(renewal.retry_base_s),
                    cap: Duration::from_secs(renewal.retry_cap_s),
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[cloud]
url = "wss://gw.trustiot.example/mqtt"
trust = ["/etc/ssl/certs/ca-certificates.crt"]

[broker]
ca = "/etc/trustiot/broker-ca.pem"

[identity]
certificate = "/var/lib/trustiot-agent/gateway.pem"
key = { source = "tpm" }
"#;

    fn error_of(text: &str) -> String {
        Config::parse(text).expect_err(text).to_string()
    }

    #[test]
    fn a_minimal_file_takes_the_documented_defaults() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.url.as_str(), "wss://gw.trustiot.example/mqtt");
        assert_eq!(
            config.key,
            KeySource::Tpm {
                device: PathBuf::from("/dev/tpmrm0"),
                handle: 0x8100_0100
            }
        );
        assert_eq!(config.heartbeat_interval, Duration::from_secs(60));
        assert_eq!(config.reconnect, BackoffSettings::default());
        assert_eq!(config.inflight_limit, 32);
        assert_eq!(config.broker_crl, None, "no root CRL unless configured");
        assert_eq!(config.renewal, RenewalSettings::default());
        assert_eq!(config.renewal.remaining_percent, 33);
        assert_eq!(
            config.renewal.retry,
            BackoffSettings {
                base: Duration::from_secs(30),
                cap: Duration::from_secs(1800)
            }
        );
    }

    #[test]
    fn the_example_in_the_crate_matches_the_parser() {
        let example = include_str!("../agent.example.toml");
        let config = Config::parse(example).unwrap();
        assert!(matches!(config.key, KeySource::Tpm { .. }));
        assert_eq!(
            config.broker_crl,
            Some(PathBuf::from("/var/lib/trustiot-agent/root-crl.pem"))
        );
        // The example says its [renewal] values are the defaults.
        assert!(example.contains("\n[renewal]\n"));
        assert_eq!(config.renewal, RenewalSettings::default());
    }

    #[test]
    fn the_broker_name_is_fixed_and_an_old_broker_name_key_is_refused() {
        let error =
            error_of(&MINIMAL.replace("[broker]", "[broker]\nname = \"mqtt.trustiot.example\""));
        assert!(error.contains("unknown field `name`"), "{error}");
        let error = error_of(&MINIMAL.replace(
            "wss://gw.trustiot.example/mqtt",
            "wss://MQTT-broker.trustiot.internal./mqtt",
        ));
        assert!(
            error.contains("cloud.url: must not be the broker's inner name"),
            "{error}"
        );
    }

    #[test]
    fn every_security_relevant_field_is_required() {
        for (key, line) in [
            ("url", "url = \"wss://gw.trustiot.example/mqtt\""),
            ("trust", "trust = [\"/etc/ssl/certs/ca-certificates.crt\"]"),
            ("ca", "ca = \"/etc/trustiot/broker-ca.pem\""),
            (
                "certificate",
                "certificate = \"/var/lib/trustiot-agent/gateway.pem\"",
            ),
            ("key", "key = { source = \"tpm\" }"),
        ] {
            let text = MINIMAL.replace(line, "");
            let error = error_of(&text);
            assert!(
                error.contains(&format!("missing field `{key}`")),
                "{key}: {error}"
            );
        }
        assert!(error_of("").contains("missing field"));
        // A key source must be named; there is no default source.
        let error = error_of(&MINIMAL.replace(
            "key = { source = \"tpm\" }",
            "key = { device = \"/dev/tpmrm0\" }",
        ));
        assert!(error.contains("source"), "{error}");
    }

    #[test]
    fn values_that_break_a_rule_are_refused_with_their_key() {
        let cases = [
            (
                "wss://gw.trustiot.example/mqtt\"",
                "ws://gw.trustiot.example/mqtt\"",
                "cloud.url: must use the wss scheme",
            ),
            (
                "wss://gw.trustiot.example/mqtt\"",
                "wss://192.0.2.1/mqtt\"",
                "cloud.url: must name a DNS host",
            ),
            (
                "[\"/etc/ssl/certs/ca-certificates.crt\"]",
                "[]",
                "cloud.trust: must list at least one",
            ),
            (
                "[\"/etc/ssl/certs/ca-certificates.crt\"]",
                "[\"certs.pem\"]",
                "cloud.trust: certs.pem must be an absolute path",
            ),
            ("\"/etc/trustiot/broker-ca.pem\"", "\"ca.pem\"", "broker.ca"),
            (
                "ca = \"/etc/trustiot/broker-ca.pem\"",
                "ca = \"/etc/trustiot/broker-ca.pem\"\ncrl = \"root-crl.pem\"",
                "broker.crl: root-crl.pem must be an absolute path",
            ),
            (
                "\"/var/lib/trustiot-agent/gateway.pem\"",
                "\"gateway.pem\"",
                "identity.certificate",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"tpm\", handle = 0x80000001 }",
                "identity.key.handle",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"tpm\", device = \"tpmrm0\" }",
                "identity.key.device",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"file\", path = \"key.pem\" }",
                "identity.key.path",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"pkcs11\" }",
                "unknown variant `pkcs11`",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"file\" }",
                "missing field `path`",
            ),
            (
                "{ source = \"tpm\" }",
                "{ source = \"tpm\", path = \"/k.pem\" }",
                "unknown field `path`",
            ),
        ];
        for (from, to, expected) in cases {
            let text = MINIMAL.replace(from, to);
            assert_ne!(text, MINIMAL, "{from}");
            let error = error_of(&text);
            assert!(error.contains(expected), "{to}: {error}");
        }
        let file_key = MINIMAL.replace(
            "{ source = \"tpm\" }",
            "{ source = \"file\", path = \"/k.pem\" }",
        );
        assert_eq!(
            Config::parse(&file_key).unwrap().key,
            KeySource::File {
                path: PathBuf::from("/k.pem")
            }
        );
    }

    #[test]
    fn unknown_keys_anywhere_are_refused() {
        for (section, key) in [
            ("[cloud]", "proxy = \"x\""),
            ("[broker]", "port = 8883"),
            ("[identity]", "gateway_id = \"gw-1\""),
        ] {
            let text = MINIMAL.replace(section, &format!("{section}\n{key}"));
            assert!(error_of(&text).contains("unknown field"), "{key}");
        }
        assert!(
            error_of(&format!("{MINIMAL}\n[tls]\nverify = false\n"))
                .contains("unknown field `tls`")
        );
        assert!(
            error_of(&format!("{MINIMAL}\n[connection]\nkeep_alive_s = 300\n"))
                .contains("unknown field")
        );
        assert!(
            error_of(&format!("{MINIMAL}\n[renewal]\nurl = \"https://x/\"\n"))
                .contains("unknown field")
        );
    }

    #[test]
    fn renewal_settings_are_bounded() {
        let with = |body: &str| format!("{MINIMAL}\n[renewal]\n{body}\n");
        let config = Config::parse(&with(
            "remaining_percent = 50\nretry_base_s = 10\nretry_cap_s = 600",
        ))
        .unwrap();
        assert_eq!(
            config.renewal,
            RenewalSettings {
                remaining_percent: 50,
                retry: BackoffSettings {
                    base: Duration::from_secs(10),
                    cap: Duration::from_secs(600)
                },
            }
        );
        for (body, key) in [
            ("remaining_percent = 9", "remaining_percent"),
            ("remaining_percent = 81", "remaining_percent"),
            ("remaining_percent = -1", "remaining_percent"),
            ("retry_base_s = 0", "retry_base_s"),
            ("retry_base_s = 3601", "retry_base_s"),
            ("retry_base_s = 60\nretry_cap_s = 59", "retry_cap_s"),
            ("retry_cap_s = 86401", "retry_cap_s"),
        ] {
            let error = error_of(&with(body));
            assert!(error.contains(key), "{body}: {error}");
        }
    }

    #[test]
    fn connection_settings_are_bounded() {
        let with = |body: &str| format!("{MINIMAL}\n[connection]\n{body}\n");
        let config = Config::parse(&with(
            "heartbeat_interval_s = 30\nreconnect_base_ms = 500\nreconnect_cap_ms = 30000\ninflight_limit = 8",
        ))
        .unwrap();
        assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
        assert_eq!(
            config.reconnect,
            BackoffSettings {
                base: Duration::from_millis(500),
                cap: Duration::from_secs(30)
            }
        );
        assert_eq!(config.inflight_limit, 8);
        for (body, key) in [
            ("heartbeat_interval_s = 9", "heartbeat_interval_s"),
            ("heartbeat_interval_s = 3601", "heartbeat_interval_s"),
            ("reconnect_base_ms = 99", "reconnect_base_ms"),
            ("reconnect_base_ms = 60001", "reconnect_base_ms"),
            (
                "reconnect_base_ms = 2000\nreconnect_cap_ms = 1000",
                "reconnect_cap_ms",
            ),
            ("reconnect_cap_ms = 3600001", "reconnect_cap_ms"),
            ("inflight_limit = 0", "inflight_limit"),
            ("inflight_limit = 257", "inflight_limit"),
            ("inflight_limit = -1", "inflight_limit"),
        ] {
            let error = error_of(&with(body));
            assert!(error.contains(key), "{body}: {error}");
        }
    }
}
