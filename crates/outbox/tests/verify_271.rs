//! Independent verification of Laysi/trustiot-v2#271 (the consumer reads as the platform role
//! and processes each event in a tenant transaction of the service role), written to break it.
//! Real Postgres with TLS, one throwaway container per test (skipped without podman or docker
//! unless `CI` is set). Report: `docs/test-reports/2026-10-11-271-verify.md`.
//!
//! Tests that show a defect assert the behaviour we want and are `#[ignore]`d with the issue
//! that tracks it, so the branch stays green; run them with `--ignored` to see them fail. The
//! `f1_` to `f4_` tests were findings F-1 to F-4 and pass since Laysi/trustiot-v2#271 fixed them; `f5_` is
//! Laysi/trustiot-v2#333.
//!
//! The handlers here deliberately run SQL that switches the tenant or the role (attacks that must
//! fail, or findings), so `tools/sql-check` exempts this file from its session-setting rule.

mod support;

use std::collections::HashSet;
use std::error::Error as _;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contract::{ServiceName, TenantId};
use outbox::{
    Aggregate, Consumer, ConsumerConfig, ConsumerError, Delivery, Handler, HandlerError, Outcome,
    Position,
};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::{PostgresConfig, SslMode};
use service_kit::tenant::{BackgroundTenantDb, PlatformActor, PlatformDb, TenantDb};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{AssertSqlSafe, PgPool, Postgres, Transaction};
use support::postgres::{SERVER_HOST, TEST_PASSWORD};
use support::{Counted, Database, Setup, TENANT_A, TENANT_B, database, eventually, tenant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a consumer may take to catch up; generous, because the queue machine is shared.
const CATCH_UP: Duration = Duration::from_secs(120);

/// A third tenant: the table owner writes one of its rows that no handler may see.
const TENANT_C: &str = "01928f6e-7a3c-7d4e-8f00-00000000000c";

/// Tables in the consumer's schema, created by the owner (the superuser). `note` is a tenant
/// table with service-kit's policy; `ledger`, `agg_state`, and `violation` hold no tenant data.
const TABLES: &str = "
CREATE TABLE note (tenant_id uuid NOT NULL, event_id uuid NOT NULL);
ALTER TABLE note ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON note
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
CREATE TABLE ledger (event_id uuid NOT NULL, delivery_tenant text, setting text,
    visible_notes bigint NOT NULL, at timestamptz NOT NULL DEFAULT clock_timestamp());
CREATE TABLE agg_state (aggregate_id text PRIMARY KEY, last_seq bigint NOT NULL);
CREATE TABLE violation (aggregate_id text NOT NULL, expected bigint NOT NULL, found bigint NOT NULL);
INSERT INTO note VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000c', '00000000-0000-7000-8000-0000000000c1');
";

async fn tables(setup: &Setup) {
    sqlx::raw_sql(TABLES)
        .execute(&setup.consumer)
        .await
        .expect("consumer tables");
}

fn uuid(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("a UUID")
}

/// Writes `Counted { n }` for `aggregate` as an event of `tenant` (platform-wide for `None`),
/// as the superuser.
async fn publish(
    setup: &Setup,
    aggregate: &str,
    n: i64,
    tenant: Option<TenantId>,
) -> Result<outbox::Published, String> {
    let mut tx = setup.producer.begin().await.map_err(|e| e.to_string())?;
    let published = outbox::publish(
        &mut tx,
        &Aggregate::new("widget", aggregate).expect("aggregate"),
        &Counted { n },
        tenant,
    )
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(published)
}

async fn publish_ok(
    setup: &Setup,
    aggregate: &str,
    n: i64,
    tenant: Option<TenantId>,
) -> outbox::Published {
    publish(setup, aggregate, n, tenant).await.expect("publish")
}

/// Notes the event in its tenant (if any) and records, in `ledger`, the delivery's tenant, the
/// transaction's tenant setting, and how many `note` rows the transaction can see.
async fn record(
    tx: &mut Transaction<'static, Postgres>,
    delivery: &Delivery,
    write_note: bool,
) -> Result<(), HandlerError> {
    let event_id = delivery.envelope.id.as_uuid();
    if let (true, Some(tenant)) = (write_note, delivery.tenant()) {
        sqlx::query("INSERT INTO note (tenant_id, event_id) VALUES ($1, $2)")
            .bind(tenant.as_uuid())
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO ledger (event_id, delivery_tenant, setting, visible_notes) \
         SELECT $1, $2, NULLIF(current_setting('trustiot.tenant_id', true), ''), count(*) \
         FROM note",
    )
    .bind(event_id)
    .bind(delivery.tenant().map(|tenant| tenant.to_string()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `(event_id, delivery_tenant, setting, visible_notes)` in processing order.
type LedgerRow = (String, Option<String>, Option<String>, i64);

async fn ledger(setup: &Setup) -> Vec<LedgerRow> {
    sqlx::query_as(
        "SELECT event_id::text, delivery_tenant, setting, visible_notes FROM ledger ORDER BY at",
    )
    .fetch_all(&setup.consumer)
    .await
    .expect("ledger")
}

async fn count(setup: &Setup, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&setup.consumer)
        .await
        .expect("count")
}

async fn notes_of(setup: &Setup, tenant: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM note WHERE tenant_id = $1")
        .bind(uuid(tenant))
        .fetch_one(&setup.consumer)
        .await
        .expect("notes")
}

fn sqlstate_of(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(error) => error.code().map(|code| code.into_owned()),
        _ => None,
    }
}

fn handler_sqlstate(error: &HandlerError) -> Option<String> {
    sqlstate_of(error.source()?.downcast_ref::<sqlx::Error>()?)
}

/// A consuming service's pools with `max` connections each.
fn pools(
    database: &Database,
    platform_role: &str,
    service_role: &str,
    max: u32,
    acquire_timeout: Duration,
) -> (PlatformDb, BackgroundTenantDb) {
    pools_with(
        database.postgres.options(platform_role),
        database.postgres.options(service_role),
        max,
        acquire_timeout,
    )
}

fn pools_with(
    platform: PgConnectOptions,
    service: PgConnectOptions,
    max: u32,
    acquire_timeout: Duration,
) -> (PlatformDb, BackgroundTenantDb) {
    let options = || {
        PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(acquire_timeout)
    };
    let platform = PlatformDb::connect_lazy(platform, options()).expect("verify-full options");
    let (_, work) = TenantDb::connect_lazy_with_background(
        service,
        options(),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    (platform, work)
}

fn config(setup: &Setup, subscription: &str) -> ConsumerConfig {
    ConsumerConfig::new(subscription, &setup.producer_schema)
        .expect("config")
        .poll_interval(Duration::from_millis(20))
        .max_backoff(Duration::from_millis(500))
        .lag_interval(Duration::from_millis(100))
}

/// Polls until a poll reports caught up and the lag is 0; panics on a retry outcome.
async fn catch_up<H: Handler>(consumer: &Consumer<H>) -> outbox::Progress {
    support::drain(consumer, CATCH_UP).await
}

/// Records every event (and a note in its tenant).
struct Ledger;

impl Handler for Ledger {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery, true).await
    }
}

// ---- 1. Tenant isolation in the consumer path ----

/// Runs `sql` (an escape attempt) and then writes a note of tenant B for the event.
struct Escape {
    sql: String,
}

impl Handler for Escape {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        sqlx::raw_sql(AssertSqlSafe(self.sql.clone()))
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO note (tenant_id, event_id) VALUES ($1, $2)")
            .bind(uuid(TENANT_B))
            .bind(delivery.envelope.id.as_uuid())
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

/// An escape attempt: its name and the SQL a handler runs.
type Attack = (&'static str, fn(&Setup) -> String);

/// Runs one escape attempt from a handler of a tenant-A event; returns whether a tenant-B note
/// was committed and the outcome as text.
async fn try_escape(database: &Database, attack: impl Fn(&Setup) -> String) -> (i64, String) {
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "w", 1, Some(tenant(TENANT_A))).await;
    let consumer = setup.subscribe(
        config(&setup, "escape"),
        Escape {
            sql: attack(&setup),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    let progress = consumer.poll_once().await.expect("poll");
    let outcome = match &progress.outcome {
        Outcome::Retry { error, .. } => format!(
            "retry, SQLSTATE {}",
            handler_sqlstate(error).unwrap_or_else(|| "-".to_owned())
        ),
        other => format!(
            "{other:?}, delivered {}, dead-lettered {}",
            progress.delivered, progress.dead_lettered
        ),
    };
    (notes_of(&setup, TENANT_B).await, outcome)
}

#[tokio::test]
async fn a_handler_cannot_leave_its_tenant_by_role_or_rls_tricks() {
    let Some(database) = database().await else {
        return;
    };
    let attacks: [Attack; 11] = [
        ("SET ROLE <consumer platform role>", |s| {
            format!("SET ROLE {}", s.consumer_platform)
        }),
        ("SET ROLE <producer service role>", |s| {
            format!("SET ROLE {}", s.producer_role)
        }),
        ("set_config('role', <consumer platform role>)", |s| {
            format!("SELECT set_config('role', '{}', true)", s.consumer_platform)
        }),
        ("SET SESSION AUTHORIZATION postgres", |_| {
            "SET SESSION AUTHORIZATION postgres".to_owned()
        }),
        ("RESET ROLE; RESET ALL", |_| {
            "RESET ROLE; RESET ALL".to_owned()
        }),
        ("SET LOCAL row_security = off", |_| {
            "SET LOCAL row_security = off".to_owned()
        }),
        ("ALTER TABLE note DISABLE ROW LEVEL SECURITY", |_| {
            "ALTER TABLE note DISABLE ROW LEVEL SECURITY".to_owned()
        }),
        ("CREATE POLICY open ON note USING (true)", |_| {
            "CREATE POLICY open ON note USING (true) WITH CHECK (true)".to_owned()
        }),
        ("read the producer's outbox_event", |s| {
            format!("SELECT count(*) FROM {}.outbox_event", s.producer_schema)
        }),
        ("CREATE TEMP TABLE", |_| {
            "CREATE TEMP TABLE stash (tenant_id uuid)".to_owned()
        }),
        ("SET ROLE NONE", |_| "SET ROLE NONE".to_owned()),
    ];
    let mut report = Vec::new();
    for (name, attack) in attacks {
        let (tenant_b_notes, outcome) = try_escape(&database, attack).await;
        report.push(format!(
            "{name}: {outcome}; tenant B notes {tenant_b_notes}"
        ));
        assert_eq!(
            tenant_b_notes, 0,
            "{name}: a handler of a tenant-A event wrote tenant B ({outcome})"
        );
        assert!(
            outcome.starts_with("retry"),
            "{name}: the delivery must fail, got {outcome}"
        );
    }
    println!("escape attempts:\n{}", report.join("\n"));
}

/// F-2: SQL in the handler can switch the transaction's tenant (decision 29 accepts this and
/// relies on `tools/sql-check`; a PL/pgSQL `EXECUTE` hides it from sql-check). The consumer does
/// not notice that the transaction it commits is no longer in the event's tenant.
#[tokio::test]
async fn f2_a_handler_that_switches_the_tenant_is_not_committed() {
    let Some(database) = database().await else {
        return;
    };
    let attacks: [Attack; 2] = [
        ("set_config local", |_| {
            format!("SELECT set_config('trustiot.tenant_id', '{TENANT_B}', true)")
        }),
        (
            "set_config hidden in PL/pgSQL EXECUTE (passes sql-check)",
            |_| {
                format!(
                    "DO $$ BEGIN EXECUTE 'SELECT set_' || 'config(''trustiot.' || 'tenant_id'', \
                 ''{TENANT_B}'', true)'; END $$"
                )
            },
        ),
    ];
    let mut outcomes = Vec::new();
    for (name, attack) in attacks {
        let (tenant_b_notes, outcome) = try_escape(&database, attack).await;
        outcomes.push((name, tenant_b_notes, outcome));
    }
    println!("tenant switches: {outcomes:?}");
    for (name, tenant_b_notes, outcome) in outcomes {
        assert_eq!(
            tenant_b_notes, 0,
            "{name}: the handler of a tenant-A event committed a tenant-B row ({outcome})"
        );
        // Fixed in Laysi/trustiot-v2#271: the consumer checks the tenant after the handler returns and
        // dead-letters the event, since every retry would switch again.
        assert!(
            outcome.ends_with("dead-lettered 1"),
            "{name}: the event is set aside, got {outcome}"
        );
    }
}

/// Sets the session's tenant (not the transaction's) to tenant B while handling a tenant event,
/// then puts the transaction's tenant back. Without that second step the consumer's check after
/// the handler (F-2's fix) already sees tenant B, rolls back, and dead-letters the event, so the
/// session setting never reaches a commit; with it, only the pool's check on release (F-1's fix)
/// stands between the leak and the next transaction.
struct SessionLeak;

impl Handler for SessionLeak {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if let Some(tenant) = delivery.tenant() {
            sqlx::query("SELECT set_config('trustiot.tenant_id', $1, false)")
                .bind(TENANT_B)
                .execute(&mut **tx)
                .await?;
            sqlx::query("SELECT set_config('trustiot.tenant_id', $1, true)")
                .bind(tenant.to_string())
                .execute(&mut **tx)
                .await?;
        }
        record(tx, delivery, false).await
    }
}

/// F-1: a session-level tenant set inside one delivery survives the commit on the pooled
/// connection; the next transaction "without a tenant" (a platform-wide event, a dead letter,
/// bookkeeping) runs in that tenant. service-kit checked for a preset tenant only on new
/// connections; since Laysi/trustiot-v2#271 it also closes a session that goes back to the pool
/// with a session-level tenant.
#[tokio::test]
async fn f1_a_platform_wide_event_never_runs_in_a_leaked_tenant() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    sqlx::query("INSERT INTO note (tenant_id, event_id) VALUES ($1, $2)")
        .bind(uuid(TENANT_B))
        .bind(Uuid::now_v7())
        .execute(&setup.consumer)
        .await
        .expect("a tenant-B row");
    let a = publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let platform_wide = publish_ok(&setup, "p", 2, None).await;
    // One connection per pool, so both deliveries use the same session.
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_role,
        1,
        Duration::from_secs(30),
    );
    let consumer = Consumer::new(&platform, work, config(&setup, "leak"), SessionLeak);
    consumer.ensure_subscription().await.expect("subscription");
    let progress = catch_up(&consumer).await;
    assert_eq!(progress.delivered, 2);
    let rows = ledger(&setup).await;
    println!("ledger after the leak: {rows:?}");
    assert_eq!(rows[0].0, a.event_id.to_string());
    let platform_row = &rows[1];
    assert_eq!(platform_row.0, platform_wide.event_id.to_string());
    assert_eq!(
        (platform_row.2.as_deref(), platform_row.3),
        (None, 0),
        "the platform-wide event's transaction must have no tenant and see no tenant row"
    );
}

/// Writes a note, then ends the transaction itself with `COMMIT` and reports a retry, once.
struct EarlyCommit {
    attempts: AtomicU32,
}

impl Handler for EarlyCommit {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery, true).await?;
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            sqlx::raw_sql("COMMIT").execute(&mut **tx).await?;
            return Err(HandlerError::retry("verify: fails after its own COMMIT"));
        }
        Ok(())
    }
}

