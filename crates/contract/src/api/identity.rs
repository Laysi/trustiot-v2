//! Internal APIs of the identity service (`services/identity`).

use serde::{Deserialize, Deserializer, Serialize};

use crate::client::{Endpoint, Method};
use crate::identity::InternalToken;
use crate::service::{self, ServiceName};

/// Exchanges the internal identity token a service received for one addressed to `audience`, so
/// that the service can call `audience` on behalf of the same user (decision 27,
/// Laysi/trustiot-v2#106). Services use it through `service_kit::identity::OnBehalf`.
///
/// The request carries the caller's own token, the one addressed to the caller, in
/// `x-internal-identity` as every internal call does. The identity service verifies it like any
/// service verifies a token, except that the audience may be any service, and answers with a
/// token for the same user, tenant, role and platform administrator flag, addressed to
/// `audience`, issued now and expiring when the caller's token expires (never later, and at most
/// 60 seconds from now), so exchanging cannot extend a request's identity. It does so only when
/// the deployment lets the caller's token's audience call `audience` on behalf of users (the
/// chart's `mesh.calls`, given to the identity service as `TRUSTIOT_ON_BEHALF_CALLS`).
///
/// | Outcome | Status | M1 code |
/// | --- | --- | --- |
/// | Exchanged | 200 | [`ExchangedToken`] |
/// | The caller's token is missing, invalid or expired (or expires within a second) | 401 | `unauthenticated` |
/// | The token's audience may not call `audience` on behalf of users | 403 | `permission_denied` |
/// | The body is not this request, or names an unknown service | 400 | `invalid_request` |
/// | No public keys or no signing key yet | 503 | `unavailable` |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeToken {
    /// The service the new token is for.
    pub audience: ServiceName,
}

impl Endpoint for ExchangeToken {
    type Service = service::Identity;
    type Body = Self;
    type Response = ExchangedToken;
    // A POST that reads: the audience travels in the body, and nothing is stored.
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/token-exchange";

    fn body(&self) -> Option<&Self> {
        Some(self)
    }
}

/// The answer to [`ExchangeToken`]. It can only be deserialized: like [`InternalToken`], it has
/// no `Serialize`, so the token cannot be written into a log or a stored document by accident.
/// The identity service writes the JSON `{"token": "<token>"}` itself.
#[derive(Debug, Deserialize)]
pub struct ExchangedToken {
    /// The token for the requested audience.
    #[serde(deserialize_with = "token")]
    pub token: InternalToken,
}

fn token<'de, D: Deserializer<'de>>(deserializer: D) -> Result<InternalToken, D::Error> {
    let text = String::deserialize(deserializer)?;
    InternalToken::new(text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::client::check_route;

    #[test]
    fn the_route_follows_the_rules_and_has_no_path_parameters() {
        let exchange = ExchangeToken {
            audience: ServiceName::Certs,
        };
        assert_eq!(check_route(ExchangeToken::ROUTE), Ok(0));
        assert!(exchange.path_params().is_empty());
        assert_eq!(exchange.body(), Some(&exchange));
    }

    #[test]
    fn the_request_names_the_audience_by_its_wire_name() {
        let exchange = ExchangeToken {
            audience: ServiceName::TelemetryQuery,
        };
        let body = serde_json::to_value(exchange).expect("serialize");
        assert_eq!(body, json!({"audience": "telemetry-query"}));
        assert_eq!(
            serde_json::from_value::<ExchangeToken>(body).expect("deserialize"),
            exchange
        );
        // Callers and the service update separately: unknown members are ignored.
        let newer = json!({"audience": "certs", "reason": "listing devices"});
        assert_eq!(
            serde_json::from_value::<ExchangeToken>(newer).expect("deserialize"),
            ExchangeToken {
                audience: ServiceName::Certs
            }
        );
        assert!(
            serde_json::from_value::<ExchangeToken>(json!({"audience": "ai-gateway"})).is_err()
        );
        assert!(serde_json::from_value::<ExchangeToken>(json!({})).is_err());
    }

    #[test]
    fn the_response_holds_a_redacted_token() {
        let response: ExchangedToken =
            serde_json::from_value(json!({"token": "a.b.c"})).expect("deserialize");
        assert_eq!(response.token.expose(), "a.b.c");
        assert_eq!(
            format!("{response:?}"),
            "ExchangedToken { token: InternalToken(<redacted>) }"
        );
        for invalid in [json!({"token": ""}), json!({"token": "a b"}), json!({})] {
            assert!(
                serde_json::from_value::<ExchangedToken>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }
}
