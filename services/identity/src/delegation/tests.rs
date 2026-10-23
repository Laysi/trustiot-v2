//! Run delegations in process (Laysi/trustiot-v2#551): every outcome of both routes, the
//! re-checks of each exchange against a store the tests control, the impersonation and replay
//! refusals, and service-kit's `RunDelegations` client against these routes and a callee that
//! verifies with `UserIdentity`. `tests/ext_authz.rs` repeats the main cases with the real binary
//! and Postgres.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request};
use axum::routing::get;
use contract::api::identity::{ExchangeToken, ExchangedToken};
use contract::client::{Endpoint, Method as CallMethod};
use contract::{InternalClient, Service, TenantRole, service};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use service_kit::identity::{
    IdentityVerifier, PublishedKeys, RunDelegationError, RunDelegations as Client, UserIdentity,
};
use tower::ServiceExt;

use super::*;
use crate::exchange::{Exchange, OnBehalfCalls};
use crate::issuer::test_keys::{issuer, keys, signing_keys};
use crate::issuer::unix_now;

const SEED: u8 = 7;
const KID: &str = "2026-10-a";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const SESSION: &str = "01928f6e-7a3c-7d4e-8f00-00000000005e";
const RUN: &str = "01928f6e-7a3c-7d4e-8f00-0000000000a1";

/// How long before now the delegations of the "later" cases are created: ten minutes, ten times
/// the request token's lifetime.
const EARLIER_SECS: i64 = 600;

fn earlier() -> i64 {
    unix_now() - EARLIER_SECS
}

fn calls(value: &Value) -> OnBehalfCalls {
    OnBehalfCalls::from_json(&value.to_string()).expect("valid call list")
}

fn assistant_calls() -> OnBehalfCalls {
    calls(&json!({"assistant": ["certs", "assets"]}))
}

fn signer() -> Issuer {
    issuer(SEED, &[(SEED, KID)])
}

fn verifier(audience: ServiceName) -> IdentityVerifier {
    IdentityVerifier::new(audience, keys(&[(SEED, KID)]))
}

fn tenant() -> TenantId {
    TENANT.parse().expect("tenant id")
}

fn grant(role: TenantRole) -> Option<TenantGrant> {
    Some(TenantGrant { id: tenant(), role })
}

/// One question to the store: the session, the user and the tenant.
type Asked = (SessionId, UserId, Option<TenantId>);

/// The standing the store answers with, and what it was asked.
#[derive(Clone)]
struct FakeStore {
    answer: Arc<Mutex<Result<Standing, ()>>>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl FakeStore {
    /// A store where the user's session is active and the user edits [`TENANT`].
    fn active() -> Self {
        Self {
            answer: Arc::new(Mutex::new(Ok(Standing::Active {
                tenant: grant(TenantRole::Editor),
                platform_admin: false,
            }))),
            asked: Arc::default(),
        }
    }

    fn set(&self, answer: Result<Standing, ()>) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) = answer;
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl DelegationStore for FakeStore {
    async fn standing(
        &self,
        session: SessionId,
        user: UserId,
        tenant: Option<TenantId>,
    ) -> Result<Standing, StoreError> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((session, user, tenant));
        (*self.answer.lock().unwrap_or_else(PoisonError::into_inner)).map_err(|()| StoreError::Busy)
    }
}

fn router(store: &FakeStore, calls: OnBehalfCalls, clock: fn() -> i64) -> Router {
    routes(RunDelegations::with_clock(
        ExchangeVerifier::new(keys(&[(SEED, KID)])),
        signer(),
        calls,
        store.clone(),
        clock,
    ))
}

/// The claims of the ingress gateway's request to `audience`, from [`SESSION`], by a user who
/// edits [`TENANT`].
fn request_claims(audience: ServiceName, iat: i64) -> InternalClaims {
    InternalClaims {
        iss: ServiceName::Identity,
        sub: USER.parse().expect("user id"),
        aud: audience,
        iat,
        exp: iat + 60,
        tenant: grant(TenantRole::Editor),
        platform_admin: false,
        act: None,
        sid: Some(SESSION.parse().expect("session id")),
    }
}

