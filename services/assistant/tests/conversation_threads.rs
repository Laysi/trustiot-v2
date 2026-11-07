//! Conversation threads in Postgres (Laysi/trustiot-v2#644): soft deletion, restoring within 30
//! days and the purge after them, discarding the last round, the record of questions, titles and
//! the archive. The product's schema, applied by the migration Job itself (`testkit::product`),
//! with the service role `assistant_app` through service-kit's tenant transactions and the purge's
//! platform role `assistant_platform` through `PlatformDb`, as the service uses them.
//!
//! - A deleted conversation does not exist for anyone but its owner's deleted list; restoring it
//!   brings back every row byte for byte. After 30 days it cannot be restored, and the purge
//!   removes it with its messages, discarded rounds and runs, and nothing else.
//! - The purge role can remove only expired conversations and nothing of the history directly;
//!   the service role can remove nothing.
//! - A restore and a purge, and a delete and an append, that meet on one conversation give one
//!   of the two orders, never a mix.
//! - A discarded round leaves the history byte for byte as it was before it; only the last round
//!   can be discarded.
//! - One question runs at a time per conversation; a question past its deadline is interrupted
//!   by the next.
//! - Another user of the tenant finds none of it.
//! - A user starts at most `MAX_STARTS_PER_DAY` conversations per tenant in 24 hours; storing
//!   one that is stored already (a retry) is not a new start.
//! - The database sets every time itself and refuses an oversized message; a store error carries
//!   no database text.
//!
//! Concurrent cases are ordered by row locks, not by timing: the second operation starts while
//! the first holds the lock, the test waits until Postgres reports it blocked
//! (`pg_blocking_pids`), then lets the first commit.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. The expired conversations are all made by one test, so no other test's purge removes
//! them.

mod platform_tenant;
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use assistant::answer_guard::RemovedContent;
use assistant::history::{Conversation, EntryKind, Principal};
use assistant::store::{
    self, ListFilter, Loaded, RunOutcome, RunStatus, StoreError, Stored, TitleSource,
};
use assistant::tools::{NoDelegation, Tool};
use assistant::{Agent, CopiedRound};
use chrono::{DateTime, TimeDelta, Utc};
use contract::identity::{TenantGrant, TenantRole};
use contract::{ServiceName, TenantId};
use service_kit::identity::{ServiceIdentity, UserIdentity};
use service_kit::tenant::{
    PlatformActor, PlatformDb, PlatformTransaction, TenantDb, TenantTransaction,
};
use sqlx::postgres::{PgDatabaseError, PgPoolOptions};
use sqlx::{Connection, PgConnection, Row};
use support::{
    Capture, CountReadings, StandIn, WAIT, agent, fixture, ok, respond, settings, tool_step,
};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use uuid::Uuid;

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// A question's messages: the question, a tool step (thinking with a signature and a tool call),
/// its tool results, the answer.
const ROUND: usize = 4;

/// The text of the questions the tests start with [`store::start_run`].
const RUNNING_QUESTION: &str = "How much energy did floor 3 use today?";

/// `$1` run: its question's text, as the superuser reads it.
const RUN_QUESTION: &str = "SELECT question FROM assistant.runs WHERE run_id = $1::uuid";

/// Everything the database holds of one conversation, as the superuser reads it: the row and
/// every row of its messages, discarded rounds and runs, as JSON (bytes as hex).
const SNAPSHOT: &str = "SELECT
    (SELECT row_to_json(c)::text FROM assistant.conversations c
     WHERE c.conversation_id = $1::uuid) AS conversation,
    (SELECT coalesce(json_agg(m ORDER BY m.seq)::text, '[]') FROM assistant.messages m
     WHERE m.conversation_id = $1::uuid) AS messages,
    (SELECT coalesce(json_agg(d ORDER BY d.from_seq)::text, '[]')
     FROM assistant.discarded_rounds d WHERE d.conversation_id = $1::uuid) AS discards,
    (SELECT coalesce(json_agg(r ORDER BY r.started_at, r.run_id)::text, '[]')
     FROM assistant.runs r WHERE r.conversation_id = $1::uuid) AS runs";

const COUNT_MESSAGES: &str =
    "SELECT count(*) FROM assistant.messages WHERE conversation_id = $1::uuid";

/// The superuser moves a deletion back in time, past the clock trigger (which replica mode
/// skips): `$1` conversation, `$2` the new `deleted_at`.
const BACKDATE: &str = "UPDATE assistant.conversations SET deleted_at = $2
WHERE conversation_id = $1::uuid";

/// The superuser moves a running question's start and deadline two hours back (the trigger
/// allows it: the run has not ended and its end time does not change).
const AGE_RUN: &str = "UPDATE assistant.runs
SET started_at = started_at - interval '2 hours', deadline_at = deadline_at - interval '2 hours'
WHERE run_id = $1::uuid";

/// The superuser stores `$3` conversations of user `$2` in tenant `$1`, started `$4` minutes
/// ago: a day's starts without making each one.
const SEED_STARTS: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl, created_at, last_activity_at)
SELECT $1::uuid, gen_random_uuid(), $2::uuid, 'seed', '', '', '[]', 'medium', '5m',
    now() - make_interval(mins => $4), now() - make_interval(mins => $4)
FROM generate_series(1, $3)";

/// How many conversations user `$2` has stored in tenant `$1`, deleted ones included.
const STARTED: &str = "SELECT count(*) FROM assistant.conversations
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";

/// Whether a session waits for a lock that the session `$1` holds.
const BLOCKS_ANOTHER: &str = "SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid))) AS blocks";

struct Db {
    product: Arc<ProductPostgres>,
    db: TenantDb,
    platform: PlatformDb,
    issuer: TestIssuer,
    agent: Agent,
    _stand_in: StandIn,
}

async fn db() -> Option<Db> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
    )
    .expect("the service role's pool verifies the server");
    let platform = PlatformDb::connect_lazy(
        product.postgres.options("assistant_platform", DATABASE),
        PgPoolOptions::new().max_connections(2),
    )
    .expect("the platform role's pool verifies the server");
    // Every question takes one tool step (thinking with a signature, then a tool call), then
    // the answer: requests alternate, and the tests ask one question at a time.
    let stand_in = StandIn::start(respond(|_request, index| {
        if index % 2 == 0 {
            ok(tool_step(index))
        } else {
            ok(fixture("answer_end_turn.json"))
        }
    }))
    .await;
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountReadings)];
    let agent = agent(&stand_in, tools, settings());
    Some(Db {
        product,
        db,
        platform,
        issuer: TestIssuer::new(),
        agent,
        _stand_in: stand_in,
    })
}

/// What the database holds of one conversation (see [`SNAPSHOT`]).
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    conversation: Option<String>,
    messages: String,
    discards: String,
    runs: String,
}

impl Snapshot {
    fn is_gone(&self) -> bool {
        self.conversation.is_none()
            && self.messages == "[]"
            && self.discards == "[]"
            && self.runs == "[]"
    }
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

    async fn tx(&self, user: &UserIdentity) -> TenantTransaction {
        self.db.begin(user).await.expect("a tenant transaction")
    }

    async fn platform_tx(&self) -> PlatformTransaction {
        self.platform
            .begin(
                PlatformActor::service(ServiceIdentity::new(ServiceName::Assistant)),
                "test-purge",
            )
            .await
            .expect("a platform transaction")
    }

    async fn superuser(&self) -> PgConnection {
        self.product.superuser().await
    }

    async fn ask(&self, conversation: &mut Conversation, question: &str) {
        self.agent
            .ask(conversation, question, Arc::new(NoDelegation))
            .await
            .expect("an answer");
    }

