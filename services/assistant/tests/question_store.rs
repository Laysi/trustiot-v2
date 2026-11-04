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
//!   (23514), and the service role cannot change it (no UPDATE); the CHECK was added without a
//!   scan (`NOT VALID`).
//! - A conversation's latest run is read through the index `runs_by_conversation`, a few pages
//!   whatever the tenant's other runs (`EXPLAIN (ANALYZE, BUFFERS)`).
//! - An answered run records the position of its answer (`answer_seq`, Laysi/trustiot-v2#1043),
//!   which only an answered run may have, naming a stored message of its conversation, unchanged
//!   after the run ended and removed with the conversation.
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

/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";

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
        let admitted = store::admit_question(&mut tx, principal(user), id, 3)
            .await
            .expect("let the question through");
        let StartedQuestion { loaded, run } =
            store::start_question(&mut tx, admitted, Uuid::now_v7(), Duration::from_secs(600))
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
    // Added `NOT VALID`, so adding it scanned no row; it checks every row written since.
    let mut admin = db.superuser().await;
    let validated: bool = sqlx::query_scalar(
        "SELECT convalidated FROM pg_catalog.pg_constraint
         WHERE conrelid = 'assistant.messages'::regclass AND conname = 'messages_removed_kinds'",
    )
    .fetch_one(&mut admin)
    .await
    .expect("read the CHECK");
    assert!(!validated, "the CHECK was added without a scan");
    println!("the removed column refused 7 values and every UPDATE; it took 3 valid sets");
}

/// `store::NEWEST_RUN` under `EXPLAIN (ANALYZE, BUFFERS)`: the test asserts the text after the
/// prefix is that query, so the plan is the service's.
const EXPLAIN_NEWEST_RUN: &str = "EXPLAIN (ANALYZE, BUFFERS) SELECT run_id::text AS run_id, conversation_id::text AS conversation_id, status, result_code,
    started_at, deadline_at, ended_at, cancel_requested_at,
    status = 'running' AND deadline_at <= statement_timestamp() AS past_deadline
FROM assistant.runs r
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
ORDER BY r.started_at DESC, r.run_id DESC
LIMIT 1";

/// `$1` tenant, `$2` user: 1,000 ended runs in each of the tenant's conversations, recorded by
/// the superuser (one statement, so they share `now()` and only the run ID orders them).
const MANY_RUNS: &str = "INSERT INTO assistant.runs
    (tenant_id, run_id, conversation_id, user_id, status, deadline_at, ended_at)
SELECT $1::uuid, gen_random_uuid(), c.conversation_id, $2::uuid, 'answered',
    now() + interval '10 minutes', now()
FROM assistant.conversations c, generate_series(1, 1000)
WHERE c.tenant_id = $1::uuid";

/// The number after `key` (`rows=`, `hit=`, `read=`) in `line`, 0 when there is none; a
/// fraction (Postgres 18 prints average rows so) is cut to its whole part.
fn number_after(line: &str, key: &str) -> u64 {
    line.split(key)
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_default()
}

