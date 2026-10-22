//! The token exchange (decision 27, Laysi/trustiot-v2#106): a service serving a user's request
//! that calls another service on the user's behalf exchanges the request's token, addressed to
//! itself, for one addressed to the callee ([`ExchangeToken`], through
//! `service_kit::identity::OnBehalf`).
//!
//! | Request | Response |
//! | --- | --- |
//! | The caller's token is valid for its own audience, was signed for the ingress gateway's request (no `act` claim), and the call list lets that audience call the requested one | 200 `{"token": "<token>"}`: same user, tenant, role and administrator flag, addressed to the requested service, with `act` naming the caller, issued now, expiring with the caller's token (at most [`TOKEN_LIFETIME_SECS`] from now) |
//! | No, repeated or invalid token, or one that expires within a second | 401 `unauthenticated` |
//! | The caller's token came from this exchange (it has `act`), or the call list does not let the token's audience call the requested service | 403 `permission_denied` |
//! | The body is not `{"audience": "<service>"}` with a known service, or exceeds [`MAX_BODY_BYTES`] | 400 `invalid_request` |
//! | No public keys yet, or no key may sign now | 503 `unavailable` |
//!
//! Every response carries `cache-control: no-store`. The exchange has its own port
//! ([`crate::config::ExchangeListen`], 8082 by default), so the mesh can let the services that
//! call on behalf of users reach it and nothing else of this service (the chart gives the port its
//! own Service, AuthorizationPolicy and NetworkPolicy, `<release>-identity-exchange`, and keeps
//! everyone else on the API port).
//!
//! The call list is the deployment's: the chart renders `mesh.calls` between services as the
//! platform setting [`CALLS_SETTING`] (`TRUSTIOT_ON_BEHALF_CALLS`), the same list that sets the
//! AuthorizationPolicies. This service cannot tell which workload sent a request, since ztunnel
//! authorizes connections at L4 and passes no caller identity to the application, so the exchange
//! is bound to the token instead: a token the ingress gateway's request brought to `assets` is
//! exchanged only for the services `assets` may call. An exchanged token carries `act` and is
//! never exchanged again, so the list is not applied transitively: a service holding a token
//! exchanged for `assets` cannot use the calls of `assets`. The cost is that a call on behalf of
//! a user goes one service further at most; longer chains need the exchange to authenticate the
//! calling workload (Laysi/trustiot-v2#509).
//!
//! The new token never outlives the caller's, and nothing is stored.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::identity::ExchangeToken;
use contract::client::Endpoint;
use contract::{Actor, ErrorBody, ErrorCode, InternalClaims, InternalToken, ServiceName};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};
use service_kit::identity::{ExchangeVerifier, IdentityError};

use crate::issuer::{Issuer, TOKEN_LIFETIME_SECS};

/// The platform setting with the call list: `platform.on_behalf_calls`, set by the chart as
/// `TRUSTIOT_ON_BEHALF_CALLS` on the identity service only.
pub const CALLS_SETTING: &str = "on_behalf_calls";

/// Largest request body accepted; `{"audience": "telemetry-query"}` is 30 bytes.
pub const MAX_BODY_BYTES: usize = 1024;

/// Which service may call which on behalf of users, checked once at startup. Cloning is cheap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OnBehalfCalls {
    calls: Arc<BTreeSet<(ServiceName, ServiceName)>>,
}

impl OnBehalfCalls {
    /// Parses the JSON object the chart renders, `{"<caller>": ["<callee>", ...]}`, for example
    /// `{"assets": ["certs"]}`. Every name must be a service that receives internal identity
    /// tokens (`ai-gateway` is none, decision 28). An empty object is valid: then nothing is
    /// exchanged.
    ///
    /// # Errors
    ///
    /// When the text is not such an object; the error names the problem, never the text.
    pub fn from_json(text: &str) -> Result<Self, InvalidCalls> {
        let raw: BTreeMap<String, Vec<String>> =
            serde_json::from_str(text).map_err(|error| InvalidCalls::NotCalls(error.into()))?;
        let mut calls = BTreeSet::new();
        for (caller, callees) in raw {
            let caller = ServiceName::from_str(&caller).map_err(|_| InvalidCalls::Service)?;
            for callee in callees {
                let callee = ServiceName::from_str(&callee).map_err(|_| InvalidCalls::Service)?;
                calls.insert((caller, callee));
            }
        }
        Ok(Self {
            calls: Arc::new(calls),
        })
    }