    /// A conversation of `user` with one answered question, stored.
    async fn started(&self, user: &UserIdentity) -> (Conversation, Stored) {
        let mut conversation = self
            .agent
            .start_conversation(principal(user), "Site: plant A.");
        self.ask(&mut conversation, "How many readings?").await;
        let mut tx = self.tx(user).await;
        let stored = store::create(&mut tx, &conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        (conversation, stored)
    }

    /// A conversation of `user` with something in every table: a first round and its run, then
    /// a second round, discarded. Returns its ID and the run's.
    async fn with_every_kind_of_row(&self, user: &UserIdentity) -> (Uuid, Uuid) {
        let (mut conversation, stored) = self.started(user).await;
        let id = conversation.id();
        let run = self.start_run(user, id).await.expect("a run");
        self.finish_run(user, run, RunOutcome::Answered).await;
        self.ask(&mut conversation, "And yesterday?").await;
        self.append(user, &conversation, stored)
            .await
            .expect("the second round");
        let loaded = self.load(user, id).await.expect("stored");
        let mut tx = self.tx(user).await;
        store::discard_last_round(&mut tx, principal(user), id, ROUND, loaded.stored)
            .await
            .expect("discard the second round");
        tx.commit().await.expect("commit");
        (id, run)
    }

    async fn append(
        &self,
        user: &UserIdentity,
        conversation: &Conversation,
        stored: Stored,
    ) -> Result<Stored, StoreError> {
        let mut tx = self.tx(user).await;
        let result = store::append(&mut tx, conversation, stored).await;
        finish(tx, result.is_ok()).await;
        result
    }

    async fn load(&self, user: &UserIdentity, id: Uuid) -> Option<Loaded> {
        let mut tx = self.tx(user).await;
        let loaded = store::load(&mut tx, principal(user), id)
            .await
            .expect("load");
        tx.commit().await.expect("commit");
        loaded
    }

    async fn start_run(&self, user: &UserIdentity, id: Uuid) -> Result<Uuid, StoreError> {
        let mut tx = self.tx(user).await;
        let result = store::start_run(
            &mut tx,
            principal(user),
            id,
            Uuid::now_v7(),
            Duration::from_secs(600),
            RUNNING_QUESTION,
        )
        .await;
        finish(tx, result.is_ok()).await;
        result.map(|run| run.run_id)
    }

    async fn finish_run(&self, user: &UserIdentity, run: Uuid, outcome: RunOutcome<'_>) {
        let mut tx = self.tx(user).await;
        store::finish_run(&mut tx, principal(user), run, outcome)
            .await
            .expect("finish the run");
        tx.commit().await.expect("commit");
    }

    async fn delete(&self, user: &UserIdentity, id: Uuid) -> store::DeletedConversation {
        let mut tx = self.tx(user).await;
        let deleted = store::delete(&mut tx, principal(user), id)
            .await
            .expect("delete");
        tx.commit().await.expect("commit");
        deleted.conversation
    }

    async fn restore(&self, user: &UserIdentity, id: Uuid) -> Result<(), StoreError> {
        let mut tx = self.tx(user).await;
        let result = store::restore(&mut tx, principal(user), id).await;
        finish(tx, result.is_ok()).await;
        result
    }

    async fn deleted_ids(&self, user: &UserIdentity) -> Vec<Uuid> {
        let mut tx = self.tx(user).await;
        let page = store::list_deleted(&mut tx, principal(user), None, store::MAX_PAGE)
            .await
            .expect("the deleted list");
        tx.commit().await.expect("commit");
        page.items.iter().map(|item| item.id).collect()
    }

    async fn listed_ids(&self, user: &UserIdentity, filter: ListFilter) -> Vec<Uuid> {
        let mut tx = self.tx(user).await;
        let page = store::list(&mut tx, principal(user), filter, None, store::MAX_PAGE)
            .await
            .expect("the list");
        tx.commit().await.expect("commit");
        page.items.iter().map(|item| item.id).collect()
    }

    async fn snapshot(&self, id: Uuid) -> Snapshot {
        let mut admin = self.superuser().await;
        let row = sqlx::query(SNAPSHOT)
            .bind(id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("read the conversation's rows");
        Snapshot {
            conversation: row.get("conversation"),
            messages: row.get("messages"),
            discards: row.get("discards"),
            runs: row.get("runs"),
        }
    }

    async fn count_messages(&self, id: Uuid) -> usize {
        let mut admin = self.superuser().await;
        let count: i64 = sqlx::query_scalar(COUNT_MESSAGES)
            .bind(id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("count the messages");
        usize::try_from(count).expect("a count")
    }

    /// Sets `deleted_at` of `id` behind the store's back.
    async fn backdate(&self, id: Uuid, deleted_at: DateTime<Utc>) {
        let mut admin = self.superuser().await;
        let mut tx = admin.begin().await.expect("a superuser transaction");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .expect("skip the clock trigger");
        let updated = sqlx::query(BACKDATE)
            .bind(id.to_string())
            .bind(deleted_at)
            .execute(&mut *tx)
            .await
            .expect("move the deletion back")
            .rows_affected();
        assert_eq!(updated, 1, "the conversation to move back exists");
        tx.commit().await.expect("commit");
    }

    /// Waits until another session waits for a lock that the session `holder` holds.
    async fn wait_until_blocking(&self, holder: i32, what: &str) {
        let mut admin = self.superuser().await;
        let started = Instant::now();
        loop {
            let blocks: bool = sqlx::query_scalar(BLOCKS_ANOTHER)
                .bind(holder)
                .fetch_one(&mut admin)
                .await
                .expect("read the lock waits");
            if blocks {
                return;
            }
            assert!(
                started.elapsed() < WAIT,
                "{what} did not wait for the row lock within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Asserts that `user` finds no conversation `id` and no run `run` in any function of the
    /// store, and that trying changes nothing. `stored` is a valid position of the
    /// conversation, for the functions that take one.
    async fn assert_not_found(&self, user: &UserIdentity, id: Uuid, run: Uuid, stored: Stored) {
        let who = principal(user);
        let mut tx = self.tx(user).await;
        assert!(
            store::load(&mut tx, who, id).await.expect("load").is_none(),
            "load"
        );
        assert!(
            store::metadata(&mut tx, who, id)
                .await
                .expect("metadata")
                .is_none(),
            "metadata"
        );
        assert!(
            store::run_status(&mut tx, who, run)
                .await
                .expect("run status")
                .is_none(),
            "run status"
        );
        tx.commit().await.expect("commit");
        for filter in [ListFilter::Active, ListFilter::Archived] {
            assert!(
                !self.listed_ids(user, filter).await.contains(&id),
                "the list of {filter:?}"
            );
        }

        let refusals: [(&str, Result<(), StoreError>); 6] = [
            ("rename", {
                let mut tx = self.tx(user).await;
                let result = store::set_title(&mut tx, who, id, "Renamed", TitleSource::User)
                    .await
                    .map(|_| ());
                finish(tx, false).await;
                result
            }),
            ("archive", {
                let mut tx = self.tx(user).await;
                let result = store::archive(&mut tx, who, id, true).await;
                finish(tx, false).await;
                result
            }),
            ("delete", {
                let mut tx = self.tx(user).await;
                let result = store::delete(&mut tx, who, id).await.map(|_| ());
                finish(tx, false).await;
                result
            }),
            ("discard", {
                let mut tx = self.tx(user).await;
                let result = store::discard_last_round(&mut tx, who, id, 0, stored).await;
                finish(tx, false).await;
                result
            }),
            ("start a question", {
                let mut tx = self.tx(user).await;
                let result = store::start_run(
                    &mut tx,
                    who,
                    id,
                    Uuid::now_v7(),
                    Duration::from_secs(600),
                    RUNNING_QUESTION,
                )
                .await
                .map(|_| ());
                finish(tx, false).await;
                result
            }),
            ("stop a question", {
                let mut tx = self.tx(user).await;
                let result = store::request_cancel(&mut tx, who, id, run)
                    .await
                    .map(|_| ());
                finish(tx, false).await;
                result
            }),
        ];
        // A stop names the conversation too, and finds it first (its row lock comes first).
        for (what, result) in refusals {
            assert!(
                matches!(result, Err(StoreError::NotStored)),
                "{what}: {result:?}"
            );
        }
    }
}

/// Commits `tx` when `commit`, otherwise rolls it back.
async fn finish(tx: TenantTransaction, commit: bool) {
    if commit {
        tx.commit().await.expect("commit");
    } else {
        tx.rollback().await.expect("roll back");
    }
}

fn principal(user: &UserIdentity) -> Principal {
    Principal {
        tenant_id: user.tenant_id().expect("the user acts in a tenant"),
        user_id: user.user_id(),
    }
}

/// The history as (kind, exact bytes).
fn entries(conversation: &Conversation) -> Vec<(EntryKind, String)> {
    conversation
        .entries()
        .map(|(kind, message)| (kind, message.json().to_owned()))
        .collect()
}

async fn backend_pid(connection: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(connection)
        .await
        .expect("the session's process ID")
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// Acceptance criterion 1: a deleted conversation is gone for its owner and the rest of the
/// tenant, except in the owner's deleted list; restoring it brings back every row unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_conversation_is_gone_until_its_owner_restores_it() {
    let Some(db) = db().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = db.user(tenant);
    let colleague = db.user(tenant);
    let (id, run) = db.with_every_kind_of_row(&owner).await;
    let loaded = db.load(&owner, id).await.expect("stored");
    let history = entries(&loaded.conversation);
    assert_eq!(history.len(), ROUND, "the second round is discarded");
    let before = db.snapshot(id).await;
    assert!(
        before.discards != "[]" && before.runs != "[]",
        "the conversation has rows in every table: {before:?}"
    );

    let deleted = db.delete(&owner, id).await;
    assert_eq!(deleted.id, id);
    assert_eq!(
        deleted.restorable_until - deleted.deleted_at,
        TimeDelta::days(30),
        "restorable for 30 days"
    );

    // Nobody finds it, not even the owner, except in the owner's deleted list.
    for (who, user) in [("owner", &owner), ("colleague", &colleague)] {
        println!("{who}: the deleted conversation is not found");
        db.assert_not_found(user, id, run, loaded.stored).await;
    }
    let mut more = loaded.conversation;
    db.ask(&mut more, "Another question?").await;
    assert!(
        matches!(
            db.append(&owner, &more, loaded.stored).await,
            Err(StoreError::NotStored)
        ),
        "nothing is appended to a deleted conversation"
    );
    assert_eq!(db.deleted_ids(&owner).await, [id]);
    assert!(db.deleted_ids(&colleague).await.is_empty());
    assert!(matches!(
        db.restore(&colleague, id).await,
        Err(StoreError::NotStored)
    ));
    assert_eq!(db.snapshot(id).await.messages, before.messages);

    // Restored: every row as it was, and the history byte for byte.
    db.restore(&owner, id).await.expect("restore");
    assert_eq!(db.snapshot(id).await, before);
    let restored = db.load(&owner, id).await.expect("restored");
    assert_eq!(entries(&restored.conversation), history);
    assert!(db.deleted_ids(&owner).await.is_empty());
    assert!(
        db.listed_ids(&owner, ListFilter::Active)
            .await
            .contains(&id)
    );
    println!(
        "restored conversation: {} messages and every row as before the deletion",
        history.len()
    );
}

/// Acceptance criterion 7: another user of the tenant finds none of a user's conversation and
/// changes nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn another_user_of_the_tenant_finds_nothing() {
    let Some(db) = db().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = db.user(tenant);
    let colleague = db.user(tenant);
    let (id, _) = db.with_every_kind_of_row(&owner).await;
    let running = db.start_run(&owner, id).await.expect("a running question");
    let stored = db.load(&owner, id).await.expect("stored").stored;
    let before = db.snapshot(id).await;

    db.assert_not_found(&colleague, id, running, stored).await;
    assert!(matches!(
        db.restore(&colleague, id).await,
        Err(StoreError::NotStored)
    ));
    let mut tx = db.tx(&colleague).await;
    assert!(matches!(
        store::finish_run(
            &mut tx,
            principal(&colleague),
            running,
            RunOutcome::Cancelled
        )
        .await,
        Err(StoreError::NoSuchRun)
    ));
    tx.rollback().await.expect("roll back");
    assert_eq!(db.snapshot(id).await, before, "nothing changed");

    // Deleted, it stays out of the colleague's reach (the owner's question ends first, so
    // the delete changes no run).
    db.finish_run(&owner, running, RunOutcome::Cancelled).await;
    db.delete(&owner, id).await;
    let deleted = db.snapshot(id).await;
    assert!(db.deleted_ids(&colleague).await.is_empty());
    assert!(matches!(
        db.restore(&colleague, id).await,
        Err(StoreError::NotStored)
    ));
    assert_eq!(db.snapshot(id).await, deleted, "nothing changed");
}

/// The partial answer of a running question (Laysi/trustiot-v2#147, `0009_answer_partials.sql`)
/// goes with its conversation when the purge removes it: a replica that stopped without ending
/// the run left it behind, and the deletion only asked the run to stop.
#[tokio::test]
async fn a_purged_conversation_takes_its_partial_answers_with_it() {
    const PARTIALS: &str = "SELECT count(*) FROM assistant.answer_partials WHERE run_id = $1::uuid";
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let run = db.start_run(&owner, id).await.expect("a run");
    let mut tx = db.tx(&owner).await;
    let written = store::write_partial(&mut tx, principal(&owner), run, "Floor 3 used 412 kWh.")
        .await
        .expect("write the partial answer");
    tx.commit().await.expect("commit");
    assert!(written, "the run is running");
    db.delete(&owner, id).await;
    db.backdate(id, Utc::now() - TimeDelta::days(31)).await;
    let mut admin = db.superuser().await;
    let partials = sqlx::query_scalar::<_, i64>(PARTIALS).bind(run.to_string());
    let left: i64 = partials
        .fetch_one(&mut admin)
        .await
        .expect("count the partial answers");
    assert_eq!(left, 1, "left behind by the run");

    let mut tx = db.platform_tx().await;
    let purged = store::purge_expired(&mut tx, 100, &[])
        .await
        .expect("purge");
    tx.commit().await.expect("commit");
    assert!(purged >= 1);
    assert!(db.snapshot(id).await.is_gone());
    let partials = sqlx::query_scalar::<_, i64>(PARTIALS).bind(run.to_string());
    let left: i64 = partials
        .fetch_one(&mut admin)
        .await
        .expect("count the partial answers");
    assert_eq!(left, 0, "gone with its run and conversation");
    println!("the purge removed the partial answer with its conversation");
}

/// An answer stream's read takes the text only when it is at most the bytes it reserved memory
/// for (Laysi/trustiot-v2#147): a longer one is left out and its length told, so the stream
/// reserves for it and reads again instead of loading more than it reserved.
#[tokio::test]
async fn a_stream_read_takes_only_a_text_within_its_limit() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let run = db.start_run(&owner, id).await.expect("a run");
    let text = "Floor 3 used 412 kWh.";
    let bytes = u64::try_from(text.len()).expect("a length");
    assert!(write_partial(&db, &owner, run, text).await.expect("write"));
    for (within, read) in [
        (None, None),
        (Some(0), None),
        (Some(bytes - 1), None),
        (Some(bytes), Some(text)),
        (Some(u64::MAX), Some(text)),
    ] {
        let mut tx = db.tx(&owner).await;
        let streamed = store::stream_run(&mut tx, principal(&owner), id, run, within)
            .await
            .expect("read the run")
            .expect("the run");
        tx.commit().await.expect("commit");
        assert_eq!(streamed.partial_bytes, Some(bytes), "{within:?}");
        assert_eq!(streamed.partial.as_deref(), read, "{within:?}");
        assert_eq!(
            streamed.text_left_out(),
            read.is_none().then_some(bytes),
            "{within:?}"
        );
    }
    println!("a stream's read took the {bytes}-byte partial answer only within {bytes} bytes");
}

/// The partial answers stored for `run`, read as the superuser.
async fn partial_texts(admin: &mut PgConnection, run: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT text FROM assistant.answer_partials WHERE run_id = $1::uuid")
        .bind(run.to_string())
        .fetch_all(admin)
        .await
        .expect("read the partial answers")
}

/// Writes `text` as `user`'s partial answer of `run` in a transaction of its own.
async fn write_partial(
    db: &Db,
    user: &UserIdentity,
    run: Uuid,
    text: &str,
) -> Result<bool, StoreError> {
    let mut tx = db.tx(user).await;
    let written = store::write_partial(&mut tx, principal(user), run, text).await;
    tx.commit().await.expect("commit");
    written
}

/// A partial answer (Laysi/trustiot-v2#147) never outlives its run: it is written only while the
/// run is `running`, every end of the run deletes it in the same transaction, and a write and an
/// end run one after the other whichever holds the run's row first. A run a replica left running
/// past its deadline (it stopped mid-question) keeps its partial answer, which no stream reads,
/// until the conversation's next question marks the run interrupted and deletes it in the same
/// statement.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_answer_never_outlives_its_run() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let mut admin = db.superuser().await;

    // It only grows in place: one row per run, the latest text.
    let run = db.start_run(&owner, id).await.expect("a run");
    let first = "Floor 3 used 412 kWh.";
    assert!(write_partial(&db, &owner, run, first).await.expect("write"));
    let longer = "Floor 3 used 412 kWh.\n\nFloor 4 used 380 kWh.";
    assert!(
        write_partial(&db, &owner, run, longer)
            .await
            .expect("write")
    );
    assert_eq!(partial_texts(&mut admin, run).await, [longer]);
    let too_long = "x".repeat(store::MAX_PARTIAL_BYTES + 1);
    assert!(matches!(
        write_partial(&db, &owner, run, &too_long).await,
        Err(StoreError::Unstorable(_))
    ));
    assert_eq!(
        partial_texts(&mut admin, run).await,
        [longer],
        "a refused write changes nothing"
    );

    // The write holds the run's row first: the end waits for it, then deletes what it wrote.
    let mut writing = db.tx(&owner).await;
    assert!(
        store::write_partial(&mut writing, who, run, "Floor 3 used 412 kWh.\n\nFloor 4")
            .await
            .expect("write")
    );
    let holder = backend_pid(&mut writing).await;
    let mut ending = db.tx(&owner).await;
    let end = tokio::spawn(async move {
        let ended = store::finish_run(&mut ending, who, run, RunOutcome::Failed("model_failed"))
            .await
            .map(|_| ());
        (ended, ending)
    });
    db.wait_until_blocking(holder, "the run's end").await;
    writing.commit().await.expect("commit the write");
    let (ended, ending) = end.await.expect("the end's task");
    ended.expect("the run ends after the write");
    ending.commit().await.expect("commit the end");
    assert!(
        partial_texts(&mut admin, run).await.is_empty(),
        "the end deleted the write"
    );
    assert!(
        !write_partial(&db, &owner, run, "late")
            .await
            .expect("write"),
        "an ended run takes no write"
    );
    assert!(partial_texts(&mut admin, run).await.is_empty());
    println!("write first: the end waited for it and deleted it; a later write writes nothing");

    // The end holds the run's row first: the write waits, then finds the run ended.
    let run = db.start_run(&owner, id).await.expect("a second run");
    assert!(write_partial(&db, &owner, run, first).await.expect("write"));
    let mut ending = db.tx(&owner).await;
    store::finish_run(&mut ending, who, run, RunOutcome::Cancelled)
        .await
        .expect("end the run");
    let holder = backend_pid(&mut ending).await;
    let mut writing = db.tx(&owner).await;
    let late = tokio::spawn(async move {
        let written =
            store::write_partial(&mut writing, who, run, "Floor 3 used 412 kWh.\n\nAnd").await;
        (written, writing)
    });
    db.wait_until_blocking(holder, "the write").await;
    ending.commit().await.expect("commit the end");
    let (written, writing) = late.await.expect("the write's task");
    assert!(
        !written.expect("the write"),
        "the write finds the run ended"
    );
    writing.commit().await.expect("commit");
    assert!(
        partial_texts(&mut admin, run).await.is_empty(),
        "nothing outlived the run"
    );
    println!("end first: the write waited, found the run ended and wrote nothing");

    // The replica stopped mid-question: the run stays running past its deadline with its
    // partial answer, until the next question interrupts it and deletes the partial with it.
    let run = db.start_run(&owner, id).await.expect("a third run");
    assert!(write_partial(&db, &owner, run, first).await.expect("write"));
    sqlx::query(AGE_RUN)
        .bind(run.to_string())
        .execute(&mut admin)
        .await
        .expect("move the run back");
    assert_eq!(partial_texts(&mut admin, run).await, [first]);
    let mut tx = db.tx(&owner).await;
    let streamed = store::stream_run(&mut tx, who, id, run, Some(u64::MAX))
        .await
        .expect("read the run")
        .expect("the run");
    tx.commit().await.expect("commit");
    assert_eq!(streamed.run.status, RunStatus::Interrupted);
    assert_eq!(
        (streamed.partial_bytes, streamed.partial),
        (None, None),
        "past its deadline no stream reads it"
    );
    let next = db.start_run(&owner, id).await.expect("a fourth run");
    let mut tx = db.tx(&owner).await;
    let interrupted = store::run_status(&mut tx, who, run)
        .await
        .expect("run status")
        .expect("the third run");
    tx.commit().await.expect("commit");
    assert_eq!(interrupted.status, RunStatus::Interrupted);
    assert!(
        partial_texts(&mut admin, run).await.is_empty(),
        "interrupting the run deleted its partial answer"
    );
    assert!(partial_texts(&mut admin, next).await.is_empty());
    println!("a run left running past its deadline lost its partial answer when interrupted");
}

/// The question's text stored on `run`, read as the superuser.
async fn run_question(admin: &mut PgConnection, run: Uuid) -> Option<String> {
    sqlx::query_scalar(RUN_QUESTION)
        .bind(run.to_string())
        .fetch_one(admin)
        .await
        .expect("read the run's question")
}

/// `user`'s latest run of `id`, as reading the conversation gets it.
async fn latest(db: &Db, user: &UserIdentity, id: Uuid) -> store::RunRecord {
    let mut tx = db.tx(user).await;
    let run = store::latest_run(&mut tx, principal(user), id)
        .await
        .expect("the latest run")
        .expect("a run");
    tx.commit().await.expect("commit");
    run
}

/// A question's text while it runs (Laysi/trustiot-v2#1157, `0010_running_questions.sql`): the
/// run keeps it from its start, the owner's reads return it, and every end of the run clears it
/// in the statement that ends it: failed, cancelled, interrupted and answered through
/// `store::finish_run`, and the next question's marking of a run left running past its
/// deadline, which reads as interrupted without its text before that. An end the previous
/// release writes, which does not name the column, clears it too (the trigger), so that release
/// keeps working on this schema; the service role cannot write the text after the start, and the
/// CHECK keeps it to running runs and to the question API's bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_questions_text_is_kept_only_while_it_runs() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let mut admin = db.superuser().await;

    for outcome in [
        RunOutcome::Failed("model_failed"),
        RunOutcome::Cancelled,
        RunOutcome::Interrupted,
        RunOutcome::Answered,
    ] {
        let mut tx = db.tx(&owner).await;
        let started = store::start_run(
            &mut tx,
            who,
            id,
            Uuid::now_v7(),
            Duration::from_secs(600),
            RUNNING_QUESTION,
        )
        .await
        .expect("start a run");
        tx.commit().await.expect("commit");
        let run = started.run_id;
        assert_eq!(started.question.as_deref(), Some(RUNNING_QUESTION));
        assert_eq!(
            run_question(&mut admin, run).await.as_deref(),
            Some(RUNNING_QUESTION),
            "{outcome:?}: stored at the start"
        );
        let running = latest(&db, &owner, id).await;
        assert_eq!(running.status, RunStatus::Running);
        assert_eq!(running.question.as_deref(), Some(RUNNING_QUESTION));
        let mut tx = db.tx(&owner).await;
        let stopping = store::request_cancel(&mut tx, who, id, run)
            .await
            .expect("ask to stop");
        tx.commit().await.expect("commit");
        assert_eq!(
            stopping.question.as_deref(),
            Some(RUNNING_QUESTION),
            "a stop request leaves the run running, with its text"
        );

        let mut tx = db.tx(&owner).await;
        let ended = store::finish_run(&mut tx, who, run, outcome)
            .await
            .expect("end the run");
        tx.commit().await.expect("commit");
        assert_eq!(ended.question, None);
        assert_eq!(
            run_question(&mut admin, run).await,
            None,
            "{outcome:?}: cleared by the end"
        );
        let read = latest(&db, &owner, id).await;
        assert_eq!((read.run_id, read.question), (run, None), "{outcome:?}");
    }
    println!("failed, cancelled, interrupted and answered: each end cleared the text");

    // Left running past its deadline: it reads as interrupted without its text at once, and
    // the next question marks it so and clears the text in the same statement.
    let run = db.start_run(&owner, id).await.expect("a run");
    sqlx::query(AGE_RUN)
        .bind(run.to_string())
        .execute(&mut admin)
        .await
        .expect("move the run back");
    let read = latest(&db, &owner, id).await;
    assert_eq!(
        (read.run_id, read.status, read.question),
        (run, RunStatus::Interrupted, None)
    );
    assert_eq!(
        run_question(&mut admin, run).await.as_deref(),
        Some(RUNNING_QUESTION),
        "still stored until the next question"
    );
    let next = db.start_run(&owner, id).await.expect("the next question");
    assert_eq!(run_question(&mut admin, run).await, None, "cleared");
    assert_eq!(
        run_question(&mut admin, next).await.as_deref(),
        Some(RUNNING_QUESTION)
    );
    println!("past its deadline: read without its text, cleared when the next question started");

    // The previous release ends a run without naming the column: the trigger clears it.
    let mut tx = db.tx(&owner).await;
    sqlx::query(
        "UPDATE assistant.runs SET status = 'failed', result_code = 'model_failed',
            ended_at = now()
        WHERE tenant_id = $1::uuid AND run_id = $2::uuid AND status = 'running'",
    )
    .bind(who.tenant_id.to_string())
    .bind(next.to_string())
    .execute(&mut *tx)
    .await
    .expect("an end as the previous release writes it");
    tx.commit().await.expect("commit");
    assert_eq!(run_question(&mut admin, next).await, None);
    println!("an end that does not name the column cleared the text");

    // The service role names the column only when it inserts a run.
    let run = db.start_run(&owner, id).await.expect("a run");
    let mut tx = db.tx(&owner).await;
    let refused =
        sqlx::query("UPDATE assistant.runs SET question = 'changed' WHERE run_id = $1::uuid")
            .bind(run.to_string())
            .execute(&mut *tx)
            .await
            .expect_err("no UPDATE of the text");
    assert_eq!(
        sqlstate(&refused).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{refused}"
    );
    tx.rollback().await.expect("roll back");

    // The CHECK, for every role: 1 to 100000 characters, on a running run only.
    for (text, accepted) in [
        (String::new(), false),
        ("x".repeat(100_000), true),
        ("x".repeat(100_001), false),
    ] {
        let updated =
            sqlx::query("UPDATE assistant.runs SET question = $2 WHERE run_id = $1::uuid")
                .bind(run.to_string())
                .bind(&text)
                .execute(&mut admin)
                .await;
        match updated {
            Ok(_) => assert!(accepted, "{} characters accepted", text.len()),
            Err(error) => {
                assert!(!accepted, "{} characters: {error}", text.len());
                assert_eq!(sqlstate(&error).as_deref(), Some(CHECK_VIOLATION));
            }
        }
    }
    let ended = sqlx::query(
        "INSERT INTO assistant.runs
            (tenant_id, run_id, conversation_id, user_id, status, deadline_at, ended_at, question)
        VALUES ($1::uuid, gen_random_uuid(), $2::uuid, $3::uuid, 'failed',
            now() + interval '10 minutes', now(), 'text')",
    )
    .bind(who.tenant_id.to_string())
    .bind(id.to_string())
    .bind(who.user_id.to_string())
    .execute(&mut admin)
    .await
    .expect_err("no text on an ended run");
    assert_eq!(sqlstate(&ended).as_deref(), Some(CHECK_VIOLATION));
    println!("the service role cannot change the text; the CHECK holds its bound and its status");

    // A question over the bound starts without its text rather than not at all.
    let mut tx = db.tx(&owner).await;
    store::finish_run(&mut tx, who, run, RunOutcome::Cancelled)
        .await
        .expect("end the run");
    let long = "x".repeat(100_001);
    let started = store::start_run(
        &mut tx,
        who,
        id,
        Uuid::now_v7(),
        Duration::from_secs(600),
        &long,
    )
    .await
    .expect("start a run");
    tx.commit().await.expect("commit");
    assert_eq!(started.question, None);
    assert_eq!(run_question(&mut admin, started.run_id).await, None);
    println!("a text over the bound: the run started without it");

    // So does one with U+0000, which the question API accepts and a Postgres text cannot hold.
    let mut tx = db.tx(&owner).await;
    store::finish_run(&mut tx, who, started.run_id, RunOutcome::Cancelled)
        .await
        .expect("end the run");
    let started = store::start_run(
        &mut tx,
        who,
        id,
        Uuid::now_v7(),
        Duration::from_secs(600),
        "before\0after",
    )
    .await
    .expect("start a run");
    tx.commit().await.expect("commit");
    assert_eq!(started.question, None);
    assert_eq!(run_question(&mut admin, started.run_id).await, None);
    println!("a text with U+0000: the run started without it");
}

/// Acceptance criteria 2 and 3: after 30 days a deleted conversation cannot be restored and the
/// purge removes it with everything it holds, across tenants, and nothing else; the purge role
/// can remove nothing but expired conversations, the service role nothing at all; a restore and
/// a purge of the same conversation give exactly one result.
#[tokio::test(flavor = "multi_thread")]
async fn expired_conversations_are_purged_and_nothing_else() {
    let Some(db) = db().await else {
        return;
    };
    let owner_tenant = TenantId::generate();
    let owner = db.user(owner_tenant);
    let other_tenant = db.user(TenantId::generate());
    let (expired, _) = db.with_every_kind_of_row(&owner).await;
    let (expired_elsewhere, _) = db.with_every_kind_of_row(&other_tenant).await;
    let (recent, _) = db.with_every_kind_of_row(&owner).await;
    let (live, _) = db.with_every_kind_of_row(&owner).await;
    for id in [expired, expired_elsewhere, recent] {
        db.delete(
            if id == expired_elsewhere {
                &other_tenant
            } else {
                &owner
            },
            id,
        )
        .await;
    }
    let month_ago = Utc::now() - TimeDelta::days(31);
    db.backdate(expired, month_ago).await;
    db.backdate(expired_elsewhere, month_ago).await;
    let recent_before = db.snapshot(recent).await;
    let live_before = db.snapshot(live).await;

    // Past 30 days: not restorable, not in the deleted list, and the database refuses the
    // service role any change to it.
    assert!(matches!(
        db.restore(&owner, expired).await,
        Err(StoreError::NotStored)
    ));
    assert_eq!(db.deleted_ids(&owner).await, [recent]);
    let mut tx = db.tx(&owner).await;
    let undeleted = sqlx::query(
        "UPDATE assistant.conversations SET deleted_at = NULL WHERE conversation_id = $1::uuid",
    )
    .bind(expired.to_string())
    .execute(&mut *tx)
    .await
    .expect("the update runs")
    .rows_affected();
    assert_eq!(
        undeleted, 0,
        "the service role cannot bring an expired one back"
    );
    tx.rollback().await.expect("roll back");

    // The service role removes nothing of the history.
    for statement in [
        "DELETE FROM assistant.messages",
        "UPDATE assistant.messages SET body = body",
        "DELETE FROM assistant.conversations",
        "DELETE FROM assistant.runs",
        "DELETE FROM assistant.discarded_rounds",
    ] {
        let mut tx = db.tx(&owner).await;
        let error = sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .expect_err("refused");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "service role: {statement}"
        );
        tx.rollback().await.expect("roll back");
    }

    // The purge role sees only the expired ones, of every tenant, and cannot delete the others
    // or touch the history.
    let all = [expired, expired_elsewhere, recent, live].map(|id| id.to_string());
    let mut tx = db.platform_tx().await;
    let mut visible: Vec<String> = sqlx::query_scalar(
        "SELECT conversation_id::text FROM assistant.conversations
         WHERE conversation_id::text = ANY ($1)",
    )
    .bind(&all[..])
    .fetch_all(&mut *tx)
    .await
    .expect("the purge role reads the keys");
    visible.sort();
    let mut expected = vec![expired.to_string(), expired_elsewhere.to_string()];
    expected.sort();
    assert_eq!(visible, expected, "the purge role sees only expired ones");
    let removed =
        sqlx::query("DELETE FROM assistant.conversations WHERE conversation_id::text = ANY ($1)")
            .bind(&all[2..])
            .execute(&mut *tx)
            .await
            .expect("the delete runs")
            .rows_affected();
    assert_eq!(
        removed, 0,
        "the purge role cannot delete a live or recent one"
    );
    tx.rollback().await.expect("roll back");

    // Nor when it sets the owner's tenant itself: 0001's tenant policy applies to every role,
    // and only the restrictive platform_expired_only keeps it from opening the tenant's live
    // and restorable conversations to the platform role.
    let kept = [recent, live].map(|id| id.to_string());
    let mut tx = db.platform_tx().await;
    platform_tenant::set_tenant(&mut tx, owner_tenant).await;
    let seen: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assistant.conversations WHERE conversation_id::text = ANY ($1)",
    )
    .bind(&kept[..])
    .fetch_one(&mut *tx)
    .await
    .expect("the purge role counts");
    assert_eq!(
        seen, 0,
        "a tenant setting shows the purge role no live or recent one"
    );
    let removed =
        sqlx::query("DELETE FROM assistant.conversations WHERE conversation_id::text = ANY ($1)")
            .bind(&kept[..])
            .execute(&mut *tx)
            .await
            .expect("the delete runs")
            .rows_affected();
    assert_eq!(
        removed, 0,
        "a tenant setting lets the purge role delete no live or recent one"
    );
    let unfiltered = sqlx::query("DELETE FROM assistant.conversations")
        .execute(&mut *tx)
        .await
        .expect("the delete runs")
        .rows_affected();
    assert_eq!(
        unfiltered, 2,
        "only the two expired conversations, whatever the tenant setting"
    );
    tx.rollback().await.expect("roll back");
    println!(
        "purge role with the owner's tenant set: saw 0 and deleted 0 of the recent and the live \
         conversation; an unfiltered DELETE reached only the 2 expired ones (rolled back)"
    );

