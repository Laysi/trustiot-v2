//! An agent run that acts for its user for longer than the request's token lives
//! (Laysi/trustiot-v2#551).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::api::identity::{
    CreateRunDelegation, ExchangeRunDelegation, RUN_DELEGATION_MAX_LIFETIME_SECS, run_proof_message,
};
use contract::client::{CallError, Endpoint, InternalClient};
use contract::service::{self, Service};
use contract::{ErrorBody, ErrorCode, InternalToken, RunId, ServiceName};
use ed25519_dalek::{Signer, SigningKey};
use zeroize::Zeroizing;

use super::{UserIdentity, unix_now};

/// Starts run delegations at the identity service: the way a service acts for a user through a
/// whole agent run, such as the assistant's main agent answering one question, when the run
/// outlives the 60 seconds of the request's token.
///
/// [`OnBehalf`](super::OnBehalf) exchanges the request's own token, so its calls end when that
/// token does. A run instead starts a [`RunDelegation`] while the request's token is still valid
/// ([`CreateRunDelegation`]), and exchanges the delegation for a token to each service it calls
/// ([`ExchangeRunDelegation`]). On every exchange the identity service checks again that the
/// user's session is active and reads the user's current role, so a logout, a revoked session or
/// a removed member stops the run's next call; the callee sees the same token as for an
/// on-behalf call (the user, the tenant and role, `act` naming this service) and needs nothing
/// new. Only the services the identity service lets hold a delegation get one (the assistant),
/// along the deployment's call list (`mesh.calls`), one hop: a token exchanged this way is never
/// exchanged again.
///
/// Each delegation is bound to an Ed25519 key that [`RunDelegations::start`] generates for the
/// run and keeps in this process; every exchange is signed with it. The delegation alone (in a
/// log, a stored run, a crash report) exchanges nothing.
///
/// Build one at startup from the same client as [`OnBehalf`](super::OnBehalf), for the identity
/// service's token exchange port (`http://<release>-identity-exchange:8082`):
///
/// ```
/// use std::time::Duration;
///
/// use axum::extract::State;
/// use contract::client::{Endpoint, Method};
/// use contract::{InternalClient, RunId, service};
/// use service_kit::identity::{RunDelegationError, RunDelegations, UserIdentity};
/// # #[derive(serde::Deserialize, serde::Serialize)]
/// # pub struct Devices {}
///
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
/// struct Agent {
///     delegations: RunDelegations,
///     assets: InternalClient<service::Assets>,
/// }
///
/// async fn answer(State(agent): State<Agent>, user: UserIdentity) -> Result<(), RunDelegationError> {
///     // While the request's token is valid: start the run's delegation.
///     let run = agent
///         .delegations
///         .start(&user, RunId::generate(), Duration::from_secs(600))
///         .await?;
///     // Minutes later, each tool call still acts as the user.
///     let _devices = run.call(&agent.assets, &ListDevices).await?;
///     Ok(())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct RunDelegations {
    identity: InternalClient<service::Identity>,
}

impl RunDelegations {
    /// Starts delegations through `identity`, the client for the identity service's token
    /// exchange port.
    #[must_use]
    pub fn new(identity: InternalClient<service::Identity>) -> Self {
        Self { identity }
    }

    /// Starts the delegation of the run `run` for `user`, the request's verified identity,
    /// living `lifetime` (whole seconds, 1 to [`RUN_DELEGATION_MAX_LIFETIME_SECS`]). Call it
    /// while the request's token is valid, before the run's first tool call.
    ///
    /// # Errors
    ///
    /// [`RunDelegationError::Lifetime`] for a lifetime out of range,
    /// [`RunDelegationError::Random`] when the run's key cannot be generated, and
    /// [`RunDelegationError::Start`] when the identity service does not issue the delegation;
    /// see [`RunDelegationError::code`].
    pub async fn start(
        &self,
        user: &UserIdentity,
        run: RunId,
        lifetime: Duration,
    ) -> Result<RunDelegation, RunDelegationError> {
        let lifetime_secs = u32::try_from(lifetime.as_secs())
            .ok()
            .filter(|secs| (1..=RUN_DELEGATION_MAX_LIFETIME_SECS).contains(secs))
            .ok_or(RunDelegationError::Lifetime)?;
        let key = run_key()?;
        let request = CreateRunDelegation {
            run_id: run,
            key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            lifetime_secs,
        };
        match self.identity.call(&request, user.token()).await {
            Ok(created) => Ok(RunDelegation {
                inner: Arc::new(Inner {
                    identity: self.identity.clone(),
                    delegation: created.delegation,
                    key,
                    run,
                    holder: user.claims().aud,
                    expires_at: created.expires_at,
                }),
            }),
            Err(source) => {
                let error = RunDelegationError::Start { source };
                error.log(run, None);
                Err(error)
            }
        }
    }
}

