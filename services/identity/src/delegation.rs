//! Run delegations (Laysi/trustiot-v2#551): a service that acts for a user for longer than the
//! request's 60-second token lives, such as the assistant's main agent during a long analysis,
//! gets a delegation for the run ([`CreateRunDelegation`]) and exchanges it for a 60-second token
//! to each service it calls ([`ExchangeRunDelegation`]), through
//! `service_kit::identity::RunDelegations`. The callee sees the same token as for an on-behalf
//! call ([`crate::exchange`]).
//!
//! Creating one (`POST /internal/run-delegations`, on the token exchange's port):
//!
//! | Request | Response |
//! | --- | --- |
//! | A token the ingress gateway's request brought (no `act`, with `sid`) to a service in [`HOLDERS`], whose session is active and whose user is still a member of the token's tenant; a run ID, an Ed25519 public key and a lifetime of 1 to [`MAX_LIFETIME_SECS`] seconds | 200 `{"delegation", "expires_at"}` |
//! | No, repeated or invalid token; no `sid`; the session is not active | 401 `unauthenticated` |
//! | The token has `act`, its audience is not in [`HOLDERS`], or the user is no longer a member of its tenant | 403 `permission_denied` |
//! | The body is not the request, the key is not a usable Ed25519 public key, or the lifetime is out of range | 400 `invalid_request` |
//! | No public keys or no signing key yet, or the database failed | 503 `unavailable` |
//!
//! Exchanging one (`POST /internal/run-delegations/token`): the delegation in
//! `x-internal-identity`, and `{"audience", "iat", "proof"}`, where `proof` is the run key's
//! signature of [`run_proof_message`] for this delegation, audience and `iat`:
//!
//! | Request | Response |
//! | --- | --- |
//! | A delegation this service signed with a key that is not revoked, current and no longer-lived than [`MAX_LIFETIME_SECS`]; a valid proof within [`RUN_PROOF_WINDOW_SECS`]; the call list lets the holder call the audience; the session is still active and the user still a member of the delegation's tenant | 200 `{"token"}`: the user, the tenant with the role read now, the platform administrator flag read now, `act` naming the holder, issued now, expiring in [`TOKEN_LIFETIME_SECS`] or with the delegation, whichever is earlier |
//! | No, repeated or invalid delegation; expired, issued in the future or longer-lived than the cap; invalid or stale proof; the session is not active | 401 `unauthenticated` |
//! | The holder may not hold a delegation (any more), the call list does not let it call the audience, or the user is no longer a member of the tenant | 403 `permission_denied` |
//! | The body is not the request | 400 `invalid_request` |
//! | No signing keys read yet, or the database failed | 503 `unavailable` |
//!
//! Every response carries `cache-control: no-store`.
//!
//! The delegation is `trd1.<payload>.<mac>`: the payload is [`DelegationClaims`] as JSON in
//! base64url, the MAC is HMAC-SHA-256 of `trd1.<payload>` under a key derived from the signing
//! key that signs at that moment ([`Issuer::sign_delegation`]). Only this service holds the
//! signing keys, so only it can make or check one, and it is never an internal identity token:
//! no service's verifier accepts it. It binds the user, the session, the tenant, the holder, the
//! run and the run's public key; the private key never leaves the holder, which signs each
//! exchange with it, so a delegation that leaks without the key (a log, a stored run, a crash
//! report) exchanges nothing. What it cannot stop: whoever controls the holder's process acts for
//! its live runs, as they could with the request's own token.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::api::identity::{
    CreateRunDelegation, ExchangeRunDelegation, RUN_DELEGATION_MAX_LIFETIME_SECS,
    RUN_PROOF_WINDOW_SECS, run_proof_message,
};
use contract::client::Endpoint;
use contract::{
    Actor, ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims, InternalToken, RunId,
    ServiceName, SessionId, TenantGrant, TenantId, UserId,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use service_kit::identity::{ExchangeVerifier, IdentityError, ServiceIdentity};
use service_kit::postgres::InsecureConnection;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Semaphore;

use crate::exchange::OnBehalfCalls;
use crate::issuer::{IssueError, Issuer, TOKEN_LIFETIME_SECS};
use crate::session::{MEMBERSHIP_ROLE, StoreError, parse_role};

/// Longest lifetime of a delegation, in seconds: [`RUN_DELEGATION_MAX_LIFETIME_SECS`] (15
/// minutes). A delegation that claims a longer one is refused even with a valid MAC.
pub const MAX_LIFETIME_SECS: i64 = RUN_DELEGATION_MAX_LIFETIME_SECS as i64;

/// The services that may hold a delegation: the assistant, whose main agent runs long analyses
/// (decision 31). Any other service acts for a user only within the request's token
/// ([`crate::exchange`]). Adding one is a reviewed code change, not a setting.
pub const HOLDERS: &[ServiceName] = &[ServiceName::Assistant];

/// The prefix of every delegation: the format and its version.
pub const PREFIX: &str = "trd1.";

/// Largest request body accepted by either route; both requests take under 200 bytes.
pub const MAX_BODY_BYTES: usize = 1024;

/// How far in the future a delegation's `iat` may be, in seconds: another replica may have
/// signed it with a slightly faster clock.
pub const LEEWAY_SECS: i64 = 5;

/// Connections of the delegations' own pool, apart from the ext_authz check's: a flood of
/// exchanges never takes the connections every request's cookie check needs.
pub const POOL_CONNECTIONS: u32 = 4;

/// How long a request waits for a connection of that pool.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest a standing check may take, including waiting for a connection.
pub const WORK_LIMIT: Duration = Duration::from_secs(5);

/// Standing checks that may wait for the database at once; more answer 503 at once.
pub const IN_FLIGHT: usize = 64;

/// What a delegation binds, signed into it. Unknown members are refused: this service is the
/// only one that writes them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationClaims {
    /// The user the run acts for.
    pub sub: UserId,
    /// The session the run was started from; the delegation works only while it is active.
    pub sid: SessionId,
    /// The tenant the run acts in, if the request had one; the role is read on every exchange.
    pub tenant: Option<TenantId>,
    /// The service that holds the delegation: the `act` of every token it exchanges for.
    pub holder: ServiceName,
    /// The run, as the holder named it.
    pub run: RunId,
    /// The run's Ed25519 public key, base64url without padding.
    pub key: String,
    /// Issued at, in seconds since the Unix epoch.
    pub iat: i64,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: i64,
}

