//! A handler that panics does not stop the consumer, and the lag gauges never freeze at an old
//! value (Laysi/trustiot-v2#333). Real Postgres with TLS, one throwaway container per test
//! (skipped without podman or docker unless `CI` is set).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use outbox::{
    Aggregate, Consumer, ConsumerConfig, Delivery, Handler, HandlerError, Lag, MAX_HANDLER_PANICS,
    Outcome, Position,
};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Postgres, Transaction};
use support::{Counted, Database, Setup, TENANT_A, database, drain, eventually, tenant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How long a consumer may take to reach a state the test waits for; generous, because the
/// machines that run the tests are often loaded.
const WAIT: Duration = Duration::from_secs(120);

/// What a handler wrote, by the superuser, in the consumer's schema. The service role may use
/// it (default privileges).
const HANDLED: &str = "CREATE TABLE handled (n bigint NOT NULL, setting text, backend integer \
     NOT NULL, at timestamptz NOT NULL DEFAULT clock_timestamp())";

fn config(setup: &Setup, subscription: &str) -> ConsumerConfig {
    ConsumerConfig::new(subscription, &setup.producer_schema)
        .expect("config")
        .poll_interval(Duration::from_millis(20))
        .max_backoff(Duration::from_millis(200))
        .lag_interval(Duration::from_millis(200))
}

async fn prepare(database: &Database) -> Setup {
    let setup = database.setup().await;
    sqlx::raw_sql(HANDLED)
        .execute(&setup.consumer)
        .await
        .expect("handled table");
    setup
}

/// Writes `Counted { n }` in tenant A, or platform-wide.
async fn publish(setup: &Setup, n: i64, in_tenant: bool) {
    let mut tx = setup.producer.begin().await.expect("begin");
    outbox::publish(
        &mut tx,
        &Aggregate::new("widget", "w").expect("aggregate"),
        &Counted { n },
        in_tenant.then(|| tenant(TENANT_A)),
    )
    .await
    .expect("publish");
    tx.commit().await.expect("commit");
}

/// `(n, tenant setting, backend pid)` of every committed handler run, in order.
async fn handled(setup: &Setup) -> Vec<(i64, Option<String>, i32)> {
    sqlx::query_as("SELECT n, setting, backend FROM handled ORDER BY at")
        .fetch_all(&setup.consumer)
        .await
        .expect("handled")
}

async fn dead_letters(setup: &Setup) -> Vec<String> {
    sqlx::query_scalar("SELECT reason FROM outbox_dead_letter ORDER BY failed_at")
        .fetch_all(&setup.consumer)
        .await
        .expect("dead letters")
}

/// Records the event (its `n`, the transaction's tenant setting, the backend), then panics on
/// the event `n` for its first `times` attempts, in `begin` or after writing in `handle`.
struct PanicsOn {
    n: i64,
    times: u32,
    in_begin: bool,
    attempts: AtomicU32,
}

impl PanicsOn {
    fn new(n: i64, times: u32, in_begin: bool) -> Self {
        Self {
            n,
            times,
            in_begin,
            attempts: AtomicU32::new(0),
        }
    }

    fn panics_now(&self, delivery: &Delivery) -> bool {
        let event: Counted = delivery.envelope.decode().expect("a Counted event");
        event.n == self.n && self.attempts.fetch_add(1, Ordering::SeqCst) < self.times
    }
}

impl Handler for PanicsOn {
    async fn begin(
        &self,
        _tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        assert!(
            !(self.in_begin && self.panics_now(delivery)),
            "test: the begin hook panics on purpose"
        );
        Ok(())
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery).await?;
        assert!(
            self.in_begin || !self.panics_now(delivery),
            "test: the handler panics on purpose after writing"
        );
        Ok(())
    }
}