    for statement in [
        "DELETE FROM assistant.messages",
        "UPDATE assistant.messages SET body = body",
        "SELECT body FROM assistant.messages",
        "DELETE FROM assistant.runs",
        "UPDATE assistant.conversations SET deleted_at = NULL",
        "SELECT system_prompt FROM assistant.conversations",
        // Row locks need the UPDATE privilege, which the purge role does not have: the reason
        // the purge waits with a lock_timeout instead of skipping locked rows
        // (store::PURGE_LOCK_TIMEOUT).
        "SELECT conversation_id FROM assistant.conversations FOR UPDATE SKIP LOCKED",
        "SELECT conversation_id FROM assistant.conversations FOR NO KEY UPDATE SKIP LOCKED",
        "SELECT conversation_id FROM assistant.conversations FOR SHARE SKIP LOCKED",
    ] {
        let mut tx = db.platform_tx().await;
        let error = sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .expect_err("refused");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "purge role: {statement}"
        );
        tx.rollback().await.expect("roll back");
    }

    // The purge removes the two expired conversations with every row they hold, and nothing
    // else.
    let mut tx = db.platform_tx().await;
    let purged = store::purge_expired(&mut tx, 100, &[])
        .await
        .expect("purge");
    tx.commit().await.expect("commit");
    assert_eq!(purged, 2);
    for id in [expired, expired_elsewhere] {
        let gone = db.snapshot(id).await;
        assert!(gone.is_gone(), "purged with everything it held: {gone:?}");
    }
    assert_eq!(db.snapshot(recent).await, recent_before);
    assert_eq!(db.snapshot(live).await, live_before);
    let mut tx = db.platform_tx().await;
    assert_eq!(
        store::purge_expired(&mut tx, 100, &[])
            .await
            .expect("purge"),
        0
    );
    tx.commit().await.expect("commit");
    db.restore(&owner, recent).await.expect("still restorable");
    assert!(db.load(&owner, live).await.is_some());
    println!("purged 2 expired conversations; the recent and the live one are unchanged");

    // A restore and a purge meet on a conversation that is still restorable for the restore's
    // transaction and already expired for the purge's.
    for restore_first in [true, false] {
        let (id, _) = db.with_every_kind_of_row(&owner).await;
        db.delete(&owner, id).await;
        let mut restoring = db.tx(&owner).await;
        let restore_now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
            .fetch_one(&mut *restoring)
            .await
            .expect("the restore's time");
        let deleted_at = restore_now - TimeDelta::days(30) + TimeDelta::milliseconds(1);
        db.backdate(id, deleted_at).await;
        // The purge's transaction starts once its now() is past the 30 days.
        let mut admin = db.superuser().await;
        let started = Instant::now();
        loop {
            let past: bool = sqlx::query_scalar("SELECT clock_timestamp() > $1")
                .bind(deleted_at + TimeDelta::days(30))
                .fetch_one(&mut admin)
                .await
                .expect("read the clock");
            if past {
                break;
            }
            assert!(
                started.elapsed() < WAIT,
                "the clock did not pass the 30 days within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut purging = db.platform_tx().await;
        let seen: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM assistant.conversations WHERE conversation_id = $1::uuid",
        )
        .bind(id.to_string())
        .fetch_one(&mut *purging)
        .await
        .expect("the purge role reads the keys");
        assert_eq!(seen, 1, "expired for the purge");
        let restorable = store::list_deleted(&mut restoring, principal(&owner), None, 10)
            .await
            .expect("the deleted list");
        assert!(
            restorable.items.iter().any(|item| item.id == id),
            "restorable for the restore"
        );

        if restore_first {
            store::restore(&mut restoring, principal(&owner), id)
                .await
                .expect("restore");
            let holder = backend_pid(&mut restoring).await;
            let purge = tokio::spawn(async move {
                let purged = store::purge_expired(&mut purging, 100, &[]).await;
                (purged, purging)
            });
            db.wait_until_blocking(holder, "the purge").await;
            restoring.commit().await.expect("commit the restore");
            let (purged, purging) = purge.await.expect("the purge task");
            assert_eq!(
                purged.expect("purge"),
                0,
                "the purge skips the restored one"
            );
            purging.commit().await.expect("commit");
            let restored = db.snapshot(id).await;
            assert!(restored.conversation.is_some() && restored.messages != "[]");
            assert!(db.load(&owner, id).await.is_some(), "restored");
            println!("restore first: restored, the purge removed nothing");
        } else {
            assert_eq!(
                store::purge_expired(&mut purging, 100, &[])
                    .await
                    .expect("purge"),
                1
            );
            let holder = backend_pid(&mut purging).await;
            let who = principal(&owner);
            let restore = tokio::spawn(async move {
                let restored = store::restore(&mut restoring, who, id).await;
                (restored, restoring)
            });
            db.wait_until_blocking(holder, "the restore").await;
            purging.commit().await.expect("commit the purge");
            let (restored, restoring) = restore.await.expect("the restore task");
            assert!(
                matches!(restored, Err(StoreError::NotStored)),
                "the restore finds nothing once purged: {restored:?}"
            );
            restoring.rollback().await.expect("roll back");
            assert!(db.snapshot(id).await.is_gone(), "purged");
            println!("purge first: purged, the restore found nothing");
        }
    }

    // A session holds a lock on the oldest expired conversation for a whole round. A batch waits
    // PURGE_LOCK_TIMEOUT, then fails with 55P03 and deletes nothing of it; the round then
    // deletes the batch's conversations one at a time, skips the locked one, and still purges
    // the newer one. Once the lock is gone, the next round purges the skipped one.
    let (older, _) = db.with_every_kind_of_row(&owner).await;
    let (newer, _) = db.with_every_kind_of_row(&owner).await;
    for (id, days) in [(older, 32), (newer, 31)] {
        db.delete(&owner, id).await;
        db.backdate(id, Utc::now() - TimeDelta::days(days)).await;
    }
    let older_before = db.snapshot(older).await;
    let mut admin = db.superuser().await;
    let mut holding = admin.begin().await.expect("a superuser transaction");
    sqlx::query(
        "SELECT 1 FROM assistant.conversations WHERE conversation_id = $1::uuid FOR UPDATE",
    )
    .bind(older.to_string())
    .fetch_one(&mut *holding)
    .await
    .expect("lock the older expired conversation");
    let mut tx = db.platform_tx().await;
    let waited = Instant::now();
    let failure = store::purge_expired(&mut tx, 100, &[])
        .await
        .expect_err("the batch gives up on the locked row");
    let waited = waited.elapsed();
    tx.rollback().await.expect("roll back");
    assert_eq!(
        failure.sqlstate(),
        Some(store::LOCK_NOT_AVAILABLE),
        "{failure:?}"
    );
    assert!(
        waited >= store::PURGE_LOCK_TIMEOUT,
        "it waited the lock timeout it set: {waited:?}"
    );
    assert!(!format!("{failure} {failure:?}").contains(&older.to_string()));
    assert!(
        !db.snapshot(newer).await.is_gone(),
        "the failed batch deleted nothing"
    );
    // Before: what the purge finds next, oldest first; it reads keys and locks nothing.
    let mut tx = db.platform_tx().await;
    let next = store::expired_conversations(&mut tx, 100, &[])
        .await
        .expect("the expired keys");
    tx.rollback().await.expect("roll back");
    let key = |id: Uuid| store::ExpiredConversation {
        tenant_id: owner_tenant.into(),
        conversation_id: id,
    };
    assert_eq!(next, [key(older), key(newer)]);

    // The job's round: the newer one is purged, the older one skipped, one WARN line of its
    // own with the counts and nothing else.
    let (capture, guard) = Capture::install();
    let round = assistant::purge::round(&db.platform)
        .await
        .expect("a round that meets a locked row ends without an error");
    drop(guard);
    assert_eq!(
        round,
        assistant::purge::Round {
            purged: 1,
            skipped: 1
        }
    );
    assert!(
        db.snapshot(newer).await.is_gone(),
        "the newer one is purged"
    );
    assert_eq!(
        db.snapshot(older).await,
        older_before,
        "nothing of the locked one was deleted"
    );
    let events = capture.events();
    let own: Vec<_> = events
        .iter()
        .filter(|event| event.target == "assistant::purge")
        .collect();
    assert_eq!(own.len(), 1, "one line per round: {events:?}");
    assert_eq!(own[0].level, tracing::Level::WARN, "{own:?}");
    assert_eq!(
        (
            own[0].fields.get("purged").map(String::as_str),
            own[0].fields.get("skipped").map(String::as_str),
            own[0].fields.get("job").map(String::as_str),
        ),
        (Some("1"), Some("1"), Some("purge")),
        "{own:?}"
    );
    let logged = format!("{events:?}");
    for id in [older, newer] {
        assert!(!logged.contains(&id.to_string()), "no ID in the log");
    }
    assert!(
        !logged.contains(&owner_tenant.to_string()),
        "no tenant in the log"
    );
    // The round leaves the skipped one out of its later batches: the keys without it.
    let mut tx = db.platform_tx().await;
    assert!(
        store::expired_conversations(&mut tx, 100, &[key(older)])
            .await
            .expect("the expired keys")
            .is_empty()
    );
    tx.rollback().await.expect("roll back");

    // Released: the next round purges it, and says so at INFO.
    holding.rollback().await.expect("release the lock");
    let (capture, guard) = Capture::install();
    let round = assistant::purge::round(&db.platform)
        .await
        .expect("the next round");
    drop(guard);
    assert_eq!(
        round,
        assistant::purge::Round {
            purged: 1,
            skipped: 0
        }
    );
    assert!(
        db.snapshot(older).await.is_gone(),
        "purged once the lock is gone"
    );
    let events = capture.events();
    let own: Vec<_> = events
        .iter()
        .filter(|event| event.target == "assistant::purge")
        .collect();
    assert_eq!(own.len(), 1, "{events:?}");
    assert_eq!(own[0].level, tracing::Level::INFO, "{own:?}");
    assert_eq!(own[0].message, "purged expired conversations", "{own:?}");
    println!(
        "a locked expired conversation: the batch gave up after {waited:?} (55P03); the round \
         purged the newer one and skipped the locked one (WARN), the next round purged it"
    );
}

