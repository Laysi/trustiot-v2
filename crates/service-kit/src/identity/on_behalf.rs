//! Calls to other services on behalf of the request's user (decision 27,
//! Laysi/trustiot-v2#106).

use axum::response::{IntoResponse, Response};
use contract::api::identity::ExchangeToken;
use contract::client::{CallError, Endpoint, InternalClient};
use contract::service::{self, Service};
use contract::{ErrorBody, ErrorCode, InternalToken, ServiceName};

use super::UserIdentity;

/// Calls other services' internal APIs on behalf of the user a request acts for.
///
/// The token a service receives is addressed to that service alone, so it cannot be passed on:
/// the service it calls would reject it (`wrong_audience`). For each call, [`OnBehalf`] first
/// exchanges the request's token at the identity service for one addressed to the callee
/// ([`ExchangeToken`]), then calls with that token. The identity service exchanges only along the
/// deployment's call list (the chart's `mesh.calls`, which also sets the AuthorizationPolicies),
/// and the new token expires when the request's token does, so a chain of calls never outlives
/// the request it serves. It exchanges one hop only: the new token carries `act` and is never
/// exchanged again, so a service called this way cannot call on the same user's behalf in turn
/// (longer chains: Laysi/trustiot-v2#509).
///
/// Build one at startup from a client for the identity service's token exchange port (the
/// chart's Service `<release>-identity-exchange`, port `token-exchange`,
/// `http://<release>-identity-exchange:8082`; the mesh lets callers on behalf of users reach that
/// port only) and keep it in the router's state:
///
/// ```
/// use axum::Json;
/// use axum::extract::State;
/// use contract::client::{Endpoint, Method};
/// use contract::{InternalClient, service};
/// use service_kit::identity::{OnBehalf, OnBehalfError, UserIdentity};
/// # #[derive(serde::Deserialize, serde::Serialize)]
/// # pub struct Devices {}
///
/// /// An internal API of the assets service (crates/contract, `api::assets`).
/// pub struct ListDevices;
///
/// impl Endpoint for ListDevices {
///     type Service = service::Assets;
///     type Body = ();
///     type Response = Devices;
///     const METHOD: Method = Method::Get;
///     const ROUTE: &'static str = "/internal/devices";
/// }
///
/// #[derive(Clone)]
/// struct Peers {
///     on_behalf: OnBehalf,
///     assets: InternalClient<service::Assets>,
/// }
///
/// async fn devices(
///     State(peers): State<Peers>,
///     user: UserIdentity,
/// ) -> Result<Json<Devices>, OnBehalfError> {
///     let devices = peers.on_behalf.call(&peers.assets, &ListDevices, &user).await?;
///     Ok(Json(devices))
/// }
/// ```
///
/// Background work acts as [`ServiceIdentity`](super::ServiceIdentity), which has no user to act
/// for, so it cannot call on anyone's behalf:
///
/// ```compile_fail,E0308
/// # use contract::client::{Endpoint, Method};
/// # use contract::{InternalClient, service};
/// # use service_kit::identity::{OnBehalf, ServiceIdentity};
/// # pub struct ListDevices;
/// # impl Endpoint for ListDevices {
/// #     type Service = service::Assets;
/// #     type Body = ();
/// #     type Response = ();
/// #     const METHOD: Method = Method::Get;
/// #     const ROUTE: &'static str = "/internal/devices";
/// # }
/// async fn nightly(on_behalf: &OnBehalf, assets: &InternalClient<service::Assets>, actor: ServiceIdentity) {
///     let _ = on_behalf.call(assets, &ListDevices, &actor).await;
/// }
/// ```
#[derive(Clone, Debug)]
pub struct OnBehalf {
    identity: InternalClient<service::Identity>,
}

impl OnBehalf {
    /// Exchanges tokens through `identity`, the client for the identity service.
    #[must_use]
    pub fn new(identity: InternalClient<service::Identity>) -> Self {
        Self { identity }
    }

    /// A token for `user`, addressed to `audience`, from the identity service.
    ///
    /// # Errors
    ///
    /// [`OnBehalfError::Exchange`] when the identity service does not exchange the token. Unless
    /// it is unavailable or too slow, that is a fault of the deployment or of this service, never
    /// of the user, and is logged at ERROR: most often the deployment's call list does not let
    /// this service call `audience`, or the request ran longer than its token lives.
    pub async fn token_for(
        &self,
        user: &UserIdentity,
        audience: ServiceName,
    ) -> Result<InternalToken, OnBehalfError> {
        match self
            .identity
            .call(&ExchangeToken { audience }, user.token())
            .await
        {
            Ok(exchanged) => Ok(exchanged.token),
            Err(source) => {
                let error = OnBehalfError::Exchange { audience, source };
                if error.code() == ErrorCode::Internal {
                    tracing::error!(
                        caller = user.claims().aud.as_str(),
                        audience = audience.as_str(),
                        peer_code = error.peer_code(),
                        "the identity service did not exchange the request's token; check that \
                         mesh.calls lets the caller call this audience and that the request \
                         finishes within its token's lifetime"
                    );
                }
                Err(error)
            }
        }
    }

