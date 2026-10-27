//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#553): the
//! assistant's service role cannot read or write another tenant's conversations or messages,
//! and every relation the role can reach is declared. The schema is the product's, applied by
//! the migration Job (`testkit::product`). The service has no HTTP API yet; the chat API
//! (Laysi/trustiot-v2#146) declares its endpoints here when it adds them.
//!
//! The service role has no UPDATE on either table (the history is append-only), so the suite's
//! update and move probes pass by the privilege refusal; `tests/history_store.rs` tests that
//! refusal on its own.
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
    .exempt(
        "assistant.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check",
    );
    Suite::new(ServiceName::Assistant, &issuer, store)
        .without_api(
            "no HTTP API yet: the chat API is Laysi/trustiot-v2#146, which declares its \
             endpoints in this test",
        )
        .run(None)
        .await
        .assert_isolated();
}
