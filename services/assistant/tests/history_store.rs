//! Conversations in Postgres (Laysi/trustiot-v2#553): the product's schema, applied by the
//! migration Job itself (`testkit::product`), and the service role `assistant_app` through
//! service-kit's tenant transactions, as the service uses them.
//!
//! - A conversation the agent ran (thinking blocks with signatures, tool calls and results)
//!   reads back byte for byte, and the request of the next question from the loaded
//!   conversation starts with exactly the bytes the stored one would have sent.
//! - The service role cannot change or remove a stored message or a conversation's owner and
//!   preamble: the database refuses UPDATE, DELETE and TRUNCATE (Laysi/trustiot-v2#644 gives it
//!   UPDATE on the thread's own columns only; tests/conversation_threads.rs).
//! - Appending is idempotent and never overwrites; gaps, unknown conversations, other tenants
//!   and other users are refused; a corrupt row fails the load.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use std::sync::Arc;

use assistant::Agent;
use assistant::history::{Conversation, EntryKind, Principal};
use assistant::model::ModelClient;
use assistant::model::anthropic::AnthropicGateway;
use assistant::store::{self, Loaded, StoreError, Stored};
use assistant::tools::{NoDelegation, Tool, ToolRegistry};
use contract::identity::{TenantGrant, TenantRole};
use contract::{ServiceName, TenantId};
use service_kit::identity::UserIdentity;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, Row};
use support::{CountReadings, StandIn, agent, fixture, limits, ok, respond, settings, tool_step};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use uuid::Uuid;

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// Every way to change or remove stored history; the service role may use none of them.
const CHANGES: [&str; 8] = [
    "UPDATE assistant.messages SET body = body",
    "UPDATE assistant.messages SET kind = 'note'",
    "DELETE FROM assistant.messages",
    "TRUNCATE assistant.messages",
    "UPDATE assistant.conversations SET system_prompt = 'rewritten'",
    "UPDATE assistant.conversations SET user_id = user_id",
    "DELETE FROM assistant.conversations",
    "TRUNCATE assistant.conversations",
];

/// Whether the service role holds a privilege on the whole table. Since Laysi/trustiot-v2#644
/// INSERT is granted only on the columns the service may choose, so the database fills
/// `created_at`.
const PRIVILEGE: &str = "SELECT has_table_privilege('assistant_app', $1, $2)";

/// What the database holds for a conversation, as the superuser reads it.
const STORED_MESSAGES: &str = "SELECT seq, kind, format, body FROM assistant.messages
WHERE conversation_id = $1::uuid ORDER BY seq";
const STORED_PREAMBLE: &str = "SELECT tenant_id::text AS tenant_id, user_id::text AS user_id,
    prompt_version, system_prompt, tenant_context, tools, effort, cache_ttl
FROM assistant.conversations WHERE conversation_id = $1::uuid";

/// Corrupt rows the superuser writes behind the service's back: `$1` tenant, `$2` conversation.
const CORRUPT_JSON: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
VALUES ($1::uuid, $2::uuid, 1, 'answer', 'anthropic_messages', convert_to('{\"role\":}', 'UTF8'))";
const GAP: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
VALUES ($1::uuid, $2::uuid, 2, 'answer', 'anthropic_messages', convert_to('{}', 'UTF8'))";
const DROP_CORRUPT: &str =
    "DELETE FROM assistant.messages WHERE conversation_id = $1::uuid AND seq >= 1";

struct Db {
    product: Arc<ProductPostgres>,
    db: TenantDb,
    issuer: TestIssuer,
}

async fn db() -> Option<Db> {
    let product = product_postgres(&["assistant_app"]).await?;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
    )
    .expect("the service role's pool verifies the server");
    Some(Db {
        product,
        db,
        issuer: TestIssuer::new(),
    })
}

impl Db {
    /// A new user acting in `tenant`, as a handler of the assistant would receive it.
    fn user(&self, tenant: TenantId) -> UserIdentity {
        let claims = TestIssuer::claims(
            ServiceName::Assistant,
            Some(TenantGrant {
                id: tenant,
                role: TenantRole::Viewer,
            }),
        );
        self.issuer.user(ServiceName::Assistant, &claims)
    }

    async fn superuser(&self) -> PgConnection {
        self.product.superuser().await
    }