    /// Calls `endpoint` of the service `client` calls, on behalf of `user`: exchanges the token
    /// ([`OnBehalf::token_for`]), then calls with the new one.
    ///
    /// # Errors
    ///
    /// [`OnBehalfError::Exchange`] when there is no token for the callee,
    /// [`OnBehalfError::Call`] when the call itself fails. Both are logged;
    /// [`OnBehalfError::to_error_body`] gives the M1 body to answer with.
    pub async fn call<S, E>(
        &self,
        client: &InternalClient<S>,
        endpoint: &E,
        user: &UserIdentity,
    ) -> Result<E::Response, OnBehalfError>
    where
        S: Service,
        E: Endpoint<Service = S>,
    {
        let token = self.token_for(user, S::NAME).await?;
        client
            .call(endpoint, &token)
            .await
            .map_err(OnBehalfError::Call)
    }
}

/// A call on behalf of a user that failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OnBehalfError {
    /// The identity service did not exchange the request's token for one addressed to the
    /// callee. M1: `timeout` or `unavailable` when the identity service was too slow or could not
    /// be reached, otherwise `internal`: a refused exchange is a fault of the deployment or of
    /// this service, so the user gets neither `permission_denied` nor `unauthenticated`.
    #[error("the identity service did not exchange the request's token for {audience}")]
    Exchange {
        /// The service the token was for.
        audience: ServiceName,
        /// The failed call to the identity service.
        #[source]
        source: CallError,
    },
    /// The call itself failed. M1: as [`CallError::code`].
    #[error(transparent)]
    Call(CallError),
}

impl OnBehalfError {
    /// The M1 code to answer with; see the variants.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Exchange { source, .. } => match source.code() {
                code @ (ErrorCode::Timeout | ErrorCode::Unavailable) => code,
                _ => ErrorCode::Internal,
            },
            Self::Call(error) => error.code(),
        }
    }

    /// The M1 body to answer with: [`CallError::to_error_body`] for a failed call, a body with
    /// [`OnBehalfError::code`] and no detail for a failed exchange.
    #[must_use]
    pub fn to_error_body(&self) -> ErrorBody {
        match self {
            Self::Exchange { .. } => ErrorBody::new(self.code()),
            Self::Call(error) => error.to_error_body(),
        }
    }

    /// The M1 code the identity service answered a refused exchange with, for the log.
    fn peer_code(&self) -> Option<&'static str> {
        match self {
            Self::Exchange { source, .. } => source.peer_error().map(|body| body.code().as_str()),
            Self::Call(_) => None,
        }
    }
}

