//! The scheduled purge (Laysi/trustiot-v2#645) runs on one replica at a time: two assistant
//! processes on one database, the product's schema applied by the migration Job
//! (`testkit::product`).
//!
//! The first process's round is held open inside its DELETE: a trigger the superuser adds waits
//! (`pg_sleep`, not a lock, so the purge's `lock_timeout` does not end it) until the test opens
//! a gate. While it is held, the test sees the first process's purge session sleeping and its job
//! lock granted, starts the second process, and waits until the second logs that it skipped the
//! round because another replica holds the lock; the second opened no platform transaction. Then
//! the gate opens, and the first round deletes the expired conversation and logs how many it
//! deleted. No step waits a fixed time: each waits for what it names.
//!
//! Its own test binary, so its own Postgres: the trigger would hold every other test's purge.
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::sync::Arc;

use assistant::store;
use assistant::tools::Tool;
use chrono::{TimeDelta, Utc};
use contract::TenantId;
use contract::identity::TenantRole;
use service::{START_LIMIT, Service, User, wait_for};
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use support::{StandIn, fixture, ok, respond, settings};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, product_postgres};

/// The gate and the trigger that holds a purge's DELETE until the gate opens; at most about ten
/// minutes, so a failed test does not leave a session sleeping for good. Owned by the superuser
/// and run as it (`SECURITY DEFINER`): the purge role reads nothing of the gate itself.
const HOLD_PURGES: &str = "
CREATE TABLE public.purge_gate (open boolean NOT NULL);
INSERT INTO public.purge_gate VALUES (false);
CREATE FUNCTION public.hold_purge() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public
    AS $$
BEGIN
    FOR i IN 1..60000 LOOP
        EXIT WHEN (SELECT open FROM public.purge_gate);
        PERFORM pg_sleep(0.01);
    END LOOP;
    RETURN OLD;
END;
$$;
REVOKE ALL ON FUNCTION public.hold_purge() FROM PUBLIC;
CREATE TRIGGER hold_purge BEFORE DELETE ON assistant.conversations
    FOR EACH ROW EXECUTE FUNCTION public.hold_purge();";

const OPEN_GATE: &str = "UPDATE public.purge_gate SET open = true";

/// The purge role's sessions that sleep in the trigger.
const HELD_PURGES: &str = "SELECT count(*) FROM pg_stat_activity
WHERE usename = 'assistant_platform' AND wait_event = 'PgSleep'";

/// Who holds an advisory lock: the job lock's session.
const LOCK_HOLDERS: &str = "SELECT a.usename::text FROM pg_locks l
JOIN pg_stat_activity a ON a.pid = l.pid
WHERE l.locktype = 'advisory' AND l.granted";

/// The superuser moves a deletion back in time, past the clock trigger (which replica mode
/// skips).
const BACKDATE: &str = "UPDATE assistant.conversations SET deleted_at = $2
WHERE conversation_id = $1::uuid";

const COUNT_ROWS: &str = "SELECT
    (SELECT count(*) FROM assistant.conversations WHERE conversation_id = $1::uuid)
    + (SELECT count(*) FROM assistant.messages WHERE conversation_id = $1::uuid)";

/// What `JobLock::run_periodically` logs at DEBUG when another replica holds the lock.
const SKIPPED: &str = "scheduled job skipped; another replica holds its lock";
/// What a finished round logs.
const PURGED: &str = "purged expired conversations";
/// What `PlatformDb::begin` logs for every transaction.
const PLATFORM_ACCESS: &str = "platform database access";

/// The job lock's own events at DEBUG, so the skip shows; everything else at INFO.
const LOG_FILTER: (&str, &str) = (
    "TRUSTIOT__TELEMETRY__LOG_FILTER",
    "info,service_kit::job_lock=debug",
);

async fn count(admin: &mut PgConnection, query: &'static str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(admin)
        .await
        .expect("query the database")
}

