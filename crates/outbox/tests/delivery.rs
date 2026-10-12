//! Delivery from a producer's log to consumers, against a real Postgres with TLS (a throwaway
//! container; skipped without podman or docker unless `CI` is set): order within an aggregate,
//! no skipped late commits, once-only processing with concurrent replicas, replay, retries and
//! dead letters, lag, and least-privilege roles. Every consumer reads as its platform role under
//! row-level security and processes each event as its service role in the event's tenant
//! (decision 29); see `support::Database::setup`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use contract::ServiceName;
use outbox::{
    Aggregate, Consumer, ConsumerConfig, ConsumerError, Delivery, Handler, HandlerError, Outcome,
    Position, Replay, StartFrom,
};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Postgres, Transaction};
use support::{
    Counted, Recorder, Setup, TENANT_A, database, drain, drain_readable, eventually, tenant,
    tenant_of,
};
use tokio_util::sync::CancellationToken;

const DRAIN: Duration = Duration::from_secs(30);

fn recorder(setup: &Setup, subscription: &str) -> Consumer<Recorder> {
    setup.subscribe(
        ConsumerConfig::new(subscription, &setup.producer_schema)
            .expect("config")
            .poll_interval(Duration::from_millis(20)),
        Recorder {
            subscription: subscription.to_owned(),
        },
    )
}

async fn ensured(consumer: Consumer<Recorder>) -> Consumer<Recorder> {
    consumer.ensure_subscription().await.expect("subscription");
    consumer
}

/// The `n` values the consumer saw, in processing order.
async fn seen_in_order(setup: &Setup) -> Vec<i64> {
    sqlx::query_scalar("SELECT n FROM seen ORDER BY handled_at")
        .fetch_all(&setup.consumer)
        .await
        .expect("seen")
}

/// Where a published event sits in the log.
async fn position_of(setup: &Setup, published: &outbox::Published) -> Position {
    let (tx, id): (i64, i64) =
        sqlx::query_as("SELECT tx_order, id FROM outbox_event WHERE event_id = $1")
            .bind(published.event_id.as_uuid())
            .fetch_one(&setup.producer)
            .await
            .expect("the event's position");
    Position::new(tx, id)
}

async fn xid(tx: &mut Transaction<'_, Postgres>) -> i64 {
    sqlx::query_scalar("SELECT pg_current_xact_id()::text::bigint")
        .fetch_one(&mut **tx)
        .await
        .expect("transaction id")
}

#[tokio::test]
async fn an_aggregate_keeps_its_order_when_transaction_ids_are_inverted() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let x = Aggregate::new("widget", "x").expect("aggregate");

    // B takes a transaction ID first, by writing something unrelated...
    let mut b = setup.producer.begin().await.expect("begin B");
    sqlx::query("INSERT INTO scratch VALUES (1)")
        .execute(&mut *b)
        .await
        .expect("B's first write");
    let xid_b = xid(&mut b).await;
    // ...then A writes the aggregate's first event and commits...
    let mut a = setup.producer.begin().await.expect("begin A");
    outbox::publish(&mut a, &x, &Counted { n: 1 }, None)
        .await
        .expect("A publishes");
    let xid_a = xid(&mut a).await;
    a.commit().await.expect("commit A");
    // ...and only then does B write the aggregate's second event.
    let second = outbox::publish(&mut b, &x, &Counted { n: 2 }, None)
        .await
        .expect("B publishes");
    b.commit().await.expect("commit B");
    assert!(xid_b < xid_a, "the scenario needs B's ID below A's");
    assert_eq!(second.aggregate_seq, 2);

    // Ordering by transaction ID alone would put B's event first; the carried-forward order
    // puts it after A's.
    let orders: Vec<(i64, i64)> =
        sqlx::query_as("SELECT aggregate_seq, tx_order FROM outbox_event ORDER BY tx_order, id")
            .fetch_all(&setup.producer)
            .await
            .expect("log order");
    assert_eq!(orders, [(1, xid_a), (2, xid_a)]);

    let consumer = ensured(recorder(&setup, "inverted")).await;
    assert_eq!(drain(&consumer, DRAIN).await.delivered, 2);
    assert_eq!(seen_in_order(&setup).await, [1, 2]);
    assert_eq!(setup.violations().await, 0);
}