    async fn create(&self, user: &UserIdentity, conversation: &Conversation) -> Stored {
        let mut tx = self.db.begin(user).await.expect("a tenant transaction");
        let stored = store::create(&mut tx, conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        stored
    }

    async fn append(
        &self,
        user: &UserIdentity,
        conversation: &Conversation,
        stored: Stored,
    ) -> Result<Stored, StoreError> {
        let mut tx = self.db.begin(user).await.expect("a tenant transaction");
        let result = store::append(&mut tx, conversation, stored).await;
        if result.is_ok() {
            tx.commit().await.expect("commit");
        } else {
            tx.rollback().await.expect("roll back");
        }
        result
    }

    async fn load(
        &self,
        user: &UserIdentity,
        principal: Principal,
        id: Uuid,
    ) -> Result<Option<Conversation>, StoreError> {
        Ok(self
            .load_stored(user, principal, id)
            .await?
            .map(|loaded| loaded.conversation))
    }

    /// The conversation and where its stored part ends.
    async fn load_stored(
        &self,
        user: &UserIdentity,
        principal: Principal,
        id: Uuid,
    ) -> Result<Option<Loaded>, StoreError> {
        let mut tx = self.db.begin(user).await.expect("a tenant transaction");
        let loaded = store::load(&mut tx, principal, id).await;
        tx.commit().await.expect("commit");
        loaded
    }
}

fn principal(user: &UserIdentity) -> Principal {
    Principal {
        tenant_id: user.tenant_id().expect("the user acts in a tenant"),
        user_id: user.user_id(),
    }
}

/// Messages written without a model call: the gateway is never contacted.
fn model() -> AnthropicGateway {
    AnthropicGateway::new("http://127.0.0.1:9", "main", 1 << 20).expect("a valid gateway URL")
}

/// A conversation of `principal` with one question.
fn conversation_with_question(principal: Principal, question: &str) -> Conversation {
    let agent = agent_without_gateway();
    let mut conversation = agent.start_conversation(principal, "Site: plant A.");
    conversation.append(
        EntryKind::Question,
        model().user_text(question).expect("a question"),
    );
    conversation
}

/// The agent with the test tool and a gateway client it never calls: only its conversation
/// preamble is used.
fn agent_without_gateway() -> Agent {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountReadings)];
    let registry = ToolRegistry::new(tools, limits()).expect("a valid tool set");
    Agent::new(Arc::new(model()), registry, settings())
}