    /// Whether `caller` may call `callee` on behalf of users.
    #[must_use]
    pub fn allows(&self, caller: ServiceName, callee: ServiceName) -> bool {
        self.calls.contains(&(caller, callee))
    }

    /// How many caller and callee pairs there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.calls.len()
    }

    /// Whether no service may call another on behalf of users.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

impl FromStr for OnBehalfCalls {
    type Err = InvalidCalls;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_json(text)
    }
}

impl<'de> Deserialize<'de> for OnBehalfCalls {
    /// From the JSON text, so it can be a configuration value.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CallsText;

        impl Visitor<'_> for CallsText {
            type Value = OnBehalfCalls;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the on-behalf call list as JSON text")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<OnBehalfCalls, E> {
                OnBehalfCalls::from_json(text).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(CallsText)
    }
}

/// Why the call list cannot be used. Messages never quote the setting.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCalls {
    /// Not a JSON object of lists of strings.
    #[error("not a JSON object of caller to callee lists ({0})")]
    NotCalls(contract::json::JsonError),
    /// A name is not a service that receives internal identity tokens.
    #[error("names a service that cannot receive internal identity tokens")]
    Service,
}

/// The exchange's state: the verifier of the caller's token, the signer, and the call list.
pub struct Exchange {
    verifier: ExchangeVerifier,
    issuer: Issuer,
    calls: OnBehalfCalls,
    clock: fn() -> i64,
}

impl Exchange {
    /// An exchange at the current time.
    #[must_use]
    pub fn new(verifier: ExchangeVerifier, issuer: Issuer, calls: OnBehalfCalls) -> Self {
        Self::with_clock(verifier, issuer, calls, crate::issuer::unix_now)
    }

    /// An exchange with another clock (seconds since the Unix epoch), for tests.
    #[must_use]
    pub fn with_clock(
        verifier: ExchangeVerifier,
        issuer: Issuer,
        calls: OnBehalfCalls,
        clock: fn() -> i64,
    ) -> Self {
        Self {
            verifier,
            issuer,
            calls,
            clock,
        }
    }

    /// Decides one exchange request.
    fn decide(&self, headers: &HeaderMap, body: Result<Bytes, BytesRejection>) -> Outcome {
        let now = (self.clock)();
        let subject = match self.verifier.verify_headers(headers, now) {
            Ok(subject) => subject,
            Err(IdentityError::NoKeys) => return Outcome::Unavailable("no_keys"),
            Err(error) => return Outcome::Unauthenticated(error.reason()),
        };
        let Some(request) = body
            .ok()
            .and_then(|body| serde_json::from_slice::<ExchangeToken>(&body).ok())
        else {
            return Outcome::InvalidRequest;
        };
        // One hop only: a token that came from this exchange is the callee's, not a request's,
        // and exchanging it again would apply the call list transitively.
        if let Some(actor) = subject.act {
            return Outcome::AlreadyExchanged {
                caller: subject.aud,
                actor: actor.sub,
                audience: request.audience,
            };
        }
        if !self.calls.allows(subject.aud, request.audience) {
            return Outcome::NotListed {
                caller: subject.aud,
                audience: request.audience,
            };
        }
        let Some(claims) = exchanged_claims(&subject, request.audience, now) else {
            return Outcome::Unauthenticated("expires_too_soon");
        };
        match self.issuer.sign_claims(&claims, now) {
            Ok(token) => Outcome::Exchanged {
                token,
                caller: subject.aud,
                audience: request.audience,
            },
            Err(error) => {
                tracing::error!(error = %error, "could not sign an exchanged internal identity");
                Outcome::Unavailable("no_signing_key")
            }
        }
    }
}

