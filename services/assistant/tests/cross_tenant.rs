//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#553 and
//! Laysi/trustiot-v2#644): the assistant's service role cannot read or write another tenant's
//! conversations, messages, discarded rounds or runs, and every relation the role can reach is
//! declared. The schema is the product's, applied by the migration Job (`testkit::product`). The
//! service has no HTTP API yet; the chat API (Laysi/trustiot-v2#645) declares its endpoints here
//! when it adds them.
//!
//! The service role may update only some columns of conversations and runs (none of
//! `tenant_id`) and nothing of messages and discarded rounds, so the suite's update and move
//! probes, which write `tenant_id`, pass by the privilege refusal; `tests/history_store.rs` and
//! `tests/conversation_threads.rs` test the refusals on their own. The platform role
//! (`assistant_platform`, the purge) sees across tenants by design and is not attacked here
//! (crates/testkit/README.md, 「限制」); its policies are tested in
//! `tests/conversation_threads.rs`.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

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

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["assistant_app"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("assistant_app", DATABASE),
    )
    .table(pg_table!(
        "assistant.conversations",
        seed = SEED_CONVERSATION
    ))
    .table(pg_table!("assistant.messages", seed = SEED_MESSAGE))
    .table(pg_table!(
        "assistant.discarded_rounds",
        seed = SEED_DISCARDED_ROUND
    ))
    .table(pg_table!("assistant.runs", seed = SEED_RUN))
    .exempt(
        "assistant.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check",
    );
    Suite::new(ServiceName::Assistant, &issuer, store)
        .without_api(
            "no HTTP API yet: the chat API is Laysi/trustiot-v2#645, which declares its \
             endpoints in this test",
        )
        .run(None)
        .await
        .assert_isolated();
}
