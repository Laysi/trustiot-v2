//! The check endpoint against an in-memory session store: every outcome, the identity header
//! rules, and the deadline. `tests/ext_authz.rs` repeats the main cases with the real binary and
//! Postgres.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Method, Request};
use contract::{ServiceName, TenantGrant, TenantRole, UserId};
use serde_json::{Value, json};
use service_kit::identity::{IdentityError, IdentityVerifier};
use tower::ServiceExt;

use super::*;
use crate::issuer::test_keys::{issuer, keys};
use crate::issuer::{TOKEN_LIFETIME_SECS, unix_now};
use crate::session::{
    SESSION_COOKIE, SessionGrant, SessionHash, SessionSecret, SessionStore, StoreError,
};

const APP: &str = "app.example.com";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const KID: &str = "2026-10-a";

/// Headers that a case adds to the check request.
type CaseHeaders = &'static [(&'static str, &'static str)];

/// What the in-memory store does for every lookup.
#[derive(Clone, Copy)]
enum Behaviour {
    Answer,
    Fail,
    Hang,
}

/// Sessions by hash, with a lookup counter.
struct MemoryStore {
    sessions: Mutex<HashMap<[u8; 32], SessionGrant>>,
    lookups: AtomicUsize,
    behaviour: Behaviour,
}

impl MemoryStore {
    fn new(behaviour: Behaviour) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            lookups: AtomicUsize::new(0),
            behaviour,
        }
    }

    fn add(&self, secret: &SessionSecret, grant: SessionGrant) {
        self.sessions
            .lock()
            .expect("sessions")
            .insert(*secret.hash().as_bytes(), grant);
    }
}

impl SessionStore for Arc<MemoryStore> {
    async fn lookup(&self, hash: SessionHash) -> Result<Option<SessionGrant>, StoreError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        match self.behaviour {
            Behaviour::Answer => Ok(self
                .sessions
                .lock()
                .expect("sessions")
                .get(hash.as_bytes())
                .cloned()),
            Behaviour::Fail => Err(StoreError::Inconsistent("test failure")),
            Behaviour::Hang => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(None)
            }
        }
    }

    async fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

fn grant() -> SessionGrant {
    SessionGrant {
        session: contract::SessionId::generate(),
        user: USER.parse().expect("user"),
        tenant: Some(TenantGrant {
            id: TENANT.parse().expect("tenant"),
            role: TenantRole::Editor,
        }),
        platform_admin: false,
    }
}

fn routes_table() -> IngressRoutes {
    IngressRoutes::from_json(
        &json!([
            {"host": APP, "path": "/", "match": "PathPrefix", "service": "assets"},
            {"host": APP, "path": "/api/certs", "match": "PathPrefix", "service": "certs"},
            {"host": "gw.example.com", "path": "/api/gw", "match": "PathPrefix", "service": "gateway-ops"},
        ])
        .to_string(),
    )
    .expect("routes")
}

struct Fixture {
    store: Arc<MemoryStore>,
    router: Router,
    secret: SessionSecret,
}

impl Fixture {
    fn new(behaviour: Behaviour) -> Self {
        Self::with_clock(behaviour, unix_now)
    }

    fn with_clock(behaviour: Behaviour, clock: fn() -> i64) -> Self {
        let store = Arc::new(MemoryStore::new(behaviour));
        let secret = SessionSecret::from_bytes([42; 32]);
        store.add(&secret, grant());
        let ext_authz = ExtAuthz::with_clock(
            Arc::clone(&store),
            issuer(1, &[(1, KID)]),
            routes_table(),
            clock,
        );
        Self {
            store,
            router: routes(ext_authz),
            secret,
        }
    }

    fn cookie(&self) -> String {
        format!("{SESSION_COOKIE}={}", self.secret.to_cookie_value())
    }

    async fn check(&self, method: Method, target: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::builder().method(method).uri(target);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.router
            .clone()
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("infallible")
    }

    /// A check as the gateway sends it: Host and the session cookie.
    async fn allowed(&self, target: &str, host: &str) -> Response {
        let cookie = self.cookie();
        self.check(Method::GET, target, &[("host", host), ("cookie", &cookie)])
            .await
    }
}

