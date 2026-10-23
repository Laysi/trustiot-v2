//! Internal APIs of the identity service (`services/identity`).

use serde::{Deserialize, Deserializer, Serialize};

use crate::client::{Endpoint, Method};
use crate::id::RunId;
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

/// Longest lifetime of a run delegation, in seconds: 15 minutes, the longest an agent run may
/// act for its user (Laysi/trustiot-v2#551). It stays within the time a retired signing key is
/// kept with the chart's default propagation time (600 s + 305 s), so a key rotation does not
/// end a run early (services/identity/README.md, 「agent 執行期間的委派」).
pub const RUN_DELEGATION_MAX_LIFETIME_SECS: u32 = 900;

/// How far the `iat` of a run delegation's proof may be from the identity service's clock, in
/// seconds, either way. A proof is made for each exchange, so this only absorbs clock skew and
/// the request's own time.
pub const RUN_PROOF_WINDOW_SECS: i64 = 30;

/// What the run's key signs for one [`ExchangeRunDelegation`]: a fixed label, the delegation as
/// sent, the audience and the proof's `iat`, separated by NUL bytes. The delegation is visible
/// ASCII and the audience a fixed name, so no two requests give the same message. Both the
/// caller (`service_kit::identity::RunDelegation`) and the identity service build it here.
#[must_use]
pub fn run_proof_message(delegation: &str, audience: ServiceName, iat: i64) -> Vec<u8> {
    format!(
        "trustiot-run-delegation-proof:v1\0{delegation}\0{}\0{iat}",
        audience.as_str()
    )
    .into_bytes()
}

/// Starts an agent run's delegation (Laysi/trustiot-v2#551): a service that acts for a user for
/// longer than the request's 60-second token lives, such as the assistant's main agent, gets a
/// delegation for the run that it exchanges for a token to each service it calls
/// ([`ExchangeRunDelegation`]). Services use it through `service_kit::identity::RunDelegations`.
///
/// The request carries the token the caller received for the user's request, addressed to the
/// caller, in `x-internal-identity`, as [`ExchangeToken`] does. The identity service issues the
/// delegation only to the services it lets hold one (the assistant), for a token the ingress
/// gateway's request brought (no `act`, with the session it came with), while that session is
/// active and the user is still a member of the token's tenant. The delegation is bound to the
/// user, the session, the tenant, the caller, `run_id` and `key`, and expires `lifetime_secs`
/// from now, at most [`RUN_DELEGATION_MAX_LIFETIME_SECS`].
///
/// | Outcome | Status | M1 code |
/// | --- | --- | --- |
/// | Issued | 200 | [`RunDelegationCreated`] |
/// | The caller's token is missing, invalid or expired, names no session, or the session is no longer active | 401 | `unauthenticated` |
/// | The token came from an exchange (`act`), its audience may not hold a delegation, or the user is no longer a member of its tenant | 403 | `permission_denied` |
/// | The body is not this request, the key is not an Ed25519 public key, or the lifetime is out of range | 400 | `invalid_request` |
/// | No public keys or no signing key yet, or the database failed | 503 | `unavailable` |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRunDelegation {
    /// The run, chosen by the caller; it is logged with every exchange of the delegation.
    pub run_id: RunId,
    /// The run's Ed25519 public key, base64url without padding (32 bytes, 43 characters). Every
    /// exchange is signed with its private key ([`run_proof_message`]), which never leaves the
    /// caller, so the delegation alone exchanges nothing.
    pub key: String,
    /// Seconds the delegation lives, 1 to [`RUN_DELEGATION_MAX_LIFETIME_SECS`].
    pub lifetime_secs: u32,
}

impl Endpoint for CreateRunDelegation {
    type Service = service::Identity;
    type Body = Self;
    type Response = RunDelegationCreated;
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/run-delegations";

    fn body(&self) -> Option<&Self> {
        Some(self)
    }
}

/// The answer to [`CreateRunDelegation`]. Like [`ExchangedToken`], it can only be deserialized,
/// and the delegation is a redacted [`InternalToken`]: it is carried in `x-internal-identity` to
/// [`ExchangeRunDelegation`], and only there.
#[derive(Debug, Deserialize)]
pub struct RunDelegationCreated {
    /// The delegation: opaque to the caller, verified by the identity service only.
    #[serde(deserialize_with = "token")]
    pub delegation: InternalToken,
    /// When it expires, in seconds since the Unix epoch.
    pub expires_at: i64,
}