async fn record(
    tx: &mut Transaction<'static, Postgres>,
    delivery: &Delivery,
) -> Result<(), HandlerError> {
    let event: Counted = delivery.envelope.decode()?;
    sqlx::query(
        "INSERT INTO handled (n, setting, backend) \
         VALUES ($1, NULLIF(current_setting('trustiot.tenant_id', true), ''), pg_backend_pid())",
    )
    .bind(event.n)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_panic_is_retried_and_the_event_then_processed_once() {
    let Some(database) = database().await else {
        return;
    };
    for in_begin in [false, true] {
        let setup = prepare(&database).await;
        publish(&setup, 1, true).await;
        publish(&setup, 2, true).await;
        let consumer =
            setup.subscribe(config(&setup, "panics-once"), PanicsOn::new(1, 1, in_begin));
        consumer.ensure_subscription().await.expect("subscription");

        let first = consumer.poll_once().await.expect("the poll returns");
        let Outcome::Retry { error, .. } = &first.outcome else {
            panic!("in_begin {in_begin}: a panic is retried, got {first:?}");
        };
        assert_eq!(error.reason(), "handler panicked");
        assert_eq!(first.delivered, 0);
        assert_eq!(
            consumer.position().await.expect("position"),
            Position::START
        );
        assert!(
            handled(&setup).await.is_empty(),
            "the panicked attempt is rolled back"
        );

        let rest = drain(&consumer, WAIT).await;
        assert_eq!((rest.delivered, rest.dead_lettered), (2, 0));
        let ns: Vec<i64> = handled(&setup).await.into_iter().map(|row| row.0).collect();
        assert_eq!(ns, [1, 2], "in_begin {in_begin}: each event once, in order");
        assert!(dead_letters(&setup).await.is_empty());
    }
}

#[tokio::test]
async fn an_event_that_keeps_panicking_is_dead_lettered_and_the_rest_processed() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    for n in 1..=3 {
        publish(&setup, n, true).await;
    }
    let consumer = setup.subscribe(
        config(&setup, "always-panics"),
        PanicsOn::new(1, u32::MAX, false),
    );
    consumer.ensure_subscription().await.expect("subscription");
    for attempt in 1..MAX_HANDLER_PANICS {
        let progress = consumer.poll_once().await.expect("the poll returns");
        assert!(
            matches!(progress.outcome, Outcome::Retry { .. }),
            "panic {attempt} is retried: {progress:?}"
        );
        assert_eq!(
            consumer.position().await.expect("position"),
            Position::START
        );
    }
    let last = consumer.poll_once().await.expect("the poll returns");
    assert_eq!(
        (last.dead_lettered, last.delivered),
        (1, 2),
        "the {MAX_HANDLER_PANICS}th panic dead-letters the event and the poll goes on: {last:?}"
    );
    assert!(matches!(last.outcome, Outcome::CaughtUp), "{last:?}");
    assert_eq!(dead_letters(&setup).await, ["handler panicked repeatedly"]);
    let ns: Vec<i64> = handled(&setup).await.into_iter().map(|row| row.0).collect();
    assert_eq!(ns, [2, 3], "nothing of the panicking event committed");
    assert_eq!(
        consumer.handler().attempts.load(Ordering::SeqCst),
        MAX_HANDLER_PANICS
    );
}

/// How [`CommitsThen`] ends its first attempt after committing the delivery's transaction.
#[derive(Clone, Copy, Debug)]
enum Then {
    Panic,
    Fail,
}

/// On event 1's first attempt, in `begin` or `handle`: writes, sends its own `COMMIT`, and then
/// panics or fails. Records every other event.
struct CommitsThen {
    in_begin: bool,
    then: Then,
    attempts: AtomicU32,
}

impl CommitsThen {
    async fn misbehave(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event: Counted = delivery.envelope.decode()?;
        if event.n != 1 || self.attempts.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(());
        }
        record(tx, delivery).await?;
        sqlx::query("COMMIT").execute(&mut **tx).await?;
        match self.then {
            Then::Panic => panic!("test: the handler panics on purpose after its own COMMIT"),
            Then::Fail => Err(HandlerError::retry("test: fails after its own COMMIT")),
        }
    }
}

impl Handler for CommitsThen {
    async fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if self.in_begin {
            self.misbehave(tx, delivery).await?;
        }
        Ok(())
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if !self.in_begin {
            self.misbehave(tx, delivery).await?;
        }
        record(tx, delivery).await
    }
}

