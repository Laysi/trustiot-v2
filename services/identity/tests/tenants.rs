//! Creating tenants end to end (Laysi/trustiot-v2#65): the real binary on the product's schema,
//! applied by the migration Job (`testkit::product`), a platform administrator's session turned
//! into an internal identity token by the service's own ext_authz check, and
//! `POST /api/identity/v1/tenants`. The tenant row and `identity.tenant_created` commit in one
//! transaction or not at all.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{
    Service, TestKey, cookie, jwks, new_secret, request, request_with_body, scratch_dir,
};
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-a";
const PATH: &str = "/api/identity/v1/tenants";
/// A route of the assets service, for a token addressed to it.
const ASSETS_PATH: &str = "/api/assets/v1/tenant";

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid, now() + interval '1 hour')";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const TENANT: &str = "SELECT name FROM identity.tenants WHERE tenant_id = $1::uuid";
const TENANT_COUNT: &str = "SELECT count(*) FROM identity.tenants";
/// The tenant's event, and whether it was written by the transaction that inserted the row: the
/// event's ordering transaction is the writer's (a new aggregate), which in a fresh database
/// equals the row's `xmin`.
const EVENT: &str = "SELECT e.event_type, e.aggregate_type, e.aggregate_id, e.aggregate_seq,
    e.envelope::text, e.tx_order = t.xmin::text::bigint
FROM identity.outbox_event AS e
JOIN identity.tenants AS t ON t.tenant_id = e.tenant_id
WHERE e.tenant_id = $1::uuid";
const REVOKE_EVENTS: &str = "REVOKE INSERT ON identity.outbox_event FROM identity_app";
const GRANT_EVENTS: &str = "GRANT INSERT ON identity.outbox_event TO identity_app";

/// A user and an active session of theirs, made by the test tooling; the cookie header value.
async fn session(admin: &mut PgConnection, platform_admin: bool) -> String {
    member_session(admin, platform_admin, None).await
}

/// Like [`session`], and when `member` names a tenant and a role, the user has that role there
/// and the session acts in that tenant.
async fn member_session(
    admin: &mut PgConnection,
    platform_admin: bool,
    member: Option<(&str, &str)>,
) -> String {
    let user = contract::UserId::generate().to_string();
    sqlx::query(INSERT_USER)
        .bind(&user)
        .bind(platform_admin)
        .execute(&mut *admin)
        .await
        .expect("insert the user");
    if let Some((tenant, role)) = member {
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(tenant)
            .bind(&user)
            .bind(role)
            .execute(&mut *admin)
            .await
            .expect("insert the membership");
    }
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&user)
        .bind(member.map(|(tenant, _)| tenant))
        .execute(&mut *admin)
        .await
        .expect("insert the session");
    cookie(&secret)
}

/// The internal identity token the ingress gateway would forward with a request to `PATH`: the
/// service's own ext_authz check signs it for `identity`.
fn token(service: &Service, cookie: &str) -> String {
    token_for(service, cookie, PATH)
}

/// The token the ingress gateway would forward with a request to `path`, signed for the service
/// that the route of `path` names.
fn token_for(service: &Service, cookie: &str, path: &str) -> String {
    let check = format!("/internal/ext-authz{path}");
    let response = request(
        service.address,
        "POST",
        &check,
        &[("Host", APP), ("Cookie", cookie)],
    )
    .expect("call the ext_authz check");
    assert_eq!(response.status, 200, "{}", response.body);
    response.header_values("x-internal-identity")[0].to_owned()
}

fn create(service: &Service, token: Option<&str>, body: &Value) -> support::Response {
    create_as(service, token, Some("application/json"), &body.to_string())
}

/// `POST PATH` with `content_type` (none when `None`) and `body`.
fn create_as(
    service: &Service,
    token: Option<&str>,
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
    request_with_body(service.address, "POST", PATH, &headers, body.as_bytes())
        .expect("call the tenant API")
}

async fn tenant_count(admin: &mut PgConnection) -> i64 {
    sqlx::query_scalar(TENANT_COUNT)
        .fetch_one(admin)
        .await
        .expect("count tenants")
}

fn start(product: &ProductPostgres) -> Service {
    let dir = scratch_dir("tenants");
    let key = TestKey::new(65);
    let key_file = dir.join("signing-key.pem");
    std::fs::write(&key_file, key.pem()).expect("write the signing key");
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
        {"host": APP, "path": "/api/assets", "match": "PathPrefix", "service": "assets"},
    ]);
    let mut env: Vec<(String, String)> = vec![
        (
            "TRUSTIOT_INTERNAL_IDENTITY_KEYS".into(),
            jwks(&[(&key, KID)]),
        ),
        (
            "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE".into(),
            key_file.display().to_string(),
        ),
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
    ];
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(Duration::from_secs(60));
    service
}

