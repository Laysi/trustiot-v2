//! Platform administrators set users' roles in tenants, end to end (Laysi/trustiot-v2#583): the
//! real binary on the product's schema, applied by the migration Job (`testkit::product`),
//! sessions turned into internal identity tokens by the service's own ext_authz check, and
//! `PUT /api/identity/v1/platform/tenants/{tenant_id}/members`.
//!
//! - The member's next request through ext_authz carries the new role.
//! - Only a platform administrator's own request may set a role: a tenant administrator, a user
//!   without a tenant and a token another service exchanged (`act`) get 403, before anything is
//!   read, and nothing changes.
//! - An unknown tenant (404) or email address (422) writes nothing and creates no user.
//! - The tenant's operation log records `identity.member_role_changed` with both roles and the
//!   administrator as the actor, and no email address; setting the same role again writes nothing.
//! - The new grants (INSERT, UPDATE of `tenant_role`) stay inside the transaction's tenant.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::identity::Actor as TokenActor;
use contract::{ServiceName, TenantId, UserId};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::{
    KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body, signing_keys,
};
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-a";
const TENANTS: &str = "/api/identity/v1/tenants";
const LOG: &str = "/api/identity/v1/event-log";
/// A route of the assets service, for a token addressed to it.
const ASSETS_PATH: &str = "/api/assets/v1/tenant";
const EVENT_TYPE: &str = "identity.member_role_changed";

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits"): starting the binary, and
/// the event log consumer's delivery of a committed event (it polls every second when idle).
const READY_LIMIT: Duration = Duration::from_secs(60);
const EVENT_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// SQLSTATE `insufficient_privilege`, which a missing grant and a row-level security
/// `WITH CHECK` both raise.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

const INSERT_USER: &str =
    "INSERT INTO identity.users (id, platform_admin, email) VALUES ($1::uuid, $2, $3)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid, now() + interval '1 hour')";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";

/// Every membership, with its creation time, so a snapshot shows any write.
const MEMBERSHIPS: &str = "SELECT tenant_id::text, user_id::text, tenant_role, created_at::text
FROM identity.memberships ORDER BY tenant_id, user_id";
const USER_COUNT: &str = "SELECT count(*) FROM identity.users";
const OUTBOX_COUNT: &str = "SELECT count(*) FROM identity.outbox_event";
/// The membership events of one membership, in order: sequence, tenant, actor and payload.
const MEMBERSHIP_EVENTS: &str = "SELECT aggregate_seq, tenant_id::text,
    envelope->'actor'->>'user_id', (envelope->'payload')::text
FROM identity.outbox_event
WHERE event_type = 'identity.member_role_changed' AND aggregate_type = 'membership'
    AND aggregate_id = $1
ORDER BY aggregate_seq";
/// The tenant's recorded membership events.
const LOGGED_MEMBER_EVENTS: &str = "SELECT count(*) FROM identity.event_log
WHERE tenant_id = $1::uuid AND event_type = 'identity.member_role_changed'";
/// The membership events of a tenant in the outbox.
const OUTBOX_MEMBER_EVENTS: &str = "SELECT count(*) FROM identity.outbox_event
WHERE tenant_id = $1::uuid AND event_type = 'identity.member_role_changed'";