fn verifier(audience: ServiceName) -> IdentityVerifier {
    IdentityVerifier::new(audience, keys(&[(1, KID)]))
}

/// Every `x-internal-identity` value of a response.
fn identity_headers(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(INTERNAL_IDENTITY_HEADER)
        .iter()
        .map(|value| value.to_str().expect("ascii").to_owned())
        .collect()
}

async fn body_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("JSON body")
}

/// Acceptance: a valid session gets exactly one identity header, and it passes
/// Laysi/trustiot-v2#52's verification with the session's user, tenant, role and admin flag.
#[tokio::test]
async fn an_active_session_gets_one_verified_identity_for_the_routed_service() {
    let fixture = Fixture::new(Behaviour::Answer);
    let response = fixture
        .allowed("/internal/ext-authz/api/certs/42?expand=all", APP)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .map(HeaderValue::as_bytes),
        Some(b"no-store".as_slice())
    );
    let tokens = identity_headers(&response);
    let [token] = tokens.as_slice() else {
        panic!("expected exactly one identity header, got {tokens:?}");
    };
    let user = verifier(ServiceName::Certs)
        .verify(token)
        .expect("certs accepts the token");
    assert_eq!(user.user_id(), grant().user);
    assert_eq!(user.tenant(), grant().tenant);
    assert!(!user.is_platform_admin());
    assert_eq!(user.claims().exp - user.claims().iat, TOKEN_LIFETIME_SECS);
    assert!((user.claims().iat - unix_now()).abs() <= 2);
    // Addressed to certs only: the backend of another route refuses it.
    assert_eq!(
        verifier(ServiceName::Assets).verify(token).err(),
        Some(IdentityError::WrongAudience)
    );
}

