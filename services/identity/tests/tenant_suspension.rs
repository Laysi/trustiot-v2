//! A suspended tenant is cut off on every request (checklist B1, Laysi/trustiot-v2#777), end to
//! end: the real binary on the product's schema, applied by the migration Job
//! (`testkit::product`); every request goes through the service's own ext_authz check first, as
//! the ingress gateway sends it, and the suspension and resumption go through the platform
//! administrator's own API (Laysi/trustiot-v2#776).
//!
//! What the tests hold the service to:
//!
//! - Once a platform administrator suspends a tenant, the next request of a session in it gets
//!   403 `tenant_suspended`: at the ingress for every other service's route (no token is
//!   signed), at this service's own tenant-scoped endpoint (the operation log), and at the run
//!   delegation's next exchange and a new delegation. A deleted tenant is cut off the same way.
//! - Meanwhile the user still reads the session (which names each tenant's state), switches
//!   tenants (to the suspended one too), changes preferences and logs out; a platform
//!   administrator whose current tenant is the suspended one still resumes it.
//! - Once the tenant is resumed, the next request works again, the delegation's next exchange
//!   too.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. The tests share one product database, so each makes users and tenants of its own.

mod support;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::api::identity::{CreateRunDelegation, ExchangeRunDelegation, run_proof_message};
use contract::client::Endpoint;
use contract::{RunId, ServiceName, SessionId, TenantId, UserId};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body};
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-s";
const SESSION: &str = "/api/identity/v1/sessions/current";
const SWITCH: &str = "/api/identity/v1/sessions/current/tenant";
const PREFERENCES: &str = "/api/identity/v1/preferences";
/// The tenant administrator's operation log: this service's own tenant-scoped endpoint.
const EVENT_LOG: &str = "/api/identity/v1/event-log";
/// Routes of two other services: assets, and the assistant, which holds run delegations.
const ASSETS: &str = "/api/assets/v1/tenant";
const ASSISTANT: &str = "/api/assistant/v1/threads";
const READY_LIMIT: Duration = Duration::from_secs(60);

const INSERT_USER: &str =
    "INSERT INTO identity.users (id, email, platform_admin) VALUES ($1::uuid, $2, $3)";
const INSERT_TENANT: &str = "INSERT INTO identity.tenants (tenant_id, name) VALUES ($1::uuid, $2)";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES ($1::uuid, $2, $3::uuid, $4::uuid, now() + interval '1 hour')";
const REVOKED_AT: &str = "SELECT revoked_at IS NOT NULL FROM identity.sessions WHERE id = $1::uuid";
/// Ends a terminated contract the way Laysi/trustiot-v2#109 will (no API sets `deleted` yet):
/// suspended for `contract_terminated`, then deleted, each a new version, as the migration's
/// trigger requires.
const TERMINATE: &str = "UPDATE identity.tenants
SET state = 'suspended', suspension_reason = 'contract_terminated', state_version = state_version + 1
WHERE tenant_id = $1::uuid";
const DELETE_TENANT: &str = "UPDATE identity.tenants
SET state = 'deleted', state_version = state_version + 1
WHERE tenant_id = $1::uuid";

/// A user made by the test.
struct Person {
    id: String,
}

/// A tenant made by the test.
struct Tenant {
    id: String,
    name: String,
}

/// A session made by the test: its ID and the `Cookie` header value.
struct Session {
    id: String,
    cookie: String,
}

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
    Person { id }
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
async fn session(admin: &mut PgConnection, user: &Person, tenant: &Tenant) -> Session {
    let id = SessionId::generate().to_string();
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(&id)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&user.id)
        .bind(&tenant.id)
        .execute(admin)
        .await
        .expect("insert the session");
    Session {
        id,
        cookie: cookie(&secret),
    }
}

/// The identity binary on `product`, with routes to itself, assets and the assistant, and the
/// call list that lets the assistant call certs on behalf of users.
fn start(product: &ProductPostgres, name: &str) -> Service {
    let key = TestKey::new(77);
    let files = KeyFiles::new(name, &key, KID);
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
        {"host": APP, "path": "/api/assets", "match": "PathPrefix", "service": "assets"},
        {"host": APP, "path": "/api/assistant", "match": "PathPrefix", "service": "assistant"},
    ]);
    let calls = json!({"assistant": ["certs"]});
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT_ON_BEHALF_CALLS".into(), calls.to_string()),
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