/// The identity binary on `product`, trusting the test issuer's tokens besides its own (for the
/// exchanged tokens no request through ext_authz can carry), with the event log on.
fn start(product: &ProductPostgres, issuer: &TestIssuer) -> Service {
    let key = TestKey::new(83);
    let mut published: Value = serde_json::from_str(&issuer.jwks()).expect("the issuer's key set");
    published["keys"]
        .as_array_mut()
        .expect("keys")
        .push(key.jwk(KID));
    let files = KeyFiles::with(
        "members",
        &signing_keys(&[(&key, KID, 1, 0)]),
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

/// A user made by the test tooling.
struct User {
    id: String,
    cookie: String,
}

/// A user (a platform administrator when `platform_admin`), with `email` when given, a role in
/// `member`'s tenant when given, and an active session acting in `active` (or in the member
/// tenant, or in none).
async fn user(
    admin: &mut PgConnection,
    platform_admin: bool,
    email: Option<&str>,
    member: Option<(&str, &str)>,
    active: Option<&str>,
) -> User {
    let id = UserId::generate().to_string();
    sqlx::query(INSERT_USER)
        .bind(&id)
        .bind(platform_admin)
        .bind(email)
        .execute(&mut *admin)
        .await
        .expect("insert the user");
    if let Some((tenant, role)) = member {
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(tenant)
            .bind(&id)
            .bind(role)
            .execute(&mut *admin)
            .await
            .expect("insert the membership");
    }
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&id)
        .bind(active.or(member.map(|(tenant, _)| tenant)))
        .execute(&mut *admin)
        .await
        .expect("insert the session");
    User {
        id,
        cookie: cookie(&secret),
    }
}

/// The token the ingress gateway would forward with a request to `path` for `cookie`: fresh,
/// from the service's own ext_authz check, which reads the role in that very check.
fn token_for(service: &Service, cookie: &str, path: &str) -> String {
    let response = request(
        service.address,
        "POST",
        &format!("/internal/ext-authz{path}"),
        &[("Host", APP), ("Cookie", cookie)],
    )
    .expect("call the ext_authz check");
    assert_eq!(response.status, 200, "{}", response.body);
    response.header_values("x-internal-identity")[0].to_owned()
}

fn token(service: &Service, cookie: &str) -> String {
    token_for(service, cookie, LOG)
}

/// The claims of a token, unverified (the test only reads what the check put in).
fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).expect("a compact JWS");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url")).expect("JSON")
}

/// The tenant grant of `cookie`'s next request: `{"id", "role"}`, or null.
fn next_grant(service: &Service, cookie: &str) -> Value {
    claims(&token(service, cookie))["tenant"].clone()
}

fn members_path(tenant: &str) -> String {
    format!("/api/identity/v1/platform/tenants/{tenant}/members")
}

/// `PUT` the members route of `tenant` with `token`, `content_type` and `body`.
fn put_as(
    service: &Service,
    token: Option<&str>,
    tenant: &str,
    content_type: Option<&str>,
    body: &str,
) -> support::Response {
    let mut headers = vec![("Host", APP)];
    if let Some(content_type) = content_type {
        headers.push(("Content-Type", content_type));
    }
    if let Some(token) = token {
        headers.push(("x-internal-identity", token));
    }
    let response = request_with_body(
        service.address,
        "PUT",
        &members_path(tenant),
        &headers,
        body.as_bytes(),
    )
    .expect("call the member API");
    // The handler's answers; a 401 comes from the kit's identity extractor, before it.
    if response.status != 401 {
        assert_eq!(
            response.header_values("cache-control"),
            ["no-store"],
            "{}",
            response.raw()
        );
    }
    response
}

fn put(service: &Service, token: &str, tenant: &str, body: &Value) -> support::Response {
    put_as(
        service,
        Some(token),
        tenant,
        Some("application/json"),
        &body.to_string(),
    )
}

fn create_tenant(service: &Service, cookie: &str, name: &str) -> String {
    let token = token_for(service, cookie, TENANTS);
    let response = request_with_body(
        service.address,
        "POST",
        TENANTS,
        &[
            ("Host", APP),
            ("Content-Type", "application/json"),
            ("x-internal-identity", &token),
        ],
        json!({ "name": name }).to_string().as_bytes(),
    )
    .expect("call the tenant API");
    assert_eq!(response.status, 201, "{}", response.body);
    response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned()
}

/// What a refused request must leave as it was.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    memberships: Vec<(String, String, String, String)>,
    users: i64,
    events: i64,
}

async fn snapshot(admin: &mut PgConnection) -> Snapshot {
    Snapshot {
        memberships: sqlx::query_as(MEMBERSHIPS)
            .fetch_all(&mut *admin)
            .await
            .expect("memberships"),
        users: sqlx::query_scalar(USER_COUNT)
            .fetch_one(&mut *admin)
            .await
            .expect("users"),
        events: sqlx::query_scalar(OUTBOX_COUNT)
            .fetch_one(&mut *admin)
            .await
            .expect("events"),
    }
}

