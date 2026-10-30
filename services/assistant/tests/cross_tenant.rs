//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#553,
//! Laysi/trustiot-v2#644, Laysi/trustiot-v2#645 and Laysi/trustiot-v2#150): the assistant's
//! service role cannot read or write another tenant's conversations, messages, discarded rounds,
//! runs, dashboards or dashboard versions, through the database or through the API, and every
//! relation the role can reach and every operation of the API is declared. The schema is the
//! product's, applied by the migration Job (`testkit::product`), and the API is the binary
//! itself (`tests/service/mod.rs`), with the stand-in token exchange and telemetry-query of
//! `dashboard_world`, so a dataset run's positive control runs end to end.
//!
//! A conversation, and a draft dashboard in it, is served only to the user who started the
//! conversation, so their seeds are `owned`: the suite's one user owns rows in both tenants, and
//! every probe's token names that user. A request in tenant A for the user's own conversation or
//! draft in tenant B must answer as for a missing one; only the tenant keeps them apart, so an
//! API that checked only the owner would fail here. A saved dashboard is the tenant's: its seed
//! has no owner.
//!
//! The service role may update only some columns of conversations, runs and dashboards (none of
//! `tenant_id`) and nothing of messages, discarded rounds and dashboard versions, so the suite's
//! update and move probes, which write `tenant_id`, pass by the privilege refusal;
//! `tests/history_store.rs`, `tests/conversation_threads.rs` and `tests/dashboards_store.rs` test
//! the refusals on their own. The platform role
//! (`assistant_platform`, the purge) sees across tenants by design and is not attacked here
//! (crates/testkit/README.md, 「限制」); its policies are tested in
//! `tests/conversation_threads.rs`.
//!
//! The deleted list and restoring serve only deleted conversations, which the suite cannot seed
//! for its probes (its rows are not deleted, so the positive controls would fail);
//! `deleted_conversations_stay_in_their_tenant` probes them by hand, the same way. A
//! conversation's drafts are a list whose path parameter is the conversation's ID, which the
//! suite cannot address; `tests/dashboards_api.rs` probes it by hand.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod dashboard_world;
mod service;

use std::sync::Arc;

use assistant::api::conversations::{
    CONVERSATION_PATH, CONVERSATIONS_PATH, DELETED_CONVERSATIONS_PATH, RESTORE_PATH,
};
use assistant::api::dashboards::{
    ARCHIVE_PATH, DASHBOARD_PATH, DASHBOARDS_PATH, DATASET_RUNS_PATH, PUBLISH_PATH,
    RESTORE_PATH as DASHBOARD_RESTORE_PATH, VERSIONS_PATH,
};
use contract::identity::TenantRole;
use contract::{ServiceName, TenantId, UserId};
use dashboard_world::Peers;
use reqwest::Method as HttpMethod;
use serde_json::{Value, json};
use service::{Service, User};
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};
use uuid::Uuid;

/// One conversation of the user `$4`: `$2` is its ID, the canary its system prompt.
const SEED_CONVERSATION: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl)
VALUES ($1::uuid, $2::uuid, $4::uuid, 'testkit', $3, '', '[]', 'medium', '5m')";

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

/// One discarded round of a new conversation with one message: `$2` is the conversation's ID,
/// the canary its system prompt.
const SEED_DISCARDED_ROUND: &str = "WITH conversation AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl)
    VALUES ($1::uuid, $2::uuid, gen_random_uuid(), 'testkit', $3, '', '[]', 'medium', '5m')
    RETURNING tenant_id, conversation_id
), message AS (
    INSERT INTO assistant.messages (tenant_id, conversation_id, seq, kind, format, body)
    SELECT tenant_id, conversation_id, 0, 'question', 'anthropic_messages',
        convert_to('{}', 'UTF8')
    FROM conversation
    RETURNING tenant_id, conversation_id, seq
)
INSERT INTO assistant.discarded_rounds (tenant_id, conversation_id, from_seq, to_seq)
SELECT tenant_id, conversation_id, seq, seq FROM message";

