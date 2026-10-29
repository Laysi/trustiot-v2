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
//! query would run. The service's ClickHouse address is a loopback port that only counts the
//! connections it accepts and closes them; the test fails when a probe opened one.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use contract::{ServiceName, TenantRole};
use support::{ClickHouseTarget, ROLES, Service, random_password};
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::product_postgres;
use tokio::net::TcpListener;

/// A loopback port that accepts connections, counts them and closes them at once: a ClickHouse
/// that is never reached unless the count says so.
async fn counting_port() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let port = listener.local_addr().expect("its address").port();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, count)
}

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
    let (clickhouse_port, clickhouse) = counting_port().await;
    let clickhouse_url = format!("http://127.0.0.1:{clickhouse_port}");
    let service = Service::start(
        &product,
        &ClickHouseTarget {
            url: &clickhouse_url,
            password: &password,
        },
        &issuer.jwks(),
    )
    .await;
    let before = clickhouse.load(Ordering::SeqCst);
    suite(&issuer)
        .run(&service.base_url)
        .await
        .assert_passed("the telemetry-query service's permission matrix");
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        before,
        "no probe may reach ClickHouse: refused callers stop at the token, admitted ones at \
         validation or the missing account"
    );
}
