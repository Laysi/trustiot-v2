//! The permission matrix of the identity service (A2, Laysi/trustiot-v2#584;
//! docs/architecture/permissions.md, section `## identity`): every operation of its client API
//! declares who may call it, the declarations must match the OpenAPI document and the matrix
//! document, and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`).
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::{ServiceName, TenantRole};
use support::{cookie, new_secret, request, request_with_body, start_on_product};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::product_postgres;

/// Every operation of `identity::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let platform = |operation: &str| Operation::new(operation, Access::PlatformAdmin);
    let own = |operation: &str| Operation::new(operation, Access::OwnRequest);
    Suite::new(ServiceName::Identity, issuer)
        .openapi(serde_json::to_value(identity::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(Operation::new("POST /auth/login", Access::Public))
        .operation(own("DELETE /api/identity/v1/sessions"))
        // Logging out revokes the session of the request's cookie, and answers 401 without one:
        // a cookie of the right form that names no session gets past it and revokes nothing.
        .operation(
            Operation::new("DELETE /api/identity/v1/sessions/current", Access::SignedIn)
                .header("cookie", cookie(&new_secret())),
        )
        .operation(own("GET /api/identity/v1/sessions/current"))
        .operation(own("PUT /api/identity/v1/sessions/current/tenant"))
        .operation(own("GET /api/identity/v1/preferences"))
        .operation(own("PATCH /api/identity/v1/preferences"))
        .operation(Operation::new(
            "GET /api/identity/v1/event-log",
            Access::Tenant(TenantRole::Admin),
        ))
        .operation(platform("POST /api/identity/v1/tenants"))
        .operation(platform(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/members",
        ))
        .operation(platform(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/state",
        ))
        .operation(platform(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/state",
        ))
        .operation(Operation::new(
            "GET /api/identity/v1/entitlements",
            Access::Tenant(TenantRole::Viewer),
        ))
        .operation(platform(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/entitlements",
        ))
        .operation(platform(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements",
        ))
        .operation(platform(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/event-log",
        ))
        .operation(platform("GET /api/identity/v1/platform/event-log"))
        .operation(platform("GET /api/identity/v1/platform/ai/model-roles"))
        .operation(platform(
            "PUT /api/identity/v1/platform/ai/model-roles/{role}",
        ))
        .operation(platform(
            "DELETE /api/identity/v1/platform/ai/model-roles/{role}",
        ))
}

#[test]
fn the_matrix_document_matches_the_declarations() {
    suite(&TestIssuer::new())
        .check_declarations()
        .assert_passed("the identity service's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    // No AI gateway admin URL: the model role API answers 503 to whoever gets past its check.
    let identity = start_on_product(&product, &issuer, "permissions");
    let base_url = format!("http://{}", identity.address);
    suite(&issuer)
        .run(&base_url)
        .await
        .assert_passed("the identity service's permission matrix");
    drop(identity);
}

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, NULL, now() + interval '1 hour')";
const LIVE_SESSIONS_OF: &str = "SELECT count(*) FROM identity.sessions
WHERE user_id = $1::uuid AND revoked_at IS NULL AND expires_at > now()";
const TENANTS_NAMED: &str = "SELECT count(*) FROM identity.tenants WHERE name = $1";

/// A token another service exchanged (`act`) cannot create a tenant or log the user out
/// everywhere: 403, and no tenant is created and no session revoked (Laysi/trustiot-v2#760).
/// The same requests with the user's own token (no `act`) do both, so the refusals are the
/// `act` claim's and nothing else's.
#[tokio::test(flavor = "multi_thread")]
async fn an_exchanged_token_creates_no_tenant_and_revokes_no_session() {
    const TENANT_NAME: &str = "Exchanged token plant";
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let identity = start_on_product(&product, &issuer, "permissions-exchanged");
    let mut admin = product.superuser().await;

    // A platform administrator with two sessions.
    let user = contract::UserId::generate();
    sqlx::query(INSERT_USER)
        .bind(user.to_string())
        .bind(true)
        .execute(&mut admin)
        .await
        .expect("insert the user");
    for _ in 0..2 {
        sqlx::query(INSERT_SESSION)
            .bind(new_secret().hash().as_bytes().as_slice())
            .bind(user.to_string())
            .execute(&mut admin)
            .await
            .expect("insert a session");
    }
    let token = |actor: Option<ServiceName>| {
        let mut claims = TestIssuer::claims(ServiceName::Identity, None);
        claims.sub = user;
        claims.platform_admin = true;
        claims.act = actor.map(|sub| contract::identity::Actor { sub });
        claims.sid = None;
        issuer.token(&claims)
    };
    let create = |token: &str| {
        request_with_body(
            identity.address,
            "POST",
            "/api/identity/v1/tenants",
            &[
                ("Content-Type", "application/json"),
                ("x-internal-identity", token),
            ],
            serde_json::json!({ "name": TENANT_NAME })
                .to_string()
                .as_bytes(),
        )
        .expect("call create tenant")
    };
    let log_out_everywhere = |token: &str| {
        request(
            identity.address,
            "DELETE",
            "/api/identity/v1/sessions",
            &[("x-internal-identity", token)],
        )
        .expect("call log out everywhere")
    };

    // 1. Exchanged by any service: 403, and nothing changed.
    for service in [ServiceName::Assistant, ServiceName::Assets] {
        let exchanged = token(Some(service));
        let response = create(&exchanged);
        assert_eq!(response.status, 403, "{service:?}: {}", response.body);
        assert_eq!(response.json()["code"], "permission_denied");
        let response = log_out_everywhere(&exchanged);
        assert_eq!(response.status, 403, "{service:?}: {}", response.body);
        assert_eq!(response.json()["code"], "permission_denied");
        assert!(
            response.header_values("set-cookie").is_empty(),
            "a refusal does not clear the cookie"
        );
    }
    let tenants: i64 = sqlx::query_scalar(TENANTS_NAMED)
        .bind(TENANT_NAME)
        .fetch_one(&mut admin)
        .await
        .expect("count the tenants");
    assert_eq!(tenants, 0, "no tenant was created");
    let sessions: i64 = sqlx::query_scalar(LIVE_SESSIONS_OF)
        .bind(user.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("count the sessions");
    assert_eq!(sessions, 2, "no session was revoked");

    // 2. The user's own requests do both.
    let own = token(None);
    let response = create(&own);
    assert_eq!(response.status, 201, "{}", response.body);
    let response = log_out_everywhere(&own);
    assert_eq!(response.status, 204, "{}", response.body);
    let sessions: i64 = sqlx::query_scalar(LIVE_SESSIONS_OF)
        .bind(user.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("count the sessions");
    assert_eq!(sessions, 0, "every session was revoked");

    // 3. Both refusals are logged at INFO with the reason, never the name.
    let output = identity.stop();
    assert!(
        output.contains("refused to create a tenant")
            && output.contains("refused to log out everywhere"),
        "{output}"
    );
    assert!(
        !output.contains(TENANT_NAME),
        "a tenant name reached the log"
    );
}
