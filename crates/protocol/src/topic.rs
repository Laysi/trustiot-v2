//! Topic names, delivery rules and payload size limits.
//!
//! Every topic has six levels: `trustiot/v1/gw/{gateway}/{up|down}/{channel}`.
//!
//! - `v1` is the wire format's major version ([`crate::PROTOCOL_VERSION`]). An incompatible
//!   format gets a new topic tree, so old and new gateways can coexist while the cloud serves
//!   both.
//! - `{gateway}` is the gateway's certificate CN ([`GatewayId`]). The broker ACL lets a gateway
//!   publish only under its own `up/` and subscribe only under its own `down/`
//!   ([`crate::acl`]), so the cloud takes the gateway's identity from the topic and never from
//!   the payload.
//! - `up` is gateway → cloud and `down` is cloud → gateway.

use std::fmt;
use std::time::Duration;

use crate::ids::{GatewayId, IdError};

/// First topic level.
pub const TOPIC_ROOT: &str = "trustiot";
/// Second topic level: the wire format's major version.
pub const VERSION_LEVEL: &str = "v1";
/// Third topic level: the topic tree for gateways.
pub const GATEWAY_LEVEL: &str = "gw";
/// The first three levels joined: `trustiot/v1/gw`.
pub const TOPIC_PREFIX: &str = "trustiot/v1/gw";

/// Largest payload of the per-gateway snapshot channels, whose size grows with the number of
/// devices: the desired configuration and the heartbeat. Both cannot be split, so
/// [`crate::config::MAX_DEVICES`] is chosen to keep their worst case within this limit.
pub const MAX_LARGE_BYTES: usize = 1024 * 1024;
/// Largest payload of the bulk channels: telemetry, logs and config reports. Telemetry and
/// logs are split to fit ([`crate::telemetry::BatchBuilder`], [`crate::logs::LogBatch::split`],
/// [`crate::logs::LogDump::split`]); a config report is bounded by
/// [`crate::config::MAX_REPORT_ISSUES`].
pub const MAX_BULK_BYTES: usize = 256 * 1024;
/// Largest payload of the medium channels: live values and live control. Live values are
/// split to fit ([`crate::live::LiveSamples::split`]).
pub const MAX_MEDIUM_BYTES: usize = 64 * 1024;
/// Largest payload of the small control channels: status, telemetry acks and log requests.
pub const MAX_CONTROL_BYTES: usize = 4 * 1024;
/// The smallest MQTT maximum packet size the broker and every client must accept: the largest
/// payload plus room for the fixed header, the topic and the properties.
pub const MIN_MAX_PACKET_BYTES: usize = MAX_LARGE_BYTES + 8 * 1024;

/// Which way a message travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Gateway to cloud.
    Up,
    /// Cloud to gateway.
    Down,
}

impl Direction {
    /// The topic level: `up` or `down`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// MQTT quality of service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QoS {
    /// QoS 0: no acknowledgement; lost if the connection drops.
    AtMostOnce,
    /// QoS 1: acknowledged with PUBACK; redelivered until acknowledged.
    AtLeastOnce,
}

impl QoS {
    /// The QoS level as sent in the PUBLISH packet.
    pub const fn level(self) -> u8 {
        match self {
            Self::AtMostOnce => 0,
            Self::AtLeastOnce => 1,
        }
    }
}

/// How the publisher must send a channel's messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Delivery {
    /// The QoS of the PUBLISH.
    pub qos: QoS,
    /// The retain flag of the PUBLISH.
    pub retain: bool,
    /// The MQTT 5 message expiry interval, for messages that are useless once stale and would
    /// otherwise wait in an offline gateway's session.
    pub message_expiry: Option<Duration>,
}