fn sign_token(claims: &InternalClaims) -> String {
    signer()
        .sign_claims(claims, claims.iat)
        .expect("sign")
        .expose()
        .to_owned()
}

/// The token of a request to the assistant, issued at `iat`.
fn request_token(iat: i64) -> String {
    sign_token(&request_claims(ServiceName::Assistant, iat))
}

/// A run's key, derived in code from a one-byte seed: a throwaway test key.
fn run_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn public(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

/// The delegation claims of a run of the assistant for the request of [`request_claims`].
fn delegation_claims(iat: i64, exp: i64, key: &SigningKey) -> DelegationClaims {
    DelegationClaims {
        sub: USER.parse().expect("user id"),
        sid: SESSION.parse().expect("session id"),
        tenant: Some(tenant()),
        holder: ServiceName::Assistant,
        run: RUN.parse().expect("run id"),
        key: public(key),
        iat,
        exp,
    }
}

/// A delegation signed as the create route would have at `claims.iat`.
fn signed(claims: &DelegationClaims) -> String {
    sign(&signer(), claims, claims.iat)
        .expect("sign")
        .expose()
        .to_owned()
}

fn proof(key: &SigningKey, delegation: &str, audience: ServiceName, iat: i64) -> String {
    let message = run_proof_message(delegation, audience, iat);
    URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes())
}

struct Answer {
    status: StatusCode,
    cache_control: Option<String>,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn delegation(&self) -> String {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        assert_eq!(self.cache_control.as_deref(), Some("no-store"));
        self.json()["delegation"]
            .as_str()
            .expect("a delegation")
            .to_owned()
    }

    /// The exchanged token, through the client's own response type.
    fn token(&self) -> InternalToken {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        assert_eq!(self.cache_control.as_deref(), Some("no-store"));
        serde_json::from_str::<ExchangedToken>(&self.body)
            .expect("an ExchangedToken")
            .token
    }

    fn assert_error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.json()["code"], json!(code), "{}", self.body);
        assert!(self.json().get("token").is_none(), "{}", self.body);
        assert!(self.json().get("delegation").is_none(), "{}", self.body);
        assert_eq!(self.cache_control.as_deref(), Some("no-store"));
    }
}