/// F-3: a handler that sends `COMMIT` commits its writes outside the delivery's atomic unit;
/// the event was then retried and processed twice, because the consumer did not detect that the
/// transaction it was given had ended.
///
/// Since Laysi/trustiot-v2#271 the consumer detects it after the handler returns and
/// dead-letters the event instead of retrying it. The writes before the handler's `COMMIT` are
/// committed by the time the consumer gets control back, and nothing can undo them, so the
/// verifier's original expectation (nothing left after the failed attempt) cannot hold; what
/// the consumer guarantees is that the event is not processed a second time and is set aside
/// with the reason.
#[tokio::test]
async fn f3_a_handler_commit_does_not_split_the_delivery() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let consumer = setup.subscribe(
        config(&setup, "early-commit"),
        EarlyCommit {
            attempts: AtomicU32::new(0),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    let first = consumer.poll_once().await.expect("poll");
    println!(
        "first attempt: {:?}, delivered {}, dead-lettered {}",
        first.outcome, first.delivered, first.dead_lettered
    );
    let after_failure = notes_of(&setup, TENANT_A).await;
    catch_up(&consumer).await;
    let total = notes_of(&setup, TENANT_A).await;
    let attempts = consumer.handler().attempts.load(Ordering::SeqCst);
    let dead: Vec<(String, String)> =
        sqlx::query_as("SELECT event_id::text, reason FROM outbox_dead_letter")
            .fetch_all(&setup.consumer)
            .await
            .expect("dead letters");
    println!(
        "tenant A notes after the failed attempt: {after_failure}; after catching up: {total}; \
         handler attempts {attempts}; dead letters {dead:?}"
    );
    assert_eq!(
        (first.delivered, first.dead_lettered),
        (0, 1),
        "the split delivery is dead-lettered, not retried"
    );
    assert_eq!(
        (after_failure, total, attempts),
        (1, 1, 1),
        "what the handler committed itself stays, and the event is processed once"
    );
    assert_eq!(
        dead.iter()
            .map(|(_, reason)| reason.as_str())
            .collect::<Vec<_>>(),
        ["handler ended the delivery's transaction"]
    );
}

// ---- 1b. Tenant IDs: missing, mismatched, malformed, huge, unknown, nil ----

#[tokio::test]
async fn odd_tenant_ids_are_dead_lettered_or_run_in_exactly_the_row_tenant() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    let unknown = TenantId::from_uuid(Uuid::now_v7());
    let nil = TenantId::from_uuid(Uuid::nil());
    let normal = publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let envelope_null = publish_ok(&setup, "b", 2, Some(tenant(TENANT_A))).await;
    let row_null = publish_ok(&setup, "c", 3, Some(tenant(TENANT_A))).await;
    let malformed = publish_ok(&setup, "d", 4, Some(tenant(TENANT_A))).await;
    let huge = publish_ok(&setup, "e", 5, Some(tenant(TENANT_A))).await;
    let number = publish_ok(&setup, "f", 6, Some(tenant(TENANT_A))).await;
    let upper = publish_ok(&setup, "g", 7, Some(tenant(TENANT_A))).await;
    let unknown_event = publish_ok(&setup, "h", 8, Some(unknown)).await;
    let nil_event = publish_ok(&setup, "i", 9, Some(nil)).await;
    let platform_wide = publish_ok(&setup, "j", 10, None).await;
    let forge = async |sql: &'static str, event: &outbox::Published| {
        sqlx::query(sql)
            .bind(event.event_id.as_uuid())
            .execute(&setup.producer)
            .await
            .expect("forge a log row");
    };
    forge(
        "UPDATE outbox_event SET envelope = jsonb_set(envelope, '{tenant_id}', 'null') \
         WHERE event_id = $1",
        &envelope_null,
    )
    .await;
    forge(
        "UPDATE outbox_event SET tenant_id = NULL WHERE event_id = $1",
        &row_null,
    )
    .await;
    forge(
        "UPDATE outbox_event SET envelope = jsonb_set(envelope, '{tenant_id}', '\"not-a-uuid\"') \
         WHERE event_id = $1",
        &malformed,
    )
    .await;
    forge(
        "UPDATE outbox_event SET envelope = jsonb_set(envelope, '{tenant_id}', \
         to_jsonb(repeat('a', 1048576))) WHERE event_id = $1",
        &huge,
    )
    .await;
    forge(
        "UPDATE outbox_event SET envelope = jsonb_set(envelope, '{tenant_id}', '12345') \
         WHERE event_id = $1",
        &number,
    )
    .await;
    forge(
        "UPDATE outbox_event SET envelope = jsonb_set(envelope, '{tenant_id}', \
         to_jsonb(upper(envelope->>'tenant_id'))) WHERE event_id = $1",
        &upper,
    )
    .await;

    let consumer = setup.subscribe(config(&setup, "odd-tenants"), Ledger);
    consumer.ensure_subscription().await.expect("subscription");
    let progress = catch_up(&consumer).await;
    let dead: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_id::text, reason FROM outbox_dead_letter ORDER BY position_tx, position_id",
    )
    .fetch_all(&setup.consumer)
    .await
    .expect("dead letters");
    let rows = ledger(&setup).await;
    println!(
        "delivered {}, dead-lettered {}\ndead letters: {dead:?}\nledger: {rows:?}",
        progress.delivered, progress.dead_lettered
    );
    let mismatch = "envelope tenant differs from the log row";
    let undecodable = "stored envelope cannot be decoded";
    assert_eq!(
        dead,
        [
            (envelope_null.event_id.to_string(), mismatch.to_owned()),
            (row_null.event_id.to_string(), mismatch.to_owned()),
            (malformed.event_id.to_string(), undecodable.to_owned()),
            (huge.event_id.to_string(), undecodable.to_owned()),
            (number.event_id.to_string(), undecodable.to_owned()),
        ]
    );
    let expected: Vec<LedgerRow> = [
        (&normal, Some(TENANT_A.to_owned()), 1),
        (&upper, Some(TENANT_A.to_owned()), 2),
        (&unknown_event, Some(unknown.to_string()), 1),
        (&nil_event, Some(nil.to_string()), 1),
        (&platform_wide, None, 0),
    ]
    .into_iter()
    .map(|(event, tenant, visible)| (event.event_id.to_string(), tenant.clone(), tenant, visible))
    .collect();
    assert_eq!(
        rows, expected,
        "each delivered event ran in its row's tenant and saw only that tenant's notes"
    );
    assert_eq!(notes_of(&setup, TENANT_C).await, 1, "tenant C untouched");
}