/// Signs `claims` into a delegation with the key that signs at `now`.
///
/// # Errors
///
/// [`IssueError::NoSigningKey`] when no key may sign at `now`; [`IssueError::Build`] only if the
/// claims cannot be serialized, which the types rule out.
pub fn sign(
    issuer: &Issuer,
    claims: &DelegationClaims,
    now: i64,
) -> Result<InternalToken, IssueError> {
    let payload = serde_json::to_vec(claims).map_err(|_| IssueError::Build)?;
    let signed = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
    let mac = issuer
        .sign_delegation(now, signed.as_bytes())
        .ok_or(IssueError::NoSigningKey)?;
    InternalToken::new(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac)))
        .map_err(|_| IssueError::Build)
}

/// Why a delegation was refused before its claims were read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelegationError {
    /// Not a delegation this service signed with a key it still accepts.
    Invalid,
    /// No signing keys file has been read yet, so nothing can be checked.
    NoKeys,
}

/// The claims of `text`, if it is a delegation this service signed with a key of its signing
/// keys file whose generation is not revoked. The MAC is checked before the payload is decoded.
/// Times are not checked here.
///
/// # Errors
///
/// As [`DelegationError`].
pub fn verify(issuer: &Issuer, text: &str) -> Result<DelegationClaims, DelegationError> {
    let (signed, mac) = text.rsplit_once('.').ok_or(DelegationError::Invalid)?;
    let payload = signed
        .strip_prefix(PREFIX)
        .filter(|payload| !payload.is_empty() && !payload.contains('.'))
        .ok_or(DelegationError::Invalid)?;
    let mac = URL_SAFE_NO_PAD
        .decode(mac)
        .map_err(|_| DelegationError::Invalid)?;
    if mac.len() != 32 {
        return Err(DelegationError::Invalid);
    }
    match issuer.delegation_signed(signed.as_bytes(), &mac) {
        None => return Err(DelegationError::NoKeys),
        Some(false) => return Err(DelegationError::Invalid),
        Some(true) => {}
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| DelegationError::Invalid)?;
    serde_json::from_slice(&payload).map_err(|_| DelegationError::Invalid)
}

/// The run's public key, if `text` is a usable one: base64url of 32 bytes that decode to an
/// Ed25519 point outside the small-order subgroup.
fn run_key(text: &str) -> Option<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let bytes = <[u8; 32]>::try_from(bytes).ok()?;
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    (!key.is_weak()).then_some(key)
}