/// Acceptance criterion 4: a delete and an append of the same conversation run one after the
/// other: the messages are written and then the conversation is deleted, or the append fails
/// because it is deleted; nothing is appended after the delete.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_and_an_append_run_one_after_the_other() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());

    // The append holds the row first: its messages are written, then the conversation is
    // deleted, and no later append gets in.
    let (mut conversation, stored) = db.started(&owner).await;
    let id = conversation.id();
    db.ask(&mut conversation, "And yesterday?").await;
    let mut appending = db.tx(&owner).await;
    let appended = store::append(&mut appending, &conversation, stored)
        .await
        .expect("append");
    let holder = backend_pid(&mut appending).await;
    let mut deleting = db.tx(&owner).await;
    let who = principal(&owner);
    let delete = tokio::spawn(async move {
        let deleted = store::delete(&mut deleting, who, id).await;
        (deleted, deleting)
    });
    db.wait_until_blocking(holder, "the delete").await;
    appending.commit().await.expect("commit the append");
    let (deleted, deleting) = delete.await.expect("the delete task");
    deleted.expect("deleted after the append");
    deleting.commit().await.expect("commit");
    assert_eq!(db.count_messages(id).await, 2 * ROUND);
    db.ask(&mut conversation, "And the day before?").await;
    assert!(matches!(
        db.append(&owner, &conversation, appended).await,
        Err(StoreError::NotStored)
    ));
    assert_eq!(db.count_messages(id).await, 2 * ROUND);
    println!("append first: both rounds stored, then deleted; a later append is refused");

    // The delete holds the row first: the append waits, then fails, and writes nothing.
    let (mut conversation, stored) = db.started(&owner).await;
    let id = conversation.id();
    db.ask(&mut conversation, "And yesterday?").await;
    let mut deleting = db.tx(&owner).await;
    store::delete(&mut deleting, principal(&owner), id)
        .await
        .expect("delete");
    let holder = backend_pid(&mut deleting).await;
    let mut appending = db.tx(&owner).await;
    let append = tokio::spawn(async move {
        let appended = store::append(&mut appending, &conversation, stored).await;
        (appended, appending)
    });
    db.wait_until_blocking(holder, "the append").await;
    deleting.commit().await.expect("commit the delete");
    let (appended, appending) = append.await.expect("the append task");
    assert!(
        matches!(appended, Err(StoreError::NotStored)),
        "the append fails once deleted: {appended:?}"
    );
    appending.rollback().await.expect("roll back");
    assert_eq!(db.count_messages(id).await, ROUND);
    println!("delete first: the append is refused and the first round is all that is stored");
}