/// One running question of a new conversation: `$2` is the run's ID, the canary the
/// conversation's system prompt.
const SEED_RUN: &str = "WITH conversation AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl)
    VALUES ($1::uuid, gen_random_uuid(), gen_random_uuid(), 'testkit', $3, '', '[]', 'medium',
            '5m')
    RETURNING tenant_id, conversation_id, user_id
)
INSERT INTO assistant.runs (tenant_id, run_id, conversation_id, user_id, deadline_at)
SELECT tenant_id, $2::uuid, conversation_id, user_id, now() + interval '10 minutes'
FROM conversation";

/// One saved dashboard with its versions 1 and 2, published: `$2` is the dashboard's ID, the
/// canary in both versions' titles, which differ, so the restore's positive control (version 1
/// on version 2) adds a version. The spec has one SQL dataset, `testkit`, which the dataset
/// run's positive control runs. Only the columns the service role may insert, so the insert probe
/// meets row-level security, not a missing grant.
const SEED_DASHBOARD: &str = "WITH dashboard AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, created_by)
    VALUES ($1::uuid, $2::uuid, 'saved', gen_random_uuid())
    RETURNING tenant_id, dashboard_id, created_by
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by)
SELECT tenant_id, dashboard_id, n, 'publish', 1, CASE n WHEN 1 THEN $3 ELSE $3 || ' 2' END,
    '{\"datasets\":[{\"name\":\"testkit\",\"title\":\"Testkit\",\"description\":\"The dataset the cross-tenant suite runs.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
    '<main><h1>testkit</h1></main>', created_by
FROM dashboard, generate_series(1, 2) AS n";

/// One draft with its versions 1 and 2, written by the `dashboard` tool in a new conversation of
/// the user `$4`: `$2` is the draft's ID, the canary in both versions' titles. The publish's
/// positive control publishes version 2 into a new saved dashboard. Only the columns the service
/// role may insert, so the insert probe meets row-level security, not a missing grant.
const SEED_DRAFT: &str = "WITH conversation AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl)
    VALUES ($1::uuid, gen_random_uuid(), $4::uuid, 'testkit', 'testkit', '', '[]', 'medium',
            '5m')
    RETURNING tenant_id, conversation_id, user_id
), dashboard AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, conversation_id, created_by)
    SELECT tenant_id, $2::uuid, 'draft', conversation_id, user_id FROM conversation
    RETURNING tenant_id, dashboard_id, created_by
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by, run_id)
SELECT tenant_id, dashboard_id, n, 'agent', 1, CASE n WHEN 1 THEN $3 ELSE $3 || ' 2' END,
    '{\"datasets\":[{\"name\":\"testkit\",\"title\":\"Testkit\",\"description\":\"The dataset the cross-tenant suite runs.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
    '<main><h1>testkit</h1></main>', created_by, gen_random_uuid()