impl IntoResponse for OnBehalfError {
    /// [`OnBehalfError::to_error_body`] with its HTTP status.
    fn into_response(self) -> Response {
        let body = self.to_error_body();
        (body.http_status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use contract::client::Method;
    use contract::{INTERNAL_IDENTITY_HEADER, TenantId};
    use serde_json::{Value, json};

    use super::*;
    use crate::identity::IdentityVerifier;
    use crate::identity::test_keys::{NOW, TestKey, claims_at, keys};

    /// What the stand-in services received: the identity header and body of each exchange, then
    /// the identity header of each call to the callee.
    #[derive(Clone, Default)]
    struct Seen {
        exchanges: Arc<Mutex<Vec<(String, Value)>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    fn identity_header(headers: &HeaderMap) -> String {
        headers
            .get(INTERNAL_IDENTITY_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// The identity service's exchange: certs gets a token, alerts is not on the call list,
    /// knowledge finds the identity service without keys.
    async fn exchange(
        State(seen): State<Seen>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        seen.exchanges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((identity_header(&headers), body.clone()));
        let refuse = |code| {
            let body = ErrorBody::new(code);
            (body.http_status(), axum::Json(body)).into_response()
        };
        match body["audience"].as_str() {
            Some("certs") => axum::Json(json!({"token": "exchanged.for.certs"})).into_response(),
            Some("alerts") => refuse(ErrorCode::PermissionDenied),
            Some("knowledge") => refuse(ErrorCode::Unavailable),
            _ => refuse(ErrorCode::InvalidRequest),
        }
    }

    async fn devices(State(seen): State<Seen>, headers: HeaderMap) -> Response {
        seen.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(identity_header(&headers));
        axum::Json(json!({"devices": 2})).into_response()
    }

    async fn missing() -> Response {
        let body = ErrorBody::new(ErrorCode::NotFound);
        (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
    }

    macro_rules! endpoint {
        ($name:ident, $service:ty, $route:literal) => {
            struct $name;
            impl Endpoint for $name {
                type Service = $service;
                type Body = ();
                type Response = Value;
                const METHOD: Method = Method::Get;
                const ROUTE: &'static str = $route;
            }
        };
    }
    endpoint!(CertsDevices, service::Certs, "/internal/devices");
    endpoint!(CertsMissing, service::Certs, "/internal/missing");
    endpoint!(AlertsDevices, service::Alerts, "/internal/devices");
    endpoint!(KnowledgeDevices, service::Knowledge, "/internal/devices");

    /// One server stands in for the identity service and every callee.
    async fn serve(seen: Seen) -> String {
        let router = Router::new()
            .route(ExchangeToken::ROUTE, post(exchange))
            .route("/internal/devices", get(devices))
            .route("/internal/missing", get(missing))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}")
    }

    fn client<S: Service>(base_url: &str) -> InternalClient<S> {
        InternalClient::new(base_url, Duration::from_secs(30)).expect("client")
    }

    #[tokio::test]
    async fn calls_with_a_token_exchanged_for_the_callee() {
        let seen = Seen::default();
        let base_url = serve(seen.clone()).await;
        let signer = TestKey::new(1);
        let token = signer.sign("k1", &claims_at(NOW));
        let user = IdentityVerifier::new(ServiceName::Assets, keys(&[(&signer, "k1")]))
            .verify_at(&token, NOW)
            .expect("valid token");
        let on_behalf = OnBehalf::new(client(&base_url));

        let devices = on_behalf
            .call(&client::<service::Certs>(&base_url), &CertsDevices, &user)
            .await
            .expect("call");
        assert_eq!(devices, json!({"devices": 2}));
        // The request's own token goes to the identity service only; the callee gets the new one.
        assert_eq!(
            *seen
                .exchanges
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            vec![(token.clone(), json!({"audience": "certs"}))]
        );
        assert_eq!(
            *seen.calls.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["exchanged.for.certs".to_owned()]
        );
        let exchanged = on_behalf
            .token_for(&user, ServiceName::Certs)
            .await
            .expect("token");
        assert_eq!(exchanged.expose(), "exchanged.for.certs");
    }

    #[tokio::test]
    async fn a_refused_exchange_is_an_internal_error_and_the_callee_is_never_called() {
        let seen = Seen::default();
        let base_url = serve(seen.clone()).await;
        let signer = TestKey::new(1);
        let user = IdentityVerifier::new(ServiceName::Assets, keys(&[(&signer, "k1")]))
            .verify_at(&signer.sign("k1", &claims_at(NOW)), NOW)
            .expect("valid token");
        let on_behalf = OnBehalf::new(client(&base_url));

        // Not on the call list: the identity service answers 403, the user gets 500 `internal`.
        let error = on_behalf
            .call(&client::<service::Alerts>(&base_url), &AlertsDevices, &user)
            .await
            .expect_err("refused");
        assert!(
            matches!(&error, OnBehalfError::Exchange { audience: ServiceName::Alerts, source } if source.status() == Some(StatusCode::FORBIDDEN)),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.to_error_body(), ErrorBody::new(ErrorCode::Internal));
        assert_eq!(
            error.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        // The identity service is unavailable: so is the call.
        let error = on_behalf
            .call(
                &client::<service::Knowledge>(&base_url),
                &KnowledgeDevices,
                &user,
            )
            .await
            .expect_err("unavailable");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(
            seen.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "no call without an exchanged token"
        );
    }

    #[tokio::test]
    async fn the_callees_answer_passes_through() {
        let base_url = serve(Seen::default()).await;
        let signer = TestKey::new(1);
        let mut claims = claims_at(NOW);
        claims["tenant"] = json!({"id": TenantId::generate().to_string(), "role": "viewer"});
        let user = IdentityVerifier::new(ServiceName::Assets, keys(&[(&signer, "k1")]))
            .verify_at(&signer.sign("k1", &claims), NOW)
            .expect("valid token");
        let error = OnBehalf::new(client(&base_url))
            .call(&client::<service::Certs>(&base_url), &CertsMissing, &user)
            .await
            .expect_err("not found");
        assert!(matches!(error, OnBehalfError::Call(_)), "{error:?}");
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.to_error_body(), ErrorBody::new(ErrorCode::NotFound));
    }
}