/// Whether `proof` is the run key's strict Ed25519 signature of [`run_proof_message`] for this
/// delegation, audience and `iat`.
fn proof_valid(
    claims: &DelegationClaims,
    delegation: &str,
    request: &ExchangeRunDelegation,
) -> bool {
    let Some(key) = run_key(&claims.key) else {
        return false;
    };
    let Ok(signature) = URL_SAFE_NO_PAD.decode(&request.proof) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    let message = run_proof_message(delegation, request.audience, request.iat);
    key.verify_strict(&message, &signature).is_ok()
}

/// Where the user of a delegation stands now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// The session is active and the user's: the tenant with the user's role there (`None` for
    /// a delegation without a tenant), and the platform administrator flag.
    Active {
        /// The delegation's tenant with the current role.
        tenant: Option<TenantGrant>,
        /// Whether the user is a platform administrator now.
        platform_admin: bool,
    },
    /// The session is revoked, expired or gone, belongs to someone else, or the user was
    /// deleted.
    NoSession,
    /// The session is active, but the user is no longer a member of the tenant.
    NotMember,
}

/// Reads where a delegation's user stands. [`PgDelegationStore`] is the real one; tests use
/// their own.
pub trait DelegationStore: Send + Sync + 'static {
    /// The standing of `user` with `session`, in `tenant` if any, read now.
    fn standing(
        &self,
        session: SessionId,
        user: UserId,
        tenant: Option<TenantId>,
    ) -> impl Future<Output = Result<Standing, StoreError>> + Send;
}

/// The session, if it is active and the user's, and the user's platform administrator flag.
/// Expiry is decided by the database clock, as in the ext_authz check.
const ACTIVE_SESSION: &str = "SELECT u.platform_admin
FROM identity.sessions AS s
JOIN identity.users AS u ON u.id = s.user_id
WHERE s.id = $1::uuid AND s.user_id = $2::uuid AND s.revoked_at IS NULL AND s.expires_at > now()";

/// Names the work in the kit's DEBUG log of each tenant transaction.
const MEMBERSHIP_PURPOSE: &str = "run-delegation-membership";

/// The standing in Postgres, read with the service role on a pool of its own
/// ([`POOL_CONNECTIONS`]).
///
/// The role is tenant data under row-level security, so it is read in a transaction for the
/// delegation's tenant. That tenant comes from a delegation this service signed, after it checked
/// the membership when it issued it, never from the request; so, like
/// [`crate::session::PgSessionStore`], this store keeps the background handle that opens a
/// transaction for a given tenant private and uses it only in [`DelegationStore::standing`].
#[derive(Clone, Debug)]
pub struct PgDelegationStore {
    db: TenantDb,
    memberships: BackgroundTenantDb,
    in_flight: Arc<Semaphore>,
}

impl PgDelegationStore {
    /// A lazily connecting store: nothing connects until the first delegation.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(options: PgConnectOptions) -> Result<Self, InsecureConnection> {
        let pool = PgPoolOptions::new()
            .max_connections(POOL_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT);
        let actor = ServiceIdentity::new(ServiceName::Identity);
        let (db, memberships) = TenantDb::connect_lazy_with_background(options, pool, actor)?;
        Ok(Self {
            db,
            memberships,
            in_flight: Arc::new(Semaphore::new(IN_FLIGHT)),
        })
    }

    async fn read(
        &self,
        session: SessionId,
        user: UserId,
        tenant: Option<TenantId>,
    ) -> Result<Standing, StoreError> {
        let platform_admin: Option<bool> = sqlx::query_scalar(ACTIVE_SESSION)
            .bind(session.to_string())
            .bind(user.to_string())
            .fetch_optional(self.db.pool())
            .await?;
        let Some(platform_admin) = platform_admin else {
            return Ok(Standing::NoSession);
        };
        let Some(tenant) = tenant else {
            return Ok(Standing::Active {
                tenant: None,
                platform_admin,
            });
        };
        let mut tx = self.memberships.begin(tenant, MEMBERSHIP_PURPOSE).await?;
        let role: Option<String> = sqlx::query_scalar(MEMBERSHIP_ROLE)
            .bind(tenant.to_string())
            .bind(user.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        match role {
            Some(role) => Ok(Standing::Active {
                tenant: Some(TenantGrant {
                    id: tenant,
                    role: parse_role(&role)?,
                }),
                platform_admin,
            }),
            None => Ok(Standing::NotMember),
        }
    }
}

