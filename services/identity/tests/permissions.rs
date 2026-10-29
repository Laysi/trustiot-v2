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
use support::{cookie, new_secret, start_on_product};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::product_postgres;

/// Why creating a tenant is declared with a known gap.
const CREATE_TENANT_GAP: &str = "creating a tenant checks the platform administrator flag only; \
     the decision of Laysi/trustiot-v2#104 (2026-10-23) says own requests only, and the fix is \
     the follow-up proposed in Laysi/trustiot-v2#584";

/// Every operation of `identity::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let platform = |operation: &str| Operation::new(operation, Access::PlatformAdmin);
    let own = |operation: &str| Operation::new(operation, Access::OwnRequest);
    Suite::new(ServiceName::Identity, issuer)
        .openapi(serde_json::to_value(identity::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(Operation::new("POST /auth/login", Access::Public))
        .operation(Operation::new(
            "DELETE /api/identity/v1/sessions",
            Access::SignedIn,
        ))
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
        .operation(
            platform("POST /api/identity/v1/tenants").accepts_exchanged_tokens(CREATE_TENANT_GAP),
        )
        .operation(platform(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/members",
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
