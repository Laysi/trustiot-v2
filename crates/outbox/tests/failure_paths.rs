//! Every failure path ends the delivery's transaction before the consumer goes on, and a `begin`
//! hook that breaks the transaction is caught on every path (Laysi/trustiot-v2#350). Real
//! Postgres with TLS, one throwaway container per test (skipped without podman or docker unless
//! `CI` is set).

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use contract::ServiceName;
use outbox::{
    Aggregate, Consumer, ConsumerConfig, Delivery, FailureKind, Handler, HandlerError, Outcome,
    Position, Replay,
};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool, Postgres, Transaction};
use support::{Counted, Database, Setup, TENANT_A, TENANT_B, database, drain, eventually, tenant};

/// How long a consumer or a pool may take to reach a state the test waits for; generous,
/// because the machines that run the tests are often loaded.
const WAIT: Duration = Duration::from_secs(120);

/// Connections the probed pool keeps idle before the test polls: the poll's position query and
/// the delivery take one each, and the transaction after a failed delivery must still find an
/// idle one, because only an idle connection passes through `before_acquire`.
const IDLE: usize = 4;

/// What a handler wrote, by the superuser, in the consumer's schema. The service role may use
/// it (default privileges).
const HANDLED: &str = "CREATE TABLE handled (n bigint NOT NULL, \
     at timestamptz NOT NULL DEFAULT clock_timestamp())";

fn config(setup: &Setup, subscription: &str) -> ConsumerConfig {
    ConsumerConfig::new(subscription, &setup.producer_schema)
        .expect("config")
        .poll_interval(Duration::from_millis(20))
        .max_backoff(Duration::from_millis(200))
}

async fn prepare(database: &Database) -> Setup {
    let setup = database.setup().await;
    sqlx::raw_sql(HANDLED)
        .execute(&setup.consumer)
        .await
        .expect("handled table");
    setup
}

/// Writes `Counted { n }` in tenant A.
async fn publish(setup: &Setup, n: i64) {
    let mut tx = setup.producer.begin().await.expect("begin");
    outbox::publish(
        &mut tx,
        &Aggregate::new("widget", "w").expect("aggregate"),
        &Counted { n },
        Some(tenant(TENANT_A)),
    )
    .await
    .expect("publish");
    tx.commit().await.expect("commit");
}

/// The `n` of every committed handler run, in order.
async fn handled(setup: &Setup) -> Vec<i64> {
    sqlx::query_scalar("SELECT n FROM handled ORDER BY at")
        .fetch_all(&setup.consumer)
        .await
        .expect("handled")
}