impl DelegationStore for PgDelegationStore {
    /// Runs on its own task, which goes on when the caller stops waiting, for at most
    /// [`WORK_LIMIT`]; at most [`IN_FLIGHT`] at once.
    async fn standing(
        &self,
        session: SessionId,
        user: UserId,
        tenant: Option<TenantId>,
    ) -> Result<Standing, StoreError> {
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| StoreError::Busy)?;
        let store = self.clone();
        let read = tokio::spawn(async move {
            let standing = tokio::time::timeout(WORK_LIMIT, store.read(session, user, tenant))
                .await
                .unwrap_or(Err(StoreError::TimedOut));
            drop(permit);
            standing
        });
        read.await.unwrap_or(Err(StoreError::Interrupted))
    }
}

/// The delegations' state: the verifier of the caller's token, the signer, the call list and
/// the store.
pub struct RunDelegations<S> {
    verifier: ExchangeVerifier,
    issuer: Issuer,
    calls: OnBehalfCalls,
    store: S,
    clock: fn() -> i64,
}

impl<S: DelegationStore> RunDelegations<S> {
    /// Delegations at the current time.
    #[must_use]
    pub fn new(verifier: ExchangeVerifier, issuer: Issuer, calls: OnBehalfCalls, store: S) -> Self {
        Self::with_clock(verifier, issuer, calls, store, crate::issuer::unix_now)
    }

    /// Delegations with another clock (seconds since the Unix epoch), for tests.
    #[must_use]
    pub fn with_clock(
        verifier: ExchangeVerifier,
        issuer: Issuer,
        calls: OnBehalfCalls,
        store: S,
        clock: fn() -> i64,
    ) -> Self {
        Self {
            verifier,
            issuer,
            calls,
            store,
            clock,
        }
    }

    /// Decides one request for a delegation.
    async fn create(&self, headers: &HeaderMap, body: Result<Bytes, BytesRejection>) -> Outcome {
        let now = (self.clock)();
        let subject = match self.verifier.verify_headers(headers, now) {
            Ok(subject) => subject,
            Err(IdentityError::NoKeys) => return Outcome::Unavailable("no_keys"),
            Err(error) => return Outcome::unauthenticated(error.reason(), None),
        };
        let Some(request) = body
            .ok()
            .and_then(|body| serde_json::from_slice::<CreateRunDelegation>(&body).ok())
        else {
            return Outcome::InvalidRequest("not_a_request");
        };
        let lifetime = i64::from(request.lifetime_secs);
        if !(1..=MAX_LIFETIME_SECS).contains(&lifetime) {
            return Outcome::InvalidRequest("lifetime");
        }
        if run_key(&request.key).is_none() {
            return Outcome::InvalidRequest("key");
        }
        let run = Some(request.run_id);
        // A token from an exchange is the callee's, not a request's: one hop only.
        if subject.act.is_some() {
            return Outcome::forbidden("already_exchanged", subject.aud, None, run);
        }
        if !HOLDERS.contains(&subject.aud) {
            return Outcome::forbidden("not_a_holder", subject.aud, None, run);
        }
        let Some(session) = subject.sid else {
            return Outcome::unauthenticated("no_session_claim", run);
        };
        let tenant = subject.tenant_id();
        match self.store.standing(session, subject.sub, tenant).await {
            Ok(Standing::Active { .. }) => {}
            Ok(Standing::NoSession) => return Outcome::unauthenticated("no_active_session", run),
            Ok(Standing::NotMember) => {
                return Outcome::forbidden("not_a_member", subject.aud, None, run);
            }
            Err(error) => return Outcome::store_failed(&error),
        }
        let Some(exp) = now.checked_add(lifetime) else {
            return Outcome::InvalidRequest("lifetime");
        };
        let claims = DelegationClaims {
            sub: subject.sub,
            sid: session,
            tenant,
            holder: subject.aud,
            run: request.run_id,
            key: request.key,
            iat: now,
            exp,
        };
        match sign(&self.issuer, &claims, now) {
            Ok(delegation) => Outcome::Created {
                delegation,
                expires_at: exp,
                holder: claims.holder,
                run: claims.run,
                lifetime,
            },
            Err(error) => {
                tracing::error!(error = %error, "could not sign a run delegation");
                Outcome::Unavailable("no_signing_key")
            }
        }
    }