async fn send(router: &Router, method: Method, uri: &str, headers: &[&str], body: &str) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for value in headers {
        request = request.header(INTERNAL_IDENTITY_HEADER, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let cache_control = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    Answer {
        status,
        cache_control,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn create_body(key: &SigningKey, lifetime: u32) -> String {
    json!({"run_id": RUN, "key": public(key), "lifetime_secs": lifetime}).to_string()
}

async fn create(router: &Router, token: &str, body: &str) -> Answer {
    send(
        router,
        Method::POST,
        CreateRunDelegation::ROUTE,
        &[token],
        body,
    )
    .await
}

/// Exchanges `delegation` for a token to `audience` with a fresh proof of `key`.
async fn exchange_with(
    router: &Router,
    delegation: &str,
    key: &SigningKey,
    audience: ServiceName,
) -> Answer {
    let iat = unix_now();
    let body = json!({
        "audience": audience.as_str(),
        "iat": iat,
        "proof": proof(key, delegation, audience, iat),
    });
    send(
        router,
        Method::POST,
        ExchangeRunDelegation::ROUTE,
        &[delegation],
        &body.to_string(),
    )
    .await
}

/// Acceptance: a delegation created ten minutes ago, long after the request's token expired,
/// still exchanges for a 60-second token addressed to the callee, which the callee accepts; the
/// token carries the user, the tenant and the role read at the exchange, names the assistant as
/// its actor, and has no session.
#[tokio::test]
async fn a_delegation_outlives_the_request_token_and_exchanges_for_minute_tokens() {
    let store = FakeStore::active();
    let key = run_key(9);
    let then = router(&store, assistant_calls(), earlier);
    let now_router = router(&store, assistant_calls(), unix_now);
    let iat = earlier();
    let request = request_token(iat);

    let created = create(&then, &request, &create_body(&key, 900)).await;
    let delegation = created.delegation();
    let expires_at = created.json()["expires_at"].as_i64().expect("expires_at");
    assert!(
        (iat..=iat + 5).contains(&(expires_at - 900)),
        "the delegation lives 900 s from its creation: {expires_at}"
    );
    assert!(!created.body.contains(&request), "the token is not echoed");
    assert!(delegation.starts_with(PREFIX), "{delegation}");
    // The request's token is long gone.
    assert_eq!(
        verifier(ServiceName::Assistant).verify(&request).err(),
        Some(IdentityError::Expired)
    );

    for audience in [ServiceName::Certs, ServiceName::Assets] {
        let token = exchange_with(&now_router, &delegation, &key, audience)
            .await
            .token();
        let user = verifier(audience)
            .verify(token.expose())
            .expect("the callee accepts the token");
        let claims = user.claims();
        let now = unix_now();
        assert!(
            (now - 5..=now).contains(&claims.iat),
            "issued at the exchange"
        );
        assert_eq!(
            claims,
            &InternalClaims {
                iss: ServiceName::Identity,
                sub: USER.parse().expect("user id"),
                aud: audience,
                iat: claims.iat,
                exp: claims.iat + TOKEN_LIFETIME_SECS,
                tenant: grant(TenantRole::Editor),
                platform_admin: false,
                act: Some(Actor {
                    sub: ServiceName::Assistant
                }),
                sid: None,
            }
        );
    }
    // Once at the creation and once per exchange, for the delegation's session, user and
    // tenant.
    let expected = (
        SESSION.parse().expect("session id"),
        USER.parse().expect("user id"),
        Some(tenant()),
    );
    assert_eq!(store.asked(), vec![expected; 3]);
}

/// A token never outlives its delegation.
#[tokio::test]
async fn the_token_expires_with_the_delegation_when_that_is_sooner() {
    let store = FakeStore::active();
    let key = run_key(9);
    let now = unix_now();
    let delegation = signed(&delegation_claims(now - 880, now + 20, &key));
    let token = exchange_with(
        &router(&store, assistant_calls(), unix_now),
        &delegation,
        &key,
        ServiceName::Certs,
    )
    .await
    .token();
    let user = verifier(ServiceName::Certs)
        .verify(token.expose())
        .expect("valid token");
    assert_eq!(user.claims().exp, now + 20);
}

/// Acceptance: after the session is revoked or ends (logout, expiry, the user deleted), the next
/// exchange is refused, and so is a new delegation.
#[tokio::test]
async fn an_ended_session_stops_the_next_exchange() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let delegation = create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .delegation();
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .token();

    store.set(Ok(Standing::NoSession));
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
}

/// Acceptance: an expired delegation, one longer-lived than the cap (even with a valid MAC), and
/// one issued in the future are refused; a lifetime over the cap is never issued.
#[tokio::test]
async fn expired_or_over_long_delegations_are_refused() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let now = unix_now();
    for (iat, exp) in [
        (now - 900, now),
        (now - 900, now - 1),
        (now - 1000, now + 10),
        (now - 10, now + MAX_LIFETIME_SECS),
        (now + 60, now + 120),
        (now + 10, now + 5),
    ] {
        let delegation = signed(&delegation_claims(iat, exp, &key));
        exchange_with(&routes, &delegation, &key, ServiceName::Certs)
            .await
            .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    }
    // At the cap, it works.
    let delegation = signed(&delegation_claims(
        now - 10,
        now - 10 + MAX_LIFETIME_SECS,
        &key,
    ));
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .token();
    assert_eq!(store.asked().len(), 1, "refused before the store is read");

    let request = request_token(now);
    for lifetime in [0, 901, u32::MAX] {
        create(&routes, &request, &create_body(&key, lifetime))
            .await
            .assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let created = create(&routes, &request, &create_body(&key, 1)).await;
    created.delegation();
}

/// Acceptance: after the user's role in the tenant changes, the next token carries the new role
/// and the platform administrator flag read then; after the user is removed from the tenant, the
/// next exchange is refused.
#[tokio::test]
async fn role_changes_show_in_the_next_token_and_a_removed_member_is_refused() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let delegation = create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .delegation();
    let claims_now = |answer: Answer| {
        verifier(ServiceName::Certs)
            .verify(answer.token().expose())
            .expect("valid token")
            .claims()
            .clone()
    };

    let first = claims_now(exchange_with(&routes, &delegation, &key, ServiceName::Certs).await);
    assert_eq!(first.tenant, grant(TenantRole::Editor));
    assert!(!first.platform_admin);

    store.set(Ok(Standing::Active {
        tenant: grant(TenantRole::Viewer),
        platform_admin: true,
    }));
    let second = claims_now(exchange_with(&routes, &delegation, &key, ServiceName::Certs).await);
    assert_eq!(second.tenant, grant(TenantRole::Viewer));
    assert!(second.platform_admin);

    store.set(Ok(Standing::NotMember));
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");
}

/// Acceptance (one hop): a token from an exchange, of either kind, starts no delegation and is
/// exchanged by neither route; a delegation is no token: no service accepts it and the plain
/// token exchange refuses it.
#[tokio::test]
async fn exchanged_tokens_go_no_further_and_a_delegation_is_no_token() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let plain = crate::exchange::routes(Exchange::new(
        ExchangeVerifier::new(keys(&[(SEED, KID)])),
        signer(),
        calls(&json!({"assistant": ["certs"], "certs": ["assistant"]})),
    ));
    let delegation = create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .delegation();
    let exchanged = exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .token();
    // A token another service exchanged for the assistant: it has `act` too.
    let now = unix_now();
    let for_assistant = sign_token(&InternalClaims {
        act: Some(Actor {
            sub: ServiceName::Certs,
        }),
        sid: None,
        ..request_claims(ServiceName::Assistant, now)
    });

    for token in [exchanged.expose(), for_assistant.as_str()] {
        create(&routes, token, &create_body(&key, 600))
            .await
            .assert_error(StatusCode::FORBIDDEN, "permission_denied");
        exchange_with(&routes, token, &key, ServiceName::Certs)
            .await
            .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
        send(
            &plain,
            Method::POST,
            ExchangeToken::ROUTE,
            &[token],
            &json!({"audience": "assets"}).to_string(),
        )
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    }

    for audience in ServiceName::ALL {
        assert!(
            verifier(audience).verify(&delegation).is_err(),
            "{audience}"
        );
    }
    send(
        &plain,
        Method::POST,
        ExchangeToken::ROUTE,
        &[&delegation],
        &json!({"audience": "certs"}).to_string(),
    )
    .await
    .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
}