#[tokio::test]
async fn an_event_committed_late_is_delivered_not_skipped() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let consumer = ensured(recorder(&setup, "late-commit")).await;

    // C writes first but stays open; D writes later and commits at once.
    let mut c = setup.producer.begin().await.expect("begin C");
    outbox::publish(
        &mut c,
        &Aggregate::new("widget", "c").expect("aggregate"),
        &Counted { n: 1 },
        None,
    )
    .await
    .expect("C publishes");
    setup.publish("d", 2).await;

    // D is committed but sorts after C, which may still commit: nothing is delivered yet, and
    // the lag shows D waiting.
    assert_eq!(drain_readable(&consumer, DRAIN).await.delivered, 0);
    assert_eq!(
        consumer.position().await.expect("position"),
        Position::START
    );
    assert_eq!(consumer.measure_lag().await.expect("lag").events, 1);

    c.commit().await.expect("commit C");
    assert_eq!(drain(&consumer, DRAIN).await.delivered, 2);
    assert_eq!(seen_in_order(&setup).await, [1, 2]);
}

/// Blocks in `handle`, before writing anything, until released.
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Handler for Gate {
    async fn handle(
        &self,
        _tx: &mut Transaction<'static, Postgres>,
        _delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_handler_that_has_not_written_does_not_hold_back_other_consumers() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    setup.publish("w", 1).await;
    let gated = setup.subscribe(
        ConsumerConfig::new("gated", &setup.producer_schema).expect("config"),
        Gate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        },
    );
    gated.ensure_subscription().await.expect("subscription");
    let polling = {
        let gated = gated.clone();
        tokio::spawn(async move { gated.poll_once().await.expect("gated poll").delivered })
    };
    gated.handler().entered.notified().await;

    // While the gated delivery's transaction is open, another subscription reads everything,
    // including an event committed after that transaction began.
    setup.publish("w", 2).await;
    let other = ensured(recorder(&setup, "unhindered")).await;
    assert_eq!(drain(&other, DRAIN).await.delivered, 2);

    // A second replica of the gated subscription finds it busy.
    let replica = setup.subscribe(
        ConsumerConfig::new("gated", &setup.producer_schema).expect("config"),
        Recorder {
            subscription: "gated".to_owned(),
        },
    );
    let progress = replica.poll_once().await.expect("replica poll");
    assert!(matches!(progress.outcome, Outcome::Busy), "{progress:?}");

    gated.handler().release.notify_one();
    assert_eq!(polling.await.expect("gated task"), 1);
}

/// Requests shutdown while handling the first event.
struct StopsOnFirst {
    stop: CancellationToken,
    handled: AtomicU32,
}

impl Handler for StopsOnFirst {
    async fn handle(
        &self,
        _tx: &mut Transaction<'static, Postgres>,
        _delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        self.stop.cancel();
        Ok(())
    }
}