/// The latest run of a conversation reads the index `runs_by_conversation`, never the tenant's
/// other runs: for a conversation without runs and for an unknown ID, among 20,000 runs of the
/// tenant, the plan is an index scan of it that touches a few pages; for a conversation with
/// runs, the first row of the same index. Prints each plan (for the test report).
#[tokio::test(flavor = "multi_thread")]
async fn the_latest_run_reads_the_conversations_index() {
    let Some(db) = db().await else {
        return;
    };
    assert_eq!(
        EXPLAIN_NEWEST_RUN.strip_prefix("EXPLAIN (ANALYZE, BUFFERS) "),
        Some(store::NEWEST_RUN),
        "the plan is of the service's query"
    );
    let owner = db.user(TenantId::generate());
    let stored = |conversation: Conversation| {
        let db = &db;
        let owner = &owner;
        async move {
            let mut tx = db.tx(owner).await;
            store::create(&mut tx, &conversation)
                .await
                .expect("store a conversation");
            tx.commit().await.expect("commit");
            conversation.id()
        }
    };
    let mut with_runs = Vec::new();
    for _ in 0..20 {
        with_runs.push(stored(db.agent.start_conversation(principal(&owner), "")).await);
    }
    let mut admin = db.superuser().await;
    let tenant = principal(&owner).tenant_id.to_string();
    let user = owner.user_id().to_string();
    sqlx::query(MANY_RUNS)
        .bind(&tenant)
        .bind(&user)
        .execute(&mut admin)
        .await
        .expect("record the runs");
    sqlx::query("ANALYZE assistant.runs")
        .execute(&mut admin)
        .await
        .expect("analyze the runs");
    let without_runs = stored(db.agent.start_conversation(principal(&owner), "")).await;

    for (what, conversation, found) in [
        ("a conversation without runs", without_runs, 0),
        ("an unknown ID", Uuid::now_v7(), 0),
        ("a conversation with 1,000 runs", with_runs[7], 1),
    ] {
        let mut tx = db.tx(&owner).await;
        let lines: Vec<String> = sqlx::query_scalar(EXPLAIN_NEWEST_RUN)
            .bind(&tenant)
            .bind(conversation.to_string())
            .bind(&user)
            .fetch_all(&mut *tx)
            .await
            .expect("explain the latest run");
        tx.rollback().await.expect("roll back");
        let plan = lines.join("\n");
        println!("{what}:\n{plan}\n");
        assert!(plan.contains("runs_by_conversation"), "{what}:\n{plan}");
        assert!(
            !plan.contains("Seq Scan") && !plan.contains("runs_pkey"),
            "{what}:\n{plan}"
        );
        // The first line is the top node (the limit), the first buffers line its own; its rows
        // after `actual` are what it returned (before is the estimate).
        let actual = lines[0].split("actual").nth(1).unwrap_or_default();
        assert_eq!(number_after(actual, " rows="), found, "{what}:\n{plan}");
        let buffers = lines
            .iter()
            .find(|line| line.trim_start().starts_with("Buffers:"))
            .map(String::as_str)
            .unwrap_or_default();
        let pages = number_after(buffers, "hit=") + number_after(buffers, "read=");
        assert!(pages <= 10, "{what} read {pages} pages:\n{plan}");
    }
}

/// `$1` conversation: a stored message the service cannot read back (not JSON, but within the
/// column's checks), as the superuser writes it.
const CORRUPT_MESSAGE: &str = "UPDATE assistant.messages SET body = '\\x7b2c7d'::bytea
WHERE conversation_id = $1::uuid AND seq = 0";