/// Acceptance (`mesh.calls`): only the audiences the call list lets the holder call are
/// exchanged, and only the holders may start a delegation, whatever the call list says.
#[tokio::test]
async fn only_listed_audiences_and_holders() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(
        &store,
        calls(&json!({"assistant": ["certs"], "assets": ["certs"]})),
        unix_now,
    );
    let delegation = create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .delegation();
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .token();
    for audience in [
        ServiceName::Assets,
        ServiceName::Alerts,
        ServiceName::Assistant,
    ] {
        exchange_with(&routes, &delegation, &key, audience)
            .await
            .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    }

    // Assets may call on behalf of users, but holds no delegation.
    let assets_request = sign_token(&request_claims(ServiceName::Assets, unix_now()));
    create(&routes, &assets_request, &create_body(&key, 600))
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    // A delegation for a holder that is not one: signed here, so only a code change could make
    // it; still refused.
    let now = unix_now();
    let forged = signed(&DelegationClaims {
        holder: ServiceName::Assets,
        ..delegation_claims(now, now + 600, &key)
    });
    exchange_with(&routes, &forged, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");

    // An empty call list: nothing is exchanged.
    let none = router(&store, OnBehalfCalls::default(), unix_now);
    exchange_with(&none, &delegation, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::FORBIDDEN, "permission_denied");
}

