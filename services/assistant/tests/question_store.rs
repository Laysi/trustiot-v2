//! The question API's store (Laysi/trustiot-v2#646): how an answered round, a stop and a delete
//! of one conversation are ordered, and what the answer guard removed, kept with the answer. The
//! product's schema, applied by the migration Job itself (`testkit::product`), with the service
//! role `assistant_app` through service-kit's tenant transactions, as the service uses them.
//!
//! - A stop or a delete that commits before the answered round's transaction wins: nothing of
//!   the round is stored and the run ends `cancelled`. An answered round that commits first
//!   wins: the stop finds the run ended (409), the delete keeps the round. Each order is forced
//!   with row locks, not timing: the second transaction starts while the first holds the
//!   conversation's row, the test waits until Postgres reports it blocked (`pg_blocking_pids`),
//!   then lets the first commit.
//! - What the guard removed from an answer is written with the answer and read back with it;
//!   the column takes only an answer's row and only the known kinds, each once and in order
//!   (23514), and the service role cannot change it (no UPDATE).
//!
//! The question API end to end, through the service binary, is tests/questions.rs.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use assistant::Agent;
use assistant::answer_guard::{Removed, RemovedContent};
use assistant::history::{Conversation, EntryKind, Principal};
use assistant::model::ModelClient;
use assistant::model::anthropic::AnthropicGateway;
use assistant::store::{self, Answered, StartedQuestion, StoreError, Stored};
use assistant::tools::NoDelegation;
use contract::identity::{TenantGrant, TenantRole};
use contract::{ServiceName, TenantId};
use service_kit::identity::UserIdentity;
use service_kit::tenant::{TenantDb, TenantTransaction};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, Row};
use support::{StandIn, WAIT, agent, fixture, ok, respond, settings};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use uuid::Uuid;

/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// A question's messages without tools: the question and the answer.
const ROUND: i64 = 2;

/// The provisional title the answered round sets.
const TITLE: &str = "How much energy did floor 3 use?";

/// Whether a session waits for a lock that the session `$1` holds.
const BLOCKS_ANOTHER: &str = "SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid))) AS blocks";

/// A message row the service role writes by hand, to probe the `removed` column's CHECK: `$1`
/// tenant, `$2` conversation, `$3` position, `$4` kind, `$5` removed.
const INSERT_REMOVED: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body, removed)
VALUES ($1::uuid, $2::uuid, $3, $4, 'anthropic_messages', '\\x7b7d'::bytea, $5::text[])";

struct Db {
    product: Arc<ProductPostgres>,
    db: TenantDb,
    issuer: TestIssuer,
    agent: Agent,
    _stand_in: StandIn,
}

async fn db() -> Option<Db> {
    let product = product_postgres(&["assistant_app"]).await?;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
    )
    .expect("the service role's pool verifies the server");
    // No tools: every question is answered at once.
    let stand_in = StandIn::start(respond(|_request, _index| {
        ok(fixture("answer_end_turn.json"))
    }))
    .await;
    let agent = agent(&stand_in, Vec::new(), settings());
    Some(Db {
        product,
        db,
        issuer: TestIssuer::new(),
        agent,
        _stand_in: stand_in,
    })
}

impl Db {
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

    async fn superuser(&self) -> PgConnection {
        self.product.superuser().await
    }

