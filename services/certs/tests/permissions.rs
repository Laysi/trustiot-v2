//! The permission matrix of the certs service (A2, Laysi/trustiot-v2#584;
//! docs/architecture/permissions.md, section `## certs`): every operation of its client API
//! declares who may call it, the declarations must match the OpenAPI document and the matrix
//! document, and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`): without a token, with a forged token sent straight to the
//! service, as every tenant role, and as a platform administrator through a token another
//! service exchanged. None of those calls may write anything: the ledger of issued and revoked
//! certificates, the registrations, their nonces and the outbox are compared before and after.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::ServiceName;
use sqlx::Connection as _;
use support::{Service, TestCa};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// What the probes must leave unchanged: certificates issued and revoked, registrations, their
/// workstation nonces, and events written.
const WRITTEN: &str = "SELECT
    (SELECT count(*) FROM certs.issued_certificates),
    (SELECT count(*) FROM certs.issued_certificates WHERE revoked_at IS NOT NULL),
    (SELECT count(*) FROM certs.gateway_registrations),
    (SELECT count(*) FROM certs.workstation_request_nonces),
    (SELECT count(*) FROM certs.outbox_event)";

/// Every operation of `certs::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let platform = |operation: &str| Operation::new(operation, Access::PlatformAdmin);
    Suite::new(ServiceName::Certs, issuer)
        .openapi(serde_json::to_value(certs::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(platform("POST /api/certs/v1/workstation-certificates"))
        .operation(platform(
            "POST /api/certs/v1/workstation-certificates/{serial}/revoke",
        ))
        .operation(platform("POST /api/certs/v1/gateway-registrations"))
        .operation(platform("POST /api/certs/v1/gateways/{hardware_id}/revoke"))
}

/// The counts of [`WRITTEN`], read as the superuser.
async fn written(product: &ProductPostgres) -> (i64, i64, i64, i64, i64) {
    let mut connection = sqlx::PgConnection::connect_with(&product.postgres.superuser(DATABASE))
        .await
        .expect("connect as the superuser");
    sqlx::query_as(WRITTEN)
        .fetch_one(&mut connection)
        .await
        .expect("count what the service wrote")
}

#[test]
fn the_matrix_document_matches_the_declarations() {
    suite(&TestIssuer::new())
        .check_declarations()
        .assert_passed("the certs service's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers_and_refusals_write_nothing() {
    let Some(product) = product_postgres(&["certs_app", "certs_platform"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let service = Service::start(&product, &ca, &[]).await;
    let before = written(&product).await;
    suite(&service.issuer)
        .run(&service.base_url)
        .await
        .assert_passed("the certs service's permission matrix");
    assert_eq!(
        written(&product).await,
        before,
        "(issued, revoked, registrations, nonces, events) changed during the probes"
    );
}
