//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#553,
//! Laysi/trustiot-v2#644, Laysi/trustiot-v2#645, Laysi/trustiot-v2#150 and
//! Laysi/trustiot-v2#650): the assistant's
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
//! suite cannot address; `a_conversations_drafts_stay_in_their_tenant` probes it by hand, with
//! the same user in both tenants. Likewise the "shared with me" list and a shared conversation's
//! read-only view (Laysi/trustiot-v2#650) serve only conversations another user of the tenant
//! shared, which the suite's one-user rows are not: `shared_conversations_stay_in_their_tenant`
//! probes them by hand. Sharing, unsharing and copying act on the caller's own conversation too,
//! so the suite probes them. The draft versions each answer lists (Laysi/trustiot-v2#1043) join
//! conversations, runs, dashboards and their versions; `answer_drafts_stay_in_their_tenant`
//! gives a conversation, its run and its draft the same IDs in two tenants, which only the tenant
//! keeps apart.
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
    ARCHIVE_PATH, CONVERSATION_DASHBOARDS_PATH, DASHBOARD_PATH, DASHBOARDS_PATH, DATASET_RUNS_PATH,
    PUBLISH_PATH, RESTORE_PATH as DASHBOARD_RESTORE_PATH, VERSIONS_PATH,
};
use assistant::api::sharing::{
    COPY_PATH, SHARE_PATH, SHARED_CONVERSATION_PATH, SHARED_CONVERSATIONS_PATH,
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
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
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

/// A draft with one version, as the `dashboard` tool writes one, in the conversation `$2` the
/// API started in tenant `$1`: `$3` is the draft's ID, `$4` its title. The conversation's
/// owner is the draft's creator.
const SEED_DRAFT_IN_CONVERSATION: &str = "WITH dashboard AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, conversation_id, created_by)
    SELECT tenant_id, $3::uuid, 'draft', conversation_id, user_id
    FROM assistant.conversations
    WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid
    RETURNING tenant_id, dashboard_id, created_by
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by, run_id)
SELECT tenant_id, dashboard_id, 1, 'agent', 1, $4,
    '{\"datasets\":[{\"name\":\"testkit\",\"title\":\"Testkit\",\"description\":\"The dataset the cross-tenant suite runs.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
    '<main><h1>testkit</h1></main>', created_by, gen_random_uuid()
