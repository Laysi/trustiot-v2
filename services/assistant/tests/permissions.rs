//! The permission matrix of the assistant (A2, Laysi/trustiot-v2#585, Laysi/trustiot-v2#645 and
//! Laysi/trustiot-v2#150; docs/architecture/permissions.md, section `## assistant`): every
//! operation of its client API declares who may call it, the declarations must match the OpenAPI
//! document and the matrix document, and each operation is called as every kind of caller its
//! requirement tells apart (crates/testkit, `permissions`): without a token, with a forged token
//! sent straight to the service, as every tenant role and a role this build does not know,
//! without a tenant, and with a token another service exchanged. None of those calls may write
//! anything: the probes send no body and no `Content-Type`, so even an admitted start, restore,
//! publish or dataset run stops at the 400 of a request without JSON, and an admitted archive
//! finds no dashboard of the made-up ID (404). The conversations, messages, discarded rounds,
//! runs, dashboards and dashboard versions are compared before and after.
//!
//! Every role of the tenant may use the conversations and read, restore and run the dashboards;
//! publishing and archiving need editor. Who owns a conversation and its drafts is the store's
//! check, which `tests/api.rs`, `tests/dashboards_api.rs` and `tests/cross_tenant.rs` test.
//!
//! No probe reaches another service: the addresses of the identity service's token exchange,
//! telemetry-query, assets and the dashboard checker are loopback ports that only count the
//! connections they accept and close them, and the test fails when a probe opened one.
//!
//! `the_matrix_document_matches_the_declarations` needs nothing. The probes start a throwaway
//! Postgres with TLS (podman or docker) and the real binary; skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod service;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assistant::api::conversations::{
    CONVERSATION_PATH, CONVERSATIONS_PATH, DELETED_CONVERSATIONS_PATH, RESTORE_PATH,
};
use assistant::api::dashboards::{
    ARCHIVE_PATH, CONVERSATION_DASHBOARDS_PATH, DASHBOARD_PATH, DASHBOARDS_PATH, DATASET_RUNS_PATH,
    PUBLISH_PATH, RESTORE_PATH as DASHBOARD_RESTORE_PATH, VERSIONS_PATH,
};
use contract::{ServiceName, TenantRole};
use service::Service;
use testkit::identity::TestIssuer;
use testkit::permissions::{Access, MATRIX, Operation, Suite};
use testkit::product::ProductPostgres;
use testkit::product::product_postgres;
use tokio::net::TcpListener;

/// What the probes must leave unchanged: conversations, their messages, discarded rounds,
/// question runs, dashboards and their versions.
const WRITTEN: &str = "SELECT
    (SELECT count(*) FROM assistant.conversations),
    (SELECT count(*) FROM assistant.messages),
    (SELECT count(*) FROM assistant.discarded_rounds),
    (SELECT count(*) FROM assistant.runs),
    (SELECT count(*) FROM assistant.dashboards),
    (SELECT count(*) FROM assistant.dashboard_versions)";

/// The counts of [`WRITTEN`].
type Written = (i64, i64, i64, i64, i64, i64);

/// The services a dashboards request could reach, by the variable that sets each address.
const PEERS: [&str; 4] = [
    "TRUSTIOT__IDENTITY__EXCHANGE_URL",
    "TRUSTIOT__TELEMETRY_QUERY__URL",
    "TRUSTIOT__ASSETS__URL",
    "TRUSTIOT__DASHBOARD_CHECK__URL",
];

/// The operations that need editor, by the action their refusal logs.
const EDITOR_ACTIONS: [&str; 3] = [
    "publish a dashboard",
    "archive a dashboard",
    "unarchive a dashboard",
];

/// Every operation of `assistant::openapi()` and what it requires: a role in the tenant, any
/// of them, except publishing and archiving a dashboard, which need editor.
fn suite(issuer: &TestIssuer) -> Suite<'_> {
    let operation = |method: &str, path: &str, role: TenantRole| {
        Operation::new(&format!("{method} {path}"), Access::Tenant(role))
    };
    let viewer = |method: &str, path: &str| operation(method, path, TenantRole::Viewer);
    let editor = |method: &str, path: &str| operation(method, path, TenantRole::Editor);
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
        .operation(viewer("GET", CONVERSATION_DASHBOARDS_PATH))
        .operation(viewer("GET", DASHBOARDS_PATH))
        .operation(viewer("GET", DASHBOARD_PATH))
        .operation(viewer("GET", VERSIONS_PATH))
        .operation(viewer("POST", DATASET_RUNS_PATH))
        .operation(viewer("POST", DASHBOARD_RESTORE_PATH))
        .operation(editor("POST", PUBLISH_PATH))
        .operation(editor("PUT", ARCHIVE_PATH))
        .operation(editor("DELETE", ARCHIVE_PATH))
}

/// The counts of [`WRITTEN`], read as the superuser.
async fn written(product: &ProductPostgres) -> Written {
    let mut connection = product.superuser().await;
    sqlx::query_as(WRITTEN)
        .fetch_one(&mut connection)
        .await
        .expect("count what the service wrote")
}

/// A loopback port that accepts connections, counts them and closes them at once: a service
/// that is never reached unless the count says so.
async fn counting_port() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let address = listener.local_addr().expect("its address");
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (format!("http://{address}"), count)
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
    let mut peers = Vec::new();
    for variable in PEERS {
        let (url, count) = counting_port().await;
        peers.push((variable, url, count));
    }
    let env: Vec<(&str, &str)> = peers
        .iter()
        .map(|(variable, url, _)| (*variable, url.as_str()))
        .collect();
    let assistant = Service::start(&product, &issuer.jwks(), &env).await;
    let before = written(&product).await;
    suite(&issuer)
        .run(&assistant.base_url)
        .await
        .assert_passed("the assistant's permission matrix");
    assert_eq!(
        written(&product).await,
        before,
        "(conversations, messages, discarded rounds, runs, dashboards, dashboard versions) \
         changed during the probes"
    );
    for (variable, _, count) in &peers {
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "no probe may reach the service at {variable}: refused callers stop at the token, \
             admitted ones at the request's validation or the missing dashboard"
        );
    }
    // The handler's refusals are service-kit's: one INFO line each, with the reason.
    for action in EDITOR_ACTIONS {
        let refusals = assistant.stdout.messages(&format!("refused to {action}"));
        let reasons: Vec<&str> = refusals
            .iter()
            .filter(|line| line["level"] == "INFO")
            .filter_map(|line| line["fields"]["reason"].as_str())
            .collect();
        assert!(
            reasons.contains(&"role_too_low") && reasons.contains(&"no_tenant"),
            "the viewer's and the tenantless caller's refusals to {action}: {refusals:?}"
        );
    }
}