    /// A conversation of `user` with one answered round, stored.
    async fn conversation(&self, user: &UserIdentity) -> Uuid {
        let mut conversation = self.agent.start_conversation(principal(user), "");
        self.agent
            .ask(
                &mut conversation,
                "Did every meter report?",
                Arc::new(NoDelegation),
            )
            .await
            .expect("an answer");
        let mut tx = self.tx(user).await;
        store::create(&mut tx, &conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        conversation.id()
    }

    /// Starts a question in `id` as the question API does, commits it, and answers it in
    /// memory: the conversation with the answered round, where it was loaded, and the run.
    async fn answered_in_memory(
        &self,
        user: &UserIdentity,
        id: Uuid,
    ) -> (Conversation, Stored, Uuid) {
        let mut tx = self.tx(user).await;
        let StartedQuestion { loaded, run } = store::start_question(
            &mut tx,
            principal(user),
            id,
            Uuid::now_v7(),
            Duration::from_secs(600),
            3,
        )
        .await
        .expect("start the question");
        tx.commit().await.expect("commit");
        let mut conversation = loaded.conversation;
        self.agent
            .ask(&mut conversation, TITLE, Arc::new(NoDelegation))
            .await
            .expect("an answer");
        (conversation, loaded.stored, run.run_id)
    }

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
                "{what} did not wait for the conversation's row lock within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The conversation's messages, its title and the run's status and stop request, as the
    /// superuser reads them.
    async fn state(&self, id: Uuid, run: Uuid) -> (i64, Option<String>, String, bool) {
        let mut admin = self.superuser().await;
        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM assistant.messages WHERE conversation_id = $1::uuid)
                    AS messages,
                (SELECT title FROM assistant.conversations WHERE conversation_id = $1::uuid)
                    AS title,
                r.status, r.cancel_requested_at IS NOT NULL AS stop_requested
             FROM assistant.runs r WHERE r.run_id = $2::uuid",
        )
        .bind(id.to_string())
        .bind(run.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("read the conversation and the run");
        (
            row.get("messages"),
            row.get("title"),
            row.get("status"),
            row.get("stop_requested"),
        )
    }
}

fn principal(user: &UserIdentity) -> Principal {
    Principal {
        tenant_id: user.tenant_id().expect("the user acts in a tenant"),
        user_id: user.user_id(),
    }
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

/// A stop that commits while the answered round's transaction waits for the conversation's row:
/// the round is not stored and the run ends `cancelled`.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_that_commits_first_wins_over_the_answer() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;

    let mut stopping = db.tx(&owner).await;
    let stop = store::request_cancel(&mut stopping, principal(&owner), id, run)
        .await
        .expect("the stop");
    assert!(stop.cancel_requested_at.is_some());
    let holder = backend_pid(&mut stopping).await;
    let mut finishing = db.tx(&owner).await;
    let finish = tokio::spawn(async move {
        let answered =
            store::finish_answered(&mut finishing, &conversation, stored, run, Some(TITLE)).await;
        (answered, finishing)
    });
    db.wait_until_blocking(holder, "the answered round").await;
    stopping.commit().await.expect("commit the stop");
    let (answered, finishing) = finish.await.expect("the answer's task");
    assert!(
        matches!(&answered, Ok(Answered::Cancelled(_))),
        "the stop committed first: {answered:?}"
    );
    finishing.commit().await.expect("commit the cancellation");
    assert_eq!(
        db.state(id, run).await,
        (ROUND, None, "cancelled".to_owned(), true),
        "(messages, title, run status, stop requested): the first round only, no title"
    );
    println!("stop first: the answered round was not stored, the run is cancelled");
}

/// An answered round that commits while the stop waits: the round is stored, the run is
/// `answered`, and the stop finds it ended.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_commits_first_wins_over_the_stop() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;

    let mut finishing = db.tx(&owner).await;
    let answered =
        store::finish_answered(&mut finishing, &conversation, stored, run, Some(TITLE)).await;
    assert!(
        matches!(&answered, Ok(Answered::Stored { .. })),
        "{answered:?}"
    );
    let holder = backend_pid(&mut finishing).await;
    let mut stopping = db.tx(&owner).await;
    let who = principal(&owner);
    let stop = tokio::spawn(async move {
        let stopped = store::request_cancel(&mut stopping, who, id, run).await;
        (stopped, stopping)
    });
    db.wait_until_blocking(holder, "the stop").await;
    finishing.commit().await.expect("commit the answer");
    let (stopped, stopping) = stop.await.expect("the stop's task");
    assert!(
        matches!(&stopped, Err(StoreError::RunEnded)),
        "the answer committed first: {stopped:?}"
    );
    stopping.rollback().await.expect("roll back");
    assert_eq!(
        db.state(id, run).await,
        (
            2 * ROUND,
            Some(TITLE.to_owned()),
            "answered".to_owned(),
            false
        ),
        "(messages, title, run status, stop requested)"
    );
    println!("answer first: the round and the provisional title are stored, the stop is 409");
}