/// One kind of message, identified by its direction and last topic level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    /// `up/telemetry`: [`crate::telemetry::TelemetryBatch`].
    Telemetry,
    /// `down/telemetry-ack`: [`crate::telemetry::TelemetryAck`].
    TelemetryAck,
    /// `down/config` (retained): [`crate::config::DesiredConfig`].
    Config,
    /// `up/config-report`: [`crate::config::ConfigReport`].
    ConfigReport,
    /// `up/status` (retained, also the last will): [`crate::health::Status`].
    Status,
    /// `up/heartbeat`: [`crate::health::Heartbeat`].
    Heartbeat,
    /// `up/live`: [`crate::live::LiveSamples`].
    Live,
    /// `down/live-control`: [`crate::live::LiveControl`].
    LiveControl,
    /// `up/log`: [`crate::logs::LogBatch`].
    Log,
    /// `down/log-request`: [`crate::logs::LogRequest`].
    LogRequest,
    /// `up/log-dump`: [`crate::logs::LogDump`].
    LogDump,
}

impl Channel {
    /// Every channel.
    pub const ALL: [Self; 11] = [
        Self::Telemetry,
        Self::TelemetryAck,
        Self::Config,
        Self::ConfigReport,
        Self::Status,
        Self::Heartbeat,
        Self::Live,
        Self::LiveControl,
        Self::Log,
        Self::LogRequest,
        Self::LogDump,
    ];

    /// Which way the channel's messages travel.
    pub const fn direction(self) -> Direction {
        match self {
            Self::Telemetry
            | Self::ConfigReport
            | Self::Status
            | Self::Heartbeat
            | Self::Live
            | Self::Log
            | Self::LogDump => Direction::Up,
            Self::TelemetryAck | Self::Config | Self::LiveControl | Self::LogRequest => {
                Direction::Down
            }
        }
    }

    /// The last topic level.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Telemetry => "telemetry",
            Self::TelemetryAck => "telemetry-ack",
            Self::Config => "config",
            Self::ConfigReport => "config-report",
            Self::Status => "status",
            Self::Heartbeat => "heartbeat",
            Self::Live => "live",
            Self::LiveControl => "live-control",
            Self::Log => "log",
            Self::LogRequest => "log-request",
            Self::LogDump => "log-dump",
        }
    }

    /// The topic of this channel for one gateway, for example
    /// `trustiot/v1/gw/gw-0001/up/telemetry`.
    pub fn topic(self, gateway: &GatewayId) -> String {
        format!(
            "{TOPIC_PREFIX}/{gateway}/{}/{}",
            self.direction().as_str(),
            self.name()
        )
    }

    /// The filter that matches this channel for every gateway, for example
    /// `trustiot/v1/gw/+/up/telemetry`.
    pub fn filter(self) -> String {
        format!(
            "{TOPIC_PREFIX}/+/{}/{}",
            self.direction().as_str(),
            self.name()
        )
    }

    /// [`Self::filter`] as an MQTT 5 shared subscription, so that each message goes to one
    /// replica of a service: `$share/{group}/trustiot/v1/gw/+/up/telemetry`.
    pub fn shared_filter(self, group: &str) -> Result<String, TopicError> {
        if group.is_empty()
            || group.len() > 64
            || !group
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(TopicError::InvalidShareGroup);
        }
        Ok(format!("$share/{group}/{}", self.filter()))
    }

    /// How the publisher sends this channel's messages.
    pub const fn delivery(self) -> Delivery {
        const fn delivery(qos: QoS, retain: bool, expiry_secs: Option<u64>) -> Delivery {
            Delivery {
                qos,
                retain,
                message_expiry: match expiry_secs {
                    Some(secs) => Some(Duration::from_secs(secs)),
                    None => None,
                },
            }
        }
        match self {
            Self::Telemetry | Self::ConfigReport | Self::Log | Self::LogDump => {
                delivery(QoS::AtLeastOnce, false, None)
            }
            // An ack is useless once the gateway has resent the batch.
            Self::TelemetryAck => delivery(QoS::AtLeastOnce, false, Some(600)),
            Self::Config | Self::Status => delivery(QoS::AtLeastOnce, true, None),
            Self::Heartbeat | Self::Live => delivery(QoS::AtMostOnce, false, None),
            // A late lease start would stream values nobody watches.
            Self::LiveControl => delivery(QoS::AtLeastOnce, false, Some(30)),
            Self::LogRequest => delivery(QoS::AtLeastOnce, false, Some(300)),
        }
    }

    /// The largest payload a sender may publish and a receiver must accept on this channel.
    pub const fn max_payload_bytes(self) -> usize {
        match self {
            Self::Config | Self::Heartbeat => MAX_LARGE_BYTES,
            Self::Telemetry | Self::Log | Self::LogDump | Self::ConfigReport => MAX_BULK_BYTES,
            Self::Live | Self::LiveControl => MAX_MEDIUM_BYTES,
            Self::Status | Self::TelemetryAck | Self::LogRequest => MAX_CONTROL_BYTES,
        }
    }

    fn from_levels(direction: &str, name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.direction().as_str() == direction && channel.name() == name)
    }
}