/// A new Ed25519 key for a run, from the operating system's CSPRNG. The seed is wiped; the key
/// wipes itself when dropped.
fn run_key() -> Result<SigningKey, RunDelegationError> {
    let mut seed = Zeroizing::new([0_u8; 32]);
    getrandom::fill(seed.as_mut()).map_err(|_| RunDelegationError::Random)?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The delegation of one run, from [`RunDelegations::start`]. Cheap to clone; clones share the
/// delegation and the run's key, so the run's tasks can call in parallel. It cannot be
/// serialized or printed: `Debug` shows the run, the holder and the expiry only. Drop it when the
/// run ends.
#[derive(Clone)]
pub struct RunDelegation {
    inner: Arc<Inner>,
}

struct Inner {
    identity: InternalClient<service::Identity>,
    delegation: InternalToken,
    key: SigningKey,
    run: RunId,
    holder: ServiceName,
    expires_at: i64,
}

impl RunDelegation {
    /// The run the delegation is for.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.inner.run
    }

    /// When the delegation expires, in seconds since the Unix epoch, as the identity service
    /// said. Exchanges fail from then on.
    #[must_use]
    pub fn expires_at(&self) -> i64 {
        self.inner.expires_at
    }

    /// A token for the run's user, addressed to `audience`, valid for 60 seconds at most (less
    /// when the delegation expires sooner). Get one for each call; [`RunDelegation::call`] does.
    ///
    /// # Errors
    ///
    /// [`RunDelegationError::Exchange`] when the identity service does not exchange the
    /// delegation; see [`RunDelegationError::code`].
    pub async fn token_for(
        &self,
        audience: ServiceName,
    ) -> Result<InternalToken, RunDelegationError> {
        let inner = &self.inner;
        let iat = unix_now();
        let message = run_proof_message(inner.delegation.expose(), audience, iat);
        let request = ExchangeRunDelegation {
            audience,
            iat,
            proof: URL_SAFE_NO_PAD.encode(inner.key.sign(&message).to_bytes()),
        };
        match inner.identity.call(&request, &inner.delegation).await {
            Ok(exchanged) => Ok(exchanged.token),
            Err(source) => {
                let error = RunDelegationError::Exchange { audience, source };
                error.log(inner.run, Some(inner.holder));
                Err(error)
            }
        }
    }

    /// Calls `endpoint` of the service `client` calls as the run's user: exchanges the
    /// delegation ([`RunDelegation::token_for`]), then calls with the new token.
    ///
    /// # Errors
    ///
    /// [`RunDelegationError::Exchange`] when there is no token for the callee,
    /// [`RunDelegationError::Call`] when the call itself fails.
    pub async fn call<S, E>(
        &self,
        client: &InternalClient<S>,
        endpoint: &E,
    ) -> Result<E::Response, RunDelegationError>
    where
        S: Service,
        E: Endpoint<Service = S>,
    {
        let token = self.token_for(S::NAME).await?;
        client
            .call(endpoint, &token)
            .await
            .map_err(RunDelegationError::Call)
    }
}

impl fmt::Debug for RunDelegation {
    /// Neither the delegation nor the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunDelegation")
            .field("run", &self.inner.run)
            .field("holder", &self.inner.holder)
            .field("expires_at", &self.inner.expires_at)
            .finish_non_exhaustive()
    }
}

