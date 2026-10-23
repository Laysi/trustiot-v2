//! The bridge's configuration (services/mqtt-bridge/README.md, "設定").

use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};

use crate::client_ip::ClientIpSource;

/// Default `websocket.ping_interval`: Cloudflare cuts a WebSocket that carried nothing in either
/// direction for 125 s, and a ping every 50 s kept an MQTT connection with keepalive 0 open
/// (docs/feasibility/2026-10-10-edge-poc.md).
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(50);
/// The longest `websocket.ping_interval` accepted: below Cloudflare's 125 s idle cut-off, with
/// room for one late ping.
pub const MAX_PING_INTERVAL: Duration = Duration::from_secs(100);
/// The shortest `websocket.ping_interval` accepted: tests use short intervals, nothing needs
/// shorter ones.
pub const MIN_PING_INTERVAL: Duration = Duration::from_millis(100);
/// Default `websocket.pong_timeout`: a gateway that answers nothing for this long after a ping is
/// gone (a half-open connection), and its slot is freed. Cellular round trips of a few seconds
/// fit many times over.
pub const DEFAULT_PONG_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `websocket.pong_timeout` accepted.
pub const MAX_PONG_TIMEOUT: Duration = Duration::from_secs(100);
/// The shortest `websocket.pong_timeout` accepted: tests use short ones.
pub const MIN_PONG_TIMEOUT: Duration = Duration::from_millis(100);
/// Default `websocket.close_timeout`.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest `websocket.close_timeout` accepted. The whole close must fit in the chart's
/// `drainSeconds` (30 s by default) together with the kit's drain.
pub const MAX_CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Default `broker.handshake_timeout`: how long the inner connection may take to make progress
/// (README.md, "連線上限"). Mosquitto 2.1.2 itself ends a connection that has not completed MQTT
/// CONNECT 30 s after it opened, whatever bytes trickle in (measured 2026-10-23,
/// docs/test-reports/2026-10-23-600-mqtt-bridge-exposure-limits.md); the bridge frees the slot
/// sooner and does not depend on the broker for it. A gateway's TLS 1.3 handshake takes two round
/// trips and a TPM signature, a few seconds on a slow cellular link.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest `broker.handshake_timeout` accepted.
pub const MAX_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// The shortest `broker.handshake_timeout` accepted: tests use short ones.
pub const MIN_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(100);
/// Default `broker.connect_timeout`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest `broker.connect_timeout` accepted.
pub const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Default `max_connections`: at the worst case of 128 KiB per connection
/// (`relay::WORST_CASE_PER_CONNECTION`), 1,536 of them take three quarters of the chart's default
/// 256 MiB memory limit (README.md, "記憶體"; `crate::memory` refuses a cap that does not fit).
pub const DEFAULT_MAX_CONNECTIONS: usize = 1536;
/// The largest `max_connections` accepted.
pub const MAX_MAX_CONNECTIONS: usize = 65_536;
/// Default `max_connections_per_ip`: room for a site whose gateways share one public address
/// (NAT) behind each bridge replica, while a single source can take at most 1/48 of the default
/// `max_connections` (README.md, "連線上限").
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 32;

/// The bridge's settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment.
    pub platform: Platform,
    /// The MQTT broker's gateway port. Defaulted as a table so that a missing address is reported
    /// by its own variable name (see [`Validate`]).
    #[serde(default)]
    pub broker: BrokerSettings,
    /// The WebSocket side.
    #[serde(default)]
    pub websocket: WebSocketSettings,
    /// `TRUSTIOT__MAX_CONNECTIONS`: relayed connections at once; an upgrade beyond it is refused
    /// with 503. Default [`DEFAULT_MAX_CONNECTIONS`].
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// `TRUSTIOT__MAX_CONNECTIONS_PER_IP`: relayed connections at once from one source (an IPv4
    /// address or an IPv6 /64, by the trusted client IP); an upgrade beyond it is refused with
    /// 429. From 1 to [`MAX_MAX_CONNECTIONS`]; a value above `max_connections` leaves only that cap
    /// ([`Config::connections_per_ip`]). Default [`DEFAULT_MAX_CONNECTIONS_PER_IP`].
    #[serde(default = "default_max_connections_per_ip")]
    pub max_connections_per_ip: usize,
}

/// The platform settings the bridge reads. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Deserialize)]
pub struct Platform {
    /// `TRUSTIOT_CLIENT_IP_SOURCE`, required: which header carries the gateway's source IP
    /// (the chart derives it from `ingress.exposure`). Parsed by [`Config::client_ip_source`];
    /// a value the bridge does not know stops it from starting.
    pub client_ip_source: String,
}

