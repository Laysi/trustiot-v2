//! The bridge's metrics (README.md, "指標"), exported through service-kit's meter provider like
//! every service's: OTLP to the collector while `telemetry.otlp_endpoint` is set, nothing
//! otherwise.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, UpDownCounter};

/// Which way relayed bytes went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the gateway's WebSocket to the broker.
    ToBroker,
    /// From the broker to the gateway's WebSocket.
    ToGateway,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ToBroker => "to_broker",
            Self::ToGateway => "to_gateway",
        }
    }
}

/// The bridge's instruments.
#[derive(Clone, Debug)]
pub struct Metrics {
    active: UpDownCounter<i64>,
    opened: Counter<u64>,
    refused: Counter<u64>,
    ended: Counter<u64>,
    traffic: Counter<u64>,
}

impl Metrics {
    /// Creates the instruments. Call it after the kit started telemetry (in `start`), or they are
    /// not exported.
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("mqtt-bridge");
        Self {
            active: meter
                .i64_up_down_counter("mqtt_bridge.connections.active")
                .with_unit("{connection}")
                .with_description("Gateway connections being relayed now")
                .build(),
            opened: meter
                .u64_counter("mqtt_bridge.connections.opened")
                .with_unit("{connection}")
                .with_description("Gateway connections whose relay started")
                .build(),
            refused: meter
                .u64_counter("mqtt_bridge.connections.refused")
                .with_unit("{connection}")
                .with_description("WebSocket upgrades refused, by reason")
                .build(),
            ended: meter
                .u64_counter("mqtt_bridge.connections.ended")
                .with_unit("{connection}")
                .with_description("Relayed gateway connections that ended, by reason")
                .build(),
            traffic: meter
                .u64_counter("mqtt_bridge.traffic")
                .with_unit("By")
                .with_description("Bytes relayed (WebSocket payload), by direction")
                .build(),
        }
    }

    pub(crate) fn opened(&self) {
        self.opened.add(1, &[]);
        self.active.add(1, &[]);
    }

    pub(crate) fn ended(&self, reason: &'static str) {
        self.active.add(-1, &[]);
        self.ended.add(1, &[KeyValue::new("reason", reason)]);
    }

    pub(crate) fn refused(&self, reason: &'static str) {
        self.refused.add(1, &[KeyValue::new("reason", reason)]);
    }

    pub(crate) fn relayed(&self, direction: Direction, bytes: usize) {
        self.traffic.add(
            u64::try_from(bytes).unwrap_or(u64::MAX),
            &[KeyValue::new("direction", direction.as_str())],
        );
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
