//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#553,
//! Laysi/trustiot-v2#150): the assistant's service role cannot read or write another tenant's
//! conversations, messages, dashboards or dashboard versions, every relation the role can reach
//! is declared, and every operation of the public API is declared or exempted. The schema is the
//! product's, applied by the migration Job (`testkit::product`); the endpoints are attacked on
//! the assistant binary as the chart starts it, with the stand-in token exchange and
//! telemetry-query of `dashboard_world`, so a dataset run's positive control runs end to end.
//!
//! The service role has no UPDATE on the history tables (they are append-only) and only a
//! column UPDATE on dashboards, without `tenant_id`, so the suite's update and move probes pass
//! by the privilege refusal; `tests/history_store.rs` and `tests/dashboards_store.rs` test those
//! refusals on their own.
//!
//! Two operations are exempt, each with a hand-written test in `tests/dashboards_api.rs`: the
//! conversation's drafts (a list with a path parameter, which the suite cannot address) and
//! publishing (only a draft's conversation owner sees it, so an admin's positive control cannot
//! pass).
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod dashboard_world;

use contract::ServiceName;
use dashboard_world::{Binary, Peers};
use serde_json::json;
use testkit::isolation::Suite;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::DATABASE;

/// One conversation: `$2` is its ID, the canary its system prompt.
const SEED_CONVERSATION: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl)
VALUES ($1::uuid, $2::uuid, gen_random_uuid(), 'testkit', $3, '', '[]', 'medium', '5m')";

/// One message of a new conversation: `$2` is the conversation's ID, the canary the message's
/// text (in a bytea column, so the conversation's system prompt repeats it as text).
const SEED_MESSAGE: &str = "WITH conversation AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl)
    VALUES ($1::uuid, $2::uuid, gen_random_uuid(), 'testkit', $3, '', '[]', 'medium', '5m')
    RETURNING tenant_id, conversation_id
)
INSERT INTO assistant.messages (tenant_id, conversation_id, seq, kind, format, body)
SELECT tenant_id, conversation_id, 0, 'question', 'anthropic_messages',
    convert_to('{\"role\":\"user\",\"content\":\"' || $3 || '\"}', 'UTF8')
FROM conversation";

/// One saved dashboard with its version 1, published: `$2` is the dashboard's ID, the canary
/// the version's title. The spec has one SQL dataset, `testkit`, which the dataset run's
/// positive control runs. Only the columns the service role may insert, so the insert probe
/// meets row-level security, not a missing grant.
const SEED_DASHBOARD: &str = "WITH dashboard AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, created_by)
    VALUES ($1::uuid, $2::uuid, 'saved', gen_random_uuid())
    RETURNING tenant_id, dashboard_id, created_by
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by)
SELECT tenant_id, dashboard_id, 1, 'publish', 1, $3,
    '{\"datasets\":[{\"name\":\"testkit\",\"title\":\"Testkit\",\"description\":\"The dataset the cross-tenant suite runs.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
    '<main><h1>testkit</h1></main>', created_by
FROM dashboard";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(world) = dashboard_world::world().await else {
        return;
    };
    let peers = Peers::start(world.issuer.clone()).await;
    let assistant = Binary::start(&world.product, &world.issuer, &peers.url).await;
    let store = PgStore::new(
        world.product.postgres.superuser(DATABASE),
        world.product.postgres.options("assistant_app", DATABASE),
    )
    .table(pg_table!(
        "assistant.conversations",
        seed = SEED_CONVERSATION
    ))
    .table(pg_table!("assistant.messages", seed = SEED_MESSAGE))
    .table(pg_table!("assistant.dashboards", seed = SEED_DASHBOARD))
    .table(pg_table!(
        "assistant.dashboard_versions",
        seed = SEED_DASHBOARD
    ))
    .exempt(
        "assistant.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check",
    );
    let dashboards = "/api/assistant/v1/dashboards";
    let dashboard = "/api/assistant/v1/dashboards/{dashboard_id}";
    Suite::new(ServiceName::Assistant, &world.issuer, store)
        .openapi(serde_json::to_value(assistant::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list(dashboards, "assistant.dashboards"))
        .endpoint(Endpoint::get(dashboard, "assistant.dashboards"))
        .endpoint(Endpoint::get(
            "/api/assistant/v1/dashboards/{dashboard_id}/versions",
            "assistant.dashboard_versions",
        ))
        .endpoint(
            Endpoint::item(
                Method::POST,
                "/api/assistant/v1/dashboards/{dashboard_id}/restore",
                "assistant.dashboards",
            )
            .body(json!({"from_version": 1, "base_version": 1})),
        )
        .endpoint(Endpoint::item(
            Method::PUT,
            "/api/assistant/v1/dashboards/{dashboard_id}/archive",
            "assistant.dashboards",
        ))
        .endpoint(Endpoint::item(
            Method::DELETE,
            "/api/assistant/v1/dashboards/{dashboard_id}/archive",
            "assistant.dashboards",
        ))
        .endpoint(
            Endpoint::item(
                Method::POST,
                "/api/assistant/v1/dashboards/{dashboard_id}/dataset-runs",
                "assistant.dashboards",
            )
            .body(json!({"version": 1, "dataset": "testkit"})),
        )
        .exempt_endpoint(
            "GET /api/assistant/v1/conversations/{conversation_id}/dashboards",
            "a conversation's drafts: a list with a path parameter, which the suite cannot \
             address; tests/dashboards_api.rs tests that another tenant's admin and another \
             member of the same tenant get 404 for it",
        )
        .exempt_endpoint(
            "POST /api/assistant/v1/dashboards/{dashboard_id}/publish",
            "publishes a draft, which only its conversation's owner sees, so an admin's \
             positive control cannot pass; tests/dashboards_api.rs tests that another tenant's \
             admin and another member of the same tenant get 404 and nothing is written",
        )
        .run(Some(&assistant.url))
        .await
        .assert_isolated();
    // Every dataset run of the positive controls ran as its caller, and nothing else was
    // called: no AI gateway, no other service.
    assert!(peers.unexpected().is_empty(), "{:?}", peers.unexpected());
    assert!(
        !peers.calls().is_empty(),
        "the positive controls ran datasets"
    );
}
