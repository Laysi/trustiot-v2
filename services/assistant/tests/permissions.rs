//! The permission matrix of the assistant (A2, Laysi/trustiot-v2#585 and Laysi/trustiot-v2#645;
//! docs/architecture/permissions.md, section `## assistant`): every operation of its client API
//! declares who may call it, the declarations must match the OpenAPI document and the matrix
//! document, and each operation is called as every kind of caller its requirement tells apart
//! (crates/testkit, `permissions`): without a token, with a forged token sent straight to the
//! service, as every tenant role and a role this build does not know, without a tenant, and with
//! a token another service exchanged. None of those calls may write anything: the probes send no
//! body and no `Content-Type`, so even an admitted start or restore stops at the 400 of a write
//! without JSON, and the conversations, messages, discarded rounds and runs are compared before
//! and after.
//!
//! Every role of the tenant may use the API, each only for its own conversations: who owns a
//! conversation is the store's check, which `tests/api.rs` and `tests/cross_tenant.rs` test.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod service;

use assistant::api::conversations::{
    CONVERSATION_PATH, CONVERSATIONS_PATH, DELETED_CONVERSATIONS_PATH, RESTORE_PATH,
};
use contract::{ServiceName, TenantRole};
use service::Service;
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::ProductPostgres;
use testkit::product::product_postgres;

/// What the probes must leave unchanged: conversations, their messages, discarded rounds and
/// question runs.
const WRITTEN: &str = "SELECT
    (SELECT count(*) FROM assistant.conversations),
    (SELECT count(*) FROM assistant.messages),
    (SELECT count(*) FROM assistant.discarded_rounds),
    (SELECT count(*) FROM assistant.runs)";

/// Every operation of `assistant::openapi()` and what it requires: a role in the tenant, any
/// of them.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let viewer = |method: &str, path: &str| {
        Operation::new(
            &format!("{method} {path}"),
            Access::Tenant(TenantRole::Viewer),
        )
    };
    Suite::new(ServiceName::Assistant, issuer)
        .openapi(serde_json::to_value(assistant::openapi()).expect("the OpenAPI document"))
        .matrix(MATRIX)
        .operation(viewer("GET", CONVERSATIONS_PATH))
        .operation(viewer("POST", CONVERSATIONS_PATH))
        .operation(viewer("GET", CONVERSATION_PATH))
        .operation(viewer("PATCH", CONVERSATION_PATH))
        .operation(viewer("DELETE", CONVERSATION_PATH))
        .operation(viewer("POST", RESTORE_PATH))
        .operation(viewer("GET", DELETED_CONVERSATIONS_PATH))
}

/// The counts of [`WRITTEN`], read as the superuser.
async fn written(product: &ProductPostgres) -> (i64, i64, i64, i64) {
    let mut connection = product.superuser().await;
    sqlx::query_as(WRITTEN)
        .fetch_one(&mut connection)
        .await
        .expect("count what the service wrote")
}

#[test]
fn the_matrix_document_matches_the_declarations() {
    suite(&TestIssuer::new())
        .check_declarations()
        .assert_passed("the assistant's permission matrix");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_operation_admits_exactly_its_declared_callers_and_refusals_write_nothing() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assistant = Service::start(&product, &issuer.jwks(), &[]).await;
    let before = written(&product).await;
    suite(&issuer)
        .run(&assistant.base_url)
        .await
        .assert_passed("the assistant's permission matrix");
    assert_eq!(
        written(&product).await,
        before,
        "(conversations, messages, discarded rounds, runs) changed during the probes"
    );
}