// ---- 2. The platform-role read pool ----

/// Runs `sql` as `pool`'s role and returns the SQLSTATE, or `ok`.
async fn outcome_of(pool: &PgPool, sql: String) -> String {
    match sqlx::raw_sql(AssertSqlSafe(sql)).execute(pool).await {
        Ok(_) => "ok".to_owned(),
        Err(error) => sqlstate_of(&error).unwrap_or_else(|| format!("{error}")),
    }
}

#[tokio::test]
async fn the_platform_role_reads_the_log_and_nothing_else() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    publish_ok(&setup, "b", 2, Some(tenant(TENANT_B))).await;
    publish_ok(&setup, "p", 3, None).await;
    let (ps, cs) = (&setup.producer_schema, &setup.consumer_schema);
    // A plain session of the consumer's platform role, as anyone holding its password has.
    let reader = database.pool_as(ps, &setup.consumer_platform).await;
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&reader)
        .await
        .expect("the platform role reads the log");
    assert_eq!(all, 3, "every tenant's events and the platform-wide one");
    let refused = [
        format!("SELECT * FROM {ps}.outbox_aggregate"),
        format!("SELECT * FROM {ps}.widget"),
        format!("SELECT * FROM {ps}.scratch"),
        format!("SELECT * FROM {cs}.outbox_inbox"),
        format!("SELECT * FROM {cs}.note"),
        format!(
            "INSERT INTO {ps}.outbox_event (tx_order, event_id, event_type, aggregate_type, \
             aggregate_id, aggregate_seq, envelope) VALUES (1, gen_random_uuid(), 'x.y', 'w', \
             'w', 99, '{{}}')"
        ),
        format!("UPDATE {ps}.outbox_event SET event_type = 'x.y'"),
        format!("DELETE FROM {ps}.outbox_event"),
        format!("TRUNCATE {ps}.outbox_event"),
        format!("ALTER TABLE {ps}.outbox_event DISABLE ROW LEVEL SECURITY"),
        format!("CREATE POLICY mine ON {ps}.outbox_event USING (true)"),
        format!("CREATE TABLE {ps}.planted (n int)"),
        "CREATE TABLE public.planted (n int)".to_owned(),
        "CREATE SCHEMA planted".to_owned(),
        "CREATE TEMP TABLE planted (n int)".to_owned(),
    ];
    let mut report = Vec::new();
    for sql in refused {
        let outcome = outcome_of(&reader, sql.clone()).await;
        report.push(format!("{outcome}  {sql}"));
        assert_ne!(outcome, "ok", "the consumer's platform role ran: {sql}");
    }
    // The consumer's service role cannot read the producer's log at all.
    let service = database.pool_as(cs, &setup.consumer_role).await;
    let outcome = outcome_of(&service, format!("SELECT * FROM {ps}.outbox_event")).await;
    report.push(format!(
        "{outcome}  (service role) SELECT * FROM {ps}.outbox_event"
    ));
    assert_eq!(outcome, "42501");
    // Through service-kit's reader, the transaction is read-only on top of the grants.
    let mut tx = setup
        .platform
        .reader(
            PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
            "verify-271",
        )
        .begin()
        .await
        .expect("reader transaction");
    let write = sqlx::raw_sql(AssertSqlSafe(format!(
        "UPDATE {ps}.outbox_event SET event_type = event_type"
    )))
    .execute(&mut *tx)
    .await
    .expect_err("a write through the reader");
    report.push(format!(
        "{}  (reader) UPDATE {ps}.outbox_event",
        sqlstate_of(&write).unwrap_or_default()
    ));
    drop(tx);
    println!("platform role probes:\n{}", report.join("\n"));
}

