//! The signed-in user's own account end to end (Laysi/trustiot-v2#497): reading the current
//! session, switching it to another of the user's tenants, and reading and changing the user's
//! preferences. The real binary on the product's schema, applied by the migration Job
//! (`testkit::product`), with the platform role the tenant list needs; every request goes through
//! the service's own ext_authz check first, as the ingress gateway sends it.
//!
//! What the tests hold the service to:
//!
//! - The current session shows the user, the tenant the session acts in with the user's role
//!   there, and the user's tenants, never another user's or another tenant's.
//! - A switch to one of the user's tenants applies to the session's next request, with the
//!   user's role in that tenant as it is then, also right after a platform administrator set it
//!   (Laysi/trustiot-v2#583); a switch to any other tenant, existing or not, is refused the
//!   same way and leaves the session where it was. Only the session the request came with
//!   changes: not the user's other sessions, not another user's.
//! - Preferences are the user's own and stay within their allow lists; a change never reaches
//!   another user's.
//! - Both writes need the user's own same-origin JSON request of at most 1024 bytes: a cross-site
//!   one is refused by the cookie check and again by the handler. A token another service
//!   exchanged neither writes nor reads.
//! - The database roles get only the rights these APIs use.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. The tests share one product database, so each makes users and tenants of its own.

mod support;

use std::io::ErrorKind;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{Actor, ServiceName, SessionId, TenantGrant, TenantId, TenantRole, UserId};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body};
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-p";
const SESSION: &str = "/api/identity/v1/sessions/current";
const SWITCH: &str = "/api/identity/v1/sessions/current/tenant";
const PREFERENCES: &str = "/api/identity/v1/preferences";
/// A route of the assets service: the token the cookie check signs for it shows the tenant and
/// role another service would act with.
const ASSETS: &str = "/api/assets/v1/tenant";
const READY_LIMIT: Duration = Duration::from_secs(60);
/// The largest body of both writes, as the API document states it. Written out rather than
/// taken from the service's constants, so a change of those fails here.
const BODY_LIMIT: usize = 1024;

const INSUFFICIENT_PRIVILEGE: &str = "42501";
const CHECK_VIOLATION: &str = "23514";

const INSERT_USER: &str =
    "INSERT INTO identity.users (id, email, platform_admin) VALUES ($1::uuid, $2, $3)";
const INSERT_TENANT: &str = "INSERT INTO identity.tenants (tenant_id, name) VALUES ($1::uuid, $2)";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const UPDATE_ROLE: &str = "UPDATE identity.memberships SET tenant_role = $3
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
const DELETE_MEMBERSHIP: &str =
    "DELETE FROM identity.memberships WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES ($1::uuid, $2, $3::uuid, $4::uuid, now() + interval '1 hour')";
const ACTIVE_TENANT: &str =
    "SELECT active_tenant_id::text FROM identity.sessions WHERE id = $1::uuid";
const REVOKE_SESSION: &str = "UPDATE identity.sessions SET revoked_at = now() WHERE id = $1::uuid";
/// Ends a session by time, moving its start back too (a session ends after it starts). The
/// cookie check's background extension never moves an ended session's end again
/// (`expires_at > now()` in its condition), so this holds.
const EXPIRE_SESSION: &str = "UPDATE identity.sessions
SET created_at = now() - interval '2 minutes', expires_at = now() - interval '1 minute'
WHERE id = $1::uuid";
const STORED_PREFERENCES: &str = "SELECT language, ai_language, time_zone, theme
FROM identity.preferences WHERE user_id = $1::uuid";
const DELETE_USER: &str = "DELETE FROM identity.users WHERE id = $1::uuid";

/// A user made by the test.
struct Person {
    id: String,
    email: String,
}

/// A tenant made by the test, with a name no other test uses (a canary for leaks).
struct Tenant {
    id: String,
    name: String,
}

/// A session made by the test: its ID and the `Cookie` header value.
struct Session {
    id: String,
    cookie: String,
}

/// One stored preferences row.
type Stored = Option<(String, String, String, String)>;

async fn run(admin: &mut PgConnection, statement: &'static str, binds: &[&str]) {
    let mut query = sqlx::query(statement);
    for bind in binds {
        query = query.bind(*bind);
    }
    query
        .execute(admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
}

async fn person(admin: &mut PgConnection, platform_admin: bool) -> Person {
    let id = UserId::generate().to_string();
    let email = format!("user-{}@example.test", id.replace('-', ""));
    sqlx::query(INSERT_USER)
        .bind(&id)
        .bind(&email)
        .bind(platform_admin)
        .execute(admin)
        .await
        .expect("insert the user");
    Person { id, email }
}

async fn tenant(admin: &mut PgConnection, label: &str) -> Tenant {
    let id = TenantId::generate().to_string();
    let name = format!("{label} {}", id.replace('-', ""));
    run(admin, INSERT_TENANT, &[&id, &name]).await;
    Tenant { id, name }
}

async fn member(admin: &mut PgConnection, tenant: &Tenant, user: &Person, role: &str) {
    run(admin, INSERT_MEMBERSHIP, &[&tenant.id, &user.id, role]).await;
}

/// An active session of `user`, acting in `tenant`.
async fn session(admin: &mut PgConnection, user: &Person, tenant: Option<&Tenant>) -> Session {
    let id = SessionId::generate().to_string();
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(&id)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&user.id)
        .bind(tenant.map(|tenant| tenant.id.as_str()))
        .execute(admin)
        .await
        .expect("insert the session");
    Session {
        id,
        cookie: cookie(&secret),
    }
}