/// The membership events of `user` in `tenant`: (sequence, tenant, actor, payload).
async fn membership_events(
    admin: &mut PgConnection,
    tenant: &str,
    user: &str,
) -> Vec<(i64, String, Option<String>, Value)> {
    let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(MEMBERSHIP_EVENTS)
        .bind(format!("{tenant}:{user}"))
        .fetch_all(admin)
        .await
        .expect("membership events");
    rows.into_iter()
        .map(|(seq, tenant, actor, payload)| {
            let payload = serde_json::from_str(&payload).expect("payload JSON");
            (seq, tenant, actor, payload)
        })
        .collect()
}

async fn count(admin: &mut PgConnection, statement: &'static str, tenant: &str) -> i64 {
    sqlx::query_scalar(statement)
        .bind(tenant)
        .fetch_one(admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"))
}

fn assert_refused(response: &support::Response, status: u16, code: &str, what: &str) {
    assert_eq!(response.status, status, "{what}: {}", response.body);
    assert_eq!(response.json()["code"], code, "{what}");
}

fn assert_field_error(response: &support::Response, errors: &[(&str, &str)], what: &str) {
    assert_eq!(response.status, 422, "{what}: {}", response.body);
    let body = response.json();
    assert_eq!(body["code"], "validation_failed", "{what}");
    let found: Vec<(String, String)> = body["field_errors"]
        .as_array()
        .expect("field errors")
        .iter()
        .map(|error| {
            (
                error["field"].as_str().expect("field").to_owned(),
                error["message_key"].as_str().expect("key").to_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = errors
        .iter()
        .map(|(field, key)| ((*field).to_owned(), (*key).to_owned()))
        .collect();
    assert_eq!(found, expected, "{what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn platform_administrators_set_member_roles_with_their_event() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = start(&product, &issuer);
    let mut admin = product.superuser().await;

    let platform = user(&mut admin, true, None, None, None).await;
    let tenant_a = create_tenant(&service, &platform.cookie, "Plant A");
    let tenant_b = create_tenant(&service, &platform.cookie, "Plant B");
    // Addresses are per run, so the shared database can run the test again.
    let run = UserId::generate().to_string();
    let member_email = format!("member-{run}@example.test");
    let member = user(
        &mut admin,
        false,
        Some(&member_email),
        None,
        Some(&tenant_a),
    )
    .await;
    let tenant_admin = user(&mut admin, false, None, Some((&tenant_a, "admin")), None).await;
    let b_admin = user(&mut admin, false, None, Some((&tenant_b, "admin")), None).await;
    let no_tenant = user(&mut admin, false, None, None, None).await;
    let platform_token = token(&service, &platform.cookie);

    // 1. Not a member yet: the session acts in tenant A, but its requests carry no tenant.
    assert_eq!(next_grant(&service, &member.cookie), Value::Null);

    // 2. A new membership: 201, and the member's next request carries the role. The address is
    //    compared as logging in compares it (trimmed, ASCII lower case).
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"email": format!("  {}  ", member_email.to_uppercase()), "role": "viewer"}),
    );
    assert_eq!(response.status, 201, "{}", response.body);
    assert_eq!(
        response.json(),
        json!({"tenant_id": tenant_a, "user_id": member.id, "role": "viewer", "previous_role": null})
    );
    assert_eq!(
        next_grant(&service, &member.cookie),
        json!({"id": tenant_a, "role": "viewer"})
    );

    // 3. A new role for the member: 200, and again on the next request.
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"email": member_email, "role": "editor"}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.json(),
        json!({"tenant_id": tenant_a, "user_id": member.id, "role": "editor", "previous_role": "viewer"})
    );
    assert_eq!(
        next_grant(&service, &member.cookie),
        json!({"id": tenant_a, "role": "editor"})
    );

    // 4. The same role again: 200, nothing written, no event.
    let before = snapshot(&mut admin).await;
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"email": member_email, "role": "editor"}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json()["previous_role"], "editor");
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "an unchanged role writes nothing"
    );

    // 5. One event per change, in the membership tenant's outbox, written by the request's
    //    transaction: the administrator as the actor, both roles, no email address.
    let events = membership_events(&mut admin, &tenant_a, &member.id).await;
    assert_eq!(
        events,
        [
            (
                1,
                tenant_a.clone(),
                Some(platform.id.clone()),
                json!({"user_id": member.id, "previous_role": null, "role": "viewer"})
            ),
            (
                2,
                tenant_a.clone(),
                Some(platform.id.clone()),
                json!({"user_id": member.id, "previous_role": "viewer", "role": "editor"})
            ),
        ]
    );

    // 6. Refusals: 403 for everyone but a platform administrator's own request, decided before
    //    anything is read. A nonexistent tenant and a body that is not even JSON show the order:
    //    each would answer 404 or 400 if they were looked at first. Nothing changes.
    let before = snapshot(&mut admin).await;
    let exchanged = |platform_admin: bool, service_name: ServiceName| {
        let mut claims = TestIssuer::claims(ServiceName::Identity, None);
        claims.sub = platform.id.parse().expect("user id");
        claims.platform_admin = platform_admin;
        claims.act = Some(TokenActor { sub: service_name });
        claims.sid = None;
        issuer.token(&claims)
    };
    let refused_tokens = [
        (
            "a tenant administrator",
            token(&service, &tenant_admin.cookie),
        ),
        (
            "another tenant's administrator",
            token(&service, &b_admin.cookie),
        ),
        (
            "a user without a tenant",
            token(&service, &no_tenant.cookie),
        ),
        (
            "a platform administrator's token exchanged by the assistant",
            exchanged(true, ServiceName::Assistant),
        ),
        (
            "a platform administrator's token exchanged by assets",
            exchanged(true, ServiceName::Assets),
        ),
    ];
    let unknown_tenant = TenantId::generate().to_string();
    let promote = json!({"email": member_email, "role": "admin"}).to_string();
    // A valid body padded past the limit: refused 400 for the administrator, 403 for others,
    // whose bodies the service does not read.
    let oversized = json!({
        "email": member_email,
        "role": "admin",
        "padding": "a".repeat(identity::members::MAX_BODY_BYTES),
    })
    .to_string();
    for (who, token) in &refused_tokens {
        for (tenant, content_type, body) in [
            (
                tenant_a.as_str(),
                Some("application/json"),
                promote.as_str(),
            ),
            (
                tenant_b.as_str(),
                Some("application/json"),
                promote.as_str(),
            ),
            (
                unknown_tenant.as_str(),
                Some("application/json"),
                promote.as_str(),
            ),
            ("not-a-uuid", Some("text/plain"), "not json"),
            (
                tenant_a.as_str(),
                Some("application/json"),
                oversized.as_str(),
            ),
        ] {
            let response = put_as(&service, Some(token), tenant, content_type, body);
            assert_refused(
                &response,
                403,
                "permission_denied",
                &format!("{who} on {tenant}"),
            );
        }
    }
    // No token, or a platform administrator's token addressed to another service.
    let response = put_as(
        &service,
        None,
        &tenant_a,
        Some("application/json"),
        &promote,
    );
    assert_refused(&response, 401, "unauthenticated", "no token");
    let for_assets = token_for(&service, &platform.cookie, ASSETS_PATH);
    let response = put_as(
        &service,
        Some(&for_assets),
        &tenant_a,
        Some("application/json"),
        &promote,
    );
    assert_refused(&response, 401, "unauthenticated", "a token for assets");
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "a refusal changes nothing"
    );
    assert_eq!(
        next_grant(&service, &member.cookie),
        json!({"id": tenant_a, "role": "editor"})
    );

    // 7. Unknown tenant or email address: nothing written, no user created; malformed requests
    //    are refused before the database.
    let response = put(
        &service,
        &platform_token,
        &unknown_tenant,
        &json!({"email": member_email, "role": "admin"}),
    );
    assert_refused(&response, 404, "not_found", "an unknown tenant");
    let unknown_email = format!("nobody-{run}@example.test");
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"email": unknown_email, "role": "viewer"}),
    );
    assert_field_error(
        &response,
        &[("/email", "member.email_unknown")],
        "an unknown email",
    );
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"email": "not an address", "role": "owner"}),
    );
    assert_field_error(
        &response,
        &[
            ("/email", "member.email_invalid"),
            ("/role", "member.role_invalid"),
        ],
        "an invalid email and role",
    );
    for role in ["unrecognized", "Admin", "platform_admin", ""] {
        let response = put(
            &service,
            &platform_token,
            &tenant_a,
            &json!({"email": member_email, "role": role}),
        );
        assert_field_error(&response, &[("/role", "member.role_invalid")], role);
    }
    let response = put(
        &service,
        &platform_token,
        "not-a-uuid",
        &json!({"email": member_email, "role": "admin"}),
    );
    assert_refused(
        &response,
        400,
        "invalid_request",
        "a tenant ID that is not a UUID",
    );
    // Only a JSON content type: a page of another origin can make the browser send the cookie
    // with the other types without a CORS preflight.
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
    ] {
        let response = put_as(
            &service,
            Some(&platform_token),
            &tenant_a,
            content_type,
            &promote,
        );
        assert_refused(
            &response,
            400,
            "invalid_request",
            &format!("{content_type:?}"),
        );
    }
    for body in [
        json!("admin"),
        json!({"email": member_email}),
        json!({"role": "admin"}),
        json!({"email": member_email, "role": 2}),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_refused(&response, 400, "invalid_request", &body.to_string());
    }
    let response = put_as(
        &service,
        Some(&platform_token),
        &tenant_a,
        Some("application/json"),
        &oversized,
    );
    assert_refused(&response, 400, "invalid_request", "a body over the limit");
    assert_eq!(snapshot(&mut admin).await, before, "nothing was written");

    // 8. The tenant's operation log, read by its administrator, has both changes with the
    //    administrator as the actor; tenant B's has none.
    let deadline = Instant::now() + EVENT_LIMIT;
    while count(&mut admin, LOGGED_MEMBER_EVENTS, &tenant_a).await < 2 {
        assert!(
            Instant::now() < deadline,
            "the event log did not record both changes within {EVENT_LIMIT:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
    let response = request(
        service.address,
        "GET",
        LOG,
        &[
            ("Host", APP),
            (
                "x-internal-identity",
                &token(&service, &tenant_admin.cookie),
            ),
        ],
    )
    .expect("read the event log");
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        !response.body.contains(&member_email),
        "an email address reached the operation log: {}",
        response.body
    );
    let logged: Vec<Value> = response.json()["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["type"] == EVENT_TYPE)
        .cloned()
        .collect();
    let summary: Vec<Value> = logged
        .iter()
        .map(|event| {
            json!({
                "tenant_id": event["tenant_id"],
                "producer": event["producer"],
                "aggregate": [event["aggregate_type"], event["aggregate_id"], event["aggregate_seq"]],
                "actor": event["actor"],
                "payload": event["payload"],
            })
        })
        .collect();
    let aggregate_id = format!("{tenant_a}:{}", member.id);
    let actor = json!({"kind": "user", "user_id": platform.id});
    assert_eq!(
        summary,
        [
            // Newest first.
            json!({
                "tenant_id": tenant_a, "producer": "identity",
                "aggregate": ["membership", aggregate_id, 2], "actor": actor,
                "payload": {"user_id": member.id, "previous_role": "viewer", "role": "editor"},
            }),
            json!({
                "tenant_id": tenant_a, "producer": "identity",
                "aggregate": ["membership", aggregate_id, 1], "actor": actor,
                "payload": {"user_id": member.id, "previous_role": null, "role": "viewer"},
            }),
        ]
    );
    assert_eq!(count(&mut admin, OUTBOX_MEMBER_EVENTS, &tenant_b).await, 0);
    assert_eq!(count(&mut admin, LOGGED_MEMBER_EVENTS, &tenant_b).await, 0);

    // 9. Two requests for one new membership at once: both succeed, one creates it, and the
    //    events chain (the second names the role the first set), whichever came first.
    let racer_email = format!("racer-{run}@example.test");
    let racer = user(&mut admin, false, Some(&racer_email), None, Some(&tenant_b)).await;
    let address = service.address;
    let racers: Vec<_> = ["admin", "viewer"]
        .into_iter()
        .map(|role| {
            let (token, email, path) = (
                platform_token.clone(),
                racer_email.clone(),
                members_path(&tenant_b),
            );
            std::thread::spawn(move || {
                let body = json!({"email": email, "role": role}).to_string();
                request_with_body(
                    address,
                    "PUT",
                    &path,
                    &[
                        ("Host", APP),
                        ("Content-Type", "application/json"),
                        ("x-internal-identity", &token),
                    ],
                    body.as_bytes(),
                )
                .expect("call the member API")
            })
        })
        .collect();
    let statuses: BTreeSet<u16> = racers
        .into_iter()
        .map(|racer| racer.join().expect("the request thread").status)
        .collect();
    assert_eq!(statuses, BTreeSet::from([200, 201]));
    let events = membership_events(&mut admin, &tenant_b, &racer.id).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].3["previous_role"], Value::Null, "{events:?}");
    assert_eq!(
        events[1].3["previous_role"], events[0].3["role"],
        "{events:?}"
    );
    assert_ne!(events[1].3["role"], events[0].3["role"], "{events:?}");
    assert_eq!(
        next_grant(&service, &racer.cookie),
        json!({"id": tenant_b, "role": events[1].3["role"]})
    );

    // 10. The service role's new grants stay in the transaction's tenant: in tenant A it cannot
    //     add a membership of tenant B, nor see (so change) one, and it can change nothing of a
    //     membership but its role.
    grants_stay_in_the_tenant(&product, &tenant_a, &tenant_b, &b_admin.id, &member.id).await;
    assert_eq!(
        next_grant(&service, &b_admin.cookie),
        json!({"id": tenant_b, "role": "admin"})
    );

    // 11. The log names who set which role, never the address.
    let output = service.stop();
    assert!(output.contains("set a member's role"), "{output}");
    assert!(
        output.contains("refused to set a member's role"),
        "{output}"
    );
    assert!(output.contains(&member.id), "the log names the member");
    for email in [&member_email, &racer_email, &unknown_email] {
        assert!(
            !output.to_lowercase().contains(email.as_str()),
            "an email address reached the log:\n{output}"
        );
    }
}