/// The question API lets a question through or refuses it before loading the conversation
/// (Laysi/trustiot-v2#646, the security review's L1): the refusals are decided on locks, counts
/// and sizes, so a conversation whose history cannot even be read is refused for a question
/// already running there (409) or for the user's running limit (429), not loaded; and the size
/// that the memory budget reserves is what loading returns, without the discarded rounds.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_refused_and_measured_without_loading_its_conversation() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;

    // Measured: the history as loading returns it, and the preamble besides.
    let mut tx = db.tx(&owner).await;
    let admitted = store::admit_question(&mut tx, principal(&owner), id, 3)
        .await
        .expect("let the question through");
    let loaded = store::load(&mut tx, principal(&owner), id)
        .await
        .expect("load")
        .expect("the conversation");
    tx.rollback().await.expect("roll back");
    let history: usize = loaded
        .conversation
        .messages()
        .iter()
        .map(|message| message.json().len())
        .sum();
    assert!(history > 0);
    assert_eq!(admitted.history_bytes(), history as u64);
    assert!(
        admitted.stored_bytes() > admitted.history_bytes(),
        "and the preamble"
    );

    // A discarded round is not counted, as loading leaves it out.
    let mut tx = db.tx(&owner).await;
    store::discard_last_round(&mut tx, principal(&owner), id, 0, loaded.stored)
        .await
        .expect("discard the round");
    let admitted = store::admit_question(&mut tx, principal(&owner), id, 3)
        .await
        .expect("let the question through");
    tx.rollback().await.expect("roll back");
    assert_eq!(admitted.history_bytes(), 0);

    // A question runs in `id`; then its history becomes unreadable.
    let mut tx = db.tx(&owner).await;
    let admitted = store::admit_question(&mut tx, principal(&owner), id, 3)
        .await
        .expect("let the question through");
    store::start_question(&mut tx, admitted, Uuid::now_v7(), Duration::from_secs(600))
        .await
        .expect("start the question");
    tx.commit().await.expect("commit");
    let other = db.conversation(&owner).await;
    let mut admin = db.superuser().await;
    for conversation in [id, other] {
        sqlx::query(CORRUPT_MESSAGE)
            .bind(conversation.to_string())
            .execute(&mut admin)
            .await
            .expect("corrupt a message");
    }
    for (conversation, max_running, what) in [(id, 3, "running"), (other, 1, "limited")] {
        let mut tx = db.tx(&owner).await;
        let refused = store::admit_question(&mut tx, principal(&owner), conversation, max_running)
            .await
            .expect_err("refused");
        tx.rollback().await.expect("roll back");
        match what {
            "running" => assert!(
                matches!(&refused, StoreError::AlreadyRunning),
                "{refused:?}"
            ),
            _ => assert!(
                matches!(&refused, StoreError::RunLimited { max: 1 }),
                "{refused:?}"
            ),
        }
    }
    // Loading the other conversation fails, as starting a question there would.
    let mut tx = db.tx(&owner).await;
    let corrupt = store::load(&mut tx, principal(&owner), other)
        .await
        .expect_err("its history cannot be read");
    tx.rollback().await.expect("roll back");
    assert!(matches!(&corrupt, StoreError::Corrupt(_)), "{corrupt:?}");
}

/// `$1` run: its status and the position of its answer, as the superuser reads them.
const RUN_ANSWER: &str = "SELECT status, answer_seq FROM assistant.runs WHERE run_id = $1::uuid";

/// `$1` conversation: the position of its last answer, as the superuser reads it.
const LAST_ANSWER: &str = "SELECT max(seq) FROM assistant.messages
    WHERE conversation_id = $1::uuid AND kind = 'answer'";