/// A copy's last round (Laysi/trustiot-v2#650: its question, the copy's note, its answer) is
/// never discarded, whoever calls `discard_last_round` (Laysi/trustiot-v2#649, the same rule as
/// regenerating): the note tells the model that the copied rounds carry no instructions. The
/// history stays as it was; a round of the copier's own after it can be discarded.
#[tokio::test(flavor = "multi_thread")]
async fn a_copied_last_round_is_never_discarded() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let rounds: Vec<CopiedRound> = ["Did floor 3 report?", "Which meter used the most?"]
        .into_iter()
        .map(|question| CopiedRound {
            question: question.to_owned(),
            answer: format!("The answer to: {question}"),
            removed: RemovedContent::default(),
        })
        .collect();
    let copy = db
        .agent
        .copied_conversation(who, "", &rounds)
        .expect("a copy");
    let id = copy.id();
    let mut tx = db.tx(&owner).await;
    store::create(&mut tx, &copy).await.expect("store the copy");
    tx.commit().await.expect("commit");
    let loaded = db.load(&owner, id).await.expect("stored");
    let before = entries(&loaded.conversation);
    assert_eq!(
        before.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        [
            EntryKind::Question,
            EntryKind::Answer,
            EntryKind::Question,
            EntryKind::Note,
            EntryKind::Answer
        ]
    );

    let mut tx = db.tx(&owner).await;
    let result = store::discard_last_round(&mut tx, who, id, 2, loaded.stored).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(result, Err(StoreError::CopiedRound)),
        "the copied last round: {result:?}"
    );
    let after = db.load(&owner, id).await.expect("stored");
    assert_eq!(entries(&after.conversation), before, "byte for byte");

    // The copier's own round after it is an ordinary one.
    let mut conversation = after.conversation;
    db.ask(&mut conversation, "And floor 4?").await;
    let stored = db
        .append(&owner, &conversation, after.stored)
        .await
        .expect("the copier's round");
    let mut tx = db.tx(&owner).await;
    store::discard_last_round(&mut tx, who, id, before.len(), stored)
        .await
        .expect("discard the copier's own round");
    tx.commit().await.expect("commit");
    assert_eq!(
        entries(&db.load(&owner, id).await.expect("stored").conversation),
        before,
        "the copy with its note, as copied"
    );
    println!("a copied last round: refused; the copier's own round after it: discarded");
}