/// F-4: `PlatformReader` transactions start with `BEGIN READ ONLY`, but nothing runs before the
/// caller's first statement, so `SET TRANSACTION READ WRITE` turns them into write transactions.
#[tokio::test]
async fn f4_a_platform_reader_transaction_stays_read_only() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    // A real platform role has back-office grants; give it one table it may write.
    let cs = &setup.consumer_schema;
    database
        .admin_sql(format!(
            "CREATE TABLE {cs}.backoffice (n int); \
             GRANT USAGE ON SCHEMA {cs} TO {p}; GRANT INSERT ON {cs}.backoffice TO {p}",
            p = setup.consumer_platform
        ))
        .await;
    let reader = setup.platform.reader(
        PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
        "verify-271",
    );
    let mut tx = reader.begin().await.expect("reader transaction");
    let flipped = sqlx::raw_sql("SET TRANSACTION READ WRITE")
        .execute(&mut *tx)
        .await;
    let insert = sqlx::raw_sql(AssertSqlSafe(format!(
        "INSERT INTO {cs}.backoffice VALUES (1)"
    )))
    .execute(&mut *tx)
    .await;
    let committed = tx.commit().await;
    let rows: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT count(*) FROM {cs}.backoffice"
    )))
    .fetch_one(&setup.consumer)
    .await
    .expect("count");
    println!(
        "SET TRANSACTION READ WRITE: {:?}; insert: {:?}; commit: {:?}; rows: {rows}",
        flipped.map(|_| ()).map_err(|e| sqlstate_of(&e)),
        insert.map(|_| ()).map_err(|e| sqlstate_of(&e)),
        committed.map_err(|e| sqlstate_of(&e)),
    );
    assert_eq!(rows, 0, "a PlatformReader transaction wrote a row");
}

