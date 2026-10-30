//! The token exchange in process: every outcome, the lifetime rule, the replay refusals of the
//! verifiers, and service-kit's `OnBehalf` client against this exchange and a callee that
//! verifies with `UserIdentity`. `tests/ext_authz.rs` repeats the main case with the real binary.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request};
use axum::routing::get;
use contract::api::identity::ExchangedToken;
use contract::client::{Endpoint, Method as CallMethod};
use contract::{
    INTERNAL_IDENTITY_HEADER, InternalClient, Service, TenantGrant, TenantRole, service,
};
use serde_json::{Value, json};
use service_kit::identity::{
    IdentityVerifier, OnBehalf, OnBehalfError, PublishedKeys, UserIdentity,
};
use tower::ServiceExt;

use super::*;
use crate::issuer::test_keys::{issuer, keys};
use crate::issuer::unix_now;

const SEED: u8 = 7;
const KID: &str = "2026-10-a";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

fn calls(value: &Value) -> OnBehalfCalls {
    OnBehalfCalls::from_json(&value.to_string()).expect("valid call list")
}

fn signer() -> Issuer {
    issuer(SEED, &[(SEED, KID)])
}

fn exchange(list: &Value) -> Router {
    routes(Exchange::new(
        ExchangeVerifier::new(keys(&[(SEED, KID)])),
        signer(),
        calls(list),
    ))
}

fn verifier(audience: ServiceName) -> IdentityVerifier {
    IdentityVerifier::new(audience, keys(&[(SEED, KID)]))
}

/// The claims of a user who edits [`TENANT`] and is a platform administrator.
fn claims(audience: ServiceName, iat: i64, exp: i64) -> InternalClaims {
    InternalClaims {
        iss: ServiceName::Identity,
        sub: USER.parse().expect("user id"),
        aud: audience,
        iat,
        exp,
        tenant: Some(TenantGrant {
            id: TENANT.parse().expect("tenant id"),
            role: TenantRole::Editor,
        }),
        platform_admin: true,
        act: None,
        sid: None,
        tenant_suspended: false,
    }
}

/// The claims of an exchanged token: [`claims`] for `audience`, acted for by `actor`.
fn acted(audience: ServiceName, actor: ServiceName, iat: i64, exp: i64) -> InternalClaims {
    InternalClaims {
        act: Some(Actor { sub: actor }),
        ..claims(audience, iat, exp)
    }
}

/// A token this service signed for the ingress gateway's request, as the ext_authz check would.
fn token(audience: ServiceName, iat: i64, exp: i64) -> String {
    signer()
        .sign_claims(&claims(audience, iat, exp), iat)
        .expect("sign")
        .expose()
        .to_owned()
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

    /// The exchanged token, through the client's own response type.
    fn token(&self) -> InternalToken {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        serde_json::from_str::<ExchangedToken>(&self.body)
            .expect("an ExchangedToken")
            .token
    }

    fn assert_error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.json()["code"], json!(code), "{}", self.body);
        assert!(self.json().get("token").is_none(), "{}", self.body);
        assert_eq!(self.cache_control.as_deref(), Some("no-store"));
    }
}