/// A run delegation that could not be started or used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunDelegationError {
    /// The requested lifetime is not 1 to [`RUN_DELEGATION_MAX_LIFETIME_SECS`] whole seconds: a
    /// fault of the caller. M1: `internal`.
    #[error("a run delegation lives 1 to {RUN_DELEGATION_MAX_LIFETIME_SECS} seconds")]
    Lifetime,
    /// The operating system gave no randomness for the run's key. M1: `internal`.
    #[error("could not generate the run's key")]
    Random,
    /// The identity service did not issue the delegation. M1: see [`RunDelegationError::code`].
    #[error("the identity service did not issue a run delegation")]
    Start {
        /// The failed call to the identity service.
        #[source]
        source: CallError,
    },
    /// The identity service did not exchange the delegation for a token addressed to
    /// `audience`. M1: see [`RunDelegationError::code`].
    #[error("the identity service did not exchange the run delegation for {audience}")]
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

impl RunDelegationError {
    /// The M1 code to answer with. A refusal of the identity service passes on what it means
    /// for the user: `unauthenticated` when the session ended (logout, revocation, expiry) or the
    /// delegation expired, `permission_denied` when the user is no longer a member of the tenant
    /// (or the deployment's call list lacks the call, which its ERROR log line tells apart),
    /// `tenant_suspended` when the tenant is suspended or deleted (Laysi/trustiot-v2#777);
    /// `timeout` and `unavailable` pass through; anything else is `internal`, a fault of this
    /// service or of the deployment.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Lifetime | Self::Random => ErrorCode::Internal,
            // `CallError::code` turns a peer's `unauthenticated` into `internal` (this service's
            // credential failed); here the credential is the user's, so it passes on.
            Self::Start { source } | Self::Exchange { source, .. } => {
                match source.peer_error().map(ErrorBody::code) {
                    Some(
                        code @ (ErrorCode::Unauthenticated
                        | ErrorCode::PermissionDenied
                        | ErrorCode::TenantSuspended),
                    ) => code,
                    _ => match source.code() {
                        code @ (ErrorCode::Timeout | ErrorCode::Unavailable) => code,
                        _ => ErrorCode::Internal,
                    },
                }
            }
            Self::Call(error) => error.code(),
        }
    }

    /// The M1 body to answer with: [`CallError::to_error_body`] for a failed call, a body with
    /// [`RunDelegationError::code`] and no detail otherwise.
    #[must_use]
    pub fn to_error_body(&self) -> ErrorBody {
        match self {
            Self::Call(error) => error.to_error_body(),
            _ => ErrorBody::new(self.code()),
        }
    }

    /// Logs a refusal of the identity service: INFO when the run's user no longer may act (the
    /// run ends as intended), ERROR when it is a fault of this service or of the deployment.
    /// Timeouts and connection failures are already logged by the client.
    fn log(&self, run: RunId, holder: Option<ServiceName>) {
        let (Self::Start { source } | Self::Exchange { source, .. }) = self else {
            return;
        };
        let Some(peer) = source.peer_error() else {
            return;
        };
        let audience = match self {
            Self::Exchange { audience, .. } => Some(audience.as_str()),
            _ => None,
        };
        let holder = holder.map(ServiceName::as_str);
        match self.code() {
            ErrorCode::Unauthenticated
            | ErrorCode::PermissionDenied
            | ErrorCode::TenantSuspended => tracing::info!(
                run = %run,
                holder,
                audience,
                peer_code = peer.code().as_str(),
                "the identity service refused the run delegation: the run's user may no longer \
                 act, or (permission_denied) mesh.calls lacks this call"
            ),
            ErrorCode::Timeout | ErrorCode::Unavailable => {}
            _ => tracing::error!(
                run = %run,
                holder,
                audience,
                peer_code = peer.code().as_str(),
                "the identity service refused a run delegation request this service made"
            ),
        }
    }
}