/// Acceptance (impersonation and replay): a caller that holds the delegation but not the run's
/// key gets no token: no proof, a proof of another key, a proof captured for another audience,
/// for another delegation or for another time are all refused (a stale or future proof:
/// [`the_proof_window_is_thirty_seconds_either_side`]), and so is a delegation that was altered
/// or signed with a key this service does not accept.
#[tokio::test]
async fn another_caller_with_the_delegation_gets_no_token() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let delegation = create(&routes, &request_token(unix_now()), &create_body(&key, 600))
        .await
        .delegation();
    let other = create(&routes, &request_token(unix_now()), &create_body(&key, 300))
        .await
        .delegation();
    assert_ne!(delegation, other);
    let attempt = |delegation: String, body: Value| {
        let routes = routes.clone();
        async move {
            send(
                &routes,
                Method::POST,
                ExchangeRunDelegation::ROUTE,
                &[&delegation],
                &body.to_string(),
            )
            .await
        }
    };
    let now = unix_now();
    let body = |audience: ServiceName, iat: i64, proof: String| json!({"audience": audience.as_str(), "iat": iat, "proof": proof});
    let certs = ServiceName::Certs;

    for (case, request) in [
        (
            "another key",
            body(certs, now, proof(&run_key(10), &delegation, certs, now)),
        ),
        (
            "a proof for another audience",
            body(
                certs,
                now,
                proof(&key, &delegation, ServiceName::Assets, now),
            ),
        ),
        (
            "a proof for another delegation",
            body(certs, now, proof(&key, &other, certs, now)),
        ),
        (
            "a proof for another time",
            body(certs, now, proof(&key, &delegation, certs, now - 1)),
        ),
        ("an empty proof", body(certs, now, String::new())),
        ("not base64url", body(certs, now, "!".repeat(86))),
        (
            "a short signature",
            body(certs, now, URL_SAFE_NO_PAD.encode([0_u8; 63])),
        ),
    ] {
        let answer = attempt(delegation.clone(), request).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{case}");
        answer.assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    }
    // The proof's audience is the token's: one captured for certs is no use for assets.
    let captured = proof(&key, &delegation, certs, now);
    attempt(delegation.clone(), body(ServiceName::Assets, now, captured))
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");

    // Altered delegations, and delegations a key this service does not accept signed.
    let claims = verify(&signer(), &delegation).expect("own delegation");
    let (signed_part, mac) = delegation.rsplit_once('.').expect("three parts");
    let swapped_key = format!(
        "{PREFIX}{}.{mac}",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&DelegationClaims {
                key: public(&run_key(10)),
                ..claims.clone()
            })
            .expect("json")
        )
    );
    let other_issuer = issuer(SEED + 1, &[(SEED + 1, KID)]);
    let other_service = sign(&other_issuer, &claims, now)
        .expect("sign")
        .expose()
        .to_owned();
    let mut flipped = mac.as_bytes().to_vec();
    flipped[0] = if flipped[0] == b'A' { b'B' } else { b'A' };
    let flipped = format!(
        "{signed_part}.{}",
        String::from_utf8(flipped).expect("ascii")
    );
    for (case, forged, key) in [
        ("the run key swapped", swapped_key, run_key(10)),
        ("another service's key", other_service, key.clone()),
        ("a flipped MAC", flipped, key.clone()),
        ("no MAC", format!("{signed_part}."), key.clone()),
        (
            "another prefix",
            delegation.replacen("trd1.", "trd2.", 1),
            key.clone(),
        ),
        ("garbage", "trd1.e30.AAAA".to_owned(), key.clone()),
    ] {
        let answer = exchange_with(&routes, &forged, &key, certs).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{case}");
        answer.assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    }

    // A revoked generation: the delegation stops at once.
    let revoked = routes_with_issuer(
        &store,
        Issuer::with_keys(
            signing_keys(&[(SEED, KID, 0)]),
            keys(&[(SEED, KID)]).into(),
            2,
        ),
    );
    exchange_with(&revoked, &delegation, &key, certs)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");

    // The delegation twice, or not at all.
    let iat = unix_now();
    let request = body(certs, iat, proof(&key, &delegation, certs, iat)).to_string();
    for headers in [vec![delegation.as_str(), delegation.as_str()], vec![]] {
        send(
            &routes,
            Method::POST,
            ExchangeRunDelegation::ROUTE,
            &headers,
            &request,
        )
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    }
}