    /// Decides one exchange of a delegation.
    async fn exchange(&self, headers: &HeaderMap, body: Result<Bytes, BytesRejection>) -> Outcome {
        let now = (self.clock)();
        let Some(text) = single_header(headers) else {
            return Outcome::unauthenticated("no_delegation", None);
        };
        let claims = match verify(&self.issuer, text) {
            Ok(claims) => claims,
            Err(DelegationError::NoKeys) => return Outcome::Unavailable("no_keys"),
            Err(DelegationError::Invalid) => {
                return Outcome::unauthenticated("invalid_delegation", None);
            }
        };
        let run = Some(claims.run);
        if now >= claims.exp {
            return Outcome::unauthenticated("delegation_expired", run);
        }
        if claims.iat > now.saturating_add(LEEWAY_SECS) {
            return Outcome::unauthenticated("issued_in_the_future", run);
        }
        if !claims
            .exp
            .checked_sub(claims.iat)
            .is_some_and(|lifetime| (1..=MAX_LIFETIME_SECS).contains(&lifetime))
        {
            return Outcome::unauthenticated("lifetime_exceeds_cap", run);
        }
        let Some(request) = body
            .ok()
            .and_then(|body| serde_json::from_slice::<ExchangeRunDelegation>(&body).ok())
        else {
            return Outcome::InvalidRequest("not_a_request");
        };
        if now.abs_diff(request.iat) > RUN_PROOF_WINDOW_SECS.unsigned_abs() {
            return Outcome::unauthenticated("stale_proof", run);
        }
        if !proof_valid(&claims, text, &request) {
            return Outcome::unauthenticated("invalid_proof", run);
        }
        if !HOLDERS.contains(&claims.holder) {
            return Outcome::forbidden("not_a_holder", claims.holder, Some(request.audience), run);
        }
        if !self.calls.allows(claims.holder, request.audience) {
            return Outcome::forbidden("not_listed", claims.holder, Some(request.audience), run);
        }
        let (tenant, platform_admin) = match self
            .store
            .standing(claims.sid, claims.sub, claims.tenant)
            .await
        {
            Ok(Standing::Active {
                tenant,
                platform_admin,
            }) => (tenant, platform_admin),
            Ok(Standing::NoSession) => return Outcome::unauthenticated("no_active_session", run),
            Ok(Standing::NotMember) => {
                return Outcome::forbidden(
                    "not_a_member",
                    claims.holder,
                    Some(request.audience),
                    run,
                );
            }
            Err(error) => return Outcome::store_failed(&error),
        };
        // The store answers after its own wait: the token is issued at the time it is signed.
        let now = (self.clock)();
        let exp = claims.exp.min(now.saturating_add(TOKEN_LIFETIME_SECS));
        if exp.saturating_sub(now) < 1 {
            return Outcome::unauthenticated("expires_too_soon", run);
        }
        let token_claims = InternalClaims {
            iss: ServiceName::Identity,
            sub: claims.sub,
            aud: request.audience,
            iat: now,
            exp,
            tenant,
            platform_admin,
            act: Some(Actor { sub: claims.holder }),
            sid: None,
        };
        match self.issuer.sign_claims(&token_claims, now) {
            Ok(token) => Outcome::Exchanged {
                token,
                holder: claims.holder,
                audience: request.audience,
                run: claims.run,
            },
            Err(error) => {
                tracing::error!(error = %error, "could not sign a delegated internal identity");
                Outcome::Unavailable("no_signing_key")
            }
        }
    }
}

/// The one value of [`INTERNAL_IDENTITY_HEADER`], if there is exactly one and it is visible
/// ASCII of at most [`InternalToken::MAX_LEN`] bytes.
fn single_header(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(INTERNAL_IDENTITY_HEADER).iter();
    let value = values.next()?;
    if values.next().is_some() || value.len() > InternalToken::MAX_LEN {
        return None;
    }
    value.to_str().ok().filter(|text| !text.is_empty())
}

/// The two routes, [`CreateRunDelegation::ROUTE`] and [`ExchangeRunDelegation::ROUTE`], for
/// `POST` only, under `/internal`, which the ingress gateway never routes. They are served on the
/// token exchange's port ([`crate::serve_exchange`]), which the mesh opens to the services that
/// call on behalf of users only.
pub fn routes<S: DelegationStore>(delegations: RunDelegations<S>) -> Router {
    Router::new()
        .route(CreateRunDelegation::ROUTE, post(handle_create::<S>))
        .route(ExchangeRunDelegation::ROUTE, post(handle_exchange::<S>))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(delegations))
}

async fn handle_create<S: DelegationStore>(
    State(delegations): State<Arc<RunDelegations<S>>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let outcome = delegations.create(&headers, body).await;
    outcome.log();
    outcome.into_response()
}

