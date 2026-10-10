//! The cloud services that take part in contracts (`docs/architecture/deployment.md`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A cloud service that produces events, receives internal identity tokens, or serves internal
/// APIs. The wire name is the service's directory under `services/`.
///
/// `ai-gateway` is deliberately absent: it must not depend on this crate (decision 28), so no
/// contract names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceName {
    /// `services/identity`: login, members, roles, tenants; signs internal identity tokens.
    Identity,
    /// `services/assets`: device models, devices, gateways, binding, gateway quota.
    Assets,
    /// `services/certs`: CA, factory registration, provisioning, renewal, revocation.
    Certs,
    /// `services/ingest`: MQTT to ClickHouse with end-to-end acknowledgement.
    Ingest,
    /// `services/telemetry-query`: reads ClickHouse with per-tenant read-only accounts.
    TelemetryQuery,
    /// `services/gateway-ops`: configuration push, presence, live charts, OTA rollout.
    GatewayOps,
    /// `services/mqtt-bridge`: WebSocket to TCP bridge in front of the broker.
    MqttBridge,
    /// `services/assistant`: conversations, agent, tools.
    Assistant,
    /// `services/alerts`: alert rules, backtests, notifications.
    Alerts,
    /// `services/field-context`: machines, parts, production records, knowledge graph.
    FieldContext,
    /// `services/knowledge`: RAG and attachments.
    Knowledge,
}

impl ServiceName {
    /// Every service, in the order of the service table in `docs/architecture/deployment.md`.
    pub const ALL: [Self; 11] = [
        Self::Identity,
        Self::Assets,
        Self::Certs,
        Self::Ingest,
        Self::TelemetryQuery,
        Self::GatewayOps,
        Self::MqttBridge,
        Self::Assistant,
        Self::Alerts,
        Self::FieldContext,
        Self::Knowledge,
    ];

    /// The wire name, which is also the directory under `services/`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Assets => "assets",
            Self::Certs => "certs",
            Self::Ingest => "ingest",
            Self::TelemetryQuery => "telemetry-query",
            Self::GatewayOps => "gateway-ops",
            Self::MqttBridge => "mqtt-bridge",
            Self::Assistant => "assistant",
            Self::Alerts => "alerts",
            Self::FieldContext => "field-context",
            Self::Knowledge => "knowledge",
        }
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text is not the wire name of any [`ServiceName`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown service name")]
pub struct UnknownServiceName;

impl FromStr for ServiceName {
    type Err = UnknownServiceName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|service| service.as_str() == name)
            .ok_or(UnknownServiceName)
    }
}

/// Type-level name of a service that serves internal APIs. An
/// [`InternalClient<S>`](crate::client::InternalClient) only accepts endpoints whose
/// [`Endpoint::Service`](crate::client::Endpoint::Service) is `S`, so calling an endpoint on the
/// wrong service does not compile.
pub trait Service: 'static {
    /// The service's name.
    const NAME: ServiceName;
}

macro_rules! service_markers {
    ($($service:ident),+ $(,)?) => {$(
        #[doc = concat!("Type-level [`ServiceName::", stringify!($service), "`]; never constructed.")]
        #[derive(Debug)]
        pub enum $service {}

        impl Service for $service {
            const NAME: ServiceName = ServiceName::$service;
        }
    )+};
}

service_markers!(
    Identity,
    Assets,
    Certs,
    Ingest,
    TelemetryQuery,
    GatewayOps,
    MqttBridge,
    Assistant,
    Alerts,
    FieldContext,
    Knowledge,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_serde_and_parse_back() {
        for service in ServiceName::ALL {
            let json = serde_json::to_string(&service).expect("serialize");
            assert_eq!(json, format!("\"{}\"", service.as_str()));
            assert_eq!(
                serde_json::from_str::<ServiceName>(&json).expect("deserialize"),
                service
            );
            assert_eq!(service.as_str().parse::<ServiceName>(), Ok(service));
            assert_eq!(service.to_string(), service.as_str());
        }
    }

    #[test]
    fn all_lists_each_service_once() {
        let mut names: Vec<&str> = ServiceName::ALL.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ServiceName::ALL.len());
    }

    #[test]
    fn ai_gateway_is_not_a_contract_service() {
        assert_eq!("ai-gateway".parse::<ServiceName>(), Err(UnknownServiceName));
        assert!(serde_json::from_str::<ServiceName>("\"ai-gateway\"").is_err());
    }

    #[test]
    fn markers_name_their_service() {
        assert_eq!(Identity::NAME, ServiceName::Identity);
        assert_eq!(TelemetryQuery::NAME.as_str(), "telemetry-query");
        assert_eq!(Knowledge::NAME, ServiceName::Knowledge);
    }
}