/// The claims of the token `subject` is exchanged for: the same user, tenant and administrator
/// flag, addressed to `audience`, acted for by `subject`'s audience (`act`), issued at `now`, and
/// expiring when `subject` does, but at most [`TOKEN_LIFETIME_SECS`] from now. `None` when that
/// leaves less than a second.
#[must_use]
pub fn exchanged_claims(
    subject: &InternalClaims,
    audience: ServiceName,
    now: i64,
) -> Option<InternalClaims> {
    let exp = subject.exp.min(now.saturating_add(TOKEN_LIFETIME_SECS));
    if exp.checked_sub(now)? < 1 {
        return None;
    }
    Some(InternalClaims {
        iss: ServiceName::Identity,
        sub: subject.sub,
        aud: audience,
        iat: now,
        exp,
        tenant: subject.tenant,
        platform_admin: subject.platform_admin,
        act: Some(Actor { sub: subject.aud }),
    })
}

/// The exchange's route, [`ExchangeToken::ROUTE`], for `POST` only. It is under `/internal`,
/// which the ingress gateway never routes.
pub fn routes(exchange: Exchange) -> Router {
    Router::new()
        .route(ExchangeToken::ROUTE, post(handle))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(exchange))
}

async fn handle(
    State(exchange): State<Arc<Exchange>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let outcome = exchange.decide(&headers, body);
    outcome.log();
    outcome.into_response()
}

/// The outcome of one exchange.
enum Outcome {
    /// The only way to a 200: carries the new token.
    Exchanged {
        token: InternalToken,
        caller: ServiceName,
        audience: ServiceName,
    },
    /// 401; the reason goes to the log only.
    Unauthenticated(&'static str),
    /// 403: the caller's token came from this exchange (one hop only).
    AlreadyExchanged {
        caller: ServiceName,
        actor: ServiceName,
        audience: ServiceName,
    },
    /// 403: the call list does not have this pair.
    NotListed {
        caller: ServiceName,
        audience: ServiceName,
    },
    /// 400.
    InvalidRequest,
    /// 503; the reason goes to the log only.
    Unavailable(&'static str),
}

impl Outcome {
    /// Never the token, the user or the request body: the services involved and the reason.
    fn log(&self) {
        match self {
            Self::Exchanged {
                caller, audience, ..
            } => tracing::debug!(
                caller = caller.as_str(),
                audience = audience.as_str(),
                "exchanged an internal identity"
            ),
            Self::Unauthenticated(reason) => {
                tracing::warn!(reason, "refused a token exchange: the caller's token");
            }
            // A service passed on a token it got from the exchange: a chain of calls longer than
            // one hop (Laysi/trustiot-v2#509), or a token used by someone it was not addressed to.
            Self::AlreadyExchanged {
                caller,
                actor,
                audience,
            } => tracing::warn!(
                reason = "already_exchanged",
                caller = caller.as_str(),
                actor = actor.as_str(),
                audience = audience.as_str(),
                "refused a token exchange: the caller's token came from an exchange, and an \
                 exchanged token is not exchanged again"
            ),
            // A service asked for a call that is not on the deployment's list: a missing entry in
            // mesh.calls, or a token used by someone it was not addressed to.
            Self::NotListed { caller, audience } => tracing::warn!(
                reason = "not_listed",
                caller = caller.as_str(),
                audience = audience.as_str(),
                "refused a token exchange: the call list does not let the caller call this \
                 service on behalf of users"
            ),
            Self::InvalidRequest => tracing::info!(
                reason = "invalid_request",
                "refused a token exchange: the body is not an exchange request"
            ),
            Self::Unavailable(reason) => {
                tracing::warn!(reason, "could not answer a token exchange");
            }
        }
    }
}

impl IntoResponse for Outcome {
    fn into_response(self) -> Response {
        let no_store = (CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let error = |code: ErrorCode| {
            let body = ErrorBody::new(code);
            (body.http_status(), [no_store.clone()], axum::Json(body)).into_response()
        };
        match self {
            Self::Exchanged { token, .. } => (
                StatusCode::OK,
                [no_store],
                axum::Json(serde_json::json!({ "token": token.expose() })),
            )
                .into_response(),
            Self::Unauthenticated(_) => error(ErrorCode::Unauthenticated),
            Self::AlreadyExchanged { .. } | Self::NotListed { .. } => {
                error(ErrorCode::PermissionDenied)
            }
            Self::InvalidRequest => error(ErrorCode::InvalidRequest),
            Self::Unavailable(_) => error(ErrorCode::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests;
