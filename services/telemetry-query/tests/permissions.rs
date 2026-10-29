//! The permission matrix of the telemetry-query service (A2, Laysi/trustiot-v2#585;
//! docs/architecture/permissions.md, section `## telemetry-query`): every operation of its client
//! API declares who may call it, the declarations must match the OpenAPI document and the matrix
//! document, and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`): without a token, with a forged token sent straight to the
//! service, as every tenant role and a role this build does not know, without a tenant, and as a
//! platform administrator without a tenant.
//!
//! The probes need no ClickHouse: a refused caller is refused from the token before anything is
//! read, and an admitted one's request without parameters or body fails validation (400), or,
//! for the latest values, finds no account of its made-up tenant in Postgres (404) before any
//! query would run. The service is pointed at a ClickHouse address that refuses connections, so
//! a probe that reached ClickHouse would show as 503 in the probe output.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::{ServiceName, TenantRole};
use support::{ClickHouseTarget, ROLES, Service, random_password};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::product_postgres;

/// A ClickHouse address nothing listens on (TCP port 1 on the loopback interface).
const NO_CLICKHOUSE: &str = "http://127.0.0.1:1";

/// Every operation of `telemetry_query::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let viewer = |operation: &str| Operation::new(operation, Access::Tenant(TenantRole::Viewer));
    Suite::new(ServiceName::TelemetryQuery, issuer)
        .openapi(serde_json::to_value(telemetry_query::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(viewer("GET /api/telemetry/v1/devices/{device_id}/series"))
        .operation(viewer("GET /api/telemetry/v1/devices/{device_id}/latest"))
        .operation(viewer("POST /api/telemetry/v1/statistics"))
        .operation(viewer("POST /api/telemetry/v1/usage"))
}

#[test]
fn the_matrix_document_matches_the_declarations() {
    suite(&TestIssuer::new())
        .check_declarations()
        .assert_passed("the telemetry-query service's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers() {
    let Some(product) = product_postgres(&ROLES).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let password = random_password();
    let service = Service::start(
        &product,
        &ClickHouseTarget {
            url: NO_CLICKHOUSE,
            password: &password,
        },
        &issuer.jwks(),
    )
    .await;
    suite(&issuer)
        .run(&service.base_url)
        .await
        .assert_passed("the telemetry-query service's permission matrix");
}