/// Acceptance criterion 5: discarding the last round leaves the history byte for byte as it was
/// before the round; the next round follows it; only the last round can be discarded.
#[tokio::test(flavor = "multi_thread")]
async fn discarding_the_last_round_restores_the_history_before_it() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (mut conversation, stored) = db.started(&owner).await;
    let id = conversation.id();
    let before = entries(&db.load(&owner, id).await.expect("stored").conversation);
    assert_eq!(before.len(), ROUND);
    db.ask(&mut conversation, "And yesterday?").await;
    let two_rounds = db
        .append(&owner, &conversation, stored)
        .await
        .expect("the second round");
    let loaded = db.load(&owner, id).await.expect("stored");
    assert_eq!(loaded.conversation.len(), 2 * ROUND);

    // Not the last round's question: the first question, a step and an answer, past the end;
    // and a position read before the second round.
    for (from, from_stored, what) in [
        (0, loaded.stored, "the first round"),
        (ROUND - 1, loaded.stored, "an answer"),
        (ROUND + 1, loaded.stored, "a step of the last round"),
        (2 * ROUND, loaded.stored, "past the end"),
    ] {
        let mut tx = db.tx(&owner).await;
        let result = store::discard_last_round(&mut tx, who, id, from, from_stored).await;
        tx.rollback().await.expect("roll back");
        assert!(
            matches!(result, Err(StoreError::NotLastRound)),
            "{what}: {result:?}"
        );
    }
    let mut tx = db.tx(&owner).await;
    let result = store::discard_last_round(&mut tx, who, id, ROUND, stored).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(result, Err(StoreError::Conflict(_))),
        "a position read before the last round: {result:?}"
    );

    let mut tx = db.tx(&owner).await;
    store::discard_last_round(&mut tx, who, id, ROUND, loaded.stored)
        .await
        .expect("discard the last round");
    tx.commit().await.expect("commit");
    let after = db.load(&owner, id).await.expect("stored");
    assert_eq!(
        entries(&after.conversation),
        before,
        "the history before the discarded round, byte for byte"
    );
    assert_eq!(after.stored.messages(), ROUND);

    // The conversation in memory still has the discarded round: refused.
    db.ask(&mut conversation, "Unrelated?").await;
    assert!(matches!(
        db.append(&owner, &conversation, two_rounds).await,
        Err(StoreError::Conflict(_))
    ));

    // The next round follows the history before the discarded one.
    let mut again = after.conversation;
    db.ask(&mut again, "And last week?").await;
    let expected = entries(&again);
    db.append(&owner, &again, after.stored)
        .await
        .expect("the next round");
    let reloaded = db.load(&owner, id).await.expect("stored");
    assert_eq!(entries(&reloaded.conversation), expected);
    assert_eq!(reloaded.conversation.len(), 2 * ROUND);
    let snapshot = db.snapshot(id).await;
    assert_eq!(
        db.count_messages(id).await,
        3 * ROUND,
        "the discarded messages stay until the purge: {snapshot:?}"
    );
    println!(
        "discarded round 2 of 2; reloaded {} messages byte for byte; round 3 stored after it",
        before.len()
    );
}