async fn send(router: &Router, method: Method, tokens: &[&str], body: &str) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(ExchangeToken::ROUTE)
        .header("content-type", "application/json");
    for token in tokens {
        request = request.header(INTERNAL_IDENTITY_HEADER, *token);
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

async fn exchange_for(router: &Router, token: &str, audience: &str) -> Answer {
    send(
        router,
        Method::POST,
        &[token],
        &json!({ "audience": audience }).to_string(),
    )
    .await
}

/// Acceptance (on-behalf calls): a listed caller gets a token for the callee that carries the
/// request's user, tenant, role and administrator flag, names the caller as its actor, and
/// expires with the request's token.
#[tokio::test]
async fn a_listed_caller_gets_a_token_for_the_callee_with_the_same_identity() {
    let router = exchange(&json!({"assets": ["certs"]}));
    let now = unix_now();
    let subject = token(ServiceName::Assets, now - 10, now + 50);

    let answer = exchange_for(&router, &subject, "certs").await;
    assert_eq!(answer.cache_control.as_deref(), Some("no-store"));
    assert!(
        !answer.body.contains(&subject),
        "the caller's token is not echoed"
    );
    let exchanged = answer.token();
    let user = verifier(ServiceName::Certs)
        .verify(exchanged.expose())
        .expect("certs accepts the exchanged token");
    let expected = acted(
        ServiceName::Certs,
        ServiceName::Assets,
        user.claims().iat,
        now + 50,
    );
    assert_eq!(user.claims(), &expected);
    assert!(
        (now..=now + 5).contains(&user.claims().iat),
        "issued at the exchange: {}",
        user.claims().iat
    );
}

/// Acceptance (replay): a token works only at its audience and only while it lives. Neither the
/// request's token nor an exchanged one is accepted by any other service, and an expired token
/// is refused by its own audience and by the exchange.
#[tokio::test]
async fn tokens_are_refused_by_other_audiences_and_once_expired() {
    let router = exchange(&json!({"assets": ["certs"]}));
    let now = unix_now();
    let subject = token(ServiceName::Assets, now, now + 60);
    let exchanged = exchange_for(&router, &subject, "certs").await.token();

    for audience in ServiceName::ALL {
        let for_subject = verifier(audience).verify(&subject);
        let for_exchanged = verifier(audience).verify(exchanged.expose());
        match audience {
            ServiceName::Assets => {
                assert!(for_subject.is_ok());
                assert_eq!(for_exchanged.err(), Some(IdentityError::WrongAudience));
            }
            ServiceName::Certs => {
                assert_eq!(for_subject.err(), Some(IdentityError::WrongAudience));
                assert!(for_exchanged.is_ok());
            }
            _ => {
                assert_eq!(for_subject.err(), Some(IdentityError::WrongAudience));
                assert_eq!(for_exchanged.err(), Some(IdentityError::WrongAudience));
            }
        }
    }

    let expired = token(ServiceName::Certs, now - 120, now - 60);
    assert_eq!(
        verifier(ServiceName::Certs).verify(&expired).err(),
        Some(IdentityError::Expired)
    );
    let expired_subject = token(ServiceName::Assets, now - 120, now - 60);
    exchange_for(&router, &expired_subject, "certs")
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
}

/// Acceptance (impersonation, the application's part): only the pairs of the call list are
/// exchanged, for the audience of the token the ingress gateway's request brought. The exchange
/// cannot tell which workload presents a token, so the list binds the token's audience, not the
/// connection; an exchanged token is never exchanged again (below), and chains of more than one
/// hop need the exchange to authenticate the calling workload (Laysi/trustiot-v2#509).
#[tokio::test]
async fn only_listed_pairs_are_exchanged() {
    let router = exchange(&json!({"assets": ["certs"], "certs": ["alerts"]}));
    let now = unix_now();
    let refused = [
        (ServiceName::Assets, "alerts"),
        (ServiceName::Certs, "assets"),
        (ServiceName::Alerts, "certs"),
        (ServiceName::Knowledge, "certs"),
        (ServiceName::Identity, "certs"),
        (ServiceName::Assets, "assets"),
    ];
    for (holder, audience) in refused {
        exchange_for(&router, &token(holder, now, now + 60), audience)
            .await
            .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    }
    let allowed = exchange_for(&router, &token(ServiceName::Certs, now, now + 60), "alerts")
        .await
        .token();
    assert!(
        verifier(ServiceName::Alerts)
            .verify(allowed.expose())
            .is_ok()
    );
}

/// One hop only: a token that came from the exchange is never exchanged again, even along calls
/// the list has, so the list is not applied transitively and a chain or a cycle cannot extend a
/// request's identity.
#[tokio::test]
async fn an_exchanged_token_is_never_exchanged_again() {
    let router = exchange(&json!({"assets": ["certs"], "certs": ["assets", "alerts"]}));
    let now = unix_now();
    let request = token(ServiceName::Assets, now - 30, now + 30);
    let at_certs = exchange_for(&router, &request, "certs").await.token();
    let user = verifier(ServiceName::Certs)
        .verify(at_certs.expose())
        .expect("verifies");
    assert_eq!(user.claims().exp, now + 30);
    for audience in ["assets", "alerts", "certs"] {
        exchange_for(&router, at_certs.expose(), audience)
            .await
            .assert_error(StatusCode::FORBIDDEN, "permission_denied");
    }
}

/// Laysi/trustiot-v2#777: a token of a suspended tenant (which only this service gets) is never
/// exchanged, even along a listed call, and the caller learns why.
#[tokio::test]
async fn a_token_of_a_suspended_tenant_is_never_exchanged() {
    let router = exchange(&json!({"identity": ["assets"], "assets": ["certs"]}));
    let now = unix_now();
    for audience in [ServiceName::Identity, ServiceName::Assets] {
        let suspended = InternalClaims {
            tenant_suspended: true,
            ..claims(audience, now, now + 60)
        };
        let token = signer()
            .sign_claims(&suspended, now)
            .expect("sign")
            .expose()
            .to_owned();
        let callee = if audience == ServiceName::Identity {
            "assets"
        } else {
            "certs"
        };
        exchange_for(&router, &token, callee)
            .await
            .assert_error(StatusCode::FORBIDDEN, "tenant_suspended");
    }
    // The same user's token without the claim goes through.
    let active = token(ServiceName::Assets, now, now + 60);
    assert_eq!(
        exchange_for(&router, &active, "certs").await.status,
        StatusCode::OK
    );
}

#[test]
fn the_new_token_expires_with_the_old_one_and_at_most_a_minute_from_now() {
    let now = 1_791_590_400;
    let subject = |exp| claims(ServiceName::Assets, now - 5, exp);
    let new_claims =
        exchanged_claims(&subject(now + 20), ServiceName::Certs, now).expect("20 s left");
    assert_eq!(
        new_claims,
        acted(ServiceName::Certs, ServiceName::Assets, now, now + 20)
    );
    // A subject that lives longer (the verifiers accept up to 300 s) is capped.
    let capped = exchanged_claims(&subject(now + 290), ServiceName::Certs, now).expect("capped");
    assert_eq!(capped.exp, now + TOKEN_LIFETIME_SECS);
    assert_eq!(
        exchanged_claims(&subject(now + 1), ServiceName::Certs, now).map(|c| c.exp - c.iat),
        Some(1)
    );
    // Expired, or within the verifiers' leeway after expiry: nothing to exchange.
    for exp in [now, now - 3, i64::MIN] {
        assert_eq!(
            exchanged_claims(&subject(exp), ServiceName::Certs, now),
            None,
            "{exp}"
        );
    }
}

/// A token the verifiers still accept (within their 5 s of leeway) but that has run out is not
/// exchanged.
#[tokio::test]
async fn a_token_without_a_second_left_is_not_exchanged() {
    let router = exchange(&json!({"assets": ["certs"]}));
    let now = unix_now();
    let subject = token(ServiceName::Assets, now - 60, now - 1);
    assert!(
        verifier(ServiceName::Assets).verify(&subject).is_ok(),
        "the verifiers' leeway still accepts it"
    );
    exchange_for(&router, &subject, "certs")
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
}

#[tokio::test]
async fn requests_without_one_valid_token_and_a_valid_body_are_refused() {
    let router = exchange(&json!({"assets": ["certs"]}));
    let now = unix_now();
    let subject = token(ServiceName::Assets, now, now + 60);
    let body = json!({"audience": "certs"}).to_string();

    send(&router, Method::POST, &[], &body)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    send(&router, Method::POST, &[&subject, &subject], &body)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    // Signed by a key nobody published.
    let forged = issuer(9, &[(9, KID)])
        .sign_claims(&claims(ServiceName::Assets, now, now + 60), now)
        .expect("sign");
    send(&router, Method::POST, &[forged.expose()], &body)
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");
    // The token is checked before the body.
    send(&router, Method::POST, &[], "not json")
        .await
        .assert_error(StatusCode::UNAUTHORIZED, "unauthenticated");

    let oversized = json!({"audience": "certs", "padding": "x".repeat(MAX_BODY_BYTES)}).to_string();
    for invalid in [
        "",
        "not json",
        "[]",
        r#"{"audience": "ai-gateway"}"#,
        r#"{"audience": null}"#,
        oversized.as_str(),
    ] {
        send(&router, Method::POST, &[&subject], invalid)
            .await
            .assert_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    assert_eq!(
        send(&router, Method::GET, &[&subject], "").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn without_keys_or_a_signing_key_it_answers_503() {
    let now = unix_now();
    let subject = token(ServiceName::Assets, now, now + 60);
    let no_keys = routes(Exchange::new(
        ExchangeVerifier::new(PublishedKeys::empty()),
        signer(),
        calls(&json!({"assets": ["certs"]})),
    ));
    exchange_for(&no_keys, &subject, "certs")
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    let no_signing_key = routes(Exchange::new(
        ExchangeVerifier::new(keys(&[(SEED, KID)])),
        Issuer::new(keys(&[(SEED, KID)]).into(), 0),
        calls(&json!({"assets": ["certs"]})),
    ));
    exchange_for(&no_signing_key, &subject, "certs")
        .await
        .assert_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
}

#[test]
fn the_call_list_names_services_that_receive_tokens_only() {
    let list = calls(&json!({"assets": ["certs", "alerts"], "certs": [], "identity": ["assets"]}));
    assert_eq!(list.len(), 3);
    assert!(list.allows(ServiceName::Assets, ServiceName::Certs));
    assert!(list.allows(ServiceName::Identity, ServiceName::Assets));
    assert!(!list.allows(ServiceName::Certs, ServiceName::Assets));
    assert!(calls(&json!({})).is_empty());
    assert_eq!(OnBehalfCalls::default(), calls(&json!({})));

    for invalid in [
        json!({"ai-gateway": ["assets"]}),
        json!({"assets": ["ai-gateway"]}),
        json!({"assets": ["stranger"]}),
    ] {
        assert_eq!(
            OnBehalfCalls::from_json(&invalid.to_string()),
            Err(InvalidCalls::Service),
            "{invalid}"
        );
    }
    for invalid in [
        json!([]),
        json!({"assets": "certs"}),
        json!({"assets": [1]}),
        json!(null),
    ] {
        let error = OnBehalfCalls::from_json(&invalid.to_string()).expect_err("not a list");
        assert!(matches!(error, InvalidCalls::NotCalls(_)), "{invalid}");
    }
    let error = OnBehalfCalls::from_json(r#"{"assets": ["secret-looking-name"]}"#)
        .expect_err("unknown service")
        .to_string();
    assert!(!error.contains("secret-looking-name"), "{error}");
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

/// The same request to alerts, which assets may not call.
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

/// service-kit's `OnBehalf` against this exchange: assets, serving a user's request, calls certs
/// on the user's behalf and certs sees that user; a call to alerts, which assets may not call,
/// gets no token and fails as `internal`.
#[tokio::test]
async fn on_behalf_calls_reach_the_callee_as_the_same_user() {
    let identity = serve(exchange(&json!({"assets": ["certs"]}))).await;
    let callee = Router::new()
        .route(Whoami::ROUTE, get(whoami))
        .with_state(verifier(ServiceName::Certs));
    let certs = serve(callee).await;
    let alerts = serve(
        Router::new()
            .route(Whoami::ROUTE, get(whoami))
            .with_state(verifier(ServiceName::Alerts)),
    )
    .await;
    let now = unix_now();
    let user = verifier(ServiceName::Assets)
        .verify(&token(ServiceName::Assets, now, now + 60))
        .expect("the request's identity");
    let on_behalf = OnBehalf::new(client(&identity));

    let seen = on_behalf
        .call(&client::<service::Certs>(&certs), &Whoami, &user)
        .await
        .expect("certs answers");
    assert_eq!(
        seen,
        json!({"user": USER, "tenant": TENANT, "audience": "certs"})
    );

    let error = on_behalf
        .call(&client::<service::Alerts>(&alerts), &AlertsWhoami, &user)
        .await
        .expect_err("assets may not call alerts");
    assert!(
        matches!(
            error,
            OnBehalfError::Exchange {
                audience: ServiceName::Alerts,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), ErrorCode::Internal);
}