/// Acceptance: when the request already carries identity headers (any spelling), the allow
/// still holds only the token this service signed.
#[tokio::test]
async fn forged_identity_headers_never_reach_the_allow() {
    let fixture = Fixture::new(Behaviour::Answer);
    let forger = issuer(2, &[(2, KID)]);
    let other_user = SessionGrant {
        session: contract::SessionId::generate(),
        user: "01928f6e-7a3c-7d4e-8f00-ffffffffffff"
            .parse()
            .expect("user"),
        tenant: None,
        platform_admin: true,
    };
    let forged_signed = forger
        .issue(&other_user, ServiceName::Assets, unix_now())
        .expect("forged token");
    let genuine_for_other = issuer(1, &[(1, KID)])
        .issue(&other_user, ServiceName::Assets, unix_now())
        .expect("token of another session");
    let cookie = fixture.cookie();
    for forged in ["forged", forged_signed.expose(), genuine_for_other.expose()] {
        let response = fixture
            .check(
                Method::POST,
                "/internal/ext-authz/api/devices",
                &[
                    ("host", APP),
                    ("cookie", &cookie),
                    ("x-internal-identity", forged),
                    ("x-internal-identity", forged),
                    ("x_internal_identity", forged),
                    ("x-internal_identity", forged),
                    ("x_internal-identity", forged),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let tokens = identity_headers(&response);
        let [token] = tokens.as_slice() else {
            panic!("expected exactly one identity header, got {tokens:?}");
        };
        assert_ne!(token, forged);
        let user = verifier(ServiceName::Assets)
            .verify(token)
            .expect("genuine");
        assert_eq!(user.user_id(), grant().user, "the session decides the user");
        assert!(!user.is_platform_admin());
        for name in [
            "x_internal_identity",
            "x-internal_identity",
            "x_internal-identity",
        ] {
            assert!(response.headers().get(name).is_none(), "{name}");
        }
    }
}

/// Acceptance: invalid sessions get 401 `unauthenticated` with no identity header.
#[tokio::test]
async fn sessions_that_are_missing_unknown_or_malformed_get_401() {
    let fixture = Fixture::new(Behaviour::Answer);
    let unknown = format!(
        "{SESSION_COOKIE}={}",
        SessionSecret::from_bytes([43; 32]).to_cookie_value()
    );
    let repeated = format!("{}; {}", fixture.cookie(), fixture.cookie());
    let cases: Vec<(Vec<(&str, String)>, usize)> = vec![
        (vec![], 0),
        (vec![("cookie", "theme=dark".to_owned())], 0),
        (vec![("cookie", format!("{SESSION_COOKIE}=valid-abc"))], 0),
        (
            vec![(
                "cookie",
                format!("sid={}", fixture.secret.to_cookie_value()),
            )],
            0,
        ),
        (vec![("cookie", repeated)], 0),
        (
            vec![(
                "authorization",
                format!("Bearer {}", fixture.secret.to_cookie_value()),
            )],
            0,
        ),
        // Well-formed but unknown (or expired, or revoked: the store does not return those).
        (vec![("cookie", unknown)], 1),
    ];
    for (extra, lookups) in cases {
        let before = fixture.store.lookups.load(Ordering::SeqCst);
        let mut headers = vec![("host", APP.to_owned())];
        headers.extend(extra.iter().cloned());
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let response = fixture
            .check(Method::GET, "/internal/ext-authz/api/devices", &borrowed)
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{extra:?}");
        assert!(identity_headers(&response).is_empty());
        assert_eq!(
            body_json(response).await,
            json!({"code": "unauthenticated", "message_key": "error.unauthenticated"})
        );
        // Cookies that cannot be a session never reach the database.
        assert_eq!(
            fixture.store.lookups.load(Ordering::SeqCst) - before,
            lookups,
            "{extra:?}"
        );
    }
}

#[tokio::test]
async fn a_session_without_a_route_for_the_request_gets_403() {
    let fixture = Fixture::new(Behaviour::Answer);
    for (host, target) in [
        ("gw.example.com", "/internal/ext-authz/api/other"),
        ("unknown.example.com", "/internal/ext-authz/api/certs"),
        ("app.example.com.evil", "/internal/ext-authz/"),
    ] {
        let response = fixture.allowed(target, host).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}{target}");
        assert!(identity_headers(&response).is_empty());
        assert_eq!(
            body_json(response).await,
            json!({"code": "permission_denied", "message_key": "error.permission_denied"})
        );
    }
    // Two Host headers are not guessed between.
    let cookie = fixture.cookie();
    let response = fixture
        .check(
            Method::GET,
            "/internal/ext-authz/",
            &[
                ("host", APP),
                ("host", "gw.example.com"),
                ("cookie", &cookie),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An unauthenticated request learns nothing about routes: 401 comes first.
    let response = fixture
        .check(
            Method::GET,
            "/internal/ext-authz/x",
            &[("host", "unknown.example.com")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn every_method_and_path_shape_of_the_check_is_served() {
    let fixture = Fixture::new(Behaviour::Answer);
    let cookie = fixture.cookie();
    for method in [
        Method::GET,
        Method::HEAD,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        for (target, audience) in [
            ("/internal/ext-authz", ServiceName::Assets),
            ("/internal/ext-authz/", ServiceName::Assets),
            ("/internal/ext-authz?x=1", ServiceName::Assets),
            ("/internal/ext-authz/api/certs", ServiceName::Certs),
            ("/internal/ext-authz/api/certs/", ServiceName::Certs),
            (
                "/internal/ext-authz/api/certs/a/b?c=d/e",
                ServiceName::Certs,
            ),
            ("/internal/ext-authz/api/certsx", ServiceName::Assets),
            ("/internal/ext-authz//api/certs", ServiceName::Assets),
        ] {
            let response = fixture
                .check(
                    method.clone(),
                    target,
                    &[("host", "APP.example.com:443"), ("cookie", &cookie)],
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK, "{method} {target}");
            let tokens = identity_headers(&response);
            let [token] = tokens.as_slice() else {
                panic!("{method} {target}: {tokens:?}");
            };
            assert!(
                verifier(audience).verify(token).is_ok(),
                "{method} {target}"
            );
        }
    }
}

/// The router serves nothing but the check: no login, no session creation, no fake login
/// (sessions come only from the test tooling in MS0).
#[tokio::test]
async fn nothing_but_the_check_is_served() {
    let fixture = Fixture::new(Behaviour::Answer);
    let cookie = fixture.cookie();
    for target in [
        "/",
        "/auth/login",
        "/auth/dev-login",
        "/v1/sessions",
        "/internal/sessions",
        "/internal/ext-authzx",
        "/internal/ext-authz-other/x",
        "/internal",
    ] {
        for method in [Method::GET, Method::POST] {
            let response = fixture
                .check(
                    method.clone(),
                    target,
                    &[("host", APP), ("cookie", &cookie)],
                )
                .await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {target}"
            );
            assert!(identity_headers(&response).is_empty());
        }
    }
}

#[tokio::test]
async fn a_failing_store_gets_503_without_an_identity() {
    let fixture = Fixture::new(Behaviour::Fail);
    let response = fixture.allowed("/internal/ext-authz/", APP).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(identity_headers(&response).is_empty());
    assert_eq!(
        body_json(response).await,
        json!({"code": "unavailable", "message_key": "error.unavailable"})
    );
}

/// Without a key that may sign (none loaded yet, or every one revoked) an active session gets
/// 503 and no identity: the check fails closed, as it does when the store fails.
#[tokio::test]
async fn no_signing_key_gets_503_without_an_identity() {
    let store = Arc::new(MemoryStore::new(Behaviour::Answer));
    let secret = SessionSecret::from_bytes([42; 32]);
    store.add(&secret, grant());
    let published = service_kit::identity::PublishedKeys::from(keys(&[(1, KID)]));
    let router = routes(ExtAuthz::new(
        Arc::clone(&store),
        Issuer::new(published, 0),
        routes_table(),
    ));
    let cookie = format!("{SESSION_COOKIE}={}", secret.to_cookie_value());
    let request = Request::builder()
        .uri("/internal/ext-authz/")
        .header("host", APP)
        .header("cookie", &cookie)
        .body(Body::empty())
        .expect("request");
    let response = router.oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(identity_headers(&response).is_empty());
    assert_eq!(
        body_json(response).await,
        json!({"code": "unavailable", "message_key": "error.unavailable"})
    );
}

/// The gateway gives up after 0.5 s; the check answers 503 before that.
#[tokio::test]
async fn a_hanging_store_gets_503_within_the_deadline() {
    let fixture = Fixture::new(Behaviour::Hang);
    let started = std::time::Instant::now();
    let response = fixture.allowed("/internal/ext-authz/", APP).await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(identity_headers(&response).is_empty());
    assert!(elapsed >= CHECK_DEADLINE, "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
}

/// A token captured from a request works at its service for its lifetime plus the verifiers'
/// clock skew, then never again.
#[tokio::test]
async fn an_issued_token_expires_after_the_replay_window() {
    fn sixty_six_seconds_ago() -> i64 {
        unix_now() - TOKEN_LIFETIME_SECS - 6
    }
    let fixture = Fixture::with_clock(Behaviour::Answer, sixty_six_seconds_ago);
    let response = fixture.allowed("/internal/ext-authz/", APP).await;
    let tokens = identity_headers(&response);
    let [token] = tokens.as_slice() else {
        panic!("{tokens:?}");
    };
    assert_eq!(
        verifier(ServiceName::Assets).verify(token).err(),
        Some(IdentityError::Expired)
    );
}

#[tokio::test]
async fn grants_without_a_tenant_or_with_admin_rights_are_signed_as_they_are() {
    let fixture = Fixture::new(Behaviour::Answer);
    let secret = SessionSecret::from_bytes([44; 32]);
    let admin = SessionGrant {
        session: contract::SessionId::generate(),
        user: UserId::from_uuid(Default::default()),
        tenant: None,
        platform_admin: true,
    };
    fixture.store.add(&secret, admin.clone());
    let cookie = format!("{SESSION_COOKIE}={}", secret.to_cookie_value());
    let response = fixture
        .check(
            Method::GET,
            "/internal/ext-authz/",
            &[("host", APP), ("cookie", &cookie)],
        )
        .await;
    let tokens = identity_headers(&response);
    let [token] = tokens.as_slice() else {
        panic!("{tokens:?}");
    };
    let user = verifier(ServiceName::Assets).verify(token).expect("valid");
    assert_eq!(user.user_id(), admin.user);
    assert_eq!(user.tenant(), None);
    assert!(user.is_platform_admin());
}

/// Acceptance (Laysi/trustiot-v2#123): a cross-site request with a valid session gets 403
/// `security.cross_site_request`, without an identity and before the session is looked up.
#[tokio::test]
async fn a_cross_site_request_gets_403_before_the_session_is_looked_up() {
    let fixture = Fixture::new(Behaviour::Answer);
    let cookie = fixture.cookie();
    let cases: [(Method, CaseHeaders); 7] = [
        (Method::POST, &[("origin", "https://attacker.example.test")]),
        (Method::POST, &[("sec-fetch-site", "cross-site")]),
        (Method::DELETE, &[("sec-fetch-site", "same-site")]),
        (Method::POST, &[("sec-fetch-site", "none")]),
        (Method::GET, &[("sec-fetch-site", "cross-site")]),
        (Method::PUT, &[("origin", "null")]),
        (
            Method::PATCH,
            &[
                ("origin", "https://app.example.com"),
                ("origin", "https://attacker.example.test"),
            ],
        ),
    ];
    for (method, extra) in cases {
        let mut headers = vec![("host", APP), ("cookie", cookie.as_str())];
        headers.extend_from_slice(extra);
        let before = fixture.store.lookups.load(Ordering::SeqCst);
        let response = fixture
            .check(method.clone(), "/internal/ext-authz/api/devices", &headers)
            .await;
        assert_eq!(
            fixture.store.lookups.load(Ordering::SeqCst),
            before,
            "{method} {extra:?}"
        );
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {extra:?}"
        );
        assert!(identity_headers(&response).is_empty());
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(HeaderValue::as_bytes),
            Some(b"no-store".as_slice())
        );
        assert_eq!(
            body_json(response).await,
            json!({"code": "permission_denied", "message_key": "security.cross_site_request"}),
            "{method} {extra:?}"
        );
    }
    // Without a session too: the request is refused for where it comes from, not for the cookie.
    let response = fixture
        .check(
            Method::POST,
            "/internal/ext-authz/api/devices",
            &[("host", APP), ("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(fixture.store.lookups.load(Ordering::SeqCst), 0);
}

/// The SPA's own requests, typed addresses and clients that are not browsers go on to the
/// session check as before.
#[tokio::test]
async fn same_origin_requests_and_requests_without_the_headers_are_checked_as_before() {
    let fixture = Fixture::new(Behaviour::Answer);
    let cookie = fixture.cookie();
    let cases: [(Method, &str, CaseHeaders); 7] = [
        (Method::POST, APP, &[("sec-fetch-site", "same-origin")]),
        (
            Method::DELETE,
            APP,
            &[
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://app.example.com"),
            ],
        ),
        (Method::GET, APP, &[("sec-fetch-site", "none")]),
        (Method::POST, APP, &[("origin", "https://app.example.com")]),
        (
            Method::PUT,
            "app.example.com:443",
            &[("origin", "https://app.example.com")],
        ),
        (Method::POST, APP, &[]),
        (
            Method::GET,
            APP,
            &[("origin", "https://attacker.example.test")],
        ),
    ];
    for (method, host, extra) in &cases {
        let mut headers = vec![("host", *host), ("cookie", cookie.as_str())];
        headers.extend_from_slice(extra);
        let response = fixture
            .check(method.clone(), "/internal/ext-authz/api/certs", &headers)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{method} {host} {extra:?}"
        );
        let tokens = identity_headers(&response);
        let [token] = tokens.as_slice() else {
            panic!("{method} {host} {extra:?}: {tokens:?}");
        };
        assert!(verifier(ServiceName::Certs).verify(token).is_ok());
    }
    assert_eq!(fixture.store.lookups.load(Ordering::SeqCst), cases.len());
    // Same origin, but no session: 401 as before.
    let response = fixture
        .check(
            Method::POST,
            "/internal/ext-authz/api/certs",
            &[("host", APP), ("sec-fetch-site", "same-origin")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