/// A delete that commits while the answered round's transaction waits: nothing of the round is
/// stored, the run ends `cancelled`, and the restored conversation is as it was before the
/// question.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_that_commits_first_leaves_nothing_of_the_round() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;

    let mut deleting = db.tx(&owner).await;
    let deletion = store::delete(&mut deleting, principal(&owner), id)
        .await
        .expect("the delete");
    assert_eq!(deletion.stopped_run, Some(run), "the delete stops the run");
    let holder = backend_pid(&mut deleting).await;
    let mut finishing = db.tx(&owner).await;
    let finish = tokio::spawn(async move {
        let answered =
            store::finish_answered(&mut finishing, &conversation, stored, run, Some(TITLE)).await;
        (answered, finishing)
    });
    db.wait_until_blocking(holder, "the answered round").await;
    deleting.commit().await.expect("commit the delete");
    let (answered, finishing) = finish.await.expect("the answer's task");
    assert!(
        matches!(&answered, Ok(Answered::Cancelled(_))),
        "the delete committed first: {answered:?}"
    );
    finishing.commit().await.expect("commit the cancellation");
    assert_eq!(
        db.state(id, run).await,
        (ROUND, None, "cancelled".to_owned(), true)
    );
    let mut tx = db.tx(&owner).await;
    store::restore(&mut tx, principal(&owner), id)
        .await
        .expect("restore");
    let loaded = store::load(&mut tx, principal(&owner), id)
        .await
        .expect("load")
        .expect("restored");
    tx.commit().await.expect("commit");
    assert_eq!(
        loaded.conversation.len(),
        2,
        "the history before the question"
    );
    println!("delete first: the answered round was not stored; restored as before the question");
}

/// An answered round that commits while the delete waits: the delete keeps it, stops no run
/// (it has ended), and the restored conversation has the round.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_commits_first_is_kept_by_the_delete() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;

    let mut finishing = db.tx(&owner).await;
    store::finish_answered(&mut finishing, &conversation, stored, run, Some(TITLE))
        .await
        .expect("the answered round");
    let holder = backend_pid(&mut finishing).await;
    let mut deleting = db.tx(&owner).await;
    let who = principal(&owner);
    let delete = tokio::spawn(async move {
        let deleted = store::delete(&mut deleting, who, id).await;
        (deleted, deleting)
    });
    db.wait_until_blocking(holder, "the delete").await;
    finishing.commit().await.expect("commit the answer");
    let (deleted, deleting) = delete.await.expect("the delete's task");
    let deletion = deleted.expect("deleted after the answer");
    deleting.commit().await.expect("commit the delete");
    assert_eq!(deletion.stopped_run, None, "the run had ended");
    assert_eq!(
        db.state(id, run).await,
        (
            2 * ROUND,
            Some(TITLE.to_owned()),
            "answered".to_owned(),
            false
        )
    );
    println!("answer first: the delete kept the stored round and stopped nothing");
}