#[tokio::test]
async fn shutdown_finishes_the_event_in_flight_and_leaves_the_rest_of_the_batch() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let first = setup.publish("w", 1).await;
    for n in 2..=5 {
        setup.publish("w", n).await;
    }
    let stop = CancellationToken::new();
    let consumer = setup.subscribe(
        ConsumerConfig::new("stopping", &setup.producer_schema).expect("config"),
        StopsOnFirst {
            stop: stop.clone(),
            handled: AtomicU32::new(0),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    // All five are committed; the first poll reads them in one batch unless another
    // transaction holds the read horizon back, and either way only one may be handled.
    eventually(DRAIN, "all events counted", || async {
        (consumer.measure_lag().await.expect("lag").events == 5).then_some(())
    })
    .await;
    tokio::time::timeout(DRAIN, consumer.process(stop))
        .await
        .expect("process stops after the event in flight");
    assert_eq!(consumer.handler().handled.load(Ordering::SeqCst), 1);
    assert_eq!(
        consumer.position().await.expect("position"),
        position_of(&setup, &first).await,
        "the event in flight committed, the rest wait"
    );
}

/// A tiny deterministic generator, so the interleaving varies without a dependency.
struct Lcg(u64);

impl Lcg {
    fn next_ms(&mut self, max: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % (max + 1)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_and_two_replicas_process_each_event_once_in_order() {
    const WRITERS: u64 = 6;
    const EVENTS_PER_WRITER: u64 = 30;
    const AGGREGATES: u64 = 4;
    let Some(database) = database().await else {
        return;
    };
    let setup = Arc::new(database.setup().await);

    // Two replicas of the consuming service, each with its own pool, as two pods would have.
    let stop = CancellationToken::new();
    let mut replicas = Vec::new();
    for _ in 0..2 {
        let (platform, work) = database.consumer_pools(
            &setup.consumer_platform,
            &setup.consumer_role,
            Duration::from_secs(30),
        );
        let consumer = Consumer::new(
            &platform,
            work,
            ConsumerConfig::new("stress", &setup.producer_schema)
                .expect("config")
                .batch_size(7)
                .poll_interval(Duration::from_millis(10)),
            Recorder {
                subscription: "stress".to_owned(),
            },
        );
        consumer.ensure_subscription().await.expect("subscription");
        let stop = stop.clone();
        replicas.push(tokio::spawn(async move { consumer.process(stop).await }));
    }

    // Writers interleave on a few aggregates; some take a transaction ID early and sleep
    // inside their transactions, so commits land out of ID order.
    let mut writers = Vec::new();
    for writer in 0..WRITERS {
        let setup = Arc::clone(&setup);
        writers.push(tokio::spawn(async move {
            let mut random = Lcg(writer + 1);
            for i in 0..EVENTS_PER_WRITER {
                let aggregate = format!("agg-{}", (writer + i) % AGGREGATES);
                let mut tx = setup.producer.begin().await.expect("begin");
                if random.next_ms(1) == 1 {
                    sqlx::query("INSERT INTO scratch VALUES (1)")
                        .execute(&mut *tx)
                        .await
                        .expect("early write");
                }
                tokio::time::sleep(Duration::from_millis(random.next_ms(4))).await;
                let tenant = tenant_of(&aggregate);
                outbox::publish(
                    &mut tx,
                    &Aggregate::new("widget", aggregate).expect("aggregate"),
                    &Counted {
                        n: i64::try_from(writer * 1000 + i).expect("small"),
                    },
                    Some(tenant),
                )
                .await
                .expect("publish");
                tokio::time::sleep(Duration::from_millis(random.next_ms(4))).await;
                tx.commit().await.expect("commit");
            }
        }));
    }
    for writer in writers {
        writer.await.expect("writer");
    }
    let total = i64::try_from(WRITERS * EVENTS_PER_WRITER).expect("small");
    assert_eq!(setup.event_count().await, total);
    eventually(Duration::from_secs(60), "every event processed", || {
        let setup = Arc::clone(&setup);
        async move { (setup.seen_count().await == total).then_some(()) }
    })
    .await;
    // Give a duplicate a chance to show up before stopping.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.cancel();
    for replica in replicas {
        replica.await.expect("replica");
    }

    assert_eq!(setup.seen_count().await, total, "each event processed once");
    assert_eq!(setup.violations().await, 0, "per-aggregate order kept");
    let per_aggregate: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT aggregate_id, count(*), max(seq) FROM seen GROUP BY 1 ORDER BY 1")
            .fetch_all(&setup.consumer)
            .await
            .expect("per aggregate");
    for (aggregate, count, max_seq) in &per_aggregate {
        assert_eq!(count, max_seq, "{aggregate}: sequence numbers without gaps");
    }
    let inbox: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_inbox")
        .fetch_one(&setup.consumer)
        .await
        .expect("inbox");
    assert_eq!(inbox, total);
}

#[tokio::test]
async fn a_duplicate_delivery_is_processed_once() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let mut published = Vec::new();
    for n in 1..=10 {
        published.push(setup.publish(&format!("w{}", n % 3), n).await);
    }
    let consumer = ensured(recorder(&setup, "dupes")).await;
    let first = drain(&consumer, DRAIN).await;
    assert_eq!((first.delivered, first.duplicates), (10, 0));
    let state = setup.recorded_state().await;
    let end = consumer.position().await.expect("position");

    // Deliver everything again: every event is recognised by its ID and skipped. (A second
    // processing would fail on `seen`'s primary key and the drain would panic.)
    let mut tx = setup.consumer.begin().await.expect("begin");
    outbox::replay_from(&mut tx, "dupes", Position::START, Replay::SkipProcessed)
        .await
        .expect("replay");
    tx.commit().await.expect("commit");
    let again = drain(&consumer, DRAIN).await;
    assert_eq!((again.delivered, again.duplicates), (0, 10));
    assert_eq!(setup.recorded_state().await, state);
    assert_eq!(setup.seen_count().await, 10);
    assert_eq!(consumer.position().await.expect("position"), end);

    // The same from the middle of the log, including the event at the replay point.
    let fifth = position_of(&setup, &published[4]).await;
    let mut tx = setup.consumer.begin().await.expect("begin");
    outbox::replay_from(&mut tx, "dupes", fifth.just_before(), Replay::SkipProcessed)
        .await
        .expect("replay");
    tx.commit().await.expect("commit");
    let middle = drain(&consumer, DRAIN).await;
    assert_eq!((middle.delivered, middle.duplicates), (0, 6));
    assert_eq!(setup.recorded_state().await, state);
}

#[tokio::test]
async fn replaying_from_a_position_rebuilds_the_same_state() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    for n in 1..=30 {
        setup.publish(&format!("w{}", n % 4), n * 7).await;
    }
    let consumer = ensured(recorder(&setup, "rebuild")).await;
    assert_eq!(drain(&consumer, DRAIN).await.delivered, 30);
    let first = setup.recorded_state().await;
    assert_eq!(first.len(), 4);

    // Rebuild: replay the whole log and clear the consumer's state in one transaction, the
    // replay first so its lock is held before any table a delivery writes.
    let mut tx = setup.consumer.begin().await.expect("begin");
    outbox::replay_from(&mut tx, "rebuild", Position::START, Replay::Reprocess)
        .await
        .expect("replay");
    sqlx::raw_sql("DELETE FROM seen; DELETE FROM aggregate_state")
        .execute(&mut *tx)
        .await
        .expect("clear the state");
    tx.commit().await.expect("commit");
    assert!(setup.recorded_state().await.is_empty());
    let rebuilt = drain(&consumer, DRAIN).await;
    assert_eq!((rebuilt.delivered, rebuilt.duplicates), (30, 0));
    assert_eq!(
        setup.recorded_state().await,
        first,
        "same state after replay"
    );
    assert_eq!(setup.violations().await, 0);

    // A replay of an unknown subscription is refused.
    let mut tx = setup.consumer.begin().await.expect("begin");
    let error = outbox::replay_from(&mut tx, "nobody", Position::START, Replay::Reprocess)
        .await
        .expect_err("unknown subscription");
    assert!(matches!(error, ConsumerError::UnknownSubscription(_)));
}

/// Fails `n == 2` twice with a retry, fails `n == 3` permanently, records the rest.
struct Flaky {
    inner: Recorder,
    attempts: AtomicU32,
}

impl Handler for Flaky {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event: Counted = delivery.envelope.decode()?;
        // Writes before the failure must roll back with it.
        self.inner.handle(tx, delivery).await?;
        match event.n {
            2 if self.attempts.fetch_add(1, Ordering::SeqCst) < 2 => {
                Err(HandlerError::retry("dependency unavailable"))
            }
            3 => Err(HandlerError::permanent("event rejected by the test")),
            _ => Ok(()),
        }
    }
}