/// Statements of the service role in a transaction of `tenant_a` on memberships, through the
/// kit's `BackgroundTenantDb` (as the member store opens it): `b_user` is a member of
/// `tenant_b`, `a_member` a member of `tenant_a`.
async fn grants_stay_in_the_tenant(
    product: &ProductPostgres,
    tenant_a: &str,
    tenant_b: &str,
    b_user: &str,
    a_member: &str,
) {
    let (_requests, db): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("a verified connection");
    let tenant: TenantId = tenant_a.parse().expect("tenant id");
    // Each statement with its parameters: `b` tenant B, `u` the user of B, `m` the member of A.
    let refused: [(&str, &[&str]); 7] = [
        // A row of another tenant (WITH CHECK).
        (
            "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES ($1::uuid, $2::uuid, 'admin')",
            &[tenant_b, b_user],
        ),
        // Columns without a grant.
        (
            "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role, created_at) VALUES ($1::uuid, $2::uuid, 'admin', now())",
            &[tenant_a, b_user],
        ),
        (
            "UPDATE identity.memberships SET tenant_id = $1::uuid WHERE user_id = $2::uuid",
            &[tenant_b, a_member],
        ),
        (
            "UPDATE identity.memberships SET user_id = $1::uuid WHERE user_id = $2::uuid",
            &[b_user, a_member],
        ),
        (
            "UPDATE identity.memberships SET created_at = now() WHERE user_id = $1::uuid",
            &[a_member],
        ),
        (
            "DELETE FROM identity.memberships WHERE user_id = $1::uuid",
            &[a_member],
        ),
        ("TRUNCATE identity.memberships", &[]),
    ];
    for (statement, args) in refused {
        let mut tx = db
            .begin(tenant, "test-grants")
            .await
            .expect("a transaction");
        let mut query = sqlx::query(statement);
        for arg in args {
            query = query.bind(*arg);
        }
        let error = query.execute(&mut *tx).await.expect_err(statement);
        let code = error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        assert_eq!(
            code.as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}: {error}"
        );
    }
    // Another tenant's membership is invisible, so not changed: no row.
    let mut tx = db
        .begin(tenant, "test-grants")
        .await
        .expect("a transaction");
    let changed = sqlx::query(
        "UPDATE identity.memberships SET tenant_role = 'viewer' WHERE tenant_id = $1::uuid AND user_id = $2::uuid",
    )
    .bind(tenant_b)
    .bind(b_user)
    .execute(&mut *tx)
    .await
    .expect("the update runs")
    .rows_affected();
    assert_eq!(changed, 0, "a membership of another tenant was changed");
    tx.rollback().await.expect("roll back");
}