/// Exchanges a run delegation ([`CreateRunDelegation`]) for a token addressed to `audience`, for
/// one call of the run on behalf of its user (Laysi/trustiot-v2#551).
///
/// The request carries the delegation in `x-internal-identity` and a proof that the caller holds
/// the run's key: `proof` is the Ed25519 signature, base64url without padding, of
/// [`run_proof_message`] for this delegation, `audience` and `iat`, which must be within
/// [`RUN_PROOF_WINDOW_SECS`] of now. Each time, the identity service checks again that the
/// session is active and reads the user's current role in the delegation's tenant and their
/// platform administrator flag, so a logout, a revoked session or a removed member stops the
/// run's next call. The token is the one an on-behalf call gets ([`ExchangeToken`]): the user,
/// the tenant with the current role, the flag, `act` naming the caller, issued now and expiring
/// in 60 seconds or with the delegation, whichever is earlier. It is never exchanged again.
///
/// | Outcome | Status | M1 code |
/// | --- | --- | --- |
/// | Exchanged | 200 | [`ExchangedToken`] |
/// | The delegation is missing, invalid, expired or longer-lived than the cap; the proof is missing, invalid or outside the window; the session is no longer active | 401 | `unauthenticated` |
/// | The caller may not call `audience` on behalf of users, or the user is no longer a member of the delegation's tenant | 403 | `permission_denied` |
/// | The body is not this request, or names an unknown service | 400 | `invalid_request` |
/// | No signing key yet, or the database failed | 503 | `unavailable` |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeRunDelegation {
    /// The service the new token is for.
    pub audience: ServiceName,
    /// When the proof was made, in seconds since the Unix epoch.
    pub iat: i64,
    /// The signature of [`run_proof_message`] with the run's key, base64url without padding.
    pub proof: String,
}

impl Endpoint for ExchangeRunDelegation {
    type Service = service::Identity;
    type Body = Self;
    type Response = ExchangedToken;
    // A POST that reads: nothing is stored.
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/run-delegations/token";

    fn body(&self) -> Option<&Self> {
        Some(self)
    }
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

    #[test]
    fn the_run_delegation_routes_follow_the_rules() {
        assert_eq!(check_route(CreateRunDelegation::ROUTE), Ok(0));
        assert_eq!(check_route(ExchangeRunDelegation::ROUTE), Ok(0));
        let create = CreateRunDelegation {
            run_id: RunId::generate(),
            key: "k".to_owned(),
            lifetime_secs: 60,
        };
        assert!(create.path_params().is_empty());
        assert_eq!(create.body(), Some(&create));
        let exchange = ExchangeRunDelegation {
            audience: ServiceName::TelemetryQuery,
            iat: 1,
            proof: "p".to_owned(),
        };
        assert!(exchange.path_params().is_empty());
        assert_eq!(exchange.body(), Some(&exchange));
    }

    #[test]
    fn the_run_delegation_requests_use_wire_names() {
        let run: RunId = "01928f6e-7a3c-7d4e-8f00-0000000000ff"
            .parse()
            .expect("run id");
        let create = CreateRunDelegation {
            run_id: run,
            key: "key".to_owned(),
            lifetime_secs: 900,
        };
        let body = serde_json::to_value(&create).expect("serialize");
        assert_eq!(
            body,
            json!({"run_id": "01928f6e-7a3c-7d4e-8f00-0000000000ff", "key": "key", "lifetime_secs": 900})
        );
        assert_eq!(
            serde_json::from_value::<CreateRunDelegation>(body).expect("deserialize"),
            create
        );
        let exchange = ExchangeRunDelegation {
            audience: ServiceName::TelemetryQuery,
            iat: 1_791_590_400,
            proof: "sig".to_owned(),
        };
        let body = serde_json::to_value(&exchange).expect("serialize");
        assert_eq!(
            body,
            json!({"audience": "telemetry-query", "iat": 1_791_590_400, "proof": "sig"})
        );
        assert_eq!(
            serde_json::from_value::<ExchangeRunDelegation>(body).expect("deserialize"),
            exchange
        );
        // A negative lifetime and an unknown service are not requests.
        assert!(
            serde_json::from_value::<CreateRunDelegation>(
                json!({"run_id": run.to_string(), "key": "k", "lifetime_secs": -1})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ExchangeRunDelegation>(
                json!({"audience": "ai-gateway", "iat": 1, "proof": "p"})
            )
            .is_err()
        );
    }

    #[test]
    fn the_created_delegation_is_redacted() {
        let response: RunDelegationCreated =
            serde_json::from_value(json!({"delegation": "trd1.a.b", "expires_at": 5}))
                .expect("deserialize");
        assert_eq!(response.delegation.expose(), "trd1.a.b");
        assert_eq!(response.expires_at, 5);
        assert!(!format!("{response:?}").contains("trd1"), "{response:?}");
        for invalid in [
            json!({"delegation": "", "expires_at": 5}),
            json!({"delegation": "a b", "expires_at": 5}),
            json!({"delegation": "a"}),
        ] {
            assert!(
                serde_json::from_value::<RunDelegationCreated>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn the_proof_message_binds_the_delegation_the_audience_and_the_time() {
        let message = run_proof_message("trd1.a.b", ServiceName::Assets, 42);
        assert_eq!(
            message,
            b"trustiot-run-delegation-proof:v1\x00trd1.a.b\x00assets\x0042".to_vec()
        );
        assert_ne!(
            message,
            run_proof_message("trd1.a.c", ServiceName::Assets, 42)
        );
        assert_ne!(
            message,
            run_proof_message("trd1.a.b", ServiceName::Certs, 42)
        );
        assert_ne!(
            message,
            run_proof_message("trd1.a.b", ServiceName::Assets, 43)
        );
    }
}