fn entries(conversation: &Conversation) -> Vec<(EntryKind, String)> {
    conversation
        .entries()
        .map(|(kind, message)| (kind, message.json().to_owned()))
        .collect()
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// The request body before its `messages`: tools, system blocks and model settings.
fn before_messages(body: &str) -> &str {
    body.split_once(r#""messages":["#)
        .map(|(head, _)| head)
        .expect("the body has messages")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_conversation_reads_back_byte_for_byte_and_continues() {
    let Some(db) = db().await else {
        return;
    };
    let user = db.user(TenantId::generate());
    let owner = principal(&user);
    // Each question takes two tool steps (thinking with a signature, then a tool call), then
    // the answer.
    let stand_in = StandIn::start(respond(|_request, index| {
        if index % 3 == 2 {
            ok(fixture("answer_end_turn.json"))
        } else {
            ok(tool_step(index))
        }
    }))
    .await;
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountReadings)];
    let agent = agent(&stand_in, tools, settings());
    let mut conversation = agent.start_conversation(owner, "Site: plant A; meters: kWh & <W>.");
    agent
        .ask(
            &mut conversation,
            "How many readings?",
            Arc::new(NoDelegation),
        )
        .await
        .expect("an answer");
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds,
        [
            EntryKind::Question,
            EntryKind::Step,
            EntryKind::ToolResults,
            EntryKind::Step,
            EntryKind::ToolResults,
            EntryKind::Answer
        ]
    );

    let stored = db.create(&user, &conversation).await;
    assert_eq!(stored.messages(), conversation.len());

    // The database holds the exact bytes of every message, thinking and signatures included,
    // and the preamble as the conversation fixed it.
    let mut admin = db.superuser().await;
    let rows = sqlx::query(STORED_MESSAGES)
        .bind(conversation.id().to_string())
        .fetch_all(&mut admin)
        .await
        .expect("read the stored messages");
    assert_eq!(rows.len(), conversation.len());
    for (index, ((kind, message), row)) in conversation.entries().zip(&rows).enumerate() {
        assert_eq!(
            row.get::<i32, _>("seq"),
            i32::try_from(index).expect("small")
        );
        assert_eq!(row.get::<String, _>("kind"), kind.as_str());
        assert_eq!(row.get::<String, _>("format"), "anthropic_messages");
        assert_eq!(
            row.get::<Vec<u8>, _>("body"),
            message.json().as_bytes(),
            "message {index} is stored byte for byte"
        );
    }
    // The first step as stored is the gateway's `content`, as received (its whitespace too),
    // thinking blocks and signatures included.
    let step = String::from_utf8(rows[1].get::<Vec<u8>, _>("body")).expect("UTF-8");
    let received = tool_step(0);
    let received_content = {
        #[derive(serde::Deserialize)]
        struct Content<'a> {
            #[serde(borrow)]
            content: &'a serde_json::value::RawValue,
        }
        let parsed: Content<'_> = serde_json::from_str(&received).expect("JSON");
        parsed.content.get().to_owned()
    };
    assert_eq!(
        step,
        format!(r#"{{"role":"assistant","content":{received_content}}}"#)
    );
    let stored_step: serde_json::Value = serde_json::from_str(&step).expect("JSON");
    let returned: serde_json::Value = serde_json::from_str(&received).expect("JSON");
    for block in [0, 1] {
        assert_eq!(stored_step["content"][block]["type"], "thinking");
        assert_eq!(
            stored_step["content"][block]["signature"], returned["content"][block]["signature"],
            "thinking block {block}'s signature, unchanged"
        );
    }
    let preamble = sqlx::query(STORED_PREAMBLE)
        .bind(conversation.id().to_string())
        .fetch_one(&mut admin)
        .await
        .expect("read the stored conversation");
    assert_eq!(
        preamble.get::<String, _>("tenant_id"),
        owner.tenant_id.to_string()
    );
    assert_eq!(
        preamble.get::<String, _>("user_id"),
        owner.user_id.to_string()
    );
    assert_eq!(
        preamble.get::<String, _>("prompt_version"),
        conversation.preamble().system_prompt().version
    );
    assert_eq!(
        preamble.get::<String, _>("tenant_context"),
        "<tenant_context>\nSite: plant A; meters: kWh &amp; &lt;W&gt;.\n</tenant_context>"
    );
    assert_eq!(preamble.get::<String, _>("effort"), "medium");
    assert_eq!(preamble.get::<String, _>("cache_ttl"), "5m");

    // Loaded back: the same ID, owner, preamble and every message's bytes.
    let loaded = db
        .load(&user, owner, conversation.id())
        .await
        .expect("load")
        .expect("the owner's conversation");
    assert_eq!(loaded.id(), conversation.id());
    assert_eq!(loaded.principal(), owner);
    assert_eq!(**loaded.preamble(), **conversation.preamble());
    assert_eq!(entries(&loaded), entries(&conversation));

    // The next question on the loaded conversation sends the stored conversation's bytes: the
    // same tools, system blocks and settings as the first request, and the whole stored history
    // as the start of `messages`.
    let mut loaded = loaded;
    agent
        .ask(&mut loaded, "And yesterday?", Arc::new(NoDelegation))
        .await
        .expect("an answer");
    let requests = stand_in.requests();
    assert_eq!(requests.len(), 6);
    let resumed = &requests[3];
    assert_eq!(
        before_messages(&resumed.body),
        before_messages(&requests[0].body),
        "the preamble's bytes are the ones the conversation started with"
    );
    let original: Vec<String> = conversation
        .messages()
        .iter()
        .map(|message| message.json().to_owned())
        .collect();
    let sent = resumed.raw_messages();
    assert_eq!(sent.get(..original.len()), Some(&original[..]));
    // And it continues the history the model last saw, byte for byte.
    let last_before = requests[2].raw_messages();
    assert_eq!(sent.get(..last_before.len()), Some(&last_before[..]));

    // The second question's messages append after the first's and read back the same way.
    let appended = db
        .append(&user, &loaded, stored)
        .await
        .expect("append the second question");
    assert_eq!(appended.messages(), loaded.len());
    assert_eq!(loaded.len(), conversation.len() + 6);
    let again = db
        .load(&user, owner, conversation.id())
        .await
        .expect("load")
        .expect("the owner's conversation");
    assert_eq!(entries(&again), entries(&loaded));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_service_role_cannot_change_or_remove_stored_history() {
    let Some(db) = db().await else {
        return;
    };
    let user = db.user(TenantId::generate());
    let conversation = conversation_with_question(principal(&user), "Kept as it is?");
    db.create(&user, &conversation).await;

    for table in ["assistant.messages", "assistant.conversations"] {
        let mut admin = db.superuser().await;
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
            let held: bool = sqlx::query_scalar(PRIVILEGE)
                .bind(table)
                .bind(privilege)
                .fetch_one(&mut admin)
                .await
                .expect("read the privilege");
            assert_eq!(
                held,
                privilege == "SELECT",
                "assistant_app's table-wide {privilege} on {table}"
            );
        }
    }

    for statement in CHANGES {
        // In a transaction of the conversation's tenant, where the rows are visible, as the
        // service would run it.
        let mut tx = db.db.begin(&user).await.expect("a tenant transaction");
        let error = sqlx::raw_sql(statement)
            .execute(&mut *tx)
            .await
            .expect_err(&format!("assistant_app may run `{statement}`"));
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "`{statement}` in a tenant transaction: {error}"
        );
        tx.rollback().await.expect("roll back");
        // And on a plain session of the role, without a tenant.
        let mut session = db.product.postgres.connect("assistant_app", DATABASE).await;
        let error = sqlx::raw_sql(statement)
            .execute(&mut session)
            .await
            .expect_err(&format!("assistant_app may run `{statement}`"));
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "`{statement}` without a tenant: {error}"
        );
        session.close().await.expect("close");
    }

    let loaded = db
        .load(&user, principal(&user), conversation.id())
        .await
        .expect("load")
        .expect("the conversation is still there");
    assert_eq!(entries(&loaded), entries(&conversation));
}