/// The topics a gateway subscribes to: one per `down` channel. Subscribing to each channel
/// instead of `down/#` keeps messages on channels a newer cloud adds away from an older agent.
pub fn gateway_subscriptions(gateway: &GatewayId) -> Vec<String> {
    Channel::ALL
        .into_iter()
        .filter(|channel| channel.direction() == Direction::Down)
        .map(|channel| channel.topic(gateway))
        .collect()
}

/// A topic this protocol defines, split into the gateway and the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedTopic {
    /// The gateway the topic belongs to. For an `up` topic the broker ACL guarantees that the
    /// publisher is this gateway.
    pub gateway: GatewayId,
    /// The channel.
    pub channel: Channel,
}

/// Why a topic is not one of this protocol's topics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicError {
    /// The topic is not under `trustiot/{version}/gw/`.
    UnknownPrefix,
    /// The topic is under another major version, such as `trustiot/v2/gw/`.
    UnsupportedVersion(String),
    /// The gateway level is not a valid gateway ID.
    InvalidGateway(IdError),
    /// The direction and channel levels name no channel, or the topic has extra levels.
    UnknownChannel,
    /// A shared subscription group must be 1–64 ASCII letters, digits, `-` or `_`.
    InvalidShareGroup,
}

impl fmt::Display for TopicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPrefix => write!(
                f,
                "topic is not under {TOPIC_ROOT}/<version>/{GATEWAY_LEVEL}/"
            ),
            Self::UnsupportedVersion(version) => {
                let excerpt: String = version.chars().take(16).collect();
                write!(f, "topic version {excerpt:?} is not {VERSION_LEVEL}")
            }
            Self::InvalidGateway(error) => write!(f, "topic gateway level: {error}"),
            Self::UnknownChannel => f.write_str("topic names no known channel"),
            Self::InvalidShareGroup => f.write_str(
                "shared subscription group must be 1-64 ASCII letters, digits, '-' or '_'",
            ),
        }
    }
}

impl std::error::Error for TopicError {}

