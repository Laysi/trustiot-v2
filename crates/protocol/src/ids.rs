//! Validated identifiers and scalar types that appear on the wire.
//!
//! Every identifier is checked when it is constructed and again when it is decoded, so a value
//! of one of these types is always safe to put in an MQTT topic, a log line, or a database
//! column. The cloud never trusts identifiers a gateway sends beyond this syntax check: the
//! gateway's identity comes from the topic (see [`crate::topic`]), which the broker ACL ties to
//! the certificate.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};

/// Longest identifier, in bytes.
pub const MAX_ID_LEN: usize = 64;

/// Longest device model name, in characters (rebuild checklist C1).
pub const MAX_MODEL_NAME_LEN: usize = 40;

/// Longest agent version string, in bytes.
pub const MAX_AGENT_VERSION_LEN: usize = 64;

/// Why a string is not a valid identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdError {
    kind: &'static str,
    excerpt: String,
    rule: &'static str,
}

impl IdError {
    fn new(kind: &'static str, value: &str, rule: &'static str) -> Self {
        Self {
            kind,
            excerpt: excerpt(value),
            rule,
        }
    }

    /// The kind of identifier that failed, for example `device ID`.
    pub fn kind(&self) -> &'static str {
        self.kind
    }
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {:?}: {}", self.kind, self.excerpt, self.rule)
    }
}

impl std::error::Error for IdError {}

/// At most the first 64 characters, so an error never echoes an unbounded attacker-chosen
/// string. `Debug` formatting in [`IdError`]'s `Display` escapes control characters.
fn excerpt(value: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut out: String = value.chars().take(MAX_CHARS).collect();
    if value.chars().nth(MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

const ID_RULE: &str =
    "must be 1-64 ASCII letters, digits, '.', '_' or '-', starting with a letter or digit";

/// The identifier rule shared by gateway, device, stream, session, lease and request IDs and
/// by data point keys. It excludes the MQTT topic separators and wildcards (`/`, `+`, `#`), the
/// `$` prefix of broker system topics, whitespace and control characters.
fn is_valid_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.len() <= MAX_ID_LEN
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

const MODEL_NAME_RULE: &str = "must be 1-40 lowercase letters or digits in groups joined by single hyphens, starting with a letter";

/// Rebuild checklist C1 and the legacy `naming.ts`: `^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$`, 1–40
/// characters.
fn is_valid_model_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() > MAX_MODEL_NAME_LEN || !bytes.first().is_some_and(u8::is_ascii_lowercase) {
        return false;
    }
    let mut previous_hyphen = false;
    for &byte in bytes {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_hyphen = false,
            b'-' if !previous_hyphen => previous_hyphen = true,
            _ => return false,
        }
    }
    !previous_hyphen
}

const AGENT_VERSION_RULE: &str =
    "must be 1-64 ASCII letters, digits, '.', '+', '_' or '-', starting with a letter or digit";

/// A semantic version with optional pre-release and build metadata, for example
/// `1.4.0-rc.1+3a94ff1`; only the character set is enforced.
fn is_valid_agent_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.len() <= MAX_AGENT_VERSION_LEN
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'_' | b'-'))
}

macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $check:ident, $rule:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if $check(&value) {
                    Ok(Self(value))
                } else {
                    Err(IdError::new($kind, &value, $rule))
                }
            }

            /// The validated string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }
    };
}

validated_string!(
    /// A gateway's identity: the common name (CN) of its client certificate, which is also its
    /// MQTT username and client ID and the gateway segment of its topics.
    GatewayId,
    "gateway ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// A device, as assigned by the assets service and listed in the desired configuration.
    DeviceId,
    "device ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// A data point's key, unique within its device model; telemetry names points by it.
    PointKey,
    "data point key",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// A telemetry stream, within which batch sequence numbers count and are never reused. The
    /// agent keeps its stream across restarts by storing the next sequence number with its
    /// buffer, and starts a new stream whenever it cannot know that number, for example after a
    /// reinstall (rebuild checklist G1).
    StreamId,
    "stream ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// One MQTT connection of a gateway; ties heartbeats and the last will to the connection
    /// that sent them.
    SessionId,
    "session ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// One live-value lease, chosen by the cloud.
    LeaseId,
    "lease ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// One cloud request that expects a correlated reply, such as a log pull.
    RequestId,
    "request ID",
    is_valid_id,
    ID_RULE
);
validated_string!(
    /// A device model's name: unique per tenant and immutable after creation.
    ModelName,
    "model name",
    is_valid_model_name,
    MODEL_NAME_RULE
);
validated_string!(
    /// The edge agent's release version.
    AgentVersion,
    "agent version",
    is_valid_agent_version,
    AGENT_VERSION_RULE
);