/// Expects `ensure_subscription` to fail because service-kit refused a session; returns it.
async fn refused_session(platform: &PlatformDb, work: BackgroundTenantDb, setup: &Setup) -> String {
    let consumer = Consumer::new(platform, work, config(setup, "refused"), Ledger);
    let error = consumer
        .ensure_subscription()
        .await
        .expect_err("an unsafe role is refused");
    let ConsumerError::UnsafeSession(problem) = &error else {
        panic!("expected a refused session, got {error:?}");
    };
    assert!(error.is_retryable());
    // Nothing was processed with the unsafe role.
    assert_eq!(count(setup, "SELECT count(*) FROM outbox_inbox").await, 0);
    assert_eq!(count(setup, "SELECT count(*) FROM ledger").await, 0);
    problem.to_string()
}

#[tokio::test]
async fn unsafe_roles_are_refused_for_either_pool() {
    let Some(database) = database().await else {
        return;
    };
    let timeout = Duration::from_secs(3);
    let mut report = Vec::new();

    // The superuser as the reader.
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let (platform, work) = pools(&database, "postgres", &setup.consumer_role, 4, timeout);
    let problem = refused_session(&platform, work, &setup).await;
    assert!(problem.contains("superuser"), "{problem}");
    report.push(format!("superuser as the reader: {problem}"));

    // The two roles swapped: the platform role as the service role.
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_platform,
        4,
        timeout,
    );
    let problem = refused_session(&platform, work, &setup).await;
    assert!(problem.contains("consumer_read"), "{problem}");
    report.push(format!("platform role as the service role: {problem}"));

    // The read policy opened to every role (PUBLIC) instead of the platform role.
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    database
        .admin_sql(format!(
            "CREATE POLICY everyone ON {}.outbox_event FOR SELECT TO PUBLIC USING (true)",
            setup.producer_schema
        ))
        .await;
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_role,
        4,
        timeout,
    );
    let problem = refused_session(&platform, work, &setup).await;
    assert!(problem.contains("everyone"), "{problem}");
    report.push(format!("read policy for PUBLIC: {problem}"));

    // The platform role given BYPASSRLS.
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    database
        .admin_sql(format!("ALTER ROLE {} BYPASSRLS", setup.consumer_platform))
        .await;
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_role,
        4,
        timeout,
    );
    let problem = refused_session(&platform, work, &setup).await;
    assert!(problem.contains("bypasses"), "{problem}");
    report.push(format!("platform role with BYPASSRLS: {problem}"));

    // The service role made a member of the platform role.
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    database
        .admin_sql(format!(
            "GRANT {} TO {}",
            setup.consumer_platform, setup.consumer_role
        ))
        .await;
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_role,
        4,
        timeout,
    );
    let problem = refused_session(&platform, work, &setup).await;
    report.push(format!(
        "service role member of the platform role: {problem}"
    ));
    println!("refused sessions:\n{}", report.join("\n"));
}

// ---- 3. Delivery semantics under failure ----

#[derive(Clone, Copy, Debug)]
enum Failure {
    Retry,
    Permanent,
    Panic,
    BackendKilled,
}

/// Writes a note and the ledger, then fails as `failure` on the first attempt; later attempts
/// succeed.
struct FailsOnce {
    failure: Failure,
    attempts: AtomicU32,
    admin: PgPool,
}