FROM dashboard";

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
        .exempt_endpoint(
            "POST /api/assistant/v1/conversations/{conversation_id}/questions",
            "starts a question in the background, which calls the identity service and the AI \
             gateway the suite does not stand in for; questions_and_stops_stay_in_their_tenant \
             in tests/questions.rs probes it across tenants",
        )
        .exempt_endpoint(
            "POST /api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/cancel",
            "two path parameters, a conversation's and its run's, which the suite cannot \
             address; questions_and_stops_stay_in_their_tenant in tests/questions.rs probes it \
             across tenants",
        )
        // Sharing, unsharing and copying act on the caller's own conversation here (the suite's
        // rows are the one user's): in tenant B's row the same person gets the one 404.
        .endpoint(Endpoint::item(Method::PUT, SHARE_PATH, conversations))
        .endpoint(Endpoint::item(Method::DELETE, SHARE_PATH, conversations))
        .endpoint(Endpoint::item(Method::POST, COPY_PATH, conversations).body(json!({})))
        .exempt_endpoint(
            "GET /api/assistant/v1/shared-conversations",
            "lists only conversations that other users of the tenant shared, which the suite's \
             rows (one user's, not shared) are not; shared_conversations_stay_in_their_tenant in \
             this file probes it across tenants",
        )
        .exempt_endpoint(
            "GET /api/assistant/v1/shared-conversations/{conversation_id}",
            "serves only conversations that other users of the tenant shared, which the suite's \
             rows (one user's, not shared) are not; shared_conversations_stay_in_their_tenant in \
             this file probes it across tenants",
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
             the suite cannot address; a_conversations_drafts_stay_in_their_tenant in this file \
             probes it across tenants",
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

/// A conversation the user starts through the API, with a draft the `dashboard` tool wrote in
/// it (seeded as the superuser); returns the conversation's and the draft's IDs.
async fn conversation_with_draft(
    service: &Service,
    issuer: &TestIssuer,
    product: &ProductPostgres,
    user: &User,
    title: &str,
) -> (String, String) {
    let created = service
        .as_user(
            issuer,
            user,
            HttpMethod::POST,
            CONVERSATIONS_PATH,
            Some(&json!({})),
        )
        .await;
    let conversation = created.expect_status(201).json()["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let draft = Uuid::new_v4().to_string();
    let mut admin = product.superuser().await;
    let written = sqlx::query(SEED_DRAFT_IN_CONVERSATION)
        .bind(user.tenant.to_string())
        .bind(&conversation)
        .bind(&draft)
        .bind(title)
        .execute(&mut admin)
        .await
        .expect("seed the draft")
        .rows_affected();
    assert_eq!(written, 1, "the draft's version");
    (conversation, draft)
}

/// The IDs and titles in the drafts list of `conversation`, as `user` sees it.
async fn drafts_list(
    service: &Service,
    issuer: &TestIssuer,
    user: &User,
    conversation: &str,
) -> (String, Vec<Value>) {
    let reply = service
        .as_user(
            issuer,
            user,
            HttpMethod::GET,
            &conversation_path(CONVERSATION_DASHBOARDS_PATH, conversation),
            None,
        )
        .await;
    reply.expect_status(200);
    let dashboards = reply.json()["dashboards"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|dashboard| json!([dashboard["dashboard_id"], dashboard["title"]]))
        .collect();
    (reply.body, dashboards)
}

/// The same person in tenants A and B (an admin of each, the most privileged tenant user) has a
/// conversation with a draft in each. In tenant A, the drafts list of B's conversation, and B's
/// draft and its versions, answer exactly as a conversation or dashboard that does not exist and
/// show nothing of B's, and a token without a tenant gets none of them; in tenant B the same
/// requests see B's.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversations_drafts_stay_in_their_tenant() {
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
    let (conversation_a, draft_a) =
        conversation_with_draft(&service, &issuer, &product, &a, &title_a).await;
    let (conversation_b, draft_b) =
        conversation_with_draft(&service, &issuer, &product, &b, &title_b).await;
    let b_markers = [
        conversation_b.clone(),
        conversation_b.replace('-', ""),
        draft_b.clone(),
        draft_b.replace('-', ""),
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

    // Tenant A: the drafts of A's conversation.
    let (body, listed) = drafts_list(&service, &issuer, &a, &conversation_a).await;
    assert_eq!(listed, [json!([draft_a, title_a])], "{body}");
    assert!(shows_b(&body).is_empty(), "{body}");

    // B's conversation and B's draft in tenant A answer as missing ones do, byte for byte.
    let unknown = Uuid::new_v4().to_string();
    let dashboard_path = |template: &str, id: &str| template.replace("{dashboard_id}", id);
    let pairs = [
        (
            conversation_path(CONVERSATION_DASHBOARDS_PATH, &unknown),
            conversation_path(CONVERSATION_DASHBOARDS_PATH, &conversation_b),
        ),
        (
            dashboard_path(DASHBOARD_PATH, &unknown),
            dashboard_path(DASHBOARD_PATH, &draft_b),
        ),
        (
            dashboard_path(VERSIONS_PATH, &unknown),
            dashboard_path(VERSIONS_PATH, &draft_b),
        ),
    ];
    for (missing_path, theirs_path) in &pairs {
        let missing = service
            .as_user(&issuer, &a, HttpMethod::GET, missing_path, None)
            .await;
        missing.expect_error(404, "not_found", "error.not_found");
        let theirs = service
            .as_user(&issuer, &a, HttpMethod::GET, theirs_path, None)
            .await;
        assert_eq!(
            (theirs.status, &theirs.body),
            (missing.status, &missing.body),
            "{theirs_path}"
        );
        assert!(shows_b(&theirs.body).is_empty(), "{}", theirs.body);
    }

    // Without a tenant: none of them.
    let no_tenant = issuer.token(&{
        let mut claims = TestIssuer::claims(ServiceName::Assistant, None);
        claims.sub = person;
        claims
    });
    for (_, path) in &pairs {
        let reply = service
            .call(HttpMethod::GET, path, Some(&no_tenant), None)
            .await;
        reply.expect_error(403, "permission_denied", "error.permission_denied");
        assert!(shows_b(&reply.body).is_empty(), "{path}");
    }

    // Tenant B sees B's draft through each of the same routes: in its conversation's list, by its
    // ID, and in its versions list.
    let (body, listed) = drafts_list(&service, &issuer, &b, &conversation_b).await;
    assert_eq!(listed, [json!([draft_b, title_b])], "{body}");
    let opened = service
        .as_user(
            &issuer,
            &b,
            HttpMethod::GET,
            &dashboard_path(DASHBOARD_PATH, &draft_b),
            None,
        )
        .await;
    let view = opened.expect_status(200).json();
    assert_eq!(
        (
            &view["dashboard"]["dashboard_id"],
            &view["version"]["title"]
        ),
        (&json!(draft_b), &json!(title_b)),
        "{}",
        opened.body
    );
    let versions = service
        .as_user(
            &issuer,
            &b,
            HttpMethod::GET,
            &dashboard_path(VERSIONS_PATH, &draft_b),
            None,
        )
        .await;
    let listed: Vec<Value> = versions.expect_status(200).json()["versions"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|version| json!([version["version"], version["title"]]))
        .collect();
    assert_eq!(listed, [json!([1, title_b])], "{}", versions.body);
    println!(
        "a conversation's drafts: tenant A lists only its own; tenant B's conversation and draft \
         answer as missing IDs"
    );
}

/// A conversation the user starts, names and shares through the API; returns its ID.
async fn shared_conversation(
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
    service
        .as_user(
            issuer,
            user,
            HttpMethod::PATCH,
            &conversation_path(CONVERSATION_PATH, &id),
            Some(&json!({"title": title})),
        )
        .await
        .expect_status(200);
    service
        .as_user(
            issuer,
            user,
            HttpMethod::PUT,
            &conversation_path(SHARE_PATH, &id),
            None,
        )
        .await
        .expect_status(200);
    id
}

/// The IDs and titles in the user's "shared with me" list.
async fn shared_list(service: &Service, issuer: &TestIssuer, user: &User) -> (String, Vec<Value>) {
    let reply = service
        .as_user(
            issuer,
            user,
            HttpMethod::GET,
            SHARED_CONVERSATIONS_PATH,
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

/// The same person in tenants A and B (an admin of each, the most privileged tenant user), and
/// in each tenant another member who shares a conversation. In tenant A the person lists only
/// A's shared conversation; reading, copying or sharing B's answers exactly as for an ID that
/// does not exist and changes nothing; a token without a tenant gets none of it. In tenant B the
/// same requests list, read and copy B's.
#[tokio::test(flavor = "multi_thread")]
async fn shared_conversations_stay_in_their_tenant() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(&product, &issuer.jwks(), &[]).await;
    let person = UserId::generate();
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());
    let in_tenant = |tenant| User {
        tenant,
        id: person,
        role: TenantRole::Admin,
    };
    let (a, b) = (in_tenant(tenant_a), in_tenant(tenant_b));
    let (owner_a, owner_b) = (
        User::new(tenant_a, TenantRole::Viewer),
        User::new(tenant_b, TenantRole::Viewer),
    );
    let (title_a, title_b) = (
        format!("canary-a-{}", Uuid::new_v4().simple()),
        format!("canary-b-{}", Uuid::new_v4().simple()),
    );
    let id_a = shared_conversation(&service, &issuer, &owner_a, &title_a).await;
    let id_b = shared_conversation(&service, &issuer, &owner_b, &title_b).await;
    let b_markers = [
        id_b.clone(),
        id_b.replace('-', ""),
        title_b.clone(),
        tenant_b.to_string(),
        owner_b.id.to_string(),
    ];
    let shows_b = |text: &str| {
        let text = text.to_lowercase();
        b_markers
            .iter()
            .filter(|marker| text.contains(marker.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };
    let owners_view = |id: String| {
        let (service, issuer, owner_b) = (&service, &issuer, &owner_b);
        async move {
            let reply = service
                .as_user(
                    issuer,
                    owner_b,
                    HttpMethod::GET,
                    &conversation_path(CONVERSATION_PATH, &id),
                    None,
                )
                .await;
            reply.expect_status(200);
            reply.body
        }
    };
    let b_before = owners_view(id_b.clone()).await;

    // Tenant A's shared list: A's conversation, nothing of B's.
    let (body, listed) = shared_list(&service, &issuer, &a).await;
    assert_eq!(listed, [json!([id_a, title_a])], "{body}");
    assert!(shows_b(&body).is_empty(), "{body}");

    // B's conversation in tenant A answers as a missing ID does, byte for byte, for the reader's
    // endpoints and the owner's.
    let missing = Uuid::new_v4().to_string();
    for (method, template, body) in [
        (HttpMethod::GET, SHARED_CONVERSATION_PATH, None),
        (HttpMethod::POST, COPY_PATH, Some(json!({}))),
        (HttpMethod::PUT, SHARE_PATH, None),
        (HttpMethod::DELETE, SHARE_PATH, None),
    ] {
        let nobodys = service
            .as_user(
                &issuer,
                &a,
                method.clone(),
                &conversation_path(template, &missing),
                body.as_ref(),
            )
            .await;
        nobodys.expect_error(404, "not_found", "error.not_found");
        let theirs = service
            .as_user(
                &issuer,
                &a,
                method.clone(),
                &conversation_path(template, &id_b),
                body.as_ref(),
            )
            .await;
        assert_eq!(
            (theirs.status, &theirs.body),
            (nobodys.status, &nobodys.body),
            "{method} {template}"
        );
        assert!(shows_b(&theirs.body).is_empty(), "{}", theirs.body);
    }
    assert_eq!(owners_view(id_b.clone()).await, b_before, "nothing changed");
    let (body, listed) = shared_list(&service, &issuer, &a).await;
    assert_eq!(
        listed,
        [json!([id_a, title_a])],
        "no copy, no change: {body}"
    );

    // Without a tenant: neither the list nor a read nor a copy.
    let no_tenant = issuer.token(&{
        let mut claims = TestIssuer::claims(ServiceName::Assistant, None);
        claims.sub = person;
        claims
    });
    for (method, path, body) in [
        (HttpMethod::GET, SHARED_CONVERSATIONS_PATH.to_owned(), None),
        (
            HttpMethod::GET,
            conversation_path(SHARED_CONVERSATION_PATH, &id_b),
            None,
        ),
        (
            HttpMethod::POST,
            conversation_path(COPY_PATH, &id_b),
            Some(("application/json", b"{}".to_vec())),
        ),
    ] {
        let reply = service
            .call(method.clone(), &path, Some(&no_tenant), body)
            .await;
        reply.expect_error(403, "permission_denied", "error.permission_denied");
        assert!(shows_b(&reply.body).is_empty(), "{method} {path}");
    }

    // In tenant B the same person lists, reads and copies B's, and not A's.
    let (body, listed) = shared_list(&service, &issuer, &b).await;
    assert_eq!(listed, [json!([id_b, title_b])], "{body}");
    let read = service
        .as_user(
            &issuer,
            &b,
            HttpMethod::GET,
            &conversation_path(SHARED_CONVERSATION_PATH, &id_b),
            None,
        )
        .await;
    read.expect_status(200);
    assert_eq!(read.json()["title"], title_b.as_str());
    let copied = service
        .as_user(
            &issuer,
            &b,
            HttpMethod::POST,
            &conversation_path(COPY_PATH, &id_b),
            Some(&json!({})),
        )
        .await;
    copied.expect_status(201);
    assert_eq!(copied.json()["title"], title_b.as_str());
    service
        .as_user(
            &issuer,
            &b,
            HttpMethod::GET,
            &conversation_path(SHARED_CONVERSATION_PATH, &id_a),
            None,
        )
        .await
        .expect_error(404, "not_found", "error.not_found");
    println!(
        "shared list, read and copy: tenant A sees only its own; tenant B's shared conversation \
         answers as a missing ID ({missing} gets the same 404)"
    );
}

/// An answered question in a conversation of the user `$3` in tenant `$1`, as the question API
/// stores one (Laysi/trustiot-v2#1043): conversation `$2` (shared) with a question and its
/// answer, run `$4` answered with that answer (`answer_seq` 1), and draft `$5` with versions 1
/// to `$6` written by the run, titled with the canary `$7`.
const SEED_ANSWER_WITH_DRAFTS: &str = "WITH conversation AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl, shared_at)
    VALUES ($1::uuid, $2::uuid, $3::uuid, 'testkit', 'testkit', '', '[]', 'medium', '5m', now())
    RETURNING tenant_id, conversation_id, user_id
), messages AS (
    INSERT INTO assistant.messages (tenant_id, conversation_id, seq, kind, format, body)
    SELECT tenant_id, conversation_id, m.seq, m.kind, 'anthropic_messages', convert_to(m.body, 'UTF8')
    FROM conversation, (VALUES
        (0, 'question', '{\"role\":\"user\",\"content\":\"How much energy?\"}'),
        (1, 'answer', '{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"412 kWh.\"}]}'))
        AS m (seq, kind, body)
    RETURNING seq
), run AS (
    INSERT INTO assistant.runs
        (tenant_id, run_id, conversation_id, user_id, status, deadline_at, ended_at, answer_seq)
    SELECT tenant_id, $4::uuid, conversation_id, user_id, 'answered',
        now() + interval '10 minutes', now(), 1
    FROM conversation
    WHERE (SELECT count(*) FROM messages) = 2
    RETURNING tenant_id, run_id, conversation_id, user_id
), dashboard AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, conversation_id, created_by)
    SELECT tenant_id, $5::uuid, 'draft', conversation_id, user_id FROM run
    RETURNING tenant_id, dashboard_id, created_by
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by, run_id)
SELECT tenant_id, dashboard_id, n, 'agent', 1, $7 || ' ' || n,
    '{\"datasets\":[{\"name\":\"testkit\",\"title\":\"Testkit\",\"description\":\"The dataset the cross-tenant suite runs.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
    '<main><h1>testkit</h1></main>', created_by, $4::uuid
FROM dashboard, generate_series(1, $6::integer) AS n";

/// The draft versions an answer lists stay in their tenant (Laysi/trustiot-v2#1043): the same
/// person has, in tenants A and B, a shared conversation with the same ID, whose answered
/// question has the same run ID and wrote a draft with the same ID: one version in A, three in
/// B. Only the tenant keeps them apart, so a join on any of the IDs without it would list B's
/// versions in A. In tenant A the owner's read and another member's shared view list A's one
/// version and show nothing of B's; in tenant B they list B's three.
#[tokio::test(flavor = "multi_thread")]
async fn answer_drafts_stay_in_their_tenant() {
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
    let (conversation, run, draft) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (canary_a, canary_b) = (
        format!("canary-a-{}", Uuid::new_v4().simple()),
        format!("canary-b-{}", Uuid::new_v4().simple()),
    );
    let mut admin = product.superuser().await;
    for (user, versions, canary) in [(&a, 1, &canary_a), (&b, 3, &canary_b)] {
        let written = sqlx::query(SEED_ANSWER_WITH_DRAFTS)
            .bind(user.tenant.to_string())
            .bind(conversation.to_string())
            .bind(person.to_string())
            .bind(run.to_string())
            .bind(draft.to_string())
            .bind(versions)
            .bind(canary)
            .execute(&mut admin)
            .await
            .expect("seed the answered question")
            .rows_affected();
        assert_eq!(written, u64::try_from(versions).expect("a count"));
    }
    let path = conversation_path(CONVERSATION_PATH, &conversation.to_string());
    let shared = conversation_path(SHARED_CONVERSATION_PATH, &conversation.to_string());
    let listed = |versions: &[u32]| {
        json!(
            versions
                .iter()
                .map(|version| json!({"dashboard_id": draft, "version": version}))
                .collect::<Vec<_>>()
        )
    };
    for (user, expected, other_canary) in [
        (&a, listed(&[1]), &canary_b),
        (&b, listed(&[1, 2, 3]), &canary_a),
    ] {
        let reader = User {
            tenant: user.tenant,
            id: UserId::generate(),
            role: TenantRole::Viewer,
        };
        for (who, route) in [(user, &path), (&reader, &shared)] {
            let reply = service
                .as_user(&issuer, who, HttpMethod::GET, route, None)
                .await;
            reply.expect_status(200);
            let messages = reply.json()["messages"].clone();
            assert_eq!(messages[1]["draft_versions"], expected, "{}", reply.body);
            assert!(
                !reply.body.contains(other_canary.as_str()),
                "{}",
                reply.body
            );
            let other_tenant = if user.tenant == a.tenant {
                b.tenant
            } else {
                a.tenant
            };
            assert!(
                !reply.body.contains(&other_tenant.to_string()),
                "{}",
                reply.body
            );
        }
    }
    println!(
        "answer drafts: the same conversation, run and draft IDs in two tenants; each tenant's \
         answer lists only its own versions"
    );
}