#[tokio::test(flavor = "multi_thread")]
async fn one_replica_purges_at_a_time() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    sqlx::raw_sql(HOLD_PURGES)
        .execute(&mut admin)
        .await
        .expect("add the gate");

    // An expired conversation, deleted 31 days ago, with messages.
    let issuer = TestIssuer::new();
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let stand_in = StandIn::start(respond(|_request, _index| {
        ok(fixture("answer_end_turn.json"))
    }))
    .await;
    let agent = support::agent(&stand_in, Vec::<Arc<dyn Tool>>::new(), settings());
    let mut conversation = agent.start_conversation(owner.principal(), "");
    agent
        .ask(
            &mut conversation,
            "Did every meter report?",
            Arc::new(assistant::tools::NoDelegation),
        )
        .await
        .expect("an answer");
    let id = conversation.id();
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the service role's pool verifies the server");
    let mut tx = db
        .begin(&owner.identity(&issuer))
        .await
        .expect("a tenant transaction");
    store::create(&mut tx, &conversation)
        .await
        .expect("store it");
    store::delete(&mut tx, owner.principal(), id)
        .await
        .expect("delete it");
    tx.commit().await.expect("commit");
    let mut backdate = admin.begin().await.expect("a superuser transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *backdate)
        .await
        .expect("skip the clock trigger");
    sqlx::query(BACKDATE)
        .bind(id.to_string())
        .bind(Utc::now() - TimeDelta::days(31))
        .execute(&mut *backdate)
        .await
        .expect("move the deletion back");
    backdate.commit().await.expect("commit");
    let mut rows = sqlx::query_scalar::<_, i64>(COUNT_ROWS)
        .bind(id.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("count its rows");
    assert_eq!(rows, 3, "the conversation, its question and its answer");

    // The first replica's round starts when it starts, and stays in its DELETE.
    let jwks = issuer.jwks();
    let first = Service::spawn(&product, &jwks, &[LOG_FILTER]).await;
    wait_for(
        "the first replica's purge held in its DELETE",
        START_LIMIT,
        || async {
            let mut admin = product.superuser().await;
            (count(&mut admin, HELD_PURGES).await == 1).then_some(())
        },
    )
    .await;
    let holders: Vec<String> = sqlx::query_scalar(LOCK_HOLDERS)
        .fetch_all(&mut admin)
        .await
        .expect("read the advisory locks");
    assert_eq!(
        holders,
        ["assistant_app"],
        "one job lock, on the service role's pool"
    );

    // The second replica finds the lock held and skips its round.
    let second = Service::spawn(&product, &jwks, &[LOG_FILTER]).await;
    let skipped = second.wait_logged(SKIPPED).await;
    assert_eq!(skipped["fields"]["job"], "purge", "{skipped}");
    assert!(
        second.stdout.messages(PLATFORM_ACCESS).is_empty(),
        "the second replica opened no platform transaction:\n{}",
        second.stdout.text()
    );
    assert!(second.stdout.messages(PURGED).is_empty());
    assert_eq!(
        count(&mut admin, HELD_PURGES).await,
        1,
        "one round at a time"
    );
    assert!(first.stdout.messages(PURGED).is_empty(), "still held");
    let access = first.wait_logged(PLATFORM_ACCESS).await;
    assert_eq!(access["fields"]["purpose"], "purge expired conversations");

    // The gate opens: the first round deletes the conversation with its messages, and says how
    // many it deleted, nothing else.
    sqlx::query(OPEN_GATE)
        .execute(&mut admin)
        .await
        .expect("open the gate");
    let purged = first.wait_logged(PURGED).await;
    assert_eq!(purged["level"], "INFO", "{purged}");
    assert_eq!(purged["fields"]["purged"], 1, "{purged}");
    assert_eq!(
        purged["fields"]["skipped"], 0,
        "nothing stayed locked: {purged}"
    );
    rows = sqlx::query_scalar::<_, i64>(COUNT_ROWS)
        .bind(id.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("count its rows");
    assert_eq!(rows, 0, "purged with its messages");
    let line = first.stdout.text();
    assert!(
        !line.contains(&id.to_string()),
        "no conversation ID in the log"
    );
    assert!(
        !line.contains(&owner.tenant.to_string()),
        "no tenant in the log"
    );
    assert!(
        second.stdout.messages(PURGED).is_empty(),
        "the second never ran a round"
    );
    // Both are still ready: the purge's role logs in from both.
    assert_eq!(first.readiness().await, Some(200));
    assert_eq!(second.readiness().await, Some(200));
    println!("first replica: {purged}\nsecond replica: {skipped}");
}