/// The ingress gateway's cookie check for the SPA's own `method path` request.
fn check(service: &Service, method: &str, path: &str, cookie: &str) -> support::Response {
    request(
        service.address,
        method,
        &format!("/internal/ext-authz{path}"),
        &[
            ("Host", APP),
            ("Cookie", cookie),
            ("Sec-Fetch-Site", "same-origin"),
        ],
    )
    .expect("call the ext_authz check")
}

/// The token the cookie check signs for `method path`.
fn token(service: &Service, method: &str, path: &str, cookie: &str) -> String {
    let response = check(service, method, path, cookie);
    assert_eq!(response.status, 200, "{method} {path}: {}", response.raw());
    let tokens = response.header_values("x-internal-identity");
    assert_eq!(tokens.len(), 1, "{}", response.raw());
    tokens[0].to_owned()
}

/// The claims of a token (its signature is the ext_authz tests' concern).
fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).expect("a payload segment");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url")).expect("JSON")
}

/// The SPA's request: the cookie check first, then the request with its token, the cookie and,
/// for a body, a JSON content type.
fn call(
    service: &Service,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&Value>,
) -> support::Response {
    let token = token(service, method, path, cookie);
    let mut headers = vec![
        ("Host", APP),
        ("Sec-Fetch-Site", "same-origin"),
        ("Cookie", cookie),
        ("x-internal-identity", token.as_str()),
    ];
    if body.is_some() {
        headers.push(("Content-Type", "application/json"));
    }
    let body = body.map(Value::to_string).unwrap_or_default();
    request_with_body(service.address, method, path, &headers, body.as_bytes())
        .expect("call the service")
}

fn state_path(tenant: &Tenant) -> String {
    format!("/api/identity/v1/platform/tenants/{}/state", tenant.id)
}

fn tenant_suspended() -> Value {
    json!({"code": "tenant_suspended", "message_key": "error.tenant_suspended"})
}

/// 403 `tenant_suspended`, not to be stored, without an identity header.
fn assert_suspended(response: &support::Response, what: &str) {
    assert_eq!(response.status, 403, "{what}: {}", response.raw());
    assert_eq!(response.json(), tenant_suspended(), "{what}");
    assert_eq!(
        response.header_values("cache-control"),
        ["no-store"],
        "{what}"
    );
    assert!(
        response.header_values("x-internal-identity").is_empty(),
        "{what}: no token"
    );
}

/// The cookie check lets `cookie` through to `path` with a token for its tenant `tenant` and
/// without the suspension claim.
fn assert_passes(service: &Service, path: &str, cookie: &str, tenant: &Tenant) {
    let claims = claims(&token(service, "GET", path, cookie));
    assert_eq!(claims["tenant"]["id"], json!(tenant.id), "{path}");
    assert!(claims.get("tenant_suspended").is_none(), "{path}: {claims}");
}

fn membership(tenant: &Tenant, role: &str, state: &str) -> Value {
    json!({"tenant_id": tenant.id, "name": tenant.name, "role": role, "state": state})
}

/// A run's key, derived in code from a one-byte seed: a throwaway test key.
fn run_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// One request to a run delegation route, on the exchange's port, as the assistant sends it.
fn delegation_request(
    service: &Service,
    route: &str,
    credential: &str,
    body: &Value,
) -> support::Response {
    request_with_body(
        service.exchange_address,
        "POST",
        route,
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", credential),
        ],
        body.to_string().as_bytes(),
    )
    .expect("run delegation request")
}

/// Starts a run delegation with the assistant's `token`.
fn create_delegation(
    service: &Service,
    token: &str,
    key: &ed25519_dalek::SigningKey,
) -> support::Response {
    delegation_request(
        service,
        CreateRunDelegation::ROUTE,
        token,
        &json!({
            "run_id": RunId::generate().to_string(),
            "key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            "lifetime_secs": 900,
        }),
    )
}

/// Exchanges `delegation` for a token to certs, with a proof of `key` made now.
fn exchange_for_certs(
    service: &Service,
    delegation: &str,
    key: &ed25519_dalek::SigningKey,
) -> support::Response {
    use ed25519_dalek::Signer;
    let iat = identity::issuer::unix_now();
    let message = run_proof_message(delegation, ServiceName::Certs, iat);
    let proof = URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes());
    delegation_request(
        service,
        ExchangeRunDelegation::ROUTE,
        delegation,
        &json!({"audience": "certs", "iat": iat, "proof": proof}),
    )
}