async fn record(
    tx: &mut Transaction<'static, Postgres>,
    delivery: &Delivery,
) -> Result<(), HandlerError> {
    let event: Counted = delivery.envelope.decode()?;
    sqlx::query("INSERT INTO handled (n) VALUES ($1)")
        .bind(event.n)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn n_of(delivery: &Delivery) -> i64 {
    let event: Counted = delivery.envelope.decode().expect("a Counted event");
    event.n
}

async fn dead_letters(setup: &Setup) -> Vec<String> {
    sqlx::query_scalar("SELECT reason FROM outbox_dead_letter ORDER BY failed_at")
        .fetch_all(&setup.consumer)
        .await
        .expect("dead letters")
}

/// Looks, at the moment the consumer takes its next connection, at what a delivery's backend
/// still holds.
///
/// A handler arms it with its backend just before failing. The next connection the consumer
/// takes from the pool (for the dead-letter transaction, or for the next poll after a retry)
/// passes through the pool's `before_acquire`, which asks the server how many transaction and
/// advisory locks that backend still holds: its virtual transaction ID lock, held for as long as
/// its transaction is open, and the subscription's lock. The question goes over a connection of
/// its own, from a thread and runtime of their own, and the hook waits for the answer by
/// blocking the test's single-threaded runtime: nothing else on that runtime runs in the
/// meantime, in particular not the pool's task that sends the `ROLLBACK` of a dropped
/// transaction. What the probe sees is therefore what the consumer left behind before it went
/// on, whatever the timing; a consumer that only dropped the transaction is seen with it open.
struct Probe {
    admin: PgConnectOptions,
    armed: Mutex<Option<i32>>,
    seen: Mutex<Vec<i64>>,
}

impl Probe {
    fn new(database: &Database) -> Arc<Self> {
        Arc::new(Self {
            admin: database.postgres.options("postgres"),
            armed: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn arm(&self, backend: i32) {
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some(backend);
    }

    fn seen(&self) -> Vec<i64> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The `before_acquire` hook: records what the armed backend holds, once.
    fn on_acquire(&self) {
        let armed = self
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(backend) = armed {
            let held = self.held_locks(backend);
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(held);
        }
    }

    /// How many transaction and advisory locks `backend` holds, asked from another thread and
    /// runtime while this thread waits.
    fn held_locks(&self, backend: i32) -> i64 {
        let admin = self.admin.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the probe");
            runtime.block_on(async move {
                let connect = PgConnection::connect_with(&admin);
                let mut connection = tokio::time::timeout(WAIT, connect)
                    .await
                    .expect("the probe connects within the wait")
                    .expect("connect as the superuser");
                let held: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_locks WHERE pid = $1 AND granted \
                     AND locktype IN ('virtualxid', 'advisory')",
                )
                .bind(backend)
                .fetch_one(&mut connection)
                .await
                .expect("read pg_locks");
                let _ = connection.close().await;
                held
            })
        })
        .join()
        .expect("the probe thread")
    }
}

/// The consuming service's pool, with the probe in `before_acquire` and [`IDLE`] connections
/// idle in it, and its handle for background work (the deliveries). Idle connections are not pinged before use, so nothing
/// between the pool handing one out and the hook lets another task run.
async fn probed_pool(
    database: &Database,
    setup: &Setup,
    probe: &Arc<Probe>,
) -> (TenantDb, BackgroundTenantDb) {
    let hook = Arc::clone(probe);
    let options = PgPoolOptions::new()
        .max_connections(u32::try_from(IDLE).expect("a small count"))
        .acquire_timeout(WAIT)
        .test_before_acquire(false)
        .before_acquire(move |_connection, _metadata| {
            let probe = Arc::clone(&hook);
            Box::pin(async move {
                probe.on_acquire();
                Ok(true)
            })
        });
    let (db, work) = TenantDb::connect_lazy_with_background(
        database.postgres.options(&setup.consumer_role),
        options,
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    // Opens every connection at once, then lets them all go back to the pool.
    let mut open = Vec::new();
    for _ in 0..IDLE {
        open.push(db.pool().begin().await.expect("open a pooled connection"));
    }
    for tx in open {
        tx.commit().await.expect("commit");
    }
    let pool: &PgPool = db.pool();
    eventually(WAIT, "every pooled connection idle", || async {
        (pool.num_idle() == IDLE).then_some(())
    })
    .await;
    (db, work)
}

/// On event 1's first attempt, in `begin` or in `handle`: arms the probe with its backend and
/// fails with `kind`. Records every successful `handle`, and counts the calls for event 1.
struct FailsOnce {
    in_begin: bool,
    kind: FailureKind,
    probe: Arc<Probe>,
    begins: AtomicU32,
    handles: AtomicU32,
}

impl FailsOnce {
    async fn fail(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        calls: &AtomicU32,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if n_of(delivery) != 1 || calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(());
        }
        let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut **tx)
            .await?;
        self.probe.arm(backend);
        Err(match self.kind {
            FailureKind::Retry => HandlerError::retry("test: fails once"),
            FailureKind::Permanent => HandlerError::permanent("test: fails for good"),
        })
    }
}