impl Handler for FailsOnce {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery, true).await?;
        if self.attempts.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(());
        }
        match self.failure {
            Failure::Retry => Err(HandlerError::retry("verify: retry after writing")),
            Failure::Permanent => Err(HandlerError::permanent("verify: permanent after writing")),
            Failure::Panic => panic!("verify: the handler panics after writing"),
            Failure::BackendKilled => {
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut **tx)
                    .await?;
                sqlx::query("SELECT pg_terminate_backend($1)")
                    .bind(pid)
                    .execute(&self.admin)
                    .await
                    .expect("terminate the delivery's backend");
                // The session is gone; this statement cannot succeed.
                sqlx::query("SELECT 1").execute(&mut **tx).await?;
                Ok(())
            }
        }
    }
}

#[tokio::test]
async fn a_failed_delivery_leaves_nothing_and_the_event_is_processed_once() {
    let Some(database) = database().await else {
        return;
    };
    let mut report = Vec::new();
    for failure in [
        Failure::Retry,
        Failure::Permanent,
        Failure::Panic,
        Failure::BackendKilled,
    ] {
        let setup = database.setup().await;
        tables(&setup).await;
        let event = publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
        let platform_wide = publish_ok(&setup, "p", 2, None).await;
        // One connection per pool: the next delivery reuses the failed delivery's session.
        let (platform, work) = pools(
            &database,
            &setup.consumer_platform,
            &setup.consumer_role,
            1,
            Duration::from_secs(30),
        );
        let consumer = Consumer::new(
            &platform,
            work,
            config(&setup, "fails-once"),
            FailsOnce {
                failure,
                attempts: AtomicU32::new(0),
                admin: database.admin().await,
            },
        );
        consumer.ensure_subscription().await.expect("subscription");
        let attempt = {
            let consumer = consumer.clone();
            tokio::spawn(async move { consumer.poll_once().await }).await
        };
        let first = match attempt {
            Ok(Ok(progress)) => format!("{:?}", progress.outcome),
            Ok(Err(error)) => format!("consumer error: {error}"),
            Err(join) => format!("panicked: {}", join.is_panic()),
        };
        // The platform-wide event may follow a dead-lettered one in the same poll; count only
        // what the failed event left.
        let of_event = async |sql: &'static str| -> i64 {
            sqlx::query_scalar(sql)
                .bind(event.event_id.as_uuid())
                .fetch_one(&setup.consumer)
                .await
                .expect("count")
        };
        let notes = of_event("SELECT count(*) FROM note WHERE event_id = $1").await;
        let rows = of_event("SELECT count(*) FROM ledger WHERE event_id = $1").await;
        let inbox = of_event("SELECT count(*) FROM outbox_inbox WHERE event_id = $1").await;
        let position = consumer.position().await.expect("position");
        report.push(format!(
            "{failure:?}: first attempt {first}; notes {notes}, ledger {rows}, inbox {inbox}, \
             position {position}"
        ));
        assert_eq!((notes, rows, inbox), (0, 0, 0), "{failure:?}: {first}");
        match failure {
            Failure::Permanent => {
                assert_ne!(position, Position::START, "dead-lettered and moved past");
                let dead = count(&setup, "SELECT count(*) FROM outbox_dead_letter").await;
                assert_eq!(dead, 1);
            }
            _ => assert_eq!(position, Position::START, "{failure:?}: the event waits"),
        }
        catch_up(&consumer).await;
        let rows = ledger(&setup).await;
        let expected_first = match failure {
            Failure::Permanent => vec![],
            _ => vec![(
                event.event_id.to_string(),
                Some(TENANT_A.to_owned()),
                Some(TENANT_A.to_owned()),
                1,
            )],
        };
        let mut expected = expected_first;
        // The platform-wide event, on the same session afterwards, runs with no tenant.
        expected.push((platform_wide.event_id.to_string(), None, None, 0));
        assert_eq!(
            rows, expected,
            "{failure:?}: processed once, in the right tenant"
        );
    }
    println!("failures:\n{}", report.join("\n"));
}

/// Panics on its first event, then records events normally.
struct PanicsOnce {
    panicked: AtomicU32,
}

impl Handler for PanicsOnce {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery, true).await?;
        assert!(
            self.panicked.fetch_add(1, Ordering::SeqCst) > 0,
            "verify: the handler panics once"
        );
        Ok(())
    }
}

/// F-5, tracked in Laysi/trustiot-v2#333 (present before Laysi/trustiot-v2#271): a handler panic ends `Consumer::run` for good (the panic unwinds
/// through `tokio::join!` of processing and lag monitoring), so the subscription stops in that
/// replica and the lag gauges keep their last value.
#[tokio::test]
#[ignore = "Laysi/trustiot-v2#333: a handler panic ends Consumer::run and freezes the lag metric"]
async fn f5_a_handler_panic_does_not_stop_the_consumer() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let consumer = setup.subscribe(
        config(&setup, "panics-once"),
        PanicsOnce {
            panicked: AtomicU32::new(0),
        },
    );
    let stop = CancellationToken::new();
    let task = tokio::spawn(consumer.clone().run(stop.clone()));
    let processed = eventually(
        Duration::from_secs(30),
        "the event or the run task ending",
        || {
            let setup = &setup;
            let task = &task;
            async move {
                let rows = count(setup, "SELECT count(*) FROM ledger").await;
                (rows > 0 || task.is_finished()).then_some(rows)
            }
        },
    )
    .await;
    let finished = task.is_finished();
    let lag_gauge = consumer.lag().map(|lag| lag.events);
    let lag_now = consumer.measure_lag().await.expect("lag").events;
    stop.cancel();
    let ended = task.await;
    println!(
        "processed {processed}; run task finished on its own: {finished} ({:?}); lag gauge {lag_gauge:?}, \
         measured now {lag_now}",
        ended.as_ref().map_err(tokio::task::JoinError::is_panic)
    );
    assert!(
        !finished,
        "Consumer::run ended after one handler panic; the subscription no longer advances"
    );
    assert_eq!(processed, 1);
}

// ---- 3b. Concurrent replicas ----

/// Records each event, checks the aggregate's order and the transaction's tenant, and fails the
/// first attempt of every event whose `n` is a multiple of 11 after writing.
struct Strict {
    failed: Arc<Mutex<HashSet<Uuid>>>,
}