/// Acceptance 1 to 3 of Laysi/trustiot-v2#777: a suspension applies to the next request of
/// every session in the tenant, a resumption too; meanwhile the session, switching, preferences
/// and logging out work, and so does a platform administrator's resumption from inside the
/// suspended tenant.
#[tokio::test(flavor = "multi_thread")]
async fn a_suspension_and_a_resumption_apply_to_the_next_request() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let service = start(&product, "suspension-applies");
    let mut admin = product.superuser().await;
    let alice = person(&mut admin, false).await;
    let pat = person(&mut admin, true).await;
    let plant = tenant(&mut admin, "Plant").await;
    let depot = tenant(&mut admin, "Depot").await;
    member(&mut admin, &plant, &alice, "admin").await;
    member(&mut admin, &depot, &alice, "viewer").await;
    // The platform administrator acts in the tenant they will suspend.
    member(&mut admin, &plant, &pat, "admin").await;
    let alices = session(&mut admin, &alice, &plant).await;
    let alices_phone = session(&mut admin, &alice, &plant).await;
    let pats = session(&mut admin, &pat, &plant).await;

    // Before: every route, the operation log and the run delegation work.
    for path in [ASSETS, ASSISTANT, EVENT_LOG] {
        assert_passes(&service, path, &alices.cookie, &plant);
    }
    let response = call(&service, "GET", EVENT_LOG, &alices.cookie, None);
    assert_eq!(response.status, 200, "{}", response.raw());
    let assistant_token = token(&service, "POST", ASSISTANT, &alices.cookie);
    let key = run_key(31);
    let created = create_delegation(&service, &assistant_token, &key);
    assert_eq!(created.status, 200, "{}", created.raw());
    let delegation = created.json()["delegation"]
        .as_str()
        .expect("a delegation")
        .to_owned();
    let exchanged = exchange_for_certs(&service, &delegation, &key);
    assert_eq!(exchanged.status, 200, "{}", exchanged.raw());

    // The platform administrator suspends the tenant through the API.
    let suspend = json!({"state": "suspended", "reason": "non_payment"});
    let response = call(
        &service,
        "PUT",
        &state_path(&plant),
        &pats.cookie,
        Some(&suspend),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["state"], json!("suspended"));
    assert_eq!(response.json()["state_version"], json!(2));

    // 1. The next request of every session in the tenant: 403 `tenant_suspended` at the
    //    ingress for other services, whatever the method and the user's role there.
    for cookie in [&alices.cookie, &alices_phone.cookie, &pats.cookie] {
        for (method, path) in [
            ("GET", ASSETS),
            ("POST", ASSETS),
            ("GET", ASSISTANT),
            ("POST", ASSISTANT),
        ] {
            assert_suspended(
                &check(&service, method, path, cookie),
                &format!("{method} {path}"),
            );
        }
    }
    //    This service's own tenant-scoped endpoint: the token names the suspension, and the
    //    endpoint refuses it.
    let identity_token = token(&service, "GET", EVENT_LOG, &alices.cookie);
    assert_eq!(claims(&identity_token)["tenant_suspended"], json!(true));
    assert_eq!(claims(&identity_token)["tenant"]["id"], json!(plant.id));
    let response = call(&service, "GET", EVENT_LOG, &alices.cookie, None);
    assert_eq!(response.status, 403, "{}", response.raw());
    assert_eq!(response.json(), tenant_suspended());
    //    The run delegation's next exchange, and a new delegation with the assistant's token
    //    from before the suspension (still within its 60 seconds).
    assert_suspended(
        &exchange_for_certs(&service, &delegation, &key),
        "exchanging the run delegation",
    );
    assert_suspended(
        &create_delegation(&service, &assistant_token, &run_key(32)),
        "starting a run delegation",
    );

    // 2. Meanwhile: the session, with each tenant's state; preferences; switching, to the
    //    suspended tenant too; logging out.
    let response = call(&service, "GET", SESSION, &alices.cookie, None);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(
        response.json()["active_tenant"],
        membership(&plant, "admin", "suspended")
    );
    let tenants = response.json()["tenants"].clone();
    let listed = tenants.as_array().expect("tenants");
    assert_eq!(listed.len(), 2, "{tenants}");
    assert!(
        listed.contains(&membership(&plant, "admin", "suspended")),
        "{tenants}"
    );
    assert!(
        listed.contains(&membership(&depot, "viewer", "active")),
        "{tenants}"
    );
    let response = call(
        &service,
        "PATCH",
        PREFERENCES,
        &alices.cookie,
        Some(&json!({"theme": "dark"})),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["theme"], json!("dark"));

    let response = call(
        &service,
        "PUT",
        SWITCH,
        &alices.cookie,
        Some(&json!({"tenant_id": depot.id})),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(
        response.json()["active_tenant"],
        membership(&depot, "viewer", "active")
    );
    assert_passes(&service, ASSETS, &alices.cookie, &depot);
    let response = call(
        &service,
        "PUT",
        SWITCH,
        &alices.cookie,
        Some(&json!({"tenant_id": plant.id})),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(
        response.json()["active_tenant"],
        membership(&plant, "admin", "suspended")
    );
    assert_suspended(
        &check(&service, "GET", ASSETS, &alices.cookie),
        "back in it",
    );

    let response = call(&service, "DELETE", SESSION, &alices_phone.cookie, None);
    assert_eq!(response.status, 204, "{}", response.raw());
    let revoked: bool = sqlx::query_scalar(REVOKED_AT)
        .bind(&alices_phone.id)
        .fetch_one(&mut admin)
        .await
        .expect("read the session");
    assert!(revoked, "logging out revoked the session");
    assert_eq!(
        check(&service, "GET", SESSION, &alices_phone.cookie).status,
        401
    );

    //    The platform administrator, whose current tenant is the suspended one, reads its state
    //    and resumes it.
    let response = call(&service, "GET", &state_path(&plant), &pats.cookie, None);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["reason"], json!("non_payment"));
    let response = call(
        &service,
        "PUT",
        &state_path(&plant),
        &pats.cookie,
        Some(&json!({"state": "active"})),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["state"], json!("active"));
    assert_eq!(response.json()["state_version"], json!(3));

    // 3. After the resumption the next request works again: other services, the operation log,
    //    the same run delegation, and the session shows the tenant active.
    for cookie in [&alices.cookie, &pats.cookie] {
        for path in [ASSETS, ASSISTANT, EVENT_LOG] {
            assert_passes(&service, path, cookie, &plant);
        }
    }
    let response = call(&service, "GET", EVENT_LOG, &alices.cookie, None);
    assert_eq!(response.status, 200, "{}", response.raw());
    let exchanged = exchange_for_certs(&service, &delegation, &key);
    assert_eq!(exchanged.status, 200, "{}", exchanged.raw());
    assert_eq!(
        call(&service, "GET", SESSION, &alices.cookie, None).json()["active_tenant"],
        membership(&plant, "admin", "active")
    );

    let logs = service.stop();
    assert!(
        logs.contains(r#""reason":"tenant_suspended""#),
        "the refusals are logged with their reason"
    );
    for credential in [&delegation, &assistant_token, &identity_token] {
        let (_, mac) = credential.rsplit_once('.').expect("a signed value");
        assert!(!logs.contains(mac), "a credential reached the logs");
    }
}

/// A deleted tenant (the end of a terminated contract, Laysi/trustiot-v2#109) is cut off like a
/// suspended one, for good: the session names it `deleted`, and the user can still switch away.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_tenant_is_cut_off_like_a_suspended_one() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let service = start(&product, "suspension-deleted");
    let mut admin = product.superuser().await;
    let bob = person(&mut admin, false).await;
    let ended = tenant(&mut admin, "Ended").await;
    let other = tenant(&mut admin, "Other").await;
    member(&mut admin, &ended, &bob, "editor").await;
    member(&mut admin, &other, &bob, "editor").await;
    let bobs = session(&mut admin, &bob, &ended).await;
    assert_passes(&service, ASSETS, &bobs.cookie, &ended);

    run(&mut admin, TERMINATE, &[&ended.id]).await;
    assert_suspended(&check(&service, "GET", ASSETS, &bobs.cookie), "terminated");
    run(&mut admin, DELETE_TENANT, &[&ended.id]).await;
    for path in [ASSETS, ASSISTANT] {
        assert_suspended(&check(&service, "GET", path, &bobs.cookie), path);
    }
    let response = call(&service, "GET", EVENT_LOG, &bobs.cookie, None);
    assert_eq!(response.status, 403, "{}", response.raw());
    assert_eq!(response.json(), tenant_suspended());
    let response = call(&service, "GET", SESSION, &bobs.cookie, None);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(
        response.json()["active_tenant"],
        membership(&ended, "editor", "deleted")
    );
    let response = call(
        &service,
        "PUT",
        SWITCH,
        &bobs.cookie,
        Some(&json!({"tenant_id": other.id})),
    );
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_passes(&service, ASSETS, &bobs.cookie, &other);
}