/// A hook that ends the delivery's transaction and then panics (or, in `begin`, fails) is
/// dead-lettered at once, as for a handler that returns after its own `COMMIT`: a retry would
/// commit its writes a second time.
#[tokio::test]
async fn a_hook_that_ends_the_transaction_and_then_panics_is_dead_lettered() {
    let Some(database) = database().await else {
        return;
    };
    for (in_begin, then) in [
        (false, Then::Panic),
        (true, Then::Panic),
        (true, Then::Fail),
    ] {
        let setup = prepare(&database).await;
        publish(&setup, 1, true).await;
        publish(&setup, 2, true).await;
        let consumer = setup.subscribe(
            config(&setup, "commits-then"),
            CommitsThen {
                in_begin,
                then,
                attempts: AtomicU32::new(0),
            },
        );
        consumer.ensure_subscription().await.expect("subscription");
        let case = format!("in_begin {in_begin}, then {then:?}");
        let progress = drain(&consumer, WAIT).await;
        assert_eq!(
            (progress.dead_lettered, progress.delivered),
            (1, 1),
            "{case}"
        );
        assert_eq!(
            dead_letters(&setup).await,
            ["handler ended the delivery's transaction"],
            "{case}"
        );
        assert_eq!(
            consumer.handler().attempts.load(Ordering::SeqCst),
            1,
            "{case}: event 1 is not retried"
        );
        let ns: Vec<i64> = handled(&setup).await.into_iter().map(|row| row.0).collect();
        // Event 1's write before its own COMMIT stays; event 2 is processed once.
        assert_eq!(ns, [1, 2], "{case}");
    }
}

/// The F-5 scenario with `run`: an event whose handler always panics. `run` keeps going,
/// dead-letters it, processes the events after it, and keeps the lag current; once `run` ends,
/// the lag is no longer reported.
#[tokio::test]
async fn run_survives_panics_and_keeps_the_lag_current() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    for n in 1..=3 {
        publish(&setup, n, true).await;
    }
    let consumer = setup.subscribe(
        config(&setup, "run-panics"),
        PanicsOn::new(1, u32::MAX, false),
    );
    let stop = CancellationToken::new();
    let task = tokio::spawn(consumer.clone().run(stop.clone()));
    eventually(WAIT, "the panicking event dead-lettered", || async {
        (!dead_letters(&setup).await.is_empty()).then_some(())
    })
    .await;
    eventually(
        WAIT,
        "a current lag of 0 after the other events",
        || async {
            let done = handled(&setup).await.len() == 2;
            (done && consumer.lag().map(|lag| lag.events) == Some(0)).then_some(())
        },
    )
    .await;
    assert!(!task.is_finished(), "run is still going");

    stop.cancel();
    let ended = tokio::time::timeout(WAIT, task)
        .await
        .expect("run ends after stop");
    assert!(matches!(ended, Ok(Ok(()))), "{ended:?}");
    assert_eq!(
        consumer.lag(),
        None,
        "a consumer that stopped reports no lag"
    );
    assert_eq!(dead_letters(&setup).await, ["handler panicked repeatedly"]);
}

/// A panic while a query's rows are still arriving leaves the delivery's session usable and
/// clean: with one connection per pool, the next deliveries run on the same backend, with no
/// tenant left for a platform-wide event, and nothing of the panicked attempt is committed.
#[tokio::test]
async fn a_panic_mid_query_leaves_the_session_clean() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    publish(&setup, 1, true).await;
    publish(&setup, 2, false).await;
    let pool = || {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(WAIT)
    };
    let platform =
        PlatformDb::connect_lazy(database.postgres.options(&setup.consumer_platform), pool())
            .expect("verify-full options");
    let (_, work) = TenantDb::connect_lazy_with_background(
        database.postgres.options(&setup.consumer_role),
        pool(),
        ServiceIdentity::new(contract::ServiceName::Assets),
    )
    .expect("verify-full options");
    let consumer = Consumer::new(
        &platform,
        work,
        config(&setup, "mid-query"),
        PanicsMidQuery {
            attempts: AtomicU32::new(0),
            first_backend: AtomicU32::new(0),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    let first = consumer.poll_once().await.expect("the poll returns");
    assert!(matches!(first.outcome, Outcome::Retry { .. }), "{first:?}");
    let progress = drain(&consumer, WAIT).await;
    assert_eq!((progress.delivered, progress.dead_lettered), (2, 0));
    let rows = handled(&setup).await;
    let backend = rows.first().expect("a row").2;
    assert_eq!(
        rows,
        [(1, Some(TENANT_A.to_owned()), backend), (2, None, backend),],
        "each event once, on the panicked delivery's backend, the platform-wide one with no tenant"
    );
    let first_backend = consumer.handler().first_backend.load(Ordering::SeqCst);
    assert_eq!(
        i32::try_from(first_backend).expect("a pid"),
        backend,
        "the connection went back to the pool and was reused"
    );
}

/// On its first attempt, after writing, records the backend, sends a statement that takes a
/// second, and panics while that statement's future is still waiting for the answer.
struct PanicsMidQuery {
    attempts: AtomicU32,
    first_backend: AtomicU32,
}

impl Handler for PanicsMidQuery {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        record(tx, delivery).await?;
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut **tx)
                .await?;
            self.first_backend
                .store(u32::try_from(backend).expect("a pid"), Ordering::SeqCst);
            let slow = sqlx::query("SELECT pg_sleep(1)").execute(&mut **tx);
            tokio::pin!(slow);
            // Polls the statement, which sends it; it cannot finish within the 50 ms. The handler
            // then panics with the statement's future alive, waiting for the answer.
            let _sent = tokio::time::timeout(Duration::from_millis(50), &mut slow).await;
            panic!("test: the handler panics on purpose with a statement in flight");
        }
        Ok(())
    }
}