/// The fixed "now" of the proof window case: 2026-10-10T00:00:00Z.
const FIXED_NOW: i64 = 1_791_590_400;

fn fixed_now() -> i64 {
    FIXED_NOW
}

/// Acceptance (replay): a proof is accepted up to [`RUN_PROOF_WINDOW_SECS`] before or after the
/// exchange, and refused a second beyond, on a clock the test controls.
#[tokio::test]
async fn the_proof_window_is_thirty_seconds_either_side() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), fixed_now);
    let delegation = signed(&delegation_claims(FIXED_NOW - 10, FIXED_NOW + 600, &key));
    let certs = ServiceName::Certs;
    let window = RUN_PROOF_WINDOW_SECS;
    for (iat, accepted) in [
        (FIXED_NOW - window - 1, false),
        (FIXED_NOW - window, true),
        (FIXED_NOW, true),
        (FIXED_NOW + window, true),
        (FIXED_NOW + window + 1, false),
    ] {
        let body = json!({
            "audience": certs.as_str(),
            "iat": iat,
            "proof": proof(&key, &delegation, certs, iat),
        });
        let answer = send(
            &routes,
            Method::POST,
            ExchangeRunDelegation::ROUTE,
            &[&delegation],
            &body.to_string(),
        )
        .await;
        if accepted {
            answer.token();
        } else {
            answer.assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
        }
    }
}

fn routes_with_issuer(store: &FakeStore, issuer: Issuer) -> Router {
    routes(RunDelegations::new(
        ExchangeVerifier::new(keys(&[(SEED, KID)])),
        issuer,
        assistant_calls(),
        store.clone(),
    ))
}

/// A delegation signed by the key that signed before a key switch keeps working while that key
/// is in the file; the delegation names no key id.
#[tokio::test]
async fn a_delegation_survives_a_key_switch() {
    let store = FakeStore::active();
    let key = run_key(9);
    let now = unix_now();
    let published = keys(&[(SEED, KID), (SEED + 1, "2026-10-b")]);
    let old = Issuer::with_keys(signing_keys(&[(SEED, KID, 0)]), published.clone().into(), 0);
    let switched = Issuer::with_keys(
        signing_keys(&[(SEED, KID, 0), (SEED + 1, "2026-10-b", now - 1)]),
        published.into(),
        0,
    );
    assert_eq!(switched.signing_kid(now).as_deref(), Some("2026-10-b"));
    let delegation = sign(
        &old,
        &delegation_claims(now - 60, now + 600, &key),
        now - 60,
    )
    .expect("sign")
    .expose()
    .to_owned();
    exchange_with(
        &routes_with_issuer(&store, switched),
        &delegation,
        &key,
        ServiceName::Certs,
    )
    .await
    .token();
}