/// `broker.*`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerSettings {
    /// `TRUSTIOT__BROKER__ADDRESS`, required: `host:port` of the broker's gateway port. The chart
    /// sets it to the broker's Service while `mqttBroker.enabled`.
    #[serde(default)]
    pub address: String,
    /// `TRUSTIOT__BROKER__CONNECT_TIMEOUT`: how long opening the TCP connection may take before the
    /// upgrade is refused with 502. Default [`DEFAULT_CONNECT_TIMEOUT`].
    #[serde(
        default = "default_connect_timeout",
        deserialize_with = "service_kit::config::duration"
    )]
    pub connect_timeout: Duration,
    /// `TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT`: how long, from the start of the relay, the inner
    /// connection has to make progress (the broker sent data, the gateway answered it, and the
    /// broker sent data again) before the bridge ends it. Default [`DEFAULT_HANDSHAKE_TIMEOUT`].
    #[serde(
        default = "default_handshake_timeout",
        deserialize_with = "service_kit::config::duration"
    )]
    pub handshake_timeout: Duration,
}

/// `websocket.*`.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSocketSettings {
    /// `TRUSTIOT__WEBSOCKET__PING_INTERVAL`: a WebSocket ping to the gateway this often. Default
    /// [`DEFAULT_PING_INTERVAL`].
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub ping_interval: Duration,
    /// `TRUSTIOT__WEBSOCKET__PONG_TIMEOUT`: after a ping, how long the gateway has to send
    /// anything (the pong, data, a ping) before the bridge ends the connection. Default
    /// [`DEFAULT_PONG_TIMEOUT`].
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub pong_timeout: Duration,
    /// `TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT`: after the bridge sends a close (SIGTERM, the broker
    /// ended the connection), how long it waits for the gateway's close before it drops the
    /// connection. Default [`DEFAULT_CLOSE_TIMEOUT`].
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub close_timeout: Duration,
}

impl Default for WebSocketSettings {
    fn default() -> Self {
        Self {
            ping_interval: DEFAULT_PING_INTERVAL,
            pong_timeout: DEFAULT_PONG_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
        }
    }
}

impl Default for BrokerSettings {
    fn default() -> Self {
        Self {
            address: String::new(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

fn default_connect_timeout() -> Duration {
    DEFAULT_CONNECT_TIMEOUT
}

fn default_handshake_timeout() -> Duration {
    DEFAULT_HANDSHAKE_TIMEOUT
}

fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}

fn default_max_connections_per_ip() -> usize {
    DEFAULT_MAX_CONNECTIONS_PER_IP
}

impl Config {
    /// The trusted source of the gateway's IP. Only call after [`Validate::validate`] passed,
    /// which `service_kit::run` does before the service starts.
    ///
    /// # Panics
    ///
    /// When `platform.client_ip_source` did not pass validation.
    pub fn client_ip_source(&self) -> ClientIpSource {
        ClientIpSource::parse(&self.platform.client_ip_source)
            .expect("platform.client_ip_source is validated at startup")
    }

    /// The cap per source in effect: `max_connections_per_ip`, but never above
    /// `max_connections`, so lowering the total alone (a small deployment) keeps the default
    /// valid.
    pub fn connections_per_ip(&self) -> usize {
        self.max_connections_per_ip.min(self.max_connections)
    }

    /// The broker's host and port. Only call after [`Validate::validate`] passed.
    ///
    /// # Panics
    ///
    /// When `broker.address` did not pass validation.
    pub fn broker_address(&self) -> (String, u16) {
        parse_address(&self.broker.address).expect("broker.address is validated at startup")
    }
}

/// `host:port` with a non-empty host (a DNS name, an IPv4 address or a bracketed IPv6 address)
/// and a port from 1 to 65535.
fn parse_address(address: &str) -> Result<(String, u16), &'static str> {
    let (host, port) = address
        .rsplit_once(':')
        .ok_or("must be host:port, for example trustiot-mqtt-broker:8883")?;
    // Digits only: `u16::from_str` also takes a leading `+`.
    let port = match port.parse::<u16>() {
        Ok(number) if number != 0 && port.bytes().all(|byte| byte.is_ascii_digit()) => number,
        _ => return Err("must end in a TCP port from 1 to 65535"),
    };
    let host = match host.strip_prefix('[') {
        Some(rest) => rest
            .strip_suffix(']')
            .ok_or("has an IPv6 address without its closing bracket")?,
        None if host.contains(':') => return Err("must put an IPv6 address in brackets"),
        None => host,
    };
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':');
    if host.is_empty() || !host.bytes().all(allowed) {
        return Err("must name a host: letters, digits, '-', '.', or an IP address");
    }
    Ok((host.to_owned(), port))
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        ClientIpSource::parse(&self.platform.client_ip_source)
            .map_err(|message| Invalid::new("platform.client_ip_source", message))?;
        if self.broker.address.is_empty() {
            return Err(Invalid::new(
                "broker.address",
                "must be set (environment variable TRUSTIOT__BROKER__ADDRESS): the broker's gateway \
                 port, which the chart sets while mqttBroker.enabled",
            ));
        }
        parse_address(&self.broker.address)
            .map_err(|message| Invalid::new("broker.address", message))?;
        within(
            "broker.connect_timeout",
            self.broker.connect_timeout,
            Duration::from_millis(1),
            MAX_CONNECT_TIMEOUT,
        )?;
        within(
            "broker.handshake_timeout",
            self.broker.handshake_timeout,
            MIN_HANDSHAKE_TIMEOUT,
            MAX_HANDSHAKE_TIMEOUT,
        )?;
        within(
            "websocket.ping_interval",
            self.websocket.ping_interval,
            MIN_PING_INTERVAL,
            MAX_PING_INTERVAL,
        )?;
        within(
            "websocket.pong_timeout",
            self.websocket.pong_timeout,
            MIN_PONG_TIMEOUT,
            MAX_PONG_TIMEOUT,
        )?;
        within(
            "websocket.close_timeout",
            self.websocket.close_timeout,
            Duration::from_millis(1),
            MAX_CLOSE_TIMEOUT,
        )?;
        if self.max_connections == 0 || self.max_connections > MAX_MAX_CONNECTIONS {
            return Err(Invalid::new(
                "max_connections",
                format!("must be from 1 to {MAX_MAX_CONNECTIONS}"),
            ));
        }
        if self.max_connections_per_ip == 0 || self.max_connections_per_ip > MAX_MAX_CONNECTIONS {
            return Err(Invalid::new(
                "max_connections_per_ip",
                format!("must be from 1 to {MAX_MAX_CONNECTIONS}"),
            ));
        }
        Ok(())
    }
}