/// What the guard removed from an answer is stored with it, in the same INSERT, and read back
/// with it; a message that is not an answer cannot carry it.
#[tokio::test(flavor = "multi_thread")]
async fn what_the_guard_removed_is_stored_and_read_back_with_the_answer() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let model =
        AnthropicGateway::new("http://127.0.0.1:9", "main", 1 << 20).expect("a valid gateway URL");
    let message = |text: &str| model.user_text(text).expect("a message");
    let removed: RemovedContent = [Removed::Sql, Removed::Html].into_iter().collect();

    let mut conversation = db.agent.start_conversation(principal(&owner), "");
    conversation.append(EntryKind::Question, message("Show me the query."));
    conversation.append_with(EntryKind::Answer, message("The total is 412 kWh."), removed);
    let mut tx = db.tx(&owner).await;
    store::create(&mut tx, &conversation)
        .await
        .expect("store the conversation");
    tx.commit().await.expect("commit");
    let mut admin = db.superuser().await;
    let stored: Vec<Option<Vec<String>>> = sqlx::query_scalar(
        "SELECT removed FROM assistant.messages WHERE conversation_id = $1::uuid ORDER BY seq",
    )
    .bind(conversation.id().to_string())
    .fetch_all(&mut admin)
    .await
    .expect("read the column");
    assert_eq!(
        stored,
        vec![None, Some(vec!["html".to_owned(), "sql".to_owned()])],
        "NULL on the question, the kinds in order on the answer"
    );
    let mut tx = db.tx(&owner).await;
    let loaded = store::load(&mut tx, principal(&owner), conversation.id())
        .await
        .expect("load")
        .expect("stored");
    tx.commit().await.expect("commit");
    assert_eq!(loaded.conversation.removed(0), RemovedContent::default());
    assert_eq!(loaded.conversation.removed(1), removed);

    // Stored again (a retry): the same messages with the same removed kinds are skipped. When
    // the stored kinds differ (the superuser changes them; the service role cannot), the stored
    // answer is not this one.
    let mut tx = db.tx(&owner).await;
    store::create(&mut tx, &conversation)
        .await
        .expect("a retry of the same messages");
    tx.rollback().await.expect("roll back");
    sqlx::query(
        "UPDATE assistant.messages SET removed = ARRAY['code']
         WHERE conversation_id = $1::uuid AND kind = 'answer'",
    )
    .bind(conversation.id().to_string())
    .execute(&mut admin)
    .await
    .expect("change the stored kinds");
    let mut tx = db.tx(&owner).await;
    let differs = store::create(&mut tx, &conversation).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(&differs, Err(StoreError::Conflict(_))),
        "{differs:?}"
    );

    // Only an answer carries it.
    let mut wrong = db.agent.start_conversation(principal(&owner), "");
    wrong.append_with(EntryKind::Question, message("Hello?"), removed);
    let mut tx = db.tx(&owner).await;
    let refused = store::create(&mut tx, &wrong).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(&refused, Err(StoreError::Unstorable(_))),
        "{refused:?}"
    );
    println!("removed kinds stored with the answer, read back, and refused on a question");
}

/// The `removed` column refuses, for the service role, a value on a message that is not an
/// answer, an unknown kind, a kind twice, an empty array and another order (23514), and it has
/// no UPDATE of it (42501).
#[tokio::test(flavor = "multi_thread")]
async fn the_removed_column_takes_only_an_answers_known_kinds_in_order() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let tenant = principal(&owner).tenant_id.to_string();
    let (db, owner) = (&db, &owner);
    let probe = |kind: &'static str, removed: Vec<&'static str>| {
        let (tenant, id) = (tenant.clone(), id.to_string());
        async move {
            let mut tx = db.tx(owner).await;
            let result = sqlx::query(INSERT_REMOVED)
                .bind(tenant)
                .bind(id)
                .bind(100_i32)
                .bind(kind)
                .bind(removed)
                .execute(&mut *tx)
                .await;
            tx.rollback().await.expect("roll back");
            result.map(|_| ())
        }
    };
    for (kind, removed) in [
        ("question", vec!["code"]),
        ("step", vec!["html"]),
        ("answer", vec!["javascript"]),
        ("answer", vec!["code", "code"]),
        ("answer", vec![]),
        ("answer", vec!["sql", "code"]),
        ("answer", vec!["Code"]),
    ] {
        let refused = probe(kind, removed.clone())
            .await
            .expect_err("the CHECK refuses it");
        assert_eq!(
            sqlstate(&refused).as_deref(),
            Some(CHECK_VIOLATION),
            "{kind} {removed:?}: {refused:?}"
        );
    }
    for removed in [
        vec!["code"],
        vec!["code", "html", "sql"],
        vec!["html", "sql"],
    ] {
        probe("answer", removed.clone())
            .await
            .unwrap_or_else(|error| panic!("{removed:?}: {error:?}"));
    }
    let mut tx = db.tx(owner).await;
    let update = sqlx::query(
        "UPDATE assistant.messages SET removed = ARRAY['code'] WHERE conversation_id = $1::uuid",
    )
    .bind(id.to_string())
    .execute(&mut *tx)
    .await
    .expect_err("no UPDATE on messages");
    tx.rollback().await.expect("roll back");
    assert_eq!(sqlstate(&update).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    println!("the removed column refused 7 values and every UPDATE; it took 3 valid sets");
}
