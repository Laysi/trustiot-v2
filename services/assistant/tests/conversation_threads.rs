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

use assistant::Agent;
use assistant::history::{Conversation, EntryKind, Principal};
use assistant::store::{self, Loaded, RunOutcome, RunStatus, StoreError, Stored, TitleSource};
use assistant::tools::{NoDelegation, Tool};
use chrono::{DateTime, TimeDelta, Utc};
use contract::identity::{TenantGrant, TenantRole};
use contract::{ServiceName, TenantId};
use service_kit::identity::{ServiceIdentity, UserIdentity};
use service_kit::tenant::{
    PlatformActor, PlatformDb, PlatformTransaction, TenantDb, TenantTransaction,
};
use sqlx::postgres::{PgDatabaseError, PgPoolOptions};
use sqlx::{Connection, PgConnection, Row};
use support::{CountReadings, StandIn, WAIT, agent, fixture, ok, respond, settings, tool_step};
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
        deleted
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

    async fn listed_ids(&self, user: &UserIdentity, include_archived: bool) -> Vec<Uuid> {
        let mut tx = self.tx(user).await;
        let page = store::list(
            &mut tx,
            principal(user),
            include_archived,
            None,
            store::MAX_PAGE,
        )
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
        assert!(
            !self.listed_ids(user, true).await.contains(&id),
            "the list, archived included"
        );

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
                let result =
                    store::start_run(&mut tx, who, id, Uuid::now_v7(), Duration::from_secs(600))
                        .await
                        .map(|_| ());
                finish(tx, false).await;
                result
            }),
            ("stop a question", {
                let mut tx = self.tx(user).await;
                let result = store::request_cancel(&mut tx, who, run).await.map(|_| ());
                finish(tx, false).await;
                result
            }),
        ];
        for (what, result) in refusals {
            let expected_run = what == "stop a question";
            assert!(
                match result {
                    Err(StoreError::NotStored) => !expected_run,
                    Err(StoreError::NoSuchRun) => expected_run,
                    _ => false,
                },
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
    assert!(db.listed_ids(&owner, false).await.contains(&id));
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

    // Deleted, it stays out of the colleague's reach.
    db.delete(&owner, id).await;
    let deleted = db.snapshot(id).await;
    assert!(db.deleted_ids(&colleague).await.is_empty());
    assert!(matches!(
        db.restore(&colleague, id).await,
        Err(StoreError::NotStored)
    ));
    assert_eq!(db.snapshot(id).await, deleted, "nothing changed");
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
    let purged = store::purge_expired(&mut tx, 100).await.expect("purge");
    tx.commit().await.expect("commit");
    assert_eq!(purged, 2);
    for id in [expired, expired_elsewhere] {
        let gone = db.snapshot(id).await;
        assert!(gone.is_gone(), "purged with everything it held: {gone:?}");
    }
    assert_eq!(db.snapshot(recent).await, recent_before);
    assert_eq!(db.snapshot(live).await, live_before);
    let mut tx = db.platform_tx().await;
    assert_eq!(store::purge_expired(&mut tx, 100).await.expect("purge"), 0);
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
                let purged = store::purge_expired(&mut purging, 100).await;
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
                store::purge_expired(&mut purging, 100)
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
            let started =
                store::start_run(&mut tx, who, id, Uuid::now_v7(), Duration::from_secs(600)).await;
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
    )
    .await
    .expect_err("the run ID is taken");
    tx.rollback().await.expect("roll back");
    let shown = format!("{again} | {again:?}");
    assert!(matches!(again, StoreError::Conflict(_)), "{shown}");
    assert!(!shown.contains(&run.to_string()), "{shown}");
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
    assert_eq!(db.listed_ids(&owner, false).await, [second, first]);

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
    for bad in [String::new(), "x".repeat(201), "a\nb".to_owned()] {
        assert!(matches!(
            store::set_title(&mut tx, who, first, &bad, TitleSource::User).await,
            Err(StoreError::Unstorable(_))
        ));
    }
    store::archive(&mut tx, who, second, true)
        .await
        .expect("archive");
    tx.commit().await.expect("commit");
    assert_eq!(db.listed_ids(&owner, false).await, [first]);
    assert_eq!(db.listed_ids(&owner, true).await, [second, first]);

    let mut tx = db.tx(&owner).await;
    let page = store::list(&mut tx, who, true, None, 1)
        .await
        .expect("the first page");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, second);
    assert!(page.items[0].archived_at.is_some());
    let next = store::list(&mut tx, who, true, page.next, 1)
        .await
        .expect("the second page");
    assert_eq!(
        next.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [first]
    );
    assert!(next.next.is_none());
    store::archive(&mut tx, who, second, false)
        .await
        .expect("take it out of the archive");
    tx.commit().await.expect("commit");
    assert_eq!(db.listed_ids(&owner, false).await, [second, first]);
}
