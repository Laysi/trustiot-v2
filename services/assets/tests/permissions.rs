//! The permission matrix of the assets service (A2, Laysi/trustiot-v2#585;
//! docs/architecture/permissions.md, section `## assets`): every operation of its client API
//! declares who may call it, the declarations must match the OpenAPI document and the matrix
//! document, and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`): without a token, with a forged token sent straight to the
//! service, as every tenant role and a role this build does not know, without a tenant, and as a
//! platform administrator with and without a token another service exchanged. None of those
//! calls may write anything: the device models, devices, gateway bindings and the outbox are
//! compared before and after.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::{ServiceName, TenantRole};
use sqlx::Connection as _;
use support::Service;
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// What the probes must leave unchanged: device models and their points, devices, gateway
/// bindings, and events written.
const WRITTEN: &str = "SELECT
    (SELECT count(*) FROM assets.device_models),
    (SELECT count(*) FROM assets.device_model_points),
    (SELECT count(*) FROM assets.devices),
    (SELECT count(*) FROM assets.gateway_bindings),
    (SELECT count(*) FROM assets.outbox_event)";

/// Every operation of `assets::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let viewer = |operation: &str| Operation::new(operation, Access::Tenant(TenantRole::Viewer));
    let editor = |operation: &str| Operation::new(operation, Access::Tenant(TenantRole::Editor));
    let platform = |operation: &str| Operation::new(operation, Access::PlatformAdmin);
    Suite::new(ServiceName::Assets, issuer)
        .openapi(serde_json::to_value(assets::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(viewer("GET /api/assets/v1/tenant"))
        .operation(viewer("GET /api/assets/v1/device-models"))
        .operation(editor("POST /api/assets/v1/device-models"))
        .operation(editor("POST /api/assets/v1/device-models/import"))
        .operation(viewer("GET /api/assets/v1/device-models/{model_id}"))
        .operation(editor("PUT /api/assets/v1/device-models/{model_id}"))
        .operation(editor("DELETE /api/assets/v1/device-models/{model_id}"))
        .operation(viewer("GET /api/assets/v1/device-models/{model_id}/export"))
        .operation(viewer("GET /api/assets/v1/devices"))
        .operation(editor("POST /api/assets/v1/devices"))
        .operation(viewer("GET /api/assets/v1/devices/{device_id}"))
        .operation(editor("PUT /api/assets/v1/devices/{device_id}"))
        .operation(editor("DELETE /api/assets/v1/devices/{device_id}"))
        .operation(platform("GET /api/assets/v1/platform/gateways"))
        .operation(platform(
            "GET /api/assets/v1/platform/gateways/{hardware_id}",
        ))
        .operation(platform(
            "PUT /api/assets/v1/platform/gateways/{hardware_id}/binding",
        ))
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
        .assert_passed("the assets service's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers_and_refusals_write_nothing() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let before = written(&product).await;
    suite(&issuer)
        .run(&assets.base_url)
        .await
        .assert_passed("the assets service's permission matrix");
    assert_eq!(
        written(&product).await,
        before,
        "(models, points, devices, bindings, events) changed during the probes"
    );
}