async fn active_tenant(admin: &mut PgConnection, session: &Session) -> Option<String> {
    sqlx::query_scalar(ACTIVE_TENANT)
        .bind(&session.id)
        .fetch_one(admin)
        .await
        .expect("read the session's tenant")
}

async fn stored(admin: &mut PgConnection, user: &Person) -> Stored {
    sqlx::query_as(STORED_PREFERENCES)
        .bind(&user.id)
        .fetch_optional(admin)
        .await
        .expect("read the stored preferences")
}

/// The identity binary on `product` with its platform role; it also trusts `issuer`'s tokens
/// when given, so a test can sign the tokens the cookie check never signs (exchanged ones).
fn start(product: &ProductPostgres, name: &str, seed: u8, issuer: Option<&TestIssuer>) -> Service {
    let key = TestKey::new(seed);
    let mut published = json!({"keys": [key.jwk(KID)]});
    if let Some(issuer) = issuer {
        let theirs: Value = serde_json::from_str(&issuer.jwks()).expect("the issuer's key set");
        published["keys"]
            .as_array_mut()
            .expect("keys")
            .extend(theirs["keys"].as_array().expect("keys").iter().cloned());
    }
    let files = KeyFiles::with(
        name,
        &support::signing_keys(&[(&key, KID, 1, 0)]),
        &published.to_string(),
    );
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
        {"host": APP, "path": "/api/assets", "match": "PathPrefix", "service": "assets"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        (
            "TRUSTIOT__DATABASE__PLATFORM_USERNAME".into(),
            "identity_platform".into(),
        ),
        (
            "TRUSTIOT__DATABASE__PLATFORM_PASSWORD".into(),
            TEST_PASSWORD.into(),
        ),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(READY_LIMIT);
    service
}

/// The ingress gateway's cookie check for `method path`, with `extra` headers.
fn check(
    service: &Service,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &[(&str, &str)],
) -> support::Response {
    let mut headers = vec![("Host", APP), ("Cookie", cookie)];
    headers.extend_from_slice(extra);
    request(
        service.address,
        method,
        &format!("/internal/ext-authz{path}"),
        &headers,
    )
    .expect("call the ext_authz check")
}

/// The token the cookie check signs for the SPA's own `method path` request.
fn token(service: &Service, method: &str, path: &str, cookie: &str) -> String {
    let response = check(
        service,
        method,
        path,
        cookie,
        &[("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    response.header_values("x-internal-identity")[0].to_owned()
}

/// The claims of a token (its signature is the ext_authz tests' concern).
fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).expect("a payload segment");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url")).expect("JSON")
}

/// The tenant grant the next request of the session to another service carries.
fn grant(service: &Service, session: &Session) -> Value {
    claims(&token(service, "GET", ASSETS, &session.cookie))["tenant"].clone()
}

/// A request as the ingress forwards it: `headers` plus the `Host`.
fn send(
    service: &Service,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> support::Response {
    let mut all = vec![("Host", APP)];
    all.extend_from_slice(headers);
    request_with_body(service.address, method, path, &all, body).expect("call the service")
}

/// The SPA's `GET path`: the cookie check first, then the request with its token.
fn get(service: &Service, path: &str, cookie: &str) -> support::Response {
    let token = token(service, "GET", path, cookie);
    send(
        service,
        "GET",
        path,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", cookie),
            ("x-internal-identity", &token),
        ],
        b"",
    )
}

/// The SPA's write: the cookie check first, then the same-origin JSON request with its token.
fn write(
    service: &Service,
    method: &str,
    path: &str,
    cookie: &str,
    body: &Value,
) -> support::Response {
    let token = token(service, method, path, cookie);
    send(
        service,
        method,
        path,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", cookie),
            ("x-internal-identity", &token),
            ("Content-Type", "application/json"),
        ],
        body.to_string().as_bytes(),
    )
}

/// `json` followed by spaces, `size` bytes in all: still the same JSON value.
fn padded(json: &str, size: usize) -> String {
    let spaces = size
        .checked_sub(json.len())
        .expect("a body shorter than the size");
    let body = format!("{json}{}", " ".repeat(spaces));
    assert_eq!(body.len(), size);
    body
}

fn membership(tenant: &Tenant, role: &str) -> Value {
    json!({"tenant_id": tenant.id, "name": tenant.name, "role": role, "state": "active"})
}

fn tenant_grant(tenant: &Tenant, role: &str) -> Value {
    json!({"id": tenant.id, "role": role})
}

fn assert_error(response: &support::Response, status: u16, body: &Value) {
    assert_eq!(response.status, status, "{}", response.raw());
    assert_eq!(&response.json(), body, "{}", response.raw());
}

fn not_member() -> Value {
    json!({"code": "permission_denied", "message_key": "session.tenant_not_member"})
}

fn unauthenticated() -> Value {
    json!({"code": "unauthenticated", "message_key": "error.unauthenticated"})
}

fn cross_site() -> Value {
    json!({"code": "permission_denied", "message_key": "security.cross_site_request"})
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// Acceptance 1 and 3: the session shows the user, the tenant and the role; a switch to one of
/// the user's tenants applies to the session's next request with the role there; a switch to
/// another tenant, existing or not, is refused the same way and the session keeps its tenant;
/// only the session the request came with changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_switches_only_to_the_users_own_tenants() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let service = start(&product, "account-switch", 97, None);
    let mut admin = product.superuser().await;
    let alice = person(&mut admin, false).await;
    let bob = person(&mut admin, false).await;
    let a = tenant(&mut admin, "Plant A").await;
    let b = tenant(&mut admin, "Plant B").await;
    let c = tenant(&mut admin, "Plant C").await;
    member(&mut admin, &a, &alice, "editor").await;
    member(&mut admin, &b, &alice, "viewer").await;
    member(&mut admin, &b, &bob, "admin").await;
    member(&mut admin, &c, &bob, "admin").await;
    let first = session(&mut admin, &alice, Some(&a)).await;
    let second = session(&mut admin, &alice, Some(&a)).await;
    let bobs = session(&mut admin, &bob, Some(&c)).await;

    // 1. The session as it is: the user, tenant A with the editor role, and the user's two
    //    tenants by name; nothing of Bob or of tenant C, which Alice is not a member of.
    let response = get(&service, SESSION, &first.cookie);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    assert_eq!(
        response.json(),
        json!({
            "user_id": alice.id,
            "email": alice.email,
            "platform_admin": false,
            "active_tenant": membership(&a, "editor"),
            "tenants": [membership(&a, "editor"), membership(&b, "viewer")],
        })
    );
    for other in [&c.id, &c.name, &bob.id, &bob.email] {
        assert!(!response.raw().contains(other.as_str()), "{other} shown");
    }
    assert_eq!(grant(&service, &first), tenant_grant(&a, "editor"));

    // 2. A switch to tenant B: the answer, the stored session, and the next request (a token
    //    for another service) all act in B with Alice's role there.
    let response = write(
        &service,
        "PUT",
        SWITCH,
        &first.cookie,
        &json!({"tenant_id": b.id}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    assert_eq!(response.json()["active_tenant"], membership(&b, "viewer"));
    assert_eq!(
        response.json()["tenants"],
        json!([membership(&a, "editor"), membership(&b, "viewer")])
    );
    assert_eq!(active_tenant(&mut admin, &first).await, Some(b.id.clone()));
    assert_eq!(grant(&service, &first), tenant_grant(&b, "viewer"));
    assert_eq!(
        get(&service, SESSION, &first.cookie).json()["active_tenant"],
        membership(&b, "viewer")
    );
    // The role is still read on every request (decision 13): a change in B applies at once.
    run(&mut admin, UPDATE_ROLE, &[&b.id, &alice.id, "admin"]).await;
    assert_eq!(grant(&service, &first), tenant_grant(&b, "admin"));
    run(&mut admin, UPDATE_ROLE, &[&b.id, &alice.id, "viewer"]).await;
    // The same when a platform administrator sets the role with the member API
    // (Laysi/trustiot-v2#583): the switched session's next request, and what it shows, carry it.
    let operator = person(&mut admin, true).await;
    let operator_session = session(&mut admin, &operator, None).await;
    let members = format!("/api/identity/v1/platform/tenants/{}/members", b.id);
    for (role, previous) in [("admin", "viewer"), ("viewer", "admin")] {
        let response = write(
            &service,
            "PUT",
            &members,
            &operator_session.cookie,
            &json!({"email": alice.email, "role": role}),
        );
        assert_eq!(response.status, 200, "{}", response.raw());
        assert_eq!(response.json()["previous_role"], previous);
        assert_eq!(grant(&service, &first), tenant_grant(&b, role));
        assert_eq!(
            get(&service, SESSION, &first.cookie).json()["active_tenant"],
            membership(&b, role)
        );
    }
    // Alice's other session and Bob's keep their tenants.
    assert_eq!(grant(&service, &second), tenant_grant(&a, "editor"));
    assert_eq!(grant(&service, &bobs), tenant_grant(&c, "admin"));

    // 3. A tenant Alice is not a member of, one that exists (C) and one that does not: the same
    //    refusal, so it tells nothing of which tenants exist, and the session stays in B.
    let missing = TenantId::generate().to_string();
    for target in [&c.id, &missing] {
        let response = write(
            &service,
            "PUT",
            SWITCH,
            &first.cookie,
            &json!({"tenant_id": target}),
        );
        assert_error(&response, 403, &not_member());
        assert!(!response.raw().contains(&c.name));
        assert_eq!(active_tenant(&mut admin, &first).await, Some(b.id.clone()));
        assert_eq!(grant(&service, &first), tenant_grant(&b, "viewer"));
    }

    // 4. Only the session the request came with, and only the user's own: the token names the
    //    session it was signed from, and the cookie must be that session's.
    let alices_token = token(&service, "PUT", SWITCH, &first.cookie);
    let bobs_token = token(&service, "PUT", SWITCH, &bobs.cookie);
    let attempts = [
        // Alice's token with the cookie of her other session.
        (&alices_token, Some(second.cookie.as_str()), &a),
        // Alice's token with Bob's cookie, to a tenant both are members of.
        (&alices_token, Some(bobs.cookie.as_str()), &b),
        // Bob's token with Alice's cookie, to a tenant of Bob's she is not a member of.
        (&bobs_token, Some(first.cookie.as_str()), &c),
        // Bob's token with Alice's cookie, to a tenant of both.
        (&bobs_token, Some(first.cookie.as_str()), &a),
        // No cookie at all.
        (&alices_token, None, &a),
    ];
    for (token, cookie, target) in attempts {
        let mut headers = vec![
            ("Sec-Fetch-Site", "same-origin"),
            ("x-internal-identity", token.as_str()),
            ("Content-Type", "application/json"),
        ];
        if let Some(cookie) = cookie {
            headers.push(("Cookie", cookie));
        }
        let body = json!({"tenant_id": target.id}).to_string();
        let response = send(&service, "PUT", SWITCH, &headers, body.as_bytes());
        assert!(
            matches!(response.status, 401 | 403),
            "{}: {}",
            target.name,
            response.raw()
        );
    }
    assert_eq!(active_tenant(&mut admin, &first).await, Some(b.id.clone()));
    assert_eq!(active_tenant(&mut admin, &second).await, Some(a.id.clone()));
    assert_eq!(active_tenant(&mut admin, &bobs).await, Some(c.id.clone()));

    // 4b. Only a session that has not ended: Alice's other session, ended by time (the
    //     database clock, not revoked) between the cookie check and the switch, switches
    //     nothing, to a tenant she is a member of.
    let late = token(&service, "PUT", SWITCH, &second.cookie);
    run(&mut admin, EXPIRE_SESSION, &[&second.id]).await;
    let body = json!({"tenant_id": b.id}).to_string();
    let response = send(
        &service,
        "PUT",
        SWITCH,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", &second.cookie),
            ("x-internal-identity", &late),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    assert_error(&response, 401, &unauthenticated());
    assert_eq!(active_tenant(&mut admin, &second).await, Some(a.id.clone()));

    // 5. Back to A: the editor role again.
    let response = write(
        &service,
        "PUT",
        SWITCH,
        &first.cookie,
        &json!({"tenant_id": a.id}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(grant(&service, &first), tenant_grant(&a, "editor"));

    // 6. A membership removed after the switch leaves the session in no tenant: the pointer
    //    grants nothing by itself.
    let response = write(
        &service,
        "PUT",
        SWITCH,
        &first.cookie,
        &json!({"tenant_id": b.id}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    run(&mut admin, DELETE_MEMBERSHIP, &[&b.id, &alice.id]).await;
    assert_eq!(grant(&service, &first), Value::Null);
    let shown = get(&service, SESSION, &first.cookie).json();
    assert_eq!(shown["active_tenant"], Value::Null);
    assert_eq!(shown["tenants"], json!([membership(&a, "editor")]));
    let response = write(
        &service,
        "PUT",
        SWITCH,
        &first.cookie,
        &json!({"tenant_id": b.id}),
    );
    assert_error(&response, 403, &not_member());

    // 7. A session revoked between the cookie check and the switch switches nothing.
    let late = token(&service, "PUT", SWITCH, &first.cookie);
    run(&mut admin, REVOKE_SESSION, &[&first.id]).await;
    let body = json!({"tenant_id": a.id}).to_string();
    let response = send(
        &service,
        "PUT",
        SWITCH,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", &first.cookie),
            ("x-internal-identity", &late),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    assert_error(&response, 401, &unauthenticated());
    assert_eq!(active_tenant(&mut admin, &first).await, Some(b.id.clone()));

    // 8. The log names users, tenants by ID and the reasons, never a tenant's name, an email
    //    address or a cookie.
    let output = service.stop();
    assert!(output.contains("switched the session's tenant"), "{output}");
    for reason in ["not_a_member", "no_active_session", "no_session_cookie"] {
        assert!(output.contains(reason), "{reason}: {output}");
    }
    let secrets = [&first.cookie, &second.cookie, &bobs.cookie].map(|cookie| {
        cookie
            .split_once('=')
            .map(|(_, value)| value.to_owned())
            .expect("a cookie value")
    });
    for leaked in [&a.name, &b.name, &c.name, &alice.email, &bob.email]
        .into_iter()
        .chain(secrets.iter())
    {
        assert!(
            !output.contains(leaked.as_str()),
            "{leaked} reached the log"
        );
    }
}

/// Decision 27 for both new writes: a cross-site request is refused by the cookie check and
/// again by the handler; a token another service exchanged, a token that names no session, a
/// body that is not JSON or is over 1024 bytes, and a token for another service change nothing.
/// An exchanged token reads nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn account_writes_need_the_users_own_same_origin_json_request() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = start(&product, "account-csrf", 98, Some(&issuer));
    let mut admin = product.superuser().await;
    let alice = person(&mut admin, false).await;
    let a = tenant(&mut admin, "Plant A").await;
    let b = tenant(&mut admin, "Plant B").await;
    member(&mut admin, &a, &alice, "editor").await;
    member(&mut admin, &b, &alice, "viewer").await;
    let first = session(&mut admin, &alice, Some(&a)).await;
    let alice_id: UserId = alice.id.parse().expect("user id");
    let a_id: TenantId = a.id.parse().expect("tenant id");

    // A token another service exchanged for a call on Alice's behalf, and one that names no
    // session: the cookie check signs neither.
    let mut exchanged = TestIssuer::claims(
        ServiceName::Identity,
        Some(TenantGrant {
            id: a_id,
            role: TenantRole::Editor,
        }),
    );
    exchanged.sub = alice_id;
    exchanged.act = Some(Actor {
        sub: ServiceName::Assets,
    });
    exchanged.sid = None;
    let exchanged = issuer.token(&exchanged);
    let mut sessionless = TestIssuer::claims(
        ServiceName::Identity,
        Some(TenantGrant {
            id: a_id,
            role: TenantRole::Editor,
        }),
    );
    sessionless.sub = alice_id;
    sessionless.sid = None;
    let sessionless = issuer.token(&sessionless);

    // Each write with a body that changes something, and one that changes nothing (the tenant
    // the session is in; no setting), for the size limit's accepted side.
    let writes = [
        (
            "PUT",
            SWITCH,
            json!({"tenant_id": b.id}),
            json!({"tenant_id": a.id}),
        ),
        ("PATCH", PREFERENCES, json!({"theme": "dark"}), json!({})),
    ];
    let attacker = "https://attacker.example.test";
    let same_origin = format!("https://{APP}");
    for (method, path, body, harmless) in &writes {
        let body = body.to_string();
        // 1. The cookie check refuses a cross-site write before it looks the session up.
        for extra in [
            [("Sec-Fetch-Site", "cross-site")],
            [("Sec-Fetch-Site", "same-site")],
            [("Origin", attacker)],
        ] {
            let response = check(&service, method, path, &first.cookie, &extra);
            assert_error(&response, 403, &cross_site());
            assert!(response.header_values("x-internal-identity").is_empty());
        }

        // 2. The handler refuses it again when a token arrives with it anyway (a gateway that
        //    stopped passing the headers to the check).
        let signed = token(&service, method, path, &first.cookie);
        let with = |extra: &[(&'static str, &str)]| {
            let mut headers = vec![
                ("Cookie", first.cookie.as_str()),
                ("x-internal-identity", signed.as_str()),
                ("Content-Type", "application/json"),
            ];
            headers.extend_from_slice(extra);
            send(&service, method, path, &headers, body.as_bytes())
        };
        for extra in [
            vec![("Sec-Fetch-Site", "cross-site")],
            vec![("Sec-Fetch-Site", "same-site")],
            vec![("Sec-Fetch-Site", "none")],
            vec![("Origin", attacker)],
            vec![
                ("Sec-Fetch-Site", "same-origin"),
                ("Sec-Fetch-Site", "same-origin"),
            ],
        ] {
            assert_error(&with(&extra), 403, &cross_site());
        }

        // 3. Only a JSON content type: a page of another origin can make the browser send the
        //    others without a CORS preflight.
        for content_type in [
            None,
            Some("text/plain"),
            Some("text/plain; charset=utf-8"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
        ] {
            let mut headers = vec![
                ("Sec-Fetch-Site", "same-origin"),
                ("Cookie", first.cookie.as_str()),
                ("x-internal-identity", signed.as_str()),
            ];
            if let Some(content_type) = content_type {
                headers.push(("Content-Type", content_type));
            }
            let response = send(&service, method, path, &headers, body.as_bytes());
            assert_eq!(response.status, 400, "{content_type:?}: {}", response.raw());
            assert_eq!(response.json()["code"], "invalid_request");
        }
        // The size limit: a valid body padded with JSON whitespace, so only its size can be the
        // reason for a refusal. Exactly the limit is read whole and answered...
        let same_origin_json = [
            ("Host", APP),
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", first.cookie.as_str()),
            ("x-internal-identity", signed.as_str()),
            ("Content-Type", "application/json"),
        ];
        let at_limit = padded(&harmless.to_string(), BODY_LIMIT);
        let response = request_with_body(
            service.address,
            method,
            path,
            &same_origin_json,
            at_limit.as_bytes(),
        )
        .expect("call the service");
        assert_eq!(
            response.status,
            200,
            "{method} {path} at the limit: {}",
            response.raw()
        );
        // ...one byte more, or many, is refused. The service answers without reading the rest,
        // so the connection may be reset before the client reads the answer; either way nothing
        // changes.
        for size in [BODY_LIMIT + 1, body.len() + 1100] {
            let over = padded(&body, size);
            match request_with_body(
                service.address,
                method,
                path,
                &same_origin_json,
                over.as_bytes(),
            ) {
                Ok(response) => {
                    assert_eq!(response.status, 400, "{size} bytes: {}", response.raw());
                    assert_eq!(response.json()["code"], "invalid_request");
                }
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        ErrorKind::ConnectionReset
                            | ErrorKind::BrokenPipe
                            | ErrorKind::ConnectionAborted
                    ),
                    "{method} {path} with {size} bytes: {error}"
                ),
            }
            assert_eq!(active_tenant(&mut admin, &first).await, Some(a.id.clone()));
            assert_eq!(stored(&mut admin, &alice).await, None);
        }

        // 4. Not the user's own request: an exchanged token, with or without the cookie.
        for cookie in [Some(first.cookie.as_str()), None] {
            let mut headers = vec![
                ("Sec-Fetch-Site", "same-origin"),
                ("x-internal-identity", exchanged.as_str()),
                ("Content-Type", "application/json"),
            ];
            if let Some(cookie) = cookie {
                headers.push(("Cookie", cookie));
            }
            let response = send(&service, method, path, &headers, body.as_bytes());
            assert_error(
                &response,
                403,
                &json!({"code": "permission_denied", "message_key": "error.permission_denied"}),
            );
        }

        // 5. No token, or a token for another service.
        let response = send(
            &service,
            method,
            path,
            &[
                ("Sec-Fetch-Site", "same-origin"),
                ("Cookie", first.cookie.as_str()),
                ("Content-Type", "application/json"),
            ],
            body.as_bytes(),
        );
        assert_error(&response, 401, &unauthenticated());
        let for_assets = token(&service, "GET", ASSETS, &first.cookie);
        let response = send(
            &service,
            method,
            path,
            &[
                ("Sec-Fetch-Site", "same-origin"),
                ("Cookie", first.cookie.as_str()),
                ("x-internal-identity", for_assets.as_str()),
                ("Content-Type", "application/json"),
            ],
            body.as_bytes(),
        );
        assert_error(&response, 401, &unauthenticated());
    }

    // 6. A switch needs the session the token was signed from: a token that names none is
    //    refused even with the right cookie.
    let body = json!({"tenant_id": b.id}).to_string();
    let response = send(
        &service,
        "PUT",
        SWITCH,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", first.cookie.as_str()),
            ("x-internal-identity", sessionless.as_str()),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    assert_error(&response, 401, &unauthenticated());

    // Nothing above changed anything.
    assert_eq!(active_tenant(&mut admin, &first).await, Some(a.id.clone()));
    assert_eq!(stored(&mut admin, &alice).await, None);

    // 7. Reading is the user's own too: an exchanged token reads neither the session (the
    //    user's tenants and address) nor the preferences, with or without the cookie.
    for path in [SESSION, PREFERENCES] {
        for cookie in [Some(first.cookie.as_str()), None] {
            let mut headers = vec![
                ("Sec-Fetch-Site", "same-origin"),
                ("x-internal-identity", exchanged.as_str()),
            ];
            if let Some(cookie) = cookie {
                headers.push(("Cookie", cookie));
            }
            let response = send(&service, "GET", path, &headers, b"");
            assert_error(
                &response,
                403,
                &json!({"code": "permission_denied", "message_key": "error.permission_denied"}),
            );
            for leaked in [&a.name, &b.name, &alice.email] {
                assert!(
                    !response.raw().contains(leaked.as_str()),
                    "{path}: {leaked}"
                );
            }
        }
        // The user's own token reads them.
        assert_eq!(get(&service, path, &first.cookie).status, 200, "{path}");
    }

    // 8. The positive control: the same writes as the SPA sends them (Origin of its own, or
    //    only Sec-Fetch-Site) succeed, so the refusals above were not a broken setup.
    let signed = token(&service, "PUT", SWITCH, &first.cookie);
    let response = send(
        &service,
        "PUT",
        SWITCH,
        &[
            ("Origin", same_origin.as_str()),
            ("Cookie", first.cookie.as_str()),
            ("x-internal-identity", signed.as_str()),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        body.as_bytes(),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(active_tenant(&mut admin, &first).await, Some(b.id.clone()));
    let response = write(
        &service,
        "PATCH",
        PREFERENCES,
        &first.cookie,
        &json!({"theme": "dark"}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(
        stored(&mut admin, &alice).await,
        Some((
            "auto".to_owned(),
            "auto".to_owned(),
            "auto".to_owned(),
            "dark".to_owned()
        ))
    );

    let output = service.stop();
    for logged in [
        "cross_site_request",
        "exchanged_token",
        "no_session_claim",
        "refused to read the current session",
        "refused to read preferences",
    ] {
        assert!(output.contains(logged), "{logged}: {output}");
    }
}

/// Acceptance 2: users read and change their own preferences only; values outside the allow
/// lists are refused and change nothing.
#[tokio::test(flavor = "multi_thread")]
async fn users_change_only_their_own_preferences_within_the_allow_lists() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let service = start(&product, "account-preferences", 99, None);
    let mut admin = product.superuser().await;
    let alice = person(&mut admin, false).await;
    let bob = person(&mut admin, false).await;
    // Preferences need no tenant.
    let alices = session(&mut admin, &alice, None).await;
    let bobs = session(&mut admin, &bob, None).await;
    let defaults =
        json!({"language": "auto", "ai_language": "auto", "time_zone": "auto", "theme": "system"});

    let as_stored = |language: &str, ai_language: &str, time_zone: &str, theme: &str| {
        Some((
            language.to_owned(),
            ai_language.to_owned(),
            time_zone.to_owned(),
            theme.to_owned(),
        ))
    };

    // 1. The defaults, with nothing stored.
    let response = get(&service, PREFERENCES, &alices.cookie);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    assert_eq!(response.json(), defaults);
    assert_eq!(stored(&mut admin, &alice).await, None);
    // Bob keeps preferences of his own, so a change of Alice's that reached other users' rows
    // would show in his (checked after each of hers below).
    let response = write(
        &service,
        "PATCH",
        PREFERENCES,
        &bobs.cookie,
        &json!({"language": "en", "theme": "light"}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    let bobs_stored = as_stored("en", "auto", "auto", "light");
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);
    let bobs_shown =
        json!({"language": "en", "ai_language": "auto", "time_zone": "auto", "theme": "light"});

    // 2. A change of two settings: the answer, what is stored, and the next read agree.
    let response = write(
        &service,
        "PATCH",
        PREFERENCES,
        &alices.cookie,
        &json!({"language": "zh-TW", "time_zone": "Asia/Taipei"}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    let changed = json!({
        "language": "zh-TW", "ai_language": "auto", "time_zone": "Asia/Taipei", "theme": "system"
    });
    assert_eq!(response.json(), changed);
    assert_eq!(get(&service, PREFERENCES, &alices.cookie).json(), changed);
    assert_eq!(
        stored(&mut admin, &alice).await,
        as_stored("zh-TW", "auto", "Asia/Taipei", "system")
    );
    // Bob's are his own.
    assert_eq!(get(&service, PREFERENCES, &bobs.cookie).json(), bobs_shown);
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);

    // 3. Fields left out keep their value; an empty change changes nothing.
    let response = write(
        &service,
        "PATCH",
        PREFERENCES,
        &alices.cookie,
        &json!({"ai_language": "en", "theme": "dark"}),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    let all = json!({
        "language": "zh-TW", "ai_language": "en", "time_zone": "Asia/Taipei", "theme": "dark"
    });
    assert_eq!(response.json(), all);
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);
    let response = write(&service, "PATCH", PREFERENCES, &alices.cookie, &json!({}));
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json(), all);
    let alices_stored = as_stored("zh-TW", "en", "Asia/Taipei", "dark");
    assert_eq!(stored(&mut admin, &alice).await, alices_stored);
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);

    // 4. Values outside the allow lists: 422 naming the field, and nothing changes, not even
    //    the valid fields of the same request.
    let too_long = format!("Asia/{}", "x".repeat(70));
    let refused = [
        ("language", "fr"),
        ("language", "zh-tw"),
        ("language", "ZH-TW"),
        ("language", "zh-Hant"),
        ("language", ""),
        ("ai_language", "ja"),
        ("ai_language", "Auto"),
        ("time_zone", "Mars/Olympus_Mons"),
        ("time_zone", "asia/taipei"),
        ("time_zone", "Factory"),
        ("time_zone", "../../etc/passwd"),
        ("time_zone", "Asia/Taipei "),
        ("time_zone", "+08:00"),
        ("time_zone", too_long.as_str()),
        ("theme", "blue"),
        ("theme", "Dark"),
    ];
    for (field, value) in refused {
        // With a valid change of another setting, which must not apply either.
        let valid = if field == "theme" {
            ("language", "en")
        } else {
            ("theme", "light")
        };
        let mut body = json!({ field: value });
        body[valid.0] = json!(valid.1);
        let response = write(&service, "PATCH", PREFERENCES, &alices.cookie, &body);
        assert_error(
            &response,
            422,
            &json!({
                "code": "validation_failed",
                "message_key": "error.validation_failed",
                "field_errors": [
                    {"field": format!("/{field}"), "message_key": format!("preferences.{field}_invalid")}
                ],
            }),
        );
    }
    assert_eq!(stored(&mut admin, &alice).await, alices_stored);
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);

    // 5. Bodies that are not the four settings as strings: 400, nothing changes. None can name
    //    another user.
    for body in [
        json!({"language": null}),
        json!({"theme": 1}),
        json!({"time_zone": ["Asia/Taipei"]}),
        json!({"user_id": bob.id}),
        json!({"theme": "light", "user_id": bob.id}),
        json!({"theme": "light", "locale": "en"}),
        json!(["en", "en", "UTC", "light"]),
        json!("light"),
        json!(null),
    ] {
        let response = write(&service, "PATCH", PREFERENCES, &alices.cookie, &body);
        assert_eq!(response.status, 400, "{body}: {}", response.raw());
        assert_eq!(response.json()["code"], "invalid_request");
    }
    assert_eq!(stored(&mut admin, &alice).await, alices_stored);
    assert_eq!(stored(&mut admin, &bob).await, bobs_stored);
    assert_eq!(get(&service, PREFERENCES, &bobs.cookie).json(), bobs_shown);

    // 6. A user deleted after the cookie check keeps no preferences.
    let carol = person(&mut admin, false).await;
    let carols = session(&mut admin, &carol, None).await;
    let late = token(&service, "PATCH", PREFERENCES, &carols.cookie);
    run(&mut admin, DELETE_USER, &[&carol.id]).await;
    let body = json!({"theme": "dark"}).to_string();
    let response = send(
        &service,
        "PATCH",
        PREFERENCES,
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("Cookie", carols.cookie.as_str()),
            ("x-internal-identity", late.as_str()),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    assert_error(&response, 401, &unauthenticated());
    assert_eq!(stored(&mut admin, &carol).await, None);

    // 7. The log says which settings changed, never their values or the user's address.
    let output = service.stop();
    assert!(output.contains("changed preferences"), "{output}");
    for leaked in [
        "Asia/Taipei",
        "zh-TW",
        "Mars/Olympus_Mons",
        alice.email.as_str(),
    ] {
        assert!(!output.contains(leaked), "{leaked} reached the log");
    }
}

/// The database rights 0007_preferences_and_tenant_switch.sql adds, and nothing more: the
/// service role points a session at a tenant and keeps preferences within the checks; the
/// platform role reads memberships and tenant names across tenants and writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_roles_get_only_the_rights_these_apis_use() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    // Fixed IDs, so the statements below stay static SQL; no other test uses them.
    for statement in [
        "INSERT INTO identity.users (id) VALUES
            ('01928f6e-7a3c-7d4e-8f00-000000497001'),
            ('01928f6e-7a3c-7d4e-8f00-000000497002'),
            ('01928f6e-7a3c-7d4e-8f00-000000497003')",
        "INSERT INTO identity.tenants (tenant_id, name) VALUES
            ('01928f6e-7a3c-7d4e-8f00-000000497a01', 'Grants A'),
            ('01928f6e-7a3c-7d4e-8f00-000000497a02', 'Grants B')",
        "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES
            ('01928f6e-7a3c-7d4e-8f00-000000497a01', '01928f6e-7a3c-7d4e-8f00-000000497001', 'viewer'),
            ('01928f6e-7a3c-7d4e-8f00-000000497a02', '01928f6e-7a3c-7d4e-8f00-000000497002', 'admin')",
        "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at) VALUES
            ('01928f6e-7a3c-7d4e-8f00-000000497501', sha256('497'),
             '01928f6e-7a3c-7d4e-8f00-000000497001', now() + interval '1 hour')",
        "INSERT INTO identity.preferences (user_id) VALUES ('01928f6e-7a3c-7d4e-8f00-000000497001')",
    ] {
        sqlx::raw_sql(statement)
            .execute(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }

    // The service role: what the APIs do is allowed.
    let mut app = product.postgres.connect("identity_app", DATABASE).await;
    for statement in [
        "UPDATE identity.sessions SET active_tenant_id = '01928f6e-7a3c-7d4e-8f00-000000497a01'
            WHERE id = '01928f6e-7a3c-7d4e-8f00-000000497501'",
        "INSERT INTO identity.preferences (user_id) VALUES ('01928f6e-7a3c-7d4e-8f00-000000497002')
            ON CONFLICT (user_id) DO NOTHING",
        "UPDATE identity.preferences
            SET language = 'en', ai_language = 'zh-TW', time_zone = 'Etc/GMT+8', theme = 'dark'
            WHERE user_id = '01928f6e-7a3c-7d4e-8f00-000000497001'",
        "SELECT language, ai_language, time_zone, theme FROM identity.preferences",
    ] {
        sqlx::raw_sql(statement)
            .execute(&mut app)
            .await
            .unwrap_or_else(|error| panic!("identity_app: {statement}: {error}"));
    }
    // Nothing else: other columns of a session, a row's user, choosing a setting at insert time
    // (the API always inserts the defaults first), deleting.
    for statement in [
        "UPDATE identity.sessions SET user_id = '01928f6e-7a3c-7d4e-8f00-000000497002'",
        "UPDATE identity.sessions SET token_hash = sha256('other')",
        "UPDATE identity.tenants SET name = 'Renamed'",
        "INSERT INTO identity.preferences (user_id, theme)
            VALUES ('01928f6e-7a3c-7d4e-8f00-000000497003', 'dark')",
        "UPDATE identity.preferences SET user_id = '01928f6e-7a3c-7d4e-8f00-000000497003'",
        "DELETE FROM identity.preferences",
        "TRUNCATE identity.preferences",
    ] {
        let error = sqlx::raw_sql(statement)
            .execute(&mut app)
            .await
            .expect_err(statement);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "identity_app: {statement}: {error}"
        );
    }
    // The checks repeat the allow lists, so a bug in the service cannot store another value.
    for statement in [
        "UPDATE identity.preferences SET language = 'fr'",
        "UPDATE identity.preferences SET ai_language = 'zh-tw'",
        "UPDATE identity.preferences SET theme = 'blue'",
        "UPDATE identity.preferences SET time_zone = ''",
        "UPDATE identity.preferences SET time_zone = '../etc/passwd'",
        "UPDATE identity.preferences SET time_zone = 'Asia/Taipei '",
        "UPDATE identity.preferences SET time_zone = 'Asia//Taipei'",
        "UPDATE identity.preferences SET time_zone = repeat('x', 65)",
    ] {
        let error = sqlx::raw_sql(statement)
            .execute(&mut app)
            .await
            .expect_err(statement);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(CHECK_VIOLATION),
            "identity_app: {statement}: {error}"
        );
    }
    // Row-level security on memberships and tenants is unchanged for the service role.
    for statement in [
        "SELECT count(*) FROM identity.memberships",
        "SELECT count(*) FROM identity.tenants",
    ] {
        let visible: i64 = sqlx::query_scalar(statement)
            .fetch_one(&mut app)
            .await
            .expect(statement);
        assert_eq!(visible, 0, "identity_app without a tenant: {statement}");
    }

    // The platform role reads memberships and tenant names across tenants...
    let mut platform = product
        .postgres
        .connect("identity_platform", DATABASE)
        .await;
    for statement in [
        "SELECT count(*) FROM identity.memberships
            WHERE tenant_id IN ('01928f6e-7a3c-7d4e-8f00-000000497a01',
                                '01928f6e-7a3c-7d4e-8f00-000000497a02')",
        "SELECT count(*) FROM identity.tenants
            WHERE tenant_id IN ('01928f6e-7a3c-7d4e-8f00-000000497a01',
                                '01928f6e-7a3c-7d4e-8f00-000000497a02')",
    ] {
        let visible: i64 = sqlx::query_scalar(statement)
            .fetch_one(&mut platform)
            .await
            .expect(statement);
        assert_eq!(visible, 2, "identity_platform: {statement}");
    }
    // ...and writes neither, nor reads or writes sessions and preferences.
    for statement in [
        "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
            VALUES ('01928f6e-7a3c-7d4e-8f00-000000497a01', '01928f6e-7a3c-7d4e-8f00-000000497003', 'admin')",
        "UPDATE identity.memberships SET tenant_role = 'admin'",
        "DELETE FROM identity.memberships",
        "INSERT INTO identity.tenants (tenant_id, name)
            VALUES ('01928f6e-7a3c-7d4e-8f00-000000497a03', 'Grants C')",
        "UPDATE identity.tenants SET name = 'Renamed'",
        "DELETE FROM identity.tenants",
        "SELECT count(*) FROM identity.sessions",
        "UPDATE identity.sessions SET active_tenant_id = NULL",
        "SELECT count(*) FROM identity.preferences",
        "UPDATE identity.preferences SET theme = 'dark'",
    ] {
        let error = sqlx::raw_sql(statement)
            .execute(&mut platform)
            .await
            .expect_err(statement);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "identity_platform: {statement}: {error}"
        );
    }
}