FROM dashboard, generate_series(1, 2) AS n";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let issuer = Arc::new(TestIssuer::new());
    // The token exchange and telemetry-query a dataset run calls: every positive control of a
    // run is answered, and any other call is recorded as unexpected.
    let peers = Peers::start(Arc::clone(&issuer)).await;
    let service = Service::start(
        &product,
        &issuer.jwks(),
        &[
            ("TRUSTIOT__IDENTITY__EXCHANGE_URL", peers.url.as_str()),
            ("TRUSTIOT__TELEMETRY_QUERY__URL", peers.url.as_str()),
        ],
    )
    .await;
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("assistant_app", DATABASE),
    )
    .table(pg_table!(
        "assistant.conversations",
        seed = SEED_CONVERSATION,
        owned
    ))
    .table(pg_table!("assistant.messages", seed = SEED_MESSAGE))
    .table(pg_table!(
        "assistant.discarded_rounds",
        seed = SEED_DISCARDED_ROUND
    ))
    .table(pg_table!("assistant.runs", seed = SEED_RUN))
    .table(pg_table!("assistant.dashboards", seed = SEED_DASHBOARD))
    .table(pg_table!(
        "assistant.dashboard_versions",
        seed = SEED_DRAFT,
        owned
    ))
    .exempt(
        "assistant.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check",
    );
    let conversations = "assistant.conversations";
    let (saved, drafts) = ("assistant.dashboards", "assistant.dashboard_versions");
    Suite::new(ServiceName::Assistant, &issuer, store)
        .openapi(serde_json::to_value(assistant::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list(CONVERSATIONS_PATH, conversations))
        .endpoint(Endpoint::get(CONVERSATION_PATH, conversations))
        .endpoint(
            Endpoint::item(Method::PATCH, CONVERSATION_PATH, conversations)
                .body(json!({"title": "{canary}", "archived": true})),
        )
        .endpoint(Endpoint::item(
            Method::DELETE,
            CONVERSATION_PATH,
            conversations,
        ))
        // Starting a conversation takes no tenant from the request: the token's tenant is the
        // only one it can write.
        .endpoint(Endpoint::create(Method::POST, CONVERSATIONS_PATH, conversations).body(json!({})))
        .exempt_endpoint(
            "GET /api/assistant/v1/deleted-conversations",
            "lists only deleted conversations, which the suite's rows are not; \
             deleted_conversations_stay_in_their_tenant in this file probes it across tenants",
        )
        .exempt_endpoint(
            "POST /api/assistant/v1/conversations/{conversation_id}/restore",
            "restores only deleted conversations, which the suite's rows are not; \
             deleted_conversations_stay_in_their_tenant in this file probes it across tenants",
        )
        // The saved dashboards, every member's of the tenant.
        .endpoint(Endpoint::list(DASHBOARDS_PATH, saved))
        .endpoint(Endpoint::get(DASHBOARD_PATH, saved))
        .endpoint(Endpoint::get(VERSIONS_PATH, saved))
        .endpoint(
            Endpoint::item(Method::POST, DASHBOARD_RESTORE_PATH, saved)
                .body(json!({"from_version": 1, "base_version": 2})),
        )
        .endpoint(Endpoint::item(Method::PUT, ARCHIVE_PATH, saved))
        .endpoint(Endpoint::item(Method::DELETE, ARCHIVE_PATH, saved))
        .endpoint(
            Endpoint::item(Method::POST, DATASET_RUNS_PATH, saved)
                .body(json!({"version": 1, "dataset": "testkit"})),
        )
        // A draft, its conversation's owner's.
        .endpoint(Endpoint::item(Method::POST, PUBLISH_PATH, drafts).body(json!({"version": 2})))
        .exempt_endpoint(
            "GET /api/assistant/v1/conversations/{conversation_id}/dashboards",
            "a conversation's drafts: a list whose path parameter is the conversation's ID, which \
             the suite cannot address; tests/dashboards_api.rs tests that another tenant's admin \
             and another member of the same tenant get 404 for it",
        )
        .run(Some(&service.base_url))
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

/// `path` with its conversation ID.
fn conversation_path(template: &str, id: &str) -> String {
    template.replace("{conversation_id}", id)
}

/// A conversation the user starts, names and deletes through the API; returns its ID.
async fn deleted_conversation(
    service: &Service,
    issuer: &TestIssuer,
    user: &User,
    title: &str,
) -> String {
    let created = service
        .as_user(
            issuer,
            user,
            HttpMethod::POST,
            CONVERSATIONS_PATH,
            Some(&json!({})),
        )
        .await;
    let id = created.expect_status(201).json()["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let path = conversation_path(CONVERSATION_PATH, &id);
    service
        .as_user(
            issuer,
            user,
            HttpMethod::PATCH,
            &path,
            Some(&json!({"title": title})),
        )
        .await
        .expect_status(200);
    service
        .as_user(issuer, user, HttpMethod::DELETE, &path, None)
        .await
        .expect_status(204);
    id
}

/// The IDs and titles in the user's deleted list.
async fn deleted_list(service: &Service, issuer: &TestIssuer, user: &User) -> (String, Vec<Value>) {
    let reply = service
        .as_user(
            issuer,
            user,
            HttpMethod::GET,
            DELETED_CONVERSATIONS_PATH,
            None,
        )
        .await;
    reply.expect_status(200);
    let conversations = reply.json()["conversations"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|conversation| json!([conversation["id"], conversation["title"]]))
        .collect();
    (reply.body, conversations)
}

/// The same person in tenants A and B (an admin of each, the most privileged tenant user)
/// deletes a conversation in each. In tenant A they see only A's in the deleted list, restoring
/// B's answers exactly as restoring an ID that does not exist (and changes nothing), and a token
/// without a tenant gets neither; in tenant B the same requests see and restore B's.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_conversations_stay_in_their_tenant() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(&product, &issuer.jwks(), &[]).await;
    let person = UserId::generate();
    let in_tenant = |tenant| User {
        tenant,
        id: person,
        role: TenantRole::Admin,
    };
    let (a, b) = (
        in_tenant(TenantId::generate()),
        in_tenant(TenantId::generate()),
    );
    let (title_a, title_b) = (
        format!("canary-a-{}", Uuid::new_v4().simple()),
        format!("canary-b-{}", Uuid::new_v4().simple()),
    );
    let id_a = deleted_conversation(&service, &issuer, &a, &title_a).await;
    let id_b = deleted_conversation(&service, &issuer, &b, &title_b).await;
    let b_markers = [
        id_b.clone(),
        id_b.replace('-', ""),
        title_b.clone(),
        b.tenant.to_string(),
    ];
    let shows_b = |text: &str| {
        let text = text.to_lowercase();
        b_markers
            .iter()
            .filter(|marker| text.contains(marker.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };

    // Tenant A's deleted list: A's conversation, nothing of B's.
    let (body, listed) = deleted_list(&service, &issuer, &a).await;
    assert_eq!(listed, [json!([id_a, title_a])], "{body}");
    assert!(shows_b(&body).is_empty(), "{body}");

    // Restoring B's conversation in tenant A answers as a missing ID does, byte for byte.
    let restore = |id: &str| conversation_path(RESTORE_PATH, id);
    let missing = service
        .as_user(
            &issuer,
            &a,
            HttpMethod::POST,
            &restore(&Uuid::new_v4().to_string()),
            Some(&json!({})),
        )
        .await;
    missing.expect_error(404, "not_found", "error.not_found");
    let theirs = service
        .as_user(
            &issuer,
            &a,
            HttpMethod::POST,
            &restore(&id_b),
            Some(&json!({})),
        )
        .await;
    assert_eq!(
        (theirs.status, &theirs.body),
        (missing.status, &missing.body)
    );
    assert!(shows_b(&theirs.body).is_empty(), "{}", theirs.body);

    // Without a tenant: neither the list nor a restore.
    let no_tenant = issuer.token(&{
        let mut claims = TestIssuer::claims(ServiceName::Assistant, None);
        claims.sub = person;
        claims
    });
    for (method, path) in [
        (HttpMethod::GET, DELETED_CONVERSATIONS_PATH.to_owned()),
        (HttpMethod::POST, restore(&id_b)),
        (HttpMethod::POST, restore(&id_a)),
    ] {
        let reply = service
            .call(method.clone(), &path, Some(&no_tenant), None)
            .await;
        reply.expect_error(403, "permission_denied", "error.permission_denied");
        assert!(shows_b(&reply.body).is_empty(), "{method} {path}");
    }

    // Tenant B still has its conversation deleted, and restores it; so does tenant A its own.
    let (body, listed) = deleted_list(&service, &issuer, &b).await;
    assert_eq!(listed, [json!([id_b, title_b])], "{body}");
    for (user, id) in [(&b, &id_b), (&a, &id_a)] {
        let restored = service
            .as_user(
                &issuer,
                user,
                HttpMethod::POST,
                &restore(id),
                Some(&json!({})),
            )
            .await;
        restored.expect_status(200);
        assert_eq!(restored.json()["id"], id.as_str(), "{}", restored.body);
    }
    for user in [&a, &b] {
        let (body, listed) = deleted_list(&service, &issuer, user).await;
        assert!(listed.is_empty(), "{body}");
    }
    println!(
        "deleted list and restore: tenant A sees and restores only its own; tenant B's \
         conversation answers as a missing ID ({} {})",
        theirs.status, theirs.body
    );
}