#[tokio::test(flavor = "multi_thread")]
async fn platform_administrators_create_tenants_with_their_event() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let service = start(&product);
    let mut admin = product.superuser().await;
    let admin_token = token(&service, &session(&mut admin, true).await);
    let user_token = token(&service, &session(&mut admin, false).await);

    // 1. A platform administrator creates a tenant: 201, the row, and its event written in the
    //    same transaction.
    let response = create(
        &service,
        Some(&admin_token),
        &json!({"name": "  Plant A  "}),
    );
    assert_eq!(response.status, 201, "{}", response.body);
    let tenant = response.json();
    assert_eq!(tenant["name"], "Plant A", "trimmed");
    let id = tenant["tenant_id"].as_str().expect("tenant_id").to_owned();
    assert!(tenant["created_at"].is_string(), "{tenant}");
    let name: String = sqlx::query_scalar(TENANT)
        .bind(&id)
        .fetch_one(&mut admin)
        .await
        .expect("the tenant row");
    assert_eq!(name, "Plant A");
    let (event_type, aggregate_type, aggregate_id, seq, envelope, same_tx): (
        String,
        String,
        String,
        i64,
        String,
        bool,
    ) = sqlx::query_as(EVENT)
        .bind(&id)
        .fetch_one(&mut admin)
        .await
        .expect("exactly one event of the tenant");
    let envelope: Value = serde_json::from_str(&envelope).expect("the envelope is JSON");
    assert_eq!(event_type, "identity.tenant_created");
    assert_eq!(
        (aggregate_type.as_str(), aggregate_id.as_str(), seq),
        ("tenant", id.as_str(), 1)
    );
    assert_eq!(envelope["tenant_id"], id.as_str());
    assert_eq!(envelope["producer"], "identity");
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["payload"], json!({}), "no name in the event");
    assert!(
        same_tx,
        "the event was written by the transaction that inserted the tenant"
    );

    // 2. All or nothing: when the event cannot be written, the tenant is not created either.
    let before = tenant_count(&mut admin).await;
    sqlx::raw_sql(REVOKE_EVENTS)
        .execute(&mut admin)
        .await
        .expect("take away the event write");
    let response = create(&service, Some(&admin_token), &json!({"name": "Plant B"}));
    assert_eq!(response.status, 503, "{}", response.body);
    assert_eq!(response.json()["code"], "unavailable");
    assert_eq!(
        tenant_count(&mut admin).await,
        before,
        "no tenant without its event"
    );
    sqlx::raw_sql(GRANT_EVENTS)
        .execute(&mut admin)
        .await
        .expect("give the event write back");

    // 3. Refusals, none of which creates anything.
    let response = create(&service, Some(&user_token), &json!({"name": "Plant C"}));
    assert_eq!(response.status, 403, "{}", response.body);
    assert_eq!(response.json()["code"], "permission_denied");
    // An administrator of a tenant is not a platform administrator.
    let tenant_admin = member_session(&mut admin, false, Some((id.as_str(), "admin"))).await;
    let tenant_admin_token = token(&service, &tenant_admin);
    let response = create(
        &service,
        Some(&tenant_admin_token),
        &json!({"name": "Plant C"}),
    );
    assert_eq!(response.status, 403, "{}", response.body);
    assert_eq!(response.json()["code"], "permission_denied");
    let response = create(&service, None, &json!({"name": "Plant C"}));
    assert_eq!(response.status, 401, "{}", response.body);
    assert_eq!(response.json()["code"], "unauthenticated");
    // A platform administrator's token addressed to another service.
    let platform_cookie = session(&mut admin, true).await;
    let for_assets = token_for(&service, &platform_cookie, ASSETS_PATH);
    let response = create(&service, Some(&for_assets), &json!({"name": "Plant C"}));
    assert_eq!(response.status, 401, "{}", response.body);
    assert_eq!(response.json()["code"], "unauthenticated");
    // Only a JSON content type: a page of another origin can make the browser send the cookie
    // with the other types (no CORS preflight), so they must not create a tenant.
    let json_body = json!({"name": "Plant C"}).to_string();
    for content_type in [
        None,
        Some("text/plain"),
        Some("text/plain; charset=utf-8"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
    ] {
        let response = create_as(&service, Some(&admin_token), content_type, &json_body);
        assert_eq!(response.status, 400, "{content_type:?}: {}", response.body);
        assert_eq!(response.json()["code"], "invalid_request");
    }
    for body in [json!("Plant C"), json!({"name": 7}), json!({})] {
        let response = create(&service, Some(&admin_token), &body);
        assert_eq!(response.status, 400, "{body}: {}", response.body);
        assert_eq!(response.json()["code"], "invalid_request");
    }
    let too_long = "x".repeat(101);
    for name in ["", "   ", "a\u{7}b", too_long.as_str()] {
        let response = create(&service, Some(&admin_token), &json!({ "name": name }));
        assert_eq!(response.status, 422, "{name:?}: {}", response.body);
        assert_eq!(
            response.json(),
            json!({
                "code": "validation_failed",
                "message_key": "error.validation_failed",
                "field_errors": [
                    {"field": "/name", "message_key": "tenant.name_invalid", "details": {"max": 100}}
                ]
            })
        );
    }
    assert_eq!(tenant_count(&mut admin).await, before);

    // 4. The JSON content type may carry parameters.
    let response = create_as(
        &service,
        Some(&admin_token),
        Some("application/json; charset=utf-8"),
        &json_body,
    );
    assert_eq!(response.status, 201, "{}", response.body);
    assert_eq!(response.json()["name"], "Plant C");
    assert_eq!(tenant_count(&mut admin).await, before + 1);

    // 5. The log names who created which tenant, never its name.
    let output = service.stop();
    assert!(output.contains("created a tenant"), "{output}");
    assert!(output.contains(&id), "the log names the tenant ID");
    assert!(
        !output.contains("Plant"),
        "a tenant name reached the log:\n{output}"
    );
}