#[tokio::test(flavor = "multi_thread")]
async fn appending_is_idempotent_and_never_overwrites() {
    let Some(db) = db().await else {
        return;
    };
    let user = db.user(TenantId::generate());
    let owner = principal(&user);
    let conversation = conversation_with_question(owner, "First?");
    let stored = db.create(&user, &conversation).await;
    assert_eq!(stored.messages(), 1);

    // A retry of what is stored changes nothing.
    {
        let mut tx = db.db.begin(&user).await.expect("a tenant transaction");
        assert_eq!(
            store::create(&mut tx, &conversation)
                .await
                .expect("a retry"),
            stored
        );
        tx.commit().await.expect("commit");
    }
    assert_eq!(
        db.append(&user, &conversation, stored)
            .await
            .expect("a retry"),
        stored
    );

    // Two writers continue the same stored conversation differently: the first one wins, the
    // second is refused and overwrites nothing.
    let load = || db.load_stored(&user, owner, conversation.id());
    let Loaded {
        conversation: mut first,
        stored: read,
    } = load().await.expect("load").expect("stored");
    let mut second = load().await.expect("load").expect("stored").conversation;
    first.append(
        EntryKind::Question,
        model().user_text("From one writer").expect("text"),
    );
    second.append(
        EntryKind::Question,
        model().user_text("From another writer").expect("text"),
    );
    let after_first = db.append(&user, &first, read).await.expect("appended");
    assert_eq!(after_first.messages(), 2);
    // The same append again, as after a commit whose outcome was lost: nothing changes.
    assert_eq!(
        db.append(&user, &first, read).await.expect("a retry"),
        after_first
    );
    assert!(matches!(
        db.append(&user, &second, read).await,
        Err(StoreError::Conflict(_))
    ));
    // Nor does a writer whose copy is shorter than what is stored pass.
    assert!(matches!(
        db.append(&user, &conversation, stored).await,
        Err(StoreError::Conflict(_))
    ));
    let stored_now = load().await.expect("load").expect("stored");
    assert_eq!(entries(&stored_now.conversation), entries(&first));

    // No gaps: a position past the conversation's own end, or past the stored end.
    let Loaded {
        conversation: mut longer,
        stored: two,
    } = load().await.expect("load").expect("stored");
    for text in ["Three", "Four"] {
        longer.append(EntryKind::Question, model().user_text(text).expect("text"));
    }
    let four = {
        // Stored in a transaction that rolls back: a position the database never reached.
        let mut tx = db.db.begin(&user).await.expect("a tenant transaction");
        let four = store::append(&mut tx, &longer, two)
            .await
            .expect("appended");
        tx.rollback().await.expect("roll back");
        four
    };
    assert!(matches!(
        db.append(&user, &first, four).await,
        Err(StoreError::Gap { from: 4, stored: 2 })
    ));
    longer.append(
        EntryKind::Question,
        model().user_text("Five").expect("text"),
    );
    assert!(matches!(
        db.append(&user, &longer, four).await,
        Err(StoreError::Gap { from: 4, stored: 2 })
    ));
    assert_eq!(
        db.append(&user, &longer, two)
            .await
            .expect("appended")
            .messages(),
        5
    );

    // A conversation that was never created cannot get messages, even from a position of
    // another conversation.
    let unknown = conversation_with_question(owner, "Never created");
    assert!(matches!(
        db.append(&user, &unknown, stored).await,
        Err(StoreError::Conflict(_))
    ));
    let mut tx = db.db.begin(&user).await.expect("a tenant transaction");
    let unknown_stored = store::create(&mut tx, &unknown).await.expect("created");
    tx.rollback().await.expect("roll back");
    assert!(matches!(
        db.append(&user, &unknown, unknown_stored).await,
        Err(StoreError::NotStored)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn other_tenants_and_other_users_get_nothing() {
    let Some(db) = db().await else {
        return;
    };
    let tenant = TenantId::generate();
    let user = db.user(tenant);
    let owner = principal(&user);
    let conversation = conversation_with_question(owner, "Mine only?");
    let stored = db.create(&user, &conversation).await;

    // Another user of the same tenant: the conversation's tools run as its owner, so only the
    // owner loads it.
    let colleague = db.user(tenant);
    assert!(
        db.load(&colleague, principal(&colleague), conversation.id())
            .await
            .expect("load")
            .is_none()
    );

    // Another tenant: row-level security hides the conversation, even under the owner's user ID.
    let stranger = db.user(TenantId::generate());
    let as_owner_elsewhere = Principal {
        tenant_id: principal(&stranger).tenant_id,
        user_id: owner.user_id,
    };
    assert!(
        db.load(&stranger, as_owner_elsewhere, conversation.id())
            .await
            .expect("load")
            .is_none()
    );

    // A transaction of one tenant cannot store, append to or load another tenant's
    // conversation.
    let mut tx = db.db.begin(&stranger).await.expect("a tenant transaction");
    assert!(matches!(
        store::create(&mut tx, &conversation).await,
        Err(StoreError::WrongTenant)
    ));
    assert!(matches!(
        store::append(&mut tx, &conversation, stored).await,
        Err(StoreError::WrongTenant)
    ));
    assert!(matches!(
        store::load(&mut tx, owner, conversation.id()).await,
        Err(StoreError::WrongTenant)
    ));
    tx.rollback().await.expect("roll back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_history_fails_the_load() {
    let Some(db) = db().await else {
        return;
    };
    let user = db.user(TenantId::generate());
    let owner = principal(&user);
    let conversation = conversation_with_question(owner, "Intact?");
    db.create(&user, &conversation).await;
    let mut admin = db.superuser().await;
    let args = [owner.tenant_id.to_string(), conversation.id().to_string()];

    for (corruption, what) in [(CORRUPT_JSON, "invalid JSON"), (GAP, "a gap")] {
        sqlx::query(corruption)
            .bind(&args[0])
            .bind(&args[1])
            .execute(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("write {what}: {error}"));
        assert!(
            matches!(
                db.load(&user, owner, conversation.id()).await,
                Err(StoreError::Corrupt(_))
            ),
            "{what} fails the load"
        );
        sqlx::query(DROP_CORRUPT)
            .bind(&args[1])
            .execute(&mut admin)
            .await
            .expect("remove the corrupt row");
    }
    let loaded = db
        .load(&user, owner, conversation.id())
        .await
        .expect("load")
        .expect("stored");
    assert_eq!(entries(&loaded), entries(&conversation));
}