/// A batch sequence number. Sequence numbers start at 1 because the cloud's high-water mark
/// for a stream with no committed batch is 0 (rebuild checklist I1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(NonZeroU64);

impl Seq {
    /// The first sequence number of every stream.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// `None` for 0, which is never a valid sequence number.
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The sequence number as an integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// The following sequence number, or `None` when the stream has used up `u64`.
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A desired-configuration version. Versions increase per gateway and start at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfigVersion(NonZeroU64);

impl ConfigVersion {
    /// `None` for 0, which is never a valid version.
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The version as an integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for ConfigVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A point in time: milliseconds since 1970-01-01T00:00:00Z, from the clock of whoever wrote
/// it. A gateway's clock may be wrong until it is synchronized; messages say so where it
/// matters (for example [`crate::telemetry::TelemetryBatch::clock_synced`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(u64);

impl UnixMillis {
    /// Wraps milliseconds since the Unix epoch.
    pub const fn new(millis: u64) -> Self {
        Self(millis)
    }

    /// Milliseconds since the Unix epoch.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Converts a system time; `None` before 1970 or beyond `u64` milliseconds.
    pub fn from_system_time(time: SystemTime) -> Option<Self> {
        let since_epoch = time.duration_since(UNIX_EPOCH).ok()?;
        u64::try_from(since_epoch.as_millis()).ok().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_the_documented_characters() {
        for value in [
            "a",
            "0",
            "gw-0001",
            "0190f5c2-7a4b-7cde-8f00-123456789abc",
            "power_factor",
            "Current",
            "kwh.total",
            &"x".repeat(MAX_ID_LEN),
        ] {
            assert!(DeviceId::new(value).is_ok(), "{value:?}");
            assert!(GatewayId::new(value).is_ok(), "{value:?}");
        }
    }

    #[test]
    fn ids_reject_topic_syntax_and_bad_lengths() {
        for value in [
            "",
            "+",
            "#",
            "a/b",
            "a+b",
            "a#",
            "$SYS",
            "-leading",
            ".leading",
            "_leading",
            "has space",
            "tab\t",
            "nul\0",
            "電壓",
            "é",
            &"x".repeat(MAX_ID_LEN + 1),
        ] {
            let error = GatewayId::new(value).unwrap_err();
            assert_eq!(error.kind(), "gateway ID");
            assert!(PointKey::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn id_errors_do_not_echo_unbounded_input() {
        let error = DeviceId::new(format!("{}\n/", "y".repeat(10_000))).unwrap_err();
        let message = error.to_string();
        assert!(message.len() < 200, "{message}");
        assert!(message.contains('…'));
        let error = DeviceId::new("bad\nline").unwrap_err();
        assert!(
            !error.to_string().contains('\n'),
            "control characters are escaped"
        );
    }

    #[test]
    fn model_names_follow_c1() {
        for value in [
            "m",
            "ms300",
            "ms300-vfd",
            "a-b-c",
            &"a".repeat(MAX_MODEL_NAME_LEN),
        ] {
            assert!(ModelName::new(value).is_ok(), "{value:?}");
        }
        for value in [
            "",
            "MS300",
            "1abc",
            "-a",
            "a-",
            "a--b",
            "a_b",
            "a.b",
            "msThree",
            &"a".repeat(MAX_MODEL_NAME_LEN + 1),
        ] {
            assert!(ModelName::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn agent_versions_allow_semver_metadata() {
        assert!(AgentVersion::new("1.4.0-rc.1+3a94ff1").is_ok());
        assert!(AgentVersion::new("").is_err());
        assert!(AgentVersion::new("v 1").is_err());
    }

    #[test]
    fn sequence_numbers_start_at_one_and_never_wrap() {
        assert_eq!(Seq::FIRST.get(), 1);
        assert_eq!(Seq::new(0), None);
        assert_eq!(Seq::FIRST.next(), Seq::new(2));
        assert_eq!(Seq::new(u64::MAX).and_then(Seq::next), None);
        assert_eq!(ConfigVersion::new(0), None);
    }

    #[test]
    fn unix_millis_converts_system_time() {
        let time = UNIX_EPOCH + std::time::Duration::from_millis(1_760_054_400_123);
        assert_eq!(
            UnixMillis::from_system_time(time),
            Some(UnixMillis::new(1_760_054_400_123))
        );
        let before_epoch = UNIX_EPOCH - std::time::Duration::from_millis(1);
        assert_eq!(UnixMillis::from_system_time(before_epoch), None);
    }
}