/// Splits a received topic into gateway and channel. Rejects anything that is not exactly a
/// topic built by [`Channel::topic`].
pub fn parse_topic(topic: &str) -> Result<ParsedTopic, TopicError> {
    let mut levels = topic.split('/');
    if levels.next() != Some(TOPIC_ROOT) {
        return Err(TopicError::UnknownPrefix);
    }
    let version = levels.next().ok_or(TopicError::UnknownPrefix)?;
    if levels.next() != Some(GATEWAY_LEVEL) {
        return Err(TopicError::UnknownPrefix);
    }
    if version != VERSION_LEVEL {
        return Err(TopicError::UnsupportedVersion(version.to_owned()));
    }
    let gateway = levels.next().ok_or(TopicError::UnknownChannel)?;
    let gateway = GatewayId::new(gateway).map_err(TopicError::InvalidGateway)?;
    let (Some(direction), Some(name), None) = (levels.next(), levels.next(), levels.next()) else {
        return Err(TopicError::UnknownChannel);
    };
    let channel = Channel::from_levels(direction, name).ok_or(TopicError::UnknownChannel)?;
    Ok(ParsedTopic { gateway, channel })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway(id: &str) -> GatewayId {
        GatewayId::new(id).unwrap()
    }

    #[test]
    fn topics_have_six_levels() {
        let gw = gateway("gw-0001");
        assert_eq!(
            Channel::Telemetry.topic(&gw),
            "trustiot/v1/gw/gw-0001/up/telemetry"
        );
        assert_eq!(
            Channel::Config.topic(&gw),
            "trustiot/v1/gw/gw-0001/down/config"
        );
        assert_eq!(Channel::Status.filter(), "trustiot/v1/gw/+/up/status");
        assert_eq!(
            Channel::Telemetry.shared_filter("ingest").unwrap(),
            "$share/ingest/trustiot/v1/gw/+/up/telemetry"
        );
        for group in ["", "a/b", "a+", "#", &"g".repeat(65)] {
            assert_eq!(
                Channel::Telemetry.shared_filter(group),
                Err(TopicError::InvalidShareGroup)
            );
        }
    }

    #[test]
    fn channel_names_are_unique_per_direction() {
        for (i, a) in Channel::ALL.iter().enumerate() {
            for b in &Channel::ALL[i + 1..] {
                assert!(
                    a.direction() != b.direction() || a.name() != b.name(),
                    "{a:?} and {b:?} share a topic"
                );
            }
        }
    }

    #[test]
    fn every_topic_parses_back() {
        let gw = gateway("0190f5c2-7a4b-7cde-8f00-123456789abc");
        for channel in Channel::ALL {
            let parsed = parse_topic(&channel.topic(&gw)).unwrap();
            assert_eq!(
                parsed,
                ParsedTopic {
                    gateway: gw.clone(),
                    channel
                }
            );
        }
    }

    #[test]
    fn foreign_topics_are_rejected() {
        let cases = [
            ("", TopicError::UnknownPrefix),
            ("trustiot", TopicError::UnknownPrefix),
            ("other/v1/gw/a/up/telemetry", TopicError::UnknownPrefix),
            (
                "trustiot/v1/devices/a/up/telemetry",
                TopicError::UnknownPrefix,
            ),
            (
                "trustiot/v2/gw/a/up/telemetry",
                TopicError::UnsupportedVersion("v2".to_owned()),
            ),
            (
                "trustiot/v1/gw/a/up/telemetry/extra",
                TopicError::UnknownChannel,
            ),
            ("trustiot/v1/gw/a/up", TopicError::UnknownChannel),
            (
                "trustiot/v1/gw/a/down/telemetry",
                TopicError::UnknownChannel,
            ),
            ("trustiot/v1/gw/a/up/config", TopicError::UnknownChannel),
            (
                "trustiot/v1/gw/a/sideways/telemetry",
                TopicError::UnknownChannel,
            ),
            ("trustiot/v1/gw", TopicError::UnknownChannel),
        ];
        for (topic, expected) in cases {
            assert_eq!(parse_topic(topic), Err(expected), "{topic:?}");
        }
        for topic in [
            "trustiot/v1/gw/+/up/telemetry",
            "trustiot/v1/gw/#/up/telemetry",
            "trustiot/v1/gw//up/telemetry",
            "trustiot/v1/gw/$SYS/up/telemetry",
        ] {
            assert!(
                matches!(parse_topic(topic), Err(TopicError::InvalidGateway(_))),
                "{topic:?}"
            );
        }
    }

    #[test]
    fn delivery_rules() {
        assert_eq!(
            Channel::Config.delivery(),
            Delivery {
                qos: QoS::AtLeastOnce,
                retain: true,
                message_expiry: None
            }
        );
        assert!(Channel::Status.delivery().retain);
        assert_eq!(Channel::Heartbeat.delivery().qos.level(), 0);
        assert_eq!(Channel::Live.delivery().qos, QoS::AtMostOnce);
        for channel in Channel::ALL {
            let delivery = channel.delivery();
            // Only the configuration and the status are retained.
            assert_eq!(
                delivery.retain,
                matches!(channel, Channel::Config | Channel::Status),
                "{channel:?}"
            );
            // Retained messages never expire: a gateway that connects later must still see them.
            assert!(!delivery.retain || delivery.message_expiry.is_none());
            assert!(channel.max_payload_bytes() + 8 * 1024 <= MIN_MAX_PACKET_BYTES);
        }
    }

    #[test]
    fn gateways_subscribe_to_each_down_channel() {
        let gw = gateway("gw-1");
        assert_eq!(
            gateway_subscriptions(&gw),
            [
                "trustiot/v1/gw/gw-1/down/telemetry-ack",
                "trustiot/v1/gw/gw-1/down/config",
                "trustiot/v1/gw/gw-1/down/live-control",
                "trustiot/v1/gw/gw-1/down/log-request",
            ]
        );
    }
}
