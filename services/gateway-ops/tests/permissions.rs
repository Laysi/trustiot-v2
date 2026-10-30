//! The permission matrix of the gateway operations service (A2; docs/architecture/permissions.md,
//! section `## gateway-ops`; Laysi/trustiot-v2#112): every operation of its client API declares
//! who may call it, the declarations must match the OpenAPI document and the matrix document,
//! and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`). None of those calls may write anything: the bindings and the
//! presence rows are compared before and after.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary, whose broker is a closed loopback
//! port (the API needs no broker); skipped without a container runtime unless `CI` is set. Run
//! with `--nocapture` to see every probe.

mod support;

use std::net::TcpListener;

use contract::{ServiceName, TenantRole};
use support::broker::{Scratch, random_hex};
use support::{LOGINS, Service};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::{ProductPostgres, product_postgres};

/// What the probes must leave unchanged.
const WRITTEN: &str = "SELECT
    (SELECT count(*) FROM gateway_ops.gateway_bindings),
    (SELECT count(*) FROM gateway_ops.gateway_presence)";

/// Every operation of `gateway_ops::openapi()` and what it requires.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let viewer = |operation: &str| Operation::new(operation, Access::Tenant(TenantRole::Viewer));
    Suite::new(ServiceName::GatewayOps, issuer)
        .openapi(serde_json::to_value(gateway_ops::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(viewer("GET /api/gateway-ops/v1/gateways"))
}

async fn written(product: &ProductPostgres) -> (i64, i64) {
    let mut admin = product.superuser().await;
    sqlx::query_as(WRITTEN)
        .fetch_one(&mut admin)
        .await
        .expect("count what the service wrote")
}

#[test]
fn the_matrix_document_matches_the_declarations() {
    suite(&TestIssuer::new())
        .check_declarations()
        .assert_passed("the gateway-ops service's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers_and_refusals_write_nothing() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    // A loopback port nothing listens on: the service keeps reconnecting and serves the API.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind a free port")
        .local_addr()
        .expect("the bound address")
        .port();
    let scratch = Scratch::new();
    let mqtt = mqtt_client::Config {
        host: "127.0.0.1".to_owned(),
        port,
        username: "service:gateway-ops".to_owned(),
        password_file: scratch.write_secret("gateway-ops-password", &random_hex()),
    };
    let service = Service::start(&product, &mqtt, "1s").await;
    let before = written(&product).await;
    suite(&service.issuer)
        .run(&format!("http://{}", service.api))
        .await
        .assert_passed("the gateway-ops service's permission matrix");
    assert_eq!(
        written(&product).await,
        before,
        "(bindings, presence rows) changed during the probes"
    );
}