#[tokio::test]
async fn requests_without_one_valid_token_and_a_valid_body_are_refused() {
    let store = FakeStore::active();
    let key = run_key(9);
    let routes = router(&store, assistant_calls(), unix_now);
    let now = unix_now();
    let request = request_token(now);
    let body = create_body(&key, 600);

    for headers in [vec![], vec![request.as_str(), request.as_str()], vec!["x"]] {
        send(
            &routes,
            Method::POST,
            CreateRunDelegation::ROUTE,
            &headers,
            &body,
        )
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    }
    // A request token from before Laysi/trustiot-v2#551, without a session.
    let no_session = sign_token(&InternalClaims {
        sid: None,
        ..request_claims(ServiceName::Assistant, now)
    });
    create(&routes, &no_session, &body)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    // An expired request token.
    create(&routes, &request_token(now - 120), &body)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");

    let small_order = URL_SAFE_NO_PAD.encode({
        let mut point = [0_u8; 32];
        point[0] = 1;
        point
    });
    for invalid in [
        String::new(),
        "{}".to_owned(),
        json!({"run_id": RUN, "key": public(&key)}).to_string(),
        json!({"run_id": "run", "key": public(&key), "lifetime_secs": 60}).to_string(),
        json!({"run_id": RUN, "key": small_order, "lifetime_secs": 60}).to_string(),
        json!({"run_id": RUN, "key": URL_SAFE_NO_PAD.encode([3_u8; 31]), "lifetime_secs": 60})
            .to_string(),
        json!({"run_id": RUN, "key": "not base64!", "lifetime_secs": 60}).to_string(),
        format!(
            "{{\"run_id\":\"{RUN}\",\"key\":\"{}\",\"lifetime_secs\":60,\"pad\":\"{}\"}}",
            public(&key),
            "x".repeat(MAX_BODY_BYTES)
        ),
    ] {
        let answer = create(&routes, &request, &invalid).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{invalid:.80}");
        answer.assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    }

    let delegation = create(&routes, &request, &body).await.delegation();
    for invalid in [
        String::new(),
        json!({"audience": "certs", "iat": now}).to_string(),
        json!({"audience": "stranger", "iat": now, "proof": ""}).to_string(),
    ] {
        send(
            &routes,
            Method::POST,
            ExchangeRunDelegation::ROUTE,
            &[&delegation],
            &invalid,
        )
        .await
        .assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    }

    for uri in [CreateRunDelegation::ROUTE, ExchangeRunDelegation::ROUTE] {
        let answer = send(&routes, Method::GET, uri, &[&request], "").await;
        assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
    }
}

#[tokio::test]
async fn without_keys_or_the_database_it_answers_503() {
    let store = FakeStore::active();
    let key = run_key(9);
    let now = unix_now();
    let request = request_token(now);
    let body = create_body(&key, 600);
    let delegation = signed(&delegation_claims(now, now + 600, &key));

    let no_public_keys = routes(RunDelegations::new(
        ExchangeVerifier::new(PublishedKeys::empty()),
        signer(),
        assistant_calls(),
        store.clone(),
    ));
    create(&no_public_keys, &request, &body)
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");

    let no_signing_keys = routes_with_issuer(&store, Issuer::new(keys(&[(SEED, KID)]).into(), 0));
    create(&no_signing_keys, &request, &body)
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    exchange_with(&no_signing_keys, &delegation, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");

    let routes = router(&store, assistant_calls(), unix_now);
    store.set(Err(()));
    create(&routes, &request, &body)
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    exchange_with(&routes, &delegation, &key, ServiceName::Certs)
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
}

#[test]
fn delegations_round_trip_and_bind_every_claim() {
    let key = run_key(9);
    let claims = delegation_claims(100, 700, &key);
    let delegation = signed(&claims);
    assert_eq!(verify(&signer(), &delegation), Ok(claims.clone()));
    let payload = delegation
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('.'))
        .map(|(payload, _)| payload)
        .expect("payload");
    let json: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url")).expect("json");
    assert_eq!(
        json,
        json!({
            "sub": USER, "sid": SESSION, "tenant": TENANT, "holder": "assistant", "run": RUN,
            "key": public(&key), "iat": 100, "exp": 700
        })
    );
    // An unknown member is refused even under a valid MAC.
    let mut extended = json;
    extended["admin"] = json!(true);
    let text = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(extended.to_string()));
    let mac = signer().sign_delegation(100, text.as_bytes()).expect("mac");
    assert_eq!(
        verify(
            &signer(),
            &format!("{text}.{}", URL_SAFE_NO_PAD.encode(mac))
        ),
        Err(DelegationError::Invalid)
    );
    assert_eq!(
        verify(&Issuer::new(keys(&[(SEED, KID)]).into(), 0), &delegation),
        Err(DelegationError::NoKeys)
    );
}