/// Blocks (asynchronously) on event 1 until released; records every other event.
struct Hangs {
    release: Arc<Notify>,
}

impl Handler for Hangs {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event: Counted = delivery.envelope.decode()?;
        if event.n == 1 {
            self.release.notified().await;
        }
        record(tx, delivery).await
    }
}

/// Processing that is stuck does not freeze the lag: it keeps being measured, so the count and
/// the age rise. Once `run` is dropped (its task aborted), the lag is no longer reported.
#[tokio::test]
async fn the_lag_rises_while_processing_is_stuck_and_stops_when_run_is_dropped() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    publish(&setup, 1, true).await;
    publish(&setup, 2, true).await;
    let release = Arc::new(Notify::new());
    let consumer = setup.subscribe(
        config(&setup, "stuck"),
        Hangs {
            release: Arc::clone(&release),
        },
    );
    let task = tokio::spawn(consumer.clone().run(CancellationToken::new()));
    let first = eventually(WAIT, "a lag of 2 while event 1 hangs", || async {
        consumer.lag().filter(|lag| lag.events == 2)
    })
    .await;
    let first_age = first.oldest_age.expect("an age");
    let later: Lag = eventually(WAIT, "the age to rise", || async {
        consumer
            .lag()
            .filter(|lag| lag.events == 2 && lag.oldest_age.is_some_and(|age| age > first_age))
    })
    .await;
    assert!(handled(&setup).await.is_empty(), "{later:?}");

    release.notify_one();
    eventually(WAIT, "a current lag of 0 once released", || async {
        consumer.lag().filter(|lag| lag.events == 0)
    })
    .await;
    task.abort();
    let aborted = task.await;
    assert!(aborted.is_err_and(|error| error.is_cancelled()));
    eventually(WAIT, "no lag reported once run is dropped", || async {
        consumer.lag().is_none().then_some(())
    })
    .await;
    assert_eq!(handled(&setup).await.len(), 2);
}

/// A measurement that does not return in time is not reported: while the producer's log is
/// locked, the lag query waits, and after three lag intervals the last value goes away. It
/// comes back once the measurement does.
#[tokio::test]
async fn a_stuck_lag_measurement_is_not_reported() {
    let Some(database) = database().await else {
        return;
    };
    let setup = prepare(&database).await;
    publish(&setup, 1, true).await;
    let consumer = setup.subscribe(config(&setup, "stuck-lag"), PanicsOn::new(0, 0, false));
    let stop = CancellationToken::new();
    let task = tokio::spawn(consumer.clone().run(stop.clone()));
    eventually(WAIT, "a current lag of 0", || async {
        consumer.lag().filter(|lag| lag.events == 0)
    })
    .await;

    let mut lock = setup.producer.begin().await.expect("begin");
    sqlx::query("LOCK TABLE outbox_event IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("lock the log");
    eventually(WAIT, "the stale lag to go away", || async {
        consumer.lag().is_none().then_some(())
    })
    .await;
    assert!(!task.is_finished(), "run is still going");

    lock.rollback().await.expect("unlock the log");
    eventually(WAIT, "a current lag again", || async {
        consumer.lag().filter(|lag| lag.events == 0)
    })
    .await;
    stop.cancel();
    let ended = tokio::time::timeout(WAIT, task)
        .await
        .expect("run ends after stop");
    assert!(matches!(ended, Ok(Ok(()))), "{ended:?}");
}