impl Handler for Strict {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event: Counted = delivery.envelope.decode()?;
        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_seq FROM agg_state WHERE aggregate_id = $1 FOR UPDATE")
                .bind(delivery.aggregate.id())
                .fetch_optional(&mut **tx)
                .await?;
        let expected = last.unwrap_or(0) + 1;
        if delivery.aggregate_seq != expected {
            sqlx::query(
                "INSERT INTO violation (aggregate_id, expected, found) VALUES ($1, $2, $3)",
            )
            .bind(delivery.aggregate.id())
            .bind(expected)
            .bind(delivery.aggregate_seq)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO agg_state (aggregate_id, last_seq) VALUES ($1, $2) \
             ON CONFLICT (aggregate_id) DO UPDATE SET last_seq = EXCLUDED.last_seq",
        )
        .bind(delivery.aggregate.id())
        .bind(delivery.aggregate_seq)
        .execute(&mut **tx)
        .await?;
        record(tx, delivery, true).await?;
        let first_time = event.n % 11 == 0
            && self
                .failed
                .lock()
                .expect("lock")
                .insert(*delivery.envelope.id.as_uuid());
        if first_time {
            return Err(HandlerError::retry(
                "verify: injected failure after writing",
            ));
        }
        Ok(())
    }
}

/// The tenant of the tests' aggregate `g<i>`: A, B, C, or platform-wide.
fn tenant_of_group(group: usize) -> Option<TenantId> {
    match group % 4 {
        0 => Some(tenant(TENANT_A)),
        1 => Some(tenant(TENANT_B)),
        2 => Some(tenant(TENANT_C)),
        _ => None,
    }
}

#[tokio::test]
async fn three_replicas_and_concurrent_writers_process_each_event_once_in_its_tenant() {
    let Some(database) = database().await else {
        return;
    };
    let setup = Arc::new(database.setup().await);
    tables(&setup).await;
    let failed = Arc::new(Mutex::new(HashSet::new()));
    let stop = CancellationToken::new();
    let mut replicas = Vec::new();
    for _ in 0..3 {
        let (platform, work) = database.consumer_pools(
            &setup.consumer_platform,
            &setup.consumer_role,
            Duration::from_secs(30),
        );
        let consumer = Consumer::new(
            &platform,
            work,
            config(&setup, "replicas"),
            Strict {
                failed: Arc::clone(&failed),
            },
        );
        replicas.push(tokio::spawn(consumer.run(stop.clone())));
    }
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 60;
    let mut writers = Vec::new();
    for writer in 0..WRITERS {
        let setup = Arc::clone(&setup);
        writers.push(tokio::spawn(async move {
            for i in 0..PER_WRITER {
                let group = (writer + i) % 8;
                let n = i64::try_from(writer * PER_WRITER + i).expect("small");
                publish_ok(&setup, &format!("g{group}"), n, tenant_of_group(group)).await;
            }
        }));
    }
    for writer in writers {
        writer.await.expect("writer");
    }
    let total = i64::try_from(WRITERS * PER_WRITER).expect("small");
    eventually(CATCH_UP, "every event in the ledger", || {
        let setup = Arc::clone(&setup);
        async move { (count(&setup, "SELECT count(*) FROM ledger").await >= total).then_some(()) }
    })
    .await;
    // Give a duplicate a chance to show up before stopping.
    eventually(CATCH_UP, "the inbox to list every event", || {
        let setup = Arc::clone(&setup);
        async move {
            (count(&setup, "SELECT count(*) FROM outbox_inbox").await >= total).then_some(())
        }
    })
    .await;
    stop.cancel();
    for replica in replicas {
        replica
            .await
            .expect("replica task")
            .expect("replica stopped cleanly");
    }
    let rows = count(&setup, "SELECT count(*) FROM ledger").await;
    let distinct = count(&setup, "SELECT count(DISTINCT event_id) FROM ledger").await;
    let wrong_tenant = count(
        &setup,
        "SELECT count(*) FROM ledger WHERE delivery_tenant IS DISTINCT FROM setting \
         OR visible_notes <> CASE WHEN setting IS NULL THEN 0 ELSE visible_notes END",
    )
    .await;
    let violations = count(&setup, "SELECT count(*) FROM violation").await;
    let notes = count(&setup, "SELECT count(*) FROM note").await;
    let injected = failed.lock().expect("lock").len();
    println!(
        "{total} events, 3 replicas, {injected} injected failures: ledger {rows} ({distinct} \
         distinct), wrong tenant {wrong_tenant}, order violations {violations}, notes {notes}"
    );
    assert_eq!(
        (rows, distinct),
        (total, total),
        "each event processed once"
    );
    assert_eq!(wrong_tenant, 0);
    assert_eq!(violations, 0);
    // Three of four groups belong to a tenant; plus tenant C's owner row.
    let tenant_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_event WHERE tenant_id IS NOT NULL")
            .fetch_one(&setup.producer)
            .await
            .expect("tenant events");
    assert_eq!(
        notes,
        tenant_events + 1,
        "one note per tenant event, plus C's"
    );
}

// ---- 4. Configuration and failure modes ----

/// The name of the container that publishes Postgres on `port`.
fn container_on(port: u16) -> (String, &'static str) {
    let needle = format!("127.0.0.1:{port}->5432");
    for runtime in ["podman", "docker"] {
        let Ok(output) = Command::new(runtime)
            .args(["ps", "--format", "{{.Names}} {{.Ports}}"])
            .output()
        else {
            continue;
        };
        if let Some(line) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| line.contains(&needle))
        {
            let name = line.split_whitespace().next().expect("a name").to_owned();
            return (name, runtime);
        }
    }
    panic!("no container publishes {needle}");
}