fn within(key: &str, value: Duration, min: Duration, max: Duration) -> Result<(), Invalid> {
    if value < min || value > max {
        return Err(Invalid::new(
            key,
            format!("must be from {} to {}", humanize(min), humanize(max)),
        ));
    }
    Ok(())
}

fn humanize(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use service_kit::config::{ConfigError, load_from};

    use super::*;

    const REQUIRED: [(&str, &str); 2] = [
        ("TRUSTIOT_CLIENT_IP_SOURCE", "cf-connecting-ip"),
        ("TRUSTIOT__BROKER__ADDRESS", "trustiot-mqtt-broker:8883"),
    ];

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars = vars
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        load_from::<Config, _>("TRUSTIOT", vars).map(|settings| settings.app)
    }

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut vars = REQUIRED.to_vec();
        vars.extend_from_slice(extra);
        vars
    }

    #[test]
    fn defaults_follow_the_feasibility_test() {
        let config = load(&REQUIRED).expect("defaults");
        assert_eq!(config.websocket.ping_interval, Duration::from_secs(50));
        assert_eq!(config.websocket.close_timeout, Duration::from_secs(5));
        assert_eq!(config.websocket.pong_timeout, Duration::from_secs(30));
        assert_eq!(config.broker.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.broker.handshake_timeout, Duration::from_secs(10));
        assert_eq!(config.max_connections, 1536);
        assert_eq!(config.max_connections_per_ip, 32);
        assert_eq!(config.client_ip_source(), ClientIpSource::CfConnectingIp);
        assert_eq!(
            config.broker_address(),
            ("trustiot-mqtt-broker".to_owned(), 8883)
        );
    }

    #[test]
    fn the_client_ip_source_and_the_broker_are_required() {
        let error = load(&[REQUIRED[1]]).expect_err("no client IP source");
        assert!(
            error.to_string().contains("TRUSTIOT_CLIENT_IP_SOURCE"),
            "{error}"
        );
        let error = load(&[REQUIRED[0]]).expect_err("no broker");
        assert!(
            error.to_string().contains("TRUSTIOT__BROKER__ADDRESS"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_client_ip_source_is_refused_without_echoing_it() {
        for value in ["x-forwarded-for", "true-client-ip", "CF-Connecting-IP", ""] {
            let vars = [("TRUSTIOT_CLIENT_IP_SOURCE", value), REQUIRED[1]];
            let Err(error) = load(&vars) else {
                panic!("{value:?} was accepted");
            };
            let text = error.to_string();
            assert!(
                text.contains("TRUSTIOT_CLIENT_IP_SOURCE"),
                "{value:?}: {text}"
            );
            if !value.is_empty() {
                assert!(!text.contains(value), "{value:?} echoed: {text}");
            }
        }
    }

    #[test]
    fn the_on_prem_source_is_refused_until_it_is_built() {
        let vars = [
            ("TRUSTIOT_CLIENT_IP_SOURCE", "x-forwarded-for-rightmost"),
            REQUIRED[1],
        ];
        let error = load(&vars).expect_err("on-prem mode");
        let text = error.to_string();
        assert!(text.contains("TRUSTIOT_CLIENT_IP_SOURCE"), "{text}");
        assert!(text.contains("Laysi/trustiot-v2#446"), "{text}");
    }

    #[test]
    fn broker_addresses_need_a_host_and_a_port() {
        for good in ["mqtt:8883", "10.0.0.1:1", "[::1]:8883", "a.b-c.svc:65535"] {
            let mut vars = with(&[]);
            vars[1] = ("TRUSTIOT__BROKER__ADDRESS", good);
            load(&vars).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in [
            "mqtt",
            "mqtt:",
            ":8883",
            "mqtt:0",
            "mqtt:65536",
            "mqtt:+1",
            "::1:8883",
            "[::1:8883",
            "a b:1",
            "tcp://mqtt:1",
        ] {
            let mut vars = with(&[]);
            vars[1] = ("TRUSTIOT__BROKER__ADDRESS", bad);
            let Err(error) = load(&vars) else {
                panic!("{bad:?} was accepted");
            };
            assert!(
                error.to_string().contains("TRUSTIOT__BROKER__ADDRESS"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn time_limits_are_bounded() {
        for (name, bad) in [
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "0s"),
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "50ms"),
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "125s"),
            ("TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT", "0s"),
            ("TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT", "1m"),
            ("TRUSTIOT__BROKER__CONNECT_TIMEOUT", "0s"),
            ("TRUSTIOT__BROKER__CONNECT_TIMEOUT", "500000000000y"),
            ("TRUSTIOT__MAX_CONNECTIONS", "0"),
            ("TRUSTIOT__MAX_CONNECTIONS", "65537"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "0s"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "99ms"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "101s"),
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "0s"),
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "99ms"),
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "61s"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "0"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "65537"),
        ] {
            let Err(error) = load(&with(&[(name, bad)])) else {
                panic!("{name}={bad} was accepted");
            };
            assert!(error.to_string().contains(name), "{name}={bad}: {error}");
        }
        let config = load(&with(&[
            ("TRUSTIOT__WEBSOCKET__PING_INTERVAL", "100ms"),
            ("TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT", "20s"),
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "100ms"),
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "100ms"),
            ("TRUSTIOT__MAX_CONNECTIONS", "1"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "1"),
        ]))
        .expect("lower bounds");
        assert_eq!(config.websocket.ping_interval, Duration::from_millis(100));
        assert_eq!(config.websocket.pong_timeout, Duration::from_millis(100));
        assert_eq!(config.broker.handshake_timeout, Duration::from_millis(100));
        assert_eq!(config.max_connections, 1);
        assert_eq!(config.max_connections_per_ip, 1);
        let config = load(&with(&[
            ("TRUSTIOT__WEBSOCKET__PONG_TIMEOUT", "100s"),
            ("TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT", "60s"),
            ("TRUSTIOT__MAX_CONNECTIONS", "64"),
            ("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "64"),
        ]))
        .expect("upper bounds");
        assert_eq!(config.websocket.pong_timeout, Duration::from_secs(100));
        assert_eq!(config.broker.handshake_timeout, Duration::from_secs(60));
        assert_eq!(config.max_connections_per_ip, 64);
        let config = load(&with(&[("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "65536")])).expect("max");
        assert_eq!(config.max_connections_per_ip, 65_536);
    }

    #[test]
    fn the_cap_per_source_never_exceeds_the_total() {
        // A small deployment lowers the total alone; the default per source still starts.
        let config = load(&with(&[("TRUSTIOT__MAX_CONNECTIONS", "8")])).expect("total only");
        assert_eq!(config.max_connections_per_ip, 32);
        assert_eq!(config.connections_per_ip(), 8);
        let config = load(&with(&[("TRUSTIOT__MAX_CONNECTIONS_PER_IP", "4")])).expect("per source");
        assert_eq!(config.connections_per_ip(), 4);
    }

    #[test]
    fn unknown_keys_are_refused() {
        let error = load(&with(&[("TRUSTIOT__WEBSOCKET__PING", "1s")])).expect_err("unknown key");
        assert!(error.to_string().contains("ping"), "{error}");
    }
}