/// The callee of the end-to-end case: answers with the user it verified.
struct Whoami;

impl Endpoint for Whoami {
    type Service = service::Certs;
    type Body = ();
    type Response = Value;
    const METHOD: CallMethod = CallMethod::Get;
    const ROUTE: &'static str = "/internal/whoami";
}

/// The same request to alerts, which the assistant may not call here.
struct AlertsWhoami;

impl Endpoint for AlertsWhoami {
    type Service = service::Alerts;
    type Body = ();
    type Response = Value;
    const METHOD: CallMethod = CallMethod::Get;
    const ROUTE: &'static str = "/internal/whoami";
}

async fn whoami(user: UserIdentity) -> axum::Json<Value> {
    axum::Json(json!({
        "user": user.user_id().to_string(),
        "tenant": user.tenant_id().map(|id| id.to_string()),
        "audience": user.claims().aud.as_str(),
        "actor": user.claims().act.as_ref().map(|act| act.sub.as_str()),
    }))
}

async fn serve(router: Router) -> String {
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

/// service-kit's `RunDelegations` against these routes: the assistant, serving a user's
/// request, starts a run and calls certs as that user; alerts, which the call list leaves out,
/// fails as `permission_denied`; after the session ends, the run's next call fails as
/// `unauthenticated` and certs is not called.
#[tokio::test]
async fn run_delegations_reach_the_callee_as_the_user() {
    let store = FakeStore::active();
    let identity = serve(router(
        &store,
        calls(&json!({"assistant": ["certs"]})),
        unix_now,
    ))
    .await;
    let certs = serve(
        Router::new()
            .route(Whoami::ROUTE, get(whoami))
            .with_state(verifier(ServiceName::Certs)),
    )
    .await;
    let alerts = serve(
        Router::new()
            .route(Whoami::ROUTE, get(whoami))
            .with_state(verifier(ServiceName::Alerts)),
    )
    .await;
    let user = verifier(ServiceName::Assistant)
        .verify(&request_token(unix_now()))
        .expect("the request's identity");
    let delegations = Client::new(client(&identity));

    let run = delegations
        .start(
            &user,
            RUN.parse().expect("run id"),
            Duration::from_secs(600),
        )
        .await
        .expect("a delegation");
    let seen = run
        .call(&client::<service::Certs>(&certs), &Whoami)
        .await
        .expect("certs answers");
    assert_eq!(
        seen,
        json!({"user": USER, "tenant": TENANT, "audience": "certs", "actor": "assistant"})
    );

    let error = run
        .call(&client::<service::Alerts>(&alerts), &AlertsWhoami)
        .await
        .expect_err("not on the call list");
    assert!(
        matches!(
            error,
            RunDelegationError::Exchange {
                audience: ServiceName::Alerts,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), ErrorCode::PermissionDenied);

    store.set(Ok(Standing::NoSession));
    let error = run
        .call(&client::<service::Certs>(&certs), &Whoami)
        .await
        .expect_err("the session ended");
    assert_eq!(error.code(), ErrorCode::Unauthenticated);
    let error = delegations
        .start(
            &user,
            RUN.parse().expect("run id"),
            Duration::from_secs(600),
        )
        .await
        .expect_err("the session ended");
    assert_eq!(error.code(), ErrorCode::Unauthenticated);
}