#[tokio::test]
async fn failures_are_retried_in_place_or_dead_lettered() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let mut published = Vec::new();
    for n in 1..=4 {
        published.push(setup.publish(&format!("a{n}"), n).await);
    }
    let consumer = setup.subscribe(
        ConsumerConfig::new("flaky", &setup.producer_schema).expect("config"),
        Flaky {
            inner: Recorder {
                subscription: "flaky".to_owned(),
            },
            attempts: AtomicU32::new(0),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");

    // Poll until everything is through; other tests' transactions can delay visibility, so
    // the number of polls varies, but the outcomes may not.
    let (mut retries, mut delivered, mut dead_lettered) = (0, 0, 0);
    let deadline = std::time::Instant::now() + DRAIN;
    while delivered + dead_lettered < 4 {
        assert!(
            std::time::Instant::now() < deadline,
            "not all events handled"
        );
        let progress = consumer.poll_once().await.expect("poll");
        delivered += progress.delivered;
        dead_lettered += progress.dead_lettered;
        if let Outcome::Retry { event_id, error } = progress.outcome {
            retries += 1;
            assert_eq!(event_id, published[1].event_id);
            assert_eq!(error.reason(), "dependency unavailable");
            assert_eq!(
                seen_in_order(&setup).await,
                [1],
                "the failed write rolled back"
            );
            assert_eq!(
                consumer.position().await.expect("position"),
                position_of(&setup, &published[0]).await,
                "the failed event stays first in line"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((retries, delivered, dead_lettered), (2, 3, 1));
    assert_eq!(seen_in_order(&setup).await, [1, 2, 4]);

    let dead: (String, String) =
        sqlx::query_as("SELECT event_id::text, reason FROM outbox_dead_letter")
            .fetch_one(&setup.consumer)
            .await
            .expect("dead letter");
    assert_eq!(
        dead,
        (
            published[2].event_id.to_string(),
            "event rejected by the test".to_owned()
        )
    );
}

/// Sets a transaction-local tenant in `begin`, as decision 29's helper will, and checks in
/// `handle` that it is still set: both run in the delivery's transaction.
struct TenantScoped;

impl Handler for TenantScoped {
    async fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let tenant = delivery
            .tenant()
            .ok_or_else(|| HandlerError::permanent("tenant event without a tenant"))?;
        sqlx::query("SELECT set_config('outbox_test.tenant', $1, true)")
            .bind(tenant.to_string())
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let current: String = sqlx::query_scalar("SELECT current_setting('outbox_test.tenant')")
            .fetch_one(&mut **tx)
            .await?;
        let expected = delivery.tenant().map(|tenant| tenant.to_string());
        if Some(current) == expected {
            Ok(())
        } else {
            Err(HandlerError::permanent("tenant setting lost"))
        }
    }
}

#[tokio::test]
async fn begin_runs_in_the_delivery_transaction() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    for _ in 0..3 {
        let mut tx = setup.producer.begin().await.expect("begin");
        outbox::publish(
            &mut tx,
            &Aggregate::new("tenant", "t").expect("aggregate"),
            &Counted { n: 1 },
            Some(contract::TenantId::generate()),
        )
        .await
        .expect("publish");
        tx.commit().await.expect("commit");
    }
    let consumer = setup.subscribe(
        ConsumerConfig::new("tenant-scoped", &setup.producer_schema).expect("config"),
        TenantScoped,
    );
    consumer.ensure_subscription().await.expect("subscription");
    let progress = drain(&consumer, DRAIN).await;
    assert_eq!((progress.delivered, progress.dead_lettered), (3, 0));
}

#[tokio::test]
async fn lag_counts_committed_events_not_yet_processed() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let consumer = ensured(recorder(&setup, "lagging")).await;
    assert_eq!(consumer.lag(), None, "nothing measured yet");
    let empty = consumer.measure_lag().await.expect("lag");
    assert_eq!((empty.events, empty.oldest_age), (0, None));

    for n in 1..=5 {
        setup.publish("w", n).await;
    }
    // An open transaction's event is not counted.
    let mut open = setup.producer.begin().await.expect("begin");
    outbox::publish(
        &mut open,
        &Aggregate::new("widget", "open").expect("aggregate"),
        &Counted { n: 6 },
        None,
    )
    .await
    .expect("publish");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let behind = consumer.measure_lag().await.expect("lag");
    assert_eq!(behind.events, 5);
    assert!(
        behind.oldest_age.expect("an age") >= Duration::from_millis(50),
        "{behind:?}"
    );
    assert_eq!(consumer.lag(), Some(behind));
    open.rollback().await.expect("rollback");

    drain(&consumer, DRAIN).await;
    let caught_up = consumer.measure_lag().await.expect("lag");
    assert_eq!((caught_up.events, caught_up.oldest_age), (0, None));
}

#[tokio::test]
async fn a_new_subscription_can_start_after_the_history() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let mut history = Vec::new();
    for n in 1..=3 {
        history.push((n, setup.publish(&format!("w{n}"), n).await));
    }
    let consumer = setup.subscribe(
        ConsumerConfig::new("from-now", &setup.producer_schema)
            .expect("config")
            .start_from(StartFrom::Latest),
        Recorder {
            subscription: "from-now".to_owned(),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    let start = consumer.position().await.expect("position");
    assert!(start > Position::START);
    setup.publish("w4", 4).await;

    // The start is the read horizon: events below it are skipped, and nothing committed after
    // the subscription was created can sort below it. A transaction still running elsewhere
    // (other tests share the database) holds the horizon back, so a few events committed just
    // before the creation may still be delivered.
    let mut expected = Vec::new();
    for (n, published) in &history {
        if position_of(&setup, published).await > start {
            expected.push(*n);
        }
    }
    expected.push(4);
    drain(&consumer, DRAIN).await;
    assert_eq!(seen_in_order(&setup).await, expected);
}

#[tokio::test]
async fn a_subscription_cannot_switch_producers() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let other = database.setup().await;
    ensured(recorder(&setup, "pinned")).await;
    let moved = setup.subscribe(
        ConsumerConfig::new("pinned", &other.producer_schema).expect("config"),
        Recorder {
            subscription: "pinned".to_owned(),
        },
    );
    let error = moved
        .ensure_subscription()
        .await
        .expect_err("another producer");
    assert!(
        matches!(error, ConsumerError::SourceMismatch { .. }),
        "{error}"
    );
    assert!(!error.is_retryable());
    let error = moved
        .run(CancellationToken::new())
        .await
        .expect_err("run refuses to start");
    assert!(matches!(error, ConsumerError::SourceMismatch { .. }));
}

/// Runs `statement` on `pool` and expects SQLSTATE 42501 (insufficient privilege).
async fn refused(pool: &sqlx::PgPool, statement: String) {
    let error = sqlx::raw_sql(AssertSqlSafe(statement.clone()))
        .execute(pool)
        .await
        .expect_err(&statement);
    let sqlx::Error::Database(error) = error else {
        panic!("{statement}: {error}");
    };
    assert_eq!(error.code().as_deref(), Some("42501"), "{statement}");
}

#[tokio::test]
async fn services_need_only_the_documented_grants() {
    let Some(database) = database().await else {
        return;
    };
    // The setup grants only what the README lists: each service uses its own schema's tables,
    // and only the consumer's platform role reads the producer's `outbox_event`.
    let setup = database.setup().await;
    let ps = &setup.producer_schema;

    // The producer's service role writes a tenant event with its business change, in a tenant
    // transaction; its platform role writes a platform-wide event.
    let (_, producer) = TenantDb::connect_lazy_with_background(
        database.postgres.options(&setup.producer_role),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    let mut tx = producer
        .begin(tenant(TENANT_A), "publish-test")
        .await
        .expect("begin");
    sqlx::query("INSERT INTO widget (id, name) VALUES ('w', 'pump')")
        .execute(&mut *tx)
        .await
        .expect("business write as the producer role");
    let event_tenant = tx.tenant_id();
    outbox::publish(
        tx.transaction(),
        &Aggregate::new("widget", "w").expect("aggregate"),
        &Counted { n: 1 },
        Some(event_tenant),
    )
    .await
    .expect("publish as the producer role");
    tx.commit().await.expect("commit");
    let producer_platform = PlatformDb::connect_lazy(
        database.postgres.options(&setup.producer_platform),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assets));
    let mut tx = producer_platform
        .begin(actor, "publish-test")
        .await
        .expect("begin");
    outbox::publish(
        tx.transaction(),
        &Aggregate::new("catalog", "c").expect("aggregate"),
        &Counted { n: 2 },
        None,
    )
    .await
    .expect("publish a platform-wide event as the platform role");
    tx.commit().await.expect("commit");

    let consumer = ensured(recorder(&setup, "least-privilege")).await;
    assert_eq!(drain(&consumer, DRAIN).await.delivered, 2);
    assert_eq!(consumer.measure_lag().await.expect("lag").events, 0);

    // The consumer's platform role reads the log and nothing else of the producer.
    let platform = database.pool_as(ps, &setup.consumer_platform).await;
    for statement in [
        format!("SELECT count(*) FROM {ps}.widget"),
        format!("SELECT count(*) FROM {ps}.outbox_aggregate"),
        format!("DELETE FROM {ps}.outbox_event"),
    ] {
        refused(&platform, statement).await;
    }
    // The consumer's service role cannot reach the producer at all.
    let service = database.pool_as(ps, &setup.consumer_role).await;
    refused(&service, format!("SELECT count(*) FROM {ps}.outbox_event")).await;
    platform.close().await;
    service.close().await;
}