impl IntoResponse for RunDelegationError {
    /// [`RunDelegationError::to_error_body`] with its HTTP status.
    fn into_response(self) -> Response {
        let body = self.to_error_body();
        (body.http_status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use contract::client::Method;
    use contract::{INTERNAL_IDENTITY_HEADER, SessionId};
    use ed25519_dalek::{Signature, VerifyingKey};
    use serde_json::{Value, json};

    use super::*;
    use crate::identity::IdentityVerifier;
    use crate::identity::test_keys::{TestKey, claims_at, keys};

    /// What the stand-in identity service received.
    #[derive(Clone, Default)]
    struct Seen {
        creates: Arc<Mutex<Vec<(String, Value)>>>,
        exchanges: Arc<Mutex<Vec<(String, Value)>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    fn header(headers: &HeaderMap) -> String {
        headers
            .get(INTERNAL_IDENTITY_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    fn refuse(code: ErrorCode) -> Response {
        let body = ErrorBody::new(code);
        (body.http_status(), axum::Json(body)).into_response()
    }

    /// Issues `delegation.for.run` to any request with a token.
    async fn create(
        State(seen): State<Seen>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        seen.creates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((header(&headers), body));
        axum::Json(json!({"delegation": "delegation.for.run", "expires_at": 42})).into_response()
    }

    /// certs: a token; alerts: the session ended; knowledge: not a member; gateway-ops: the
    /// tenant is suspended; assets: unavailable; anything else: a request this service should not
    /// have made.
    async fn exchange(
        State(seen): State<Seen>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        seen.exchanges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((header(&headers), body.clone()));
        match body["audience"].as_str() {
            Some("certs") => axum::Json(json!({"token": "token.for.certs"})).into_response(),
            Some("alerts") => refuse(ErrorCode::Unauthenticated),
            Some("knowledge") => refuse(ErrorCode::PermissionDenied),
            Some("gateway-ops") => refuse(ErrorCode::TenantSuspended),
            Some("assets") => refuse(ErrorCode::Unavailable),
            _ => refuse(ErrorCode::InvalidRequest),
        }
    }

    async fn devices(State(seen): State<Seen>, headers: HeaderMap) -> Response {
        seen.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(header(&headers));
        axum::Json(json!({"devices": 2})).into_response()
    }

    async fn serve(seen: Seen) -> String {
        let router = Router::new()
            .route(CreateRunDelegation::ROUTE, post(create))
            .route(ExchangeRunDelegation::ROUTE, post(exchange))
            .route("/internal/devices", get(devices))
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

    struct CertsDevices;
    impl Endpoint for CertsDevices {
        type Service = service::Certs;
        type Body = ();
        type Response = Value;
        const METHOD: Method = Method::Get;
        const ROUTE: &'static str = "/internal/devices";
    }

    /// The request's identity at the assistant, with a session, as the ext_authz check signs it.
    fn user() -> (UserIdentity, String) {
        let signer = TestKey::new(1);
        let now = unix_now();
        let mut claims = claims_at(now);
        claims["aud"] = json!("assistant");
        claims["sid"] = json!(SessionId::generate().to_string());
        let token = signer.sign("k1", &claims);
        let user = IdentityVerifier::new(ServiceName::Assistant, keys(&[(&signer, "k1")]))
            .verify(&token)
            .expect("valid token");
        (user, token)
    }

    #[tokio::test]
    async fn a_run_starts_with_the_request_token_and_signs_every_exchange_with_its_own_key() {
        let seen = Seen::default();
        let base_url = serve(seen.clone()).await;
        let (user, token) = user();
        let delegations = RunDelegations::new(client(&base_url));
        let run_id = RunId::generate();

        let run = delegations
            .start(&user, run_id, Duration::from_secs(900))
            .await
            .expect("delegation");
        assert_eq!(run.run_id(), run_id);
        assert_eq!(run.expires_at(), 42);
        let debug = format!("{run:?}");
        assert!(!debug.contains("delegation.for.run"), "{debug}");

        // The request's token goes to the identity service once, with the run and a public key.
        let creates = seen
            .creates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let [(sent_token, body)] = creates.as_slice() else {
            panic!("one create: {creates:?}");
        };
        assert_eq!(sent_token, &token);
        assert_eq!(body["run_id"], json!(run_id.to_string()));
        assert_eq!(body["lifetime_secs"], json!(900));
        let key_bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(body["key"].as_str().expect("key"))
            .expect("base64url")
            .try_into()
            .expect("32 bytes");
        let public = VerifyingKey::from_bytes(&key_bytes).expect("a public key");

        let devices = run
            .call(&client::<service::Certs>(&base_url), &CertsDevices)
            .await
            .expect("call");
        assert_eq!(devices, json!({"devices": 2}));
        // The delegation goes to the identity service only, with a proof the run's key signed
        // for this delegation, audience and time; the callee gets the exchanged token.
        let exchanges = seen
            .exchanges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let [(sent_delegation, body)] = exchanges.as_slice() else {
            panic!("one exchange: {exchanges:?}");
        };
        assert_eq!(sent_delegation, "delegation.for.run");
        assert_eq!(body["audience"], json!("certs"));
        let iat = body["iat"].as_i64().expect("iat");
        assert!((iat - unix_now()).abs() <= 5, "{iat}");
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(body["proof"].as_str().expect("proof"))
                .expect("base64url"),
        )
        .expect("a signature");
        let message = run_proof_message("delegation.for.run", ServiceName::Certs, iat);
        public
            .verify_strict(&message, &signature)
            .expect("the proof verifies with the run's public key");
        assert!(
            public
                .verify_strict(
                    &run_proof_message("delegation.for.run", ServiceName::Assets, iat),
                    &signature
                )
                .is_err(),
            "the proof is for certs only"
        );
        assert_eq!(
            *seen.calls.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["token.for.certs".to_owned()]
        );

        // Another run gets another key.
        delegations
            .start(&user, RunId::generate(), Duration::from_secs(60))
            .await
            .expect("second delegation");
        let creates = seen
            .creates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_ne!(creates[0].1["key"], creates[1].1["key"]);
    }

    #[tokio::test]
    async fn refusals_tell_the_user_what_happened_and_the_callee_is_never_called() {
        let seen = Seen::default();
        let base_url = serve(seen.clone()).await;
        let (user, _) = user();
        let run = RunDelegations::new(client(&base_url))
            .start(&user, RunId::generate(), Duration::from_secs(60))
            .await
            .expect("delegation");

        for (audience, code, status) in [
            (
                ServiceName::Alerts,
                ErrorCode::Unauthenticated,
                StatusCode::UNAUTHORIZED,
            ),
            (
                ServiceName::Knowledge,
                ErrorCode::PermissionDenied,
                StatusCode::FORBIDDEN,
            ),
            (
                ServiceName::GatewayOps,
                ErrorCode::TenantSuspended,
                StatusCode::FORBIDDEN,
            ),
            (
                ServiceName::Assets,
                ErrorCode::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ServiceName::Identity,
                ErrorCode::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let error = run.token_for(audience).await.expect_err("refused");
            assert!(
                matches!(&error, RunDelegationError::Exchange { audience: a, .. } if *a == audience),
                "{error:?}"
            );
            assert_eq!(error.code(), code, "{audience}");
            assert_eq!(error.to_error_body(), ErrorBody::new(code));
            assert_eq!(error.into_response().status(), status);
        }
        assert!(
            seen.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_lifetime_out_of_range_is_refused_before_any_call() {
        let seen = Seen::default();
        let base_url = serve(seen.clone()).await;
        let (user, _) = user();
        let delegations = RunDelegations::new(client(&base_url));
        for lifetime in [
            Duration::ZERO,
            Duration::from_millis(999),
            Duration::from_secs(u64::from(RUN_DELEGATION_MAX_LIFETIME_SECS) + 1),
            Duration::MAX,
        ] {
            let error = delegations
                .start(&user, RunId::generate(), lifetime)
                .await
                .expect_err("refused");
            assert!(
                matches!(error, RunDelegationError::Lifetime),
                "{lifetime:?}"
            );
            assert_eq!(error.code(), ErrorCode::Internal);
        }
        assert!(
            seen.creates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }
}