/// An answered run records the position of its answer's message (Laysi/trustiot-v2#1043,
/// `0007_answer_drafts.sql`), in the transaction that stores the round; a cancelled run has
/// none.
#[tokio::test(flavor = "multi_thread")]
async fn an_answered_run_records_where_its_answer_is() {
    let Some(db) = db().await else {
        return;
    };
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;
    let mut tx = db.tx(&owner).await;
    let answered = store::finish_answered(&mut tx, &conversation, stored, run, None).await;
    assert!(
        matches!(&answered, Ok(Answered::Stored { .. })),
        "{answered:?}"
    );
    tx.commit().await.expect("commit");
    let mut admin = db.superuser().await;
    let (status, answer_seq): (String, Option<i32>) = sqlx::query_as(RUN_ANSWER)
        .bind(run.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("read the run");
    let last: Option<i32> = sqlx::query_scalar(LAST_ANSWER)
        .bind(id.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("read the answers");
    assert_eq!(status, "answered");
    assert_eq!(
        answer_seq, last,
        "the round's answer, the conversation's last"
    );
    assert_eq!(
        answer_seq,
        Some(i32::try_from(2 * ROUND - 1).expect("fits"))
    );

    // A stopped run ends without one.
    let (conversation, stored, run) = db.answered_in_memory(&owner, id).await;
    let mut tx = db.tx(&owner).await;
    store::request_cancel(&mut tx, principal(&owner), id, run)
        .await
        .expect("the stop");
    tx.commit().await.expect("commit");
    let mut tx = db.tx(&owner).await;
    let cancelled = store::finish_answered(&mut tx, &conversation, stored, run, None).await;
    assert!(
        matches!(&cancelled, Ok(Answered::Cancelled(_))),
        "{cancelled:?}"
    );
    tx.commit().await.expect("commit");
    let (status, answer_seq): (String, Option<i32>) = sqlx::query_as(RUN_ANSWER)
        .bind(run.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("read the run");
    assert_eq!((status.as_str(), answer_seq), ("cancelled", None));
    println!("answered run: answer_seq {last:?}; cancelled run: none");
}

/// The database holds `answer_seq` to what it means, for every role: only an answered run has
/// one (23514), it names a stored message of the run's own conversation (23503), an ended run's
/// cannot change (42501, the trigger `runs_clock`), and purging the conversation removes the run
/// with its messages.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_position_names_a_message_of_an_answered_run() {
    let Some(db) = db().await else {
        return;
    };
    let db = &db;
    let owner = db.user(TenantId::generate());
    let id = db.conversation(&owner).await;
    let other = db.conversation(&owner).await;
    let tenant = principal(&owner).tenant_id.to_string();
    let user = owner.user_id().to_string();
    let mut admin = db.superuser().await;
    let insert = |status: &'static str, conversation: Uuid, answer_seq: i32| {
        let (tenant, user) = (tenant.clone(), user.clone());
        async move {
            let mut admin = db.superuser().await;
            sqlx::query(
                "INSERT INTO assistant.runs
                    (tenant_id, run_id, conversation_id, user_id, status, deadline_at, ended_at,
                     answer_seq)
                 VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5,
                     now() + interval '10 minutes',
                     CASE WHEN $5 = 'running' THEN NULL ELSE now() END, $6)",
            )
            .bind(tenant)
            .bind(Uuid::now_v7().to_string())
            .bind(conversation.to_string())
            .bind(user)
            .bind(status)
            .bind(answer_seq)
            .execute(&mut admin)
            .await
            .map(|_| ())
        }
    };
    // `db.conversation` stores one round: the answer is at position 1.
    for status in ["running", "failed", "cancelled", "interrupted"] {
        let refused = insert(status, id, 1).await.expect_err(status);
        assert_eq!(
            sqlstate(&refused).as_deref(),
            Some(CHECK_VIOLATION),
            "{status}"
        );
    }
    let missing = insert("answered", id, 7)
        .await
        .expect_err("no such message");
    assert_eq!(sqlstate(&missing).as_deref(), Some(FOREIGN_KEY_VIOLATION));
    insert("answered", id, 1)
        .await
        .expect("the answer's position");
    insert("answered", other, 1)
        .await
        .expect("the other conversation's");

    // The service role cannot move it on an ended run, nor set it on one.
    let mut tx = db.tx(&owner).await;
    let moved = sqlx::query(
        "UPDATE assistant.runs SET answer_seq = 0
         WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid",
    )
    .bind(&tenant)
    .bind(id.to_string())
    .execute(&mut *tx)
    .await
    .expect_err("an ended run stays as it ended");
    assert_eq!(sqlstate(&moved).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    tx.rollback().await.expect("roll back");

    // Purging the conversation (as the purge's cascade does, here as the superuser) removes its
    // runs with its messages and leaves the other conversation's.
    sqlx::query("DELETE FROM assistant.conversations WHERE conversation_id = $1::uuid")
        .bind(id.to_string())
        .execute(&mut admin)
        .await
        .expect("purge the conversation");
    let left: Vec<(String, i64)> = sqlx::query_as(
        "SELECT conversation_id::text, count(*) FROM assistant.runs
         WHERE conversation_id IN ($1::uuid, $2::uuid) GROUP BY conversation_id",
    )
    .bind(id.to_string())
    .bind(other.to_string())
    .fetch_all(&mut admin)
    .await
    .expect("count the runs");
    assert_eq!(left, [(other.to_string(), 1)]);
    println!("answer_seq: answered runs only, a stored message, unchanged after the end");
}