#[tokio::test]
async fn a_database_restart_mid_run_loses_and_duplicates_nothing() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    let (platform, work) = pools(
        &database,
        &setup.consumer_platform,
        &setup.consumer_role,
        4,
        Duration::from_secs(5),
    );
    let consumer = Consumer::new(&platform, work, config(&setup, "restart"), Ledger);
    let stop = CancellationToken::new();
    let task = tokio::spawn(consumer.clone().run(stop.clone()));
    for n in 0..10 {
        let group = usize::try_from(n).expect("small");
        publish_ok(&setup, &format!("g{group}"), n, tenant_of_group(group)).await;
    }
    eventually(CATCH_UP, "the first 10 events", || {
        let setup = &setup;
        async move { (count(setup, "SELECT count(*) FROM ledger").await >= 10).then_some(()) }
    })
    .await;
    let port = database.postgres.address.port();
    let (name, runtime) = container_on(port);
    // SIGKILL and start again: a crash, then crash recovery.
    let status = Command::new(runtime)
        .args(["restart", "--time", "0", &name])
        .status()
        .expect("restart the container");
    assert!(status.success(), "{runtime} restart failed");
    let (_, again) = container_on(port);
    assert_eq!(again, runtime, "the port mapping survives the restart");
    for n in 10..20 {
        let group = usize::try_from(n).expect("small");
        eventually(
            CATCH_UP,
            "Postgres to accept writes after the restart",
            || {
                let setup = &setup;
                let aggregate = format!("g{group}");
                async move {
                    publish(setup, &aggregate, n, tenant_of_group(group))
                        .await
                        .ok()
                }
            },
        )
        .await;
    }
    eventually(CATCH_UP, "all 20 events after the restart", || {
        let setup = &setup;
        async move { (count(setup, "SELECT count(*) FROM ledger").await >= 20).then_some(()) }
    })
    .await;
    stop.cancel();
    task.await.expect("consumer task").expect("stopped cleanly");
    let rows = count(&setup, "SELECT count(*) FROM ledger").await;
    let distinct = count(&setup, "SELECT count(DISTINCT event_id) FROM ledger").await;
    let wrong_tenant = count(
        &setup,
        "SELECT count(*) FROM ledger WHERE delivery_tenant IS DISTINCT FROM setting",
    )
    .await;
    println!(
        "restart: ledger {rows} ({distinct} distinct), wrong tenant {wrong_tenant}, events \
         {}",
        setup.event_count().await
    );
    assert_eq!((rows, distinct, wrong_tenant), (20, 20, 0));
}

/// A CA that did not sign the server's certificate.
fn foreign_ca() -> Vec<u8> {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "verify 271 foreign CA");
    params.self_signed(&key).expect("CA").pem().into_bytes()
}

#[tokio::test]
async fn misconfigured_pools_fail_without_processing_anything() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    tables(&setup).await;
    let event = publish_ok(&setup, "a", 1, Some(tenant(TENANT_A))).await;
    let platform_role = setup.consumer_platform.clone();
    let service_role = setup.consumer_role.clone();
    let good = |role: &str| database.postgres.options(role);
    let foreign = |role: &str| good(role).ssl_root_cert_from_pem(foreign_ca());
    let no_root = |role: &str| {
        PgConnectOptions::new_without_pgpass()
            .host(SERVER_HOST)
            .port(database.postgres.address.port())
            .username(role)
            .password(TEST_PASSWORD)
            .database("postgres")
            .ssl_mode(PgSslMode::VerifyFull)
    };
    let missing_file = |role: &str| {
        PostgresConfig {
            host: SERVER_HOST.to_owned(),
            port: database.postgres.address.port(),
            database: "postgres".to_owned(),
            ssl_mode: SslMode::VerifyFull,
            ca_file: std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("verify-271-missing-{}.pem", Uuid::now_v7())),
            username: role.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        }
        .connect_options()
    };
    let wrong_secret = |role: &str| good(role).password(&format!("not-{}", Uuid::now_v7()));
    let cases: Vec<(&str, PgConnectOptions, PgConnectOptions)> = vec![
        (
            "reader trusts another CA",
            foreign(&platform_role),
            good(&service_role),
        ),
        (
            "service role trusts another CA",
            good(&platform_role),
            foreign(&service_role),
        ),
        (
            "reader without a CA",
            no_root(&platform_role),
            good(&service_role),
        ),
        (
            "reader with a missing CA file",
            missing_file(&platform_role),
            good(&service_role),
        ),
        (
            "service role with a missing CA file",
            good(&platform_role),
            missing_file(&service_role),
        ),
        (
            "reader with a wrong password",
            wrong_secret(&platform_role),
            good(&service_role),
        ),
        (
            "service role with a wrong password",
            good(&platform_role),
            wrong_secret(&service_role),
        ),
    ];
    let mut report = Vec::new();
    for (name, platform_options, service_options) in cases {
        let (platform, work) =
            pools_with(platform_options, service_options, 2, Duration::from_secs(5));
        let consumer = Consumer::new(&platform, work, config(&setup, "misconfigured"), Ledger);
        let ensured = consumer.ensure_subscription().await;
        let polled = consumer.poll_once().await;
        let describe = |result: Result<String, ConsumerError>| match result {
            Ok(ok) => format!("ok ({ok})"),
            Err(error) => format!(
                "{} [{}]",
                error,
                error.source().map(ToString::to_string).unwrap_or_default()
            ),
        };
        let ensured_text = describe(ensured.map(|()| String::new()));
        let polled_is_err = polled.is_err();
        let polled_text = describe(polled.map(|progress| format!("{progress:?}")));
        report.push(format!("{name}: ensure {ensured_text}; poll {polled_text}"));
        assert!(
            polled_is_err,
            "{name}: the consumer polled with a bad configuration"
        );
        assert!(
            !ensured_text.contains("refused the database session"),
            "{name}: a transport or login failure, not a session check"
        );
        assert_eq!(
            count(&setup, "SELECT count(*) FROM ledger").await,
            0,
            "{name}"
        );
        assert_eq!(
            count(&setup, "SELECT count(*) FROM outbox_inbox").await,
            0,
            "{name}"
        );
    }
    // Configured right, the same subscription processes the event once, in its tenant.
    let (platform, work) = pools(
        &database,
        &platform_role,
        &service_role,
        2,
        Duration::from_secs(30),
    );
    let consumer = Consumer::new(&platform, work, config(&setup, "misconfigured"), Ledger);
    consumer.ensure_subscription().await.expect("subscription");
    catch_up(&consumer).await;
    assert_eq!(
        ledger(&setup).await,
        [(
            event.event_id.to_string(),
            Some(TENANT_A.to_owned()),
            Some(TENANT_A.to_owned()),
            1
        )]
    );
    println!("misconfigurations:\n{}", report.join("\n"));
}