/// Acceptance criterion 6: of two questions started at once in one conversation only one runs;
/// one that is still running past its deadline becomes interrupted when the next one starts.
#[tokio::test(flavor = "multi_thread")]
async fn one_question_runs_at_a_time() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();

    let mut starts = Vec::new();
    for _ in 0..2 {
        let mut tx = db.tx(&owner).await;
        starts.push(tokio::spawn(async move {
            let started = store::start_run(
                &mut tx,
                who,
                id,
                Uuid::now_v7(),
                Duration::from_secs(600),
                RUNNING_QUESTION,
            )
            .await;
            finish(tx, started.is_ok()).await;
            started
        }));
    }
    let mut running = Vec::new();
    let mut refused = 0;
    for start in starts {
        match start.await.expect("the start task") {
            Ok(run) => running.push(run),
            Err(StoreError::AlreadyRunning) => refused += 1,
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(
        (running.len(), refused),
        (1, 1),
        "exactly one question runs"
    );
    let first = running.pop().expect("one run");
    assert_eq!(first.status, RunStatus::Running);

    // The service stopped mid-question: the deadline passes with the run still running.
    let mut admin = db.superuser().await;
    sqlx::query(AGE_RUN)
        .bind(first.run_id.to_string())
        .execute(&mut admin)
        .await
        .expect("move the run back");
    let second = db.start_run(&owner, id).await.expect("the next question");
    let mut tx = db.tx(&owner).await;
    let interrupted = store::run_status(&mut tx, who, first.run_id)
        .await
        .expect("run status")
        .expect("the first run");
    assert_eq!(interrupted.status, RunStatus::Interrupted);
    assert!(interrupted.ended_at.is_some());
    assert!(matches!(
        store::finish_run(&mut tx, who, first.run_id, RunOutcome::Answered).await,
        Err(StoreError::RunEnded)
    ));
    let failed = store::finish_run(&mut tx, who, second, RunOutcome::Failed("model_timeout"))
        .await
        .expect("finish the second");
    tx.commit().await.expect("commit");
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(failed.result_code.as_deref(), Some("model_timeout"));
    println!("two starts: one ran, one refused; the expired run became interrupted");
}

/// Laysi/trustiot-v2#646: deleting a conversation with a question running deletes it and records
/// a stop request on the question in the same transaction (`Deletion::stopped_run`); the run
/// stays `running` until its task records `cancelled`. A question still running past its
/// deadline is not stopped, also when the deadline passes while the delete's transaction is
/// open: the delete leaves such a run as it is, and the next question after a restore marks it
/// interrupted.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_during_a_question_stops_it() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let run = db.start_run(&owner, id).await.expect("a question runs");
    let mut tx = db.tx(&owner).await;
    let deletion = store::delete(&mut tx, who, id).await.expect("delete");
    tx.commit().await.expect("commit");
    assert_eq!(deletion.conversation.id, id);
    assert_eq!(
        deletion.stopped_run,
        Some(run),
        "the running question is stopped"
    );
    let mut admin = db.superuser().await;
    let (status, stop_requested): (String, bool) = sqlx::query_as(
        "SELECT status, cancel_requested_at IS NOT NULL FROM assistant.runs
         WHERE run_id = $1::uuid",
    )
    .bind(run.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("read the run");
    assert_eq!(
        (status.as_str(), stop_requested),
        ("running", true),
        "a stop is requested; the question's task ends it"
    );
    db.finish_run(&owner, run, RunOutcome::Cancelled).await;
    db.restore(&owner, id).await.expect("restore");

    // The service stopped mid-question: past its deadline the run is not stopped.
    let run = db.start_run(&owner, id).await.expect("a question runs");
    sqlx::query(AGE_RUN)
        .bind(run.to_string())
        .execute(&mut admin)
        .await
        .expect("move the run back");
    let before = db.snapshot(id).await;
    let mut tx = db.tx(&owner).await;
    let deletion = store::delete(&mut tx, who, id).await.expect("delete");
    tx.commit().await.expect("commit");
    assert_eq!(deletion.conversation.id, id);
    assert_eq!(deletion.stopped_run, None);
    assert_eq!(
        db.snapshot(id).await.runs,
        before.runs,
        "the delete changes no run"
    );
    db.restore(&owner, id).await.expect("restore");
    db.start_run(&owner, id).await.expect("the next question");
    let status: String =
        sqlx::query_scalar("SELECT status FROM assistant.runs WHERE run_id = $1::uuid")
            .bind(run.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("read the run");
    assert_eq!(status, "interrupted", "the next question interrupted it");

    // The deadline passes while the delete's transaction is already open (as when it waits for
    // the row lock): the delete sees it passed, though the transaction's now() is before it.
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let run = db.start_run(&owner, id).await.expect("a question runs");
    let mut tx = db.tx(&owner).await;
    let opened: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await
        .expect("the transaction's time");
    let deadline: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE assistant.runs SET deadline_at = clock_timestamp() WHERE run_id = $1::uuid
         RETURNING deadline_at",
    )
    .bind(run.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("end the deadline now");
    assert!(
        deadline > opened,
        "the deadline passes after the transaction opened"
    );
    let deletion = store::delete(&mut tx, who, id).await.expect("delete");
    tx.commit().await.expect("commit");
    assert_eq!(deletion.conversation.id, id);
    assert_eq!(deletion.stopped_run, None, "past its deadline: not stopped");
    println!(
        "a delete stopped the running question; past its deadline it did not, also when the \
         deadline passed after the delete's transaction opened"
    );
}

/// Laysi/trustiot-v2#645, #646: a question start and a delete of one conversation that meet run
/// one after the other, in both orders. The delete locks the conversation's row before it looks
/// for a running question, so it stops a question committed while it waited.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_start_and_a_delete_run_one_after_the_other() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);

    // The start holds the row first: the question runs, and the delete waits, then stops it.
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let mut starting = db.tx(&owner).await;
    let run = store::start_run(
        &mut starting,
        who,
        id,
        Uuid::now_v7(),
        Duration::from_secs(600),
        RUNNING_QUESTION,
    )
    .await
    .expect("start the question");
    let holder = backend_pid(&mut starting).await;
    let mut deleting = db.tx(&owner).await;
    let delete = tokio::spawn(async move {
        let deleted = store::delete(&mut deleting, who, id).await;
        (deleted, deleting)
    });
    db.wait_until_blocking(holder, "the delete").await;
    starting.commit().await.expect("commit the start");
    let (deleted, deleting) = delete.await.expect("the delete task");
    let deletion = deleted.expect("deleted after the start");
    deleting.commit().await.expect("commit the delete");
    assert_eq!(
        deletion.stopped_run,
        Some(run.run_id),
        "the delete sees the question that started first, and stops it"
    );
    assert!(db.load(&owner, id).await.is_none(), "deleted");
    println!("start first: the question started, then the delete stopped it");

    // The delete holds the row first: the start waits, then finds no conversation.
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id();
    let mut deleting = db.tx(&owner).await;
    store::delete(&mut deleting, who, id).await.expect("delete");
    let holder = backend_pid(&mut deleting).await;
    let mut starting = db.tx(&owner).await;
    let start = tokio::spawn(async move {
        let started = store::start_run(
            &mut starting,
            who,
            id,
            Uuid::now_v7(),
            Duration::from_secs(600),
            RUNNING_QUESTION,
        )
        .await;
        (started, starting)
    });
    db.wait_until_blocking(holder, "the question start").await;
    deleting.commit().await.expect("commit the delete");
    let (started, starting) = start.await.expect("the start task");
    assert!(
        matches!(started, Err(StoreError::NotStored)),
        "the start finds the conversation deleted: {started:?}"
    );
    starting.rollback().await.expect("roll back");
    let runs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM assistant.runs WHERE conversation_id = $1::uuid")
            .bind(id.to_string())
            .fetch_one(&mut db.superuser().await)
            .await
            .expect("count the runs");
    assert_eq!(runs, 0, "no question started");
    assert_eq!(db.deleted_ids(&owner).await.first(), Some(&id));
    println!("delete first: deleted, the question start is refused");
}

/// Acceptance criterion 8: the database sets every time itself and refuses a message over
/// 32 MiB, whatever the service role sends.
#[tokio::test(flavor = "multi_thread")]
async fn the_database_keeps_its_own_times_and_the_size_limit() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let (conversation, _) = db.started(&owner).await;
    let tenant = principal(&owner).tenant_id.to_string();
    let user = principal(&owner).user_id.to_string();
    let id = conversation.id().to_string();
    // Each statement with its parameters.
    let refused: [(&str, Vec<&String>, &str); 6] = [
        (
            "INSERT INTO assistant.conversations
                (tenant_id, conversation_id, user_id, prompt_version, system_prompt,
                 tenant_context, tools, effort, cache_ttl, created_at)
            VALUES ($1::uuid, gen_random_uuid(), $2::uuid, 'test', 'prompt', '', '[]', 'medium',
                    '5m', now() - interval '1 day')",
            vec![&tenant, &user],
            INSUFFICIENT_PRIVILEGE,
        ),
        (
            "INSERT INTO assistant.messages
                (tenant_id, conversation_id, seq, kind, format, body, created_at)
            VALUES ($1::uuid, $2::uuid, 4, 'question', 'anthropic_messages',
                    convert_to('{}', 'UTF8'), now() - interval '1 day')",
            vec![&tenant, &id],
            INSUFFICIENT_PRIVILEGE,
        ),
        (
            "INSERT INTO assistant.discarded_rounds
                (tenant_id, conversation_id, from_seq, to_seq, created_at)
            VALUES ($1::uuid, $2::uuid, 0, 3, now() - interval '1 day')",
            vec![&tenant, &id],
            INSUFFICIENT_PRIVILEGE,
        ),
        (
            "INSERT INTO assistant.runs
                (tenant_id, run_id, conversation_id, user_id, started_at, deadline_at)
            VALUES ($1::uuid, gen_random_uuid(), $2::uuid, $3::uuid, now() - interval '1 day',
                    now() - interval '23 hours')",
            vec![&tenant, &id, &user],
            INSUFFICIENT_PRIVILEGE,
        ),
        (
            "UPDATE assistant.conversations SET deleted_at = now() - interval '31 days'
            WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid",
            vec![&tenant, &id],
            INSUFFICIENT_PRIVILEGE,
        ),
        (
            "UPDATE assistant.conversations SET last_activity_at = now() - interval '1 day'
            WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid",
            vec![&tenant, &id],
            INSUFFICIENT_PRIVILEGE,
        ),
    ];
    for (statement, parameters, expected) in refused {
        let mut tx = db.tx(&owner).await;
        let mut query = sqlx::query(statement);
        for parameter in parameters {
            query = query.bind(parameter);
        }
        let error = query.execute(&mut *tx).await.expect_err("refused");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(expected),
            "{statement}: {error}"
        );
        tx.rollback().await.expect("roll back");
    }
    // A JSON object of `$3` bytes in all: `{"x":"` and `"}` around the padding. 32 MiB is
    // stored (and rolled back), one byte more is refused by `messages_body_size`.
    for (size, refused) in [
        (store::MAX_MESSAGE_BYTES, false),
        (store::MAX_MESSAGE_BYTES + 1, true),
    ] {
        let mut tx = db.tx(&owner).await;
        let result = sqlx::query(
            "INSERT INTO assistant.messages (tenant_id, conversation_id, seq, kind, format, body)
            VALUES ($1::uuid, $2::uuid, 4, 'question', 'anthropic_messages',
                    convert_to('{\"x\":\"' || repeat('x', $3 - 8) || '\"}', 'UTF8'))",
        )
        .bind(&tenant)
        .bind(&id)
        .bind(i32::try_from(size).expect("a small size"))
        .execute(&mut *tx)
        .await;
        tx.rollback().await.expect("roll back");
        match result {
            Ok(_) => assert!(!refused, "a message of {size} bytes is refused"),
            Err(error) => {
                assert!(refused, "a message of {size} bytes is stored: {error}");
                assert_eq!(sqlstate(&error).as_deref(), Some(CHECK_VIOLATION));
                assert_eq!(
                    error
                        .as_database_error()
                        .and_then(|database| database.constraint()),
                    Some("messages_body_size"),
                    "{error}"
                );
            }
        }
    }

    // Once deleted, the deletion keeps its time: moving it to a later now() would postpone the
    // purge for ever.
    db.delete(&owner, conversation.id()).await;
    let mut tx = db.tx(&owner).await;
    let error = sqlx::query(
        "UPDATE assistant.conversations SET deleted_at = now()
        WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid",
    )
    .bind(&tenant)
    .bind(&id)
    .execute(&mut *tx)
    .await
    .expect_err("refused");
    tx.rollback().await.expect("roll back");
    assert_eq!(sqlstate(&error).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    println!(
        "created_at, started_at, back-dated and renewed times refused (42501); 32 MiB stored, \
         32 MiB + 1 refused"
    );
}