impl Handler for FailsOnce {
    async fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if self.in_begin {
            self.fail(tx, &self.begins, delivery).await
        } else {
            if n_of(delivery) == 1 {
                self.begins.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if self.in_begin {
            if n_of(delivery) == 1 {
                self.handles.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            self.fail(tx, &self.handles, delivery).await?;
        }
        record(tx, delivery).await
    }
}

/// What a series of polls did, until every committed event was processed.
#[derive(Debug, Default)]
struct Polls {
    delivered: usize,
    duplicates: usize,
    dead_lettered: usize,
    retries: usize,
    busy: usize,
}

/// Polls until a poll catches up with no lag. After a retry it polls again at once (the
/// consumer's own loop would back off first), so the next poll's first transaction follows the
/// failed delivery directly.
async fn poll_until_done<H: Handler>(consumer: &Consumer<H>) -> Polls {
    let deadline = Instant::now() + WAIT;
    let mut polls = Polls::default();
    loop {
        let progress = consumer.poll_once().await.expect("poll");
        polls.delivered += progress.delivered;
        polls.duplicates += progress.duplicates;
        polls.dead_lettered += progress.dead_lettered;
        match progress.outcome {
            Outcome::Retry { .. } => {
                polls.retries += 1;
                continue;
            }
            Outcome::CaughtUp => {
                if consumer.measure_lag().await.expect("lag").events == 0 {
                    return polls;
                }
            }
            Outcome::Busy => polls.busy += 1,
            Outcome::MoreAvailable => {}
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the consumer to process every event: {polls:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The probe can tell an open delivery transaction from an ended one: a transaction of the
/// service role that took an advisory lock holds two locks, and none once rolled back.
#[tokio::test]
async fn the_probe_sees_an_open_transaction_and_its_advisory_lock() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    let probe = Probe::new(&database);
    let (db, _) = probed_pool(&database, &setup, &probe).await;
    let mut tx = db.pool().begin().await.expect("begin");
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .expect("backend");
    sqlx::query("SELECT pg_advisory_xact_lock(350)")
        .execute(&mut *tx)
        .await
        .expect("advisory lock");
    assert_eq!(probe.held_locks(backend), 2, "the transaction and its lock");
    tx.rollback().await.expect("rollback");
    assert_eq!(probe.held_locks(backend), 0, "nothing once rolled back");
    assert!(probe.seen().is_empty(), "the hook records only when armed");
}

/// A handler that fails, in `begin` or `handle`, permanently or for a retry: by the time the
/// consumer takes its next connection (for the dead-letter transaction, or the next poll), the
/// failed delivery's transaction and the subscription's lock are gone. So the dead-letter
/// transaction is never refused as busy, and the handler sees the event exactly as often as the
/// failures call for: once when it fails for good, twice when it fails once and is retried.
#[tokio::test]
async fn a_failed_delivery_is_rolled_back_before_the_consumer_goes_on() {
    let Some(database) = database().await else {
        return;
    };
    for (in_begin, kind) in [
        (false, FailureKind::Permanent),
        (true, FailureKind::Permanent),
        (false, FailureKind::Retry),
        (true, FailureKind::Retry),
    ] {
        let case = format!("in_begin {in_begin}, {kind:?}");
        let setup = prepare(&database).await;
        publish(&setup, 1).await;
        publish(&setup, 2).await;
        let probe = Probe::new(&database);
        let (_db, work) = probed_pool(&database, &setup, &probe).await;
        let handler = FailsOnce {
            in_begin,
            kind,
            probe: Arc::clone(&probe),
            begins: AtomicU32::new(0),
            handles: AtomicU32::new(0),
        };
        let consumer = Consumer::new(&setup.platform, work, config(&setup, "fails"), handler);
        consumer.ensure_subscription().await.expect("subscription");
        let polls = poll_until_done(&consumer).await;

        assert_eq!(
            probe.seen(),
            [0],
            "{case}: the failed delivery's backend held no transaction or lock when the \
             consumer took its next connection"
        );
        assert_eq!(polls.busy, 0, "{case}: never refused as busy: {polls:?}");
        let handler = consumer.handler();
        let (begins, handles) = (
            handler.begins.load(Ordering::SeqCst),
            handler.handles.load(Ordering::SeqCst),
        );
        match kind {
            FailureKind::Permanent => {
                assert_eq!(
                    (polls.dead_lettered, polls.delivered, polls.retries),
                    (1, 1, 0),
                    "{case}"
                );
                assert_eq!((begins, handles), (1, u32::from(!in_begin)), "{case}");
                assert_eq!(
                    dead_letters(&setup).await,
                    ["test: fails for good"],
                    "{case}"
                );
                assert_eq!(handled(&setup).await, [2], "{case}");
            }
            FailureKind::Retry => {
                assert_eq!(
                    (polls.dead_lettered, polls.delivered, polls.retries),
                    (0, 2, 1),
                    "{case}"
                );
                assert_eq!((begins, handles), (2, 2 - u32::from(in_begin)), "{case}");
                assert!(dead_letters(&setup).await.is_empty(), "{case}");
                assert_eq!(handled(&setup).await, [1, 2], "{case}");
            }
        }
    }
}

/// How [`BreaksTransaction`]'s `begin` breaks the delivery's transaction.
#[derive(Clone, Copy, Debug)]
enum Break {
    /// Sends its own `COMMIT`.
    Commit,
    /// Moves the transaction to tenant B.
    Tenant,
}

/// From its call number `from` on (counting from 0), `begin` breaks the delivery's transaction
/// as `how` says and returns `Ok`. Counts its calls; `handle` records the event and counts its
/// calls.
struct BreaksTransaction {
    how: Break,
    from: u32,
    begins: AtomicU32,
    handles: AtomicU32,
}

impl Handler for BreaksTransaction {
    async fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        _delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if self.begins.fetch_add(1, Ordering::SeqCst) < self.from {
            return Ok(());
        }
        match self.how {
            Break::Commit => {
                sqlx::query("COMMIT").execute(&mut **tx).await?;
            }
            Break::Tenant => {
                sqlx::query("SELECT set_config('trustiot.tenant_id', $1, true)")
                    .bind(TENANT_B)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        self.handles.fetch_add(1, Ordering::SeqCst);
        record(tx, delivery).await
    }
}

/// A `begin` hook that ends the delivery's transaction or moves its tenant, and then returns
/// `Ok`, is caught before the consumer goes on, and the event is dead-lettered as for `handle`:
///
/// - on a duplicate delivery (the inbox lists the event, so `handle` is never called), which
///   before Laysi/trustiot-v2#350 went unchecked and counted as skipped;
/// - on a first delivery, before `handle` runs: after a `COMMIT` in `begin`, everything `handle`
///   wrote would commit statement by statement.
#[tokio::test]
async fn a_begin_hook_that_breaks_the_transaction_is_caught_on_every_path() {
    let Some(database) = database().await else {
        return;
    };
    for duplicate in [true, false] {
        for how in [Break::Commit, Break::Tenant] {
            let case = format!("duplicate {duplicate}, {how:?}");
            let setup = prepare(&database).await;
            publish(&setup, 1).await;
            let consumer = setup.subscribe(
                config(&setup, "breaks"),
                BreaksTransaction {
                    how,
                    from: u32::from(duplicate),
                    begins: AtomicU32::new(0),
                    handles: AtomicU32::new(0),
                },
            );
            consumer.ensure_subscription().await.expect("subscription");
            if duplicate {
                let first = drain(&consumer, WAIT).await;
                assert_eq!(first.delivered, 1, "{case}");
                let mut tx = setup.consumer.begin().await.expect("begin");
                outbox::replay_from(&mut tx, "breaks", Position::START, Replay::SkipProcessed)
                    .await
                    .expect("replay");
                tx.commit().await.expect("commit");
            }
            assert_eq!(
                consumer.position().await.expect("position"),
                Position::START,
                "{case}"
            );
            let polls = poll_until_done(&consumer).await;

            let expected = match how {
                Break::Commit => "handler ended the delivery's transaction",
                Break::Tenant => "handler changed the delivery's tenant",
            };
            assert_eq!(
                (polls.dead_lettered, polls.duplicates, polls.delivered),
                (1, 0, 0),
                "{case}: {polls:?}"
            );
            assert_eq!(dead_letters(&setup).await, [expected], "{case}");
            let handler = consumer.handler();
            assert_eq!(
                (
                    handler.begins.load(Ordering::SeqCst),
                    handler.handles.load(Ordering::SeqCst)
                ),
                (1 + u32::from(duplicate), u32::from(duplicate)),
                "{case}: `handle` runs only on the first, unbroken delivery"
            );
            let expected_handled: &[i64] = if duplicate { &[1] } else { &[] };
            assert_eq!(handled(&setup).await, expected_handled, "{case}");
            assert!(
                consumer.position().await.expect("position") > Position::START,
                "{case}: the subscription moved past the event"
            );
        }
    }
}