async fn handle_exchange<S: DelegationStore>(
    State(delegations): State<Arc<RunDelegations<S>>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let outcome = delegations.exchange(&headers, body).await;
    outcome.log();
    outcome.into_response()
}

/// The outcome of one request to either route.
enum Outcome {
    /// 200 with a new delegation.
    Created {
        delegation: InternalToken,
        expires_at: i64,
        holder: ServiceName,
        run: RunId,
        lifetime: i64,
    },
    /// 200 with a token for the audience.
    Exchanged {
        token: InternalToken,
        holder: ServiceName,
        audience: ServiceName,
        run: RunId,
    },
    /// 401; the reason and the run go to the log only.
    Unauthenticated {
        reason: &'static str,
        run: Option<RunId>,
    },
    /// 403; the reason, the services and the run go to the log only.
    Forbidden {
        reason: &'static str,
        holder: ServiceName,
        audience: Option<ServiceName>,
        run: Option<RunId>,
    },
    /// 400; the reason goes to the log only.
    InvalidRequest(&'static str),
    /// 503; the reason goes to the log only.
    Unavailable(&'static str),
}

impl Outcome {
    const fn unauthenticated(reason: &'static str, run: Option<RunId>) -> Self {
        Self::Unauthenticated { reason, run }
    }

    const fn forbidden(
        reason: &'static str,
        holder: ServiceName,
        audience: Option<ServiceName>,
        run: Option<RunId>,
    ) -> Self {
        Self::Forbidden {
            reason,
            holder,
            audience,
            run,
        }
    }

    fn store_failed(error: &StoreError) -> Self {
        tracing::warn!(%error, "could not read the standing of a run delegation's user");
        Self::Unavailable("database")
    }

    /// Never the token, the delegation, the proof, the key or the user: the services, the run
    /// and the reason. A session that ended or a member who was removed stops a run as intended
    /// (INFO); anything else means a fault or someone using what is not theirs (WARN).
    fn log(&self) {
        match self {
            Self::Created {
                holder,
                run,
                lifetime,
                ..
            } => tracing::info!(
                holder = holder.as_str(),
                run = %run,
                lifetime_secs = lifetime,
                "issued a run delegation"
            ),
            Self::Exchanged {
                holder,
                audience,
                run,
                ..
            } => tracing::debug!(
                holder = holder.as_str(),
                audience = audience.as_str(),
                run = %run,
                "exchanged a run delegation"
            ),
            Self::Unauthenticated { reason, run } => {
                let run = run.map(|run| run.to_string());
                if matches!(*reason, "no_active_session" | "delegation_expired") {
                    tracing::info!(reason, run, "refused a run delegation: it no longer acts");
                } else {
                    tracing::warn!(reason, run, "refused a run delegation request");
                }
            }
            Self::Forbidden {
                reason,
                holder,
                audience,
                run,
            } => {
                let run = run.map(|run| run.to_string());
                let audience = audience.map(ServiceName::as_str);
                if *reason == "not_a_member" {
                    tracing::info!(
                        reason,
                        holder = holder.as_str(),
                        audience,
                        run,
                        "refused a run delegation: the user is no longer a member of its tenant"
                    );
                } else {
                    tracing::warn!(
                        reason,
                        holder = holder.as_str(),
                        audience,
                        run,
                        "refused a run delegation request: the caller may not do this"
                    );
                }
            }
            Self::InvalidRequest(reason) => tracing::info!(
                reason,
                "refused a run delegation request: the body is not the request"
            ),
            Self::Unavailable(reason) => {
                tracing::warn!(reason, "could not answer a run delegation request");
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
            Self::Created {
                delegation,
                expires_at,
                ..
            } => (
                StatusCode::OK,
                [no_store],
                axum::Json(serde_json::json!({
                    "delegation": delegation.expose(),
                    "expires_at": expires_at,
                })),
            )
                .into_response(),
            Self::Exchanged { token, .. } => (
                StatusCode::OK,
                [no_store],
                axum::Json(serde_json::json!({ "token": token.expose() })),
            )
                .into_response(),
            Self::Unauthenticated { .. } => error(ErrorCode::Unauthenticated),
            Self::Forbidden { .. } => error(ErrorCode::PermissionDenied),
            Self::InvalidRequest(_) => error(ErrorCode::InvalidRequest),
            Self::Unavailable(_) => error(ErrorCode::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests;