/// Acceptance criterion 9: a store error from a database refusal names its class, never the
/// values Postgres puts into its message and DETAIL.
#[tokio::test(flavor = "multi_thread")]
async fn store_errors_carry_no_database_text() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (conversation, _) = db.started(&owner).await;
    let id = conversation.id().to_string();

    // A unique-key violation with its DETAIL: the key, tenant and conversation ID. Postgres
    // leaves the key out of the DETAIL for a role that row-level security applies to, as the
    // service role, so the superuser makes the error here; the conversion must not depend on
    // that.
    let mut admin = db.superuser().await;
    let error = sqlx::query(
        "INSERT INTO assistant.conversations
            (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
             tools, effort, cache_ttl)
        VALUES ($1::uuid, $2::uuid, $3::uuid, 'test', 'prompt', '', '[]', 'medium', '5m')",
    )
    .bind(who.tenant_id.to_string())
    .bind(&id)
    .bind(who.user_id.to_string())
    .execute(&mut admin)
    .await
    .expect_err("a duplicate key");
    let detail = error
        .as_database_error()
        .and_then(|database| database.try_downcast_ref::<PgDatabaseError>())
        .and_then(PgDatabaseError::detail)
        .expect("Postgres sends a DETAIL")
        .to_owned();
    assert!(detail.contains(&id), "the DETAIL names the key: {detail}");
    let error = StoreError::from(error);
    let shown = format!("{error} | {error:?}");
    for leaked in [
        id.as_str(),
        &who.tenant_id.to_string(),
        "conversations_pkey",
        "duplicate",
        "already exists",
    ] {
        assert!(!shown.contains(leaked), "{leaked} in {shown}");
    }
    assert_eq!(error.sqlstate(), Some("23505"));
    assert_eq!(format!("{error}"), "database error (SQLSTATE 23505)");
    println!("a unique violation shows as: {shown}");

    // The same through the store: a run ID used twice.
    let run = db
        .start_run(&owner, conversation.id())
        .await
        .expect("a run");
    db.finish_run(&owner, run, RunOutcome::Answered).await;
    let mut tx = db.tx(&owner).await;
    let again = store::start_run(
        &mut tx,
        who,
        conversation.id(),
        run,
        Duration::from_secs(600),
        RUNNING_QUESTION,
    )
    .await
    .expect_err("the run ID is taken");
    tx.rollback().await.expect("roll back");
    let shown = format!("{again} | {again:?}");
    assert!(matches!(again, StoreError::Conflict(_)), "{shown}");
    assert!(!shown.contains(&run.to_string()), "{shown}");
}

/// At `MAX_STARTS_PER_DAY` starts in 24 hours the store refuses a new conversation with
/// `StartLimited` (when the oldest start leaves the day) and stores nothing; storing a
/// conversation that is stored already, a retry, is not a new start and still succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn starts_stop_at_the_daily_limit_but_a_retry_is_not_a_start() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let started = || async {
        let mut admin = db.superuser().await;
        sqlx::query_scalar::<_, i64>(STARTED)
            .bind(who.tenant_id.to_string())
            .bind(who.user_id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("count the user's conversations")
    };
    // The day's starts but one, all an hour ago.
    let mut admin = db.superuser().await;
    sqlx::query(SEED_STARTS)
        .bind(who.tenant_id.to_string())
        .bind(who.user_id.to_string())
        .bind(i32::try_from(store::MAX_STARTS_PER_DAY - 1).expect("a small limit"))
        .bind(60_i32)
        .execute(&mut admin)
        .await
        .expect("store the day's starts");

    let last = db.agent.start_conversation(who, "");
    for attempt in ["the last start of the day", "its retry"] {
        let mut tx = db.tx(&owner).await;
        store::create(&mut tx, &last)
            .await
            .unwrap_or_else(|error| panic!("{attempt}: {error}"));
        tx.commit().await.expect("commit");
        assert_eq!(started().await, store::MAX_STARTS_PER_DAY, "{attempt}");
    }

    let next = db.agent.start_conversation(who, "");
    let mut tx = db.tx(&owner).await;
    let refused = store::create(&mut tx, &next)
        .await
        .expect_err("over the limit");
    tx.rollback().await.expect("roll back");
    let StoreError::StartLimited { retry_after_secs } = refused else {
        panic!("not the start limit: {refused:?}");
    };
    // The oldest start was an hour ago, so it leaves the 24 hours within 23.
    assert!(
        (22 * 3600..=23 * 3600).contains(&retry_after_secs),
        "{retry_after_secs}"
    );
    assert_eq!(
        refused.to_string(),
        "the user started too many conversations today"
    );
    assert_eq!(started().await, store::MAX_STARTS_PER_DAY, "nothing stored");
    assert!(
        db.load(&owner, next.id()).await.is_none(),
        "the refused one is not stored"
    );
}

/// Titles, the archive and the list's order and pages.
#[tokio::test(flavor = "multi_thread")]
async fn titles_archive_and_the_list() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let who = principal(&owner);
    let (first, _) = db.started(&owner).await;
    let (second, _) = db.started(&owner).await;
    let (first, second) = (first.id(), second.id());
    assert_eq!(
        db.listed_ids(&owner, ListFilter::Active).await,
        [second, first]
    );

    let mut tx = db.tx(&owner).await;
    for (title, source, changed) in [
        ("How many readings?", TitleSource::Provisional, true),
        ("Readings today", TitleSource::Model, true),
        ("My readings", TitleSource::User, true),
        ("Readings again", TitleSource::Model, false),
        ("Another first question", TitleSource::Provisional, false),
    ] {
        assert_eq!(
            store::set_title(&mut tx, who, first, title, source)
                .await
                .expect("set the title"),
            changed,
            "{title} ({source:?})"
        );
    }
    let title = store::metadata(&mut tx, who, first)
        .await
        .expect("metadata")
        .expect("listed")
        .title
        .expect("a title");
    assert_eq!(
        (title.text.as_str(), title.source),
        ("My readings", TitleSource::User)
    );
    // One rule for every source (store::title_is_valid): nothing visible, bidirectional
    // controls and separators are refused whoever names the conversation.
    for bad in [
        String::new(),
        "x".repeat(201),
        "a\nb".to_owned(),
        "\u{200B}".to_owned(),
        "\u{FEFF}\u{200D}".to_owned(),
        "\u{E0100}".to_owned(),
        "\u{2800}".to_owned(),
        " ".to_owned(),
        "report\u{202E}fdp.exe".to_owned(),
        "a\u{2066}b\u{2069}".to_owned(),
        "a\u{2028}b".to_owned(),
    ] {
        for source in [
            TitleSource::User,
            TitleSource::Model,
            TitleSource::Provisional,
        ] {
            assert!(
                matches!(
                    store::set_title(&mut tx, who, first, &bad, source).await,
                    Err(StoreError::Unstorable(_))
                ),
                "{bad:?} ({source:?})"
            );
        }
    }
    // Joiners and marks beside visible characters are a title.
    let joined = "\u{1F469}\u{200D}\u{1F4BB} \u{200F}\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
    assert!(
        store::set_title(&mut tx, who, second, joined, TitleSource::User)
            .await
            .expect("a title with a ZWJ sequence and a right-to-left mark")
    );
    store::archive(&mut tx, who, second, true)
        .await
        .expect("archive");
    tx.commit().await.expect("commit");
    // The default list leaves the archived one out; the archive lists only it.
    assert_eq!(db.listed_ids(&owner, ListFilter::Active).await, [first]);
    assert_eq!(db.listed_ids(&owner, ListFilter::Archived).await, [second]);

    let mut tx = db.tx(&owner).await;
    let page = store::list(&mut tx, who, ListFilter::Archived, None, 1)
        .await
        .expect("the archive");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, second);
    assert!(page.items[0].archived_at.is_some());
    assert!(page.next.is_none());
    store::archive(&mut tx, who, second, false)
        .await
        .expect("take it out of the archive");
    let page = store::list(&mut tx, who, ListFilter::Active, None, 1)
        .await
        .expect("the first page");
    assert_eq!(
        page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [second]
    );
    assert!(page.items[0].archived_at.is_none());
    let next = store::list(&mut tx, who, ListFilter::Active, page.next, 1)
        .await
        .expect("the second page");
    assert_eq!(
        next.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [first]
    );
    assert!(next.next.is_none());
    tx.commit().await.expect("commit");
    assert_eq!(
        db.listed_ids(&owner, ListFilter::Active).await,
        [second, first]
    );
    assert!(db.listed_ids(&owner, ListFilter::Archived).await.is_empty());
}

/// Conversations with one `last_activity_at` are listed by ID (descending) and paged without a
/// gap or a repeat: the order and the cursor compare the UUID column, not its text output.
#[tokio::test(flavor = "multi_thread")]
async fn conversations_with_one_activity_time_page_in_id_order() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let mut ids = Vec::new();
    for _ in 0..7 {
        ids.push(db.started(&owner).await.0.id());
    }
    // The database sets the times itself; make them equal behind the store's back.
    let mut admin = db.superuser().await;
    let mut tx = admin.begin().await.expect("a superuser transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("skip the clock trigger");
    let moved = sqlx::query(
        "UPDATE assistant.conversations SET last_activity_at = '2026-01-01T00:00:00Z'
         WHERE conversation_id = ANY($1::text[]::uuid[])",
    )
    .bind(ids.iter().map(Uuid::to_string).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await
    .expect("equal activity times")
    .rows_affected();
    assert_eq!(moved, ids.len() as u64);
    tx.commit().await.expect("commit");

    ids.sort_unstable_by(|a, b| b.cmp(a));
    let who = principal(&owner);
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let mut tx = db.tx(&owner).await;
        let page = store::list(&mut tx, who, ListFilter::Active, after, 3)
            .await
            .expect("a page");
        tx.commit().await.expect("commit");
        seen.extend(page.items.iter().map(|item| item.id));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen, ids, "every conversation once, by descending ID");
}
