//! Notification mode against a real Postgres with TLS (a throwaway container; skipped without
//! podman or docker unless `CI` is set): consumers read the producer's current state, and a
//! late, replayed, or racing old reading never overwrites a newer one.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use outbox::notify::{store_if_newer, stored_version};
use outbox::{Aggregate, ConsumerConfig, Delivery, Handler, HandlerError, Position, Replay};
use sqlx::{PgPool, Postgres, Transaction};
use support::{Counted, Setup, database, drain};

const SCOPE: &str = "widget_copy";

/// Renames a widget and writes the notification in the same transaction.
async fn rename(setup: &Setup, id: &str, name: &str) -> i64 {
    let mut tx = setup.producer.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO widget (id, name) VALUES ($1, $2) \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(id)
    .bind(name)
    .execute(&mut *tx)
    .await
    .expect("rename");
    let published = outbox::publish(
        &mut tx,
        &Aggregate::new("widget", id).expect("aggregate"),
        &Counted { n: 0 },
        None,
    )
    .await
    .expect("publish");
    tx.commit().await.expect("commit");
    published.aggregate_seq
}

/// What the producer's internal API would return: the current name and the aggregate's
/// version, read in one snapshot.
async fn current(source: &PgPool, id: &str) -> Option<(String, i64)> {
    let mut tx = source
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .expect("begin a snapshot");
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM widget WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .expect("read the widget");
    let version = outbox::aggregate_version(&mut tx, &Aggregate::new("widget", id).expect("id"))
        .await
        .expect("read the version");
    tx.commit().await.expect("end the snapshot");
    name.zip(version)
}

async fn create_copy_table(setup: &Setup) {
    sqlx::raw_sql(
        "CREATE TABLE widget_copy (id text PRIMARY KEY, name text NOT NULL, \
         version bigint NOT NULL, writes integer NOT NULL)",
    )
    .execute(&setup.consumer)
    .await
    .expect("copy table");
}

/// Stores a reading through the version guard; returns whether it was written.
async fn store(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    (name, version): &(String, i64),
) -> bool {
    let aggregate = Aggregate::new("widget", id).expect("aggregate");
    let newer = store_if_newer(tx, SCOPE, &aggregate, *version)
        .await
        .expect("version guard");
    if newer {
        sqlx::query(
            "INSERT INTO widget_copy (id, name, version, writes) VALUES ($1, $2, $3, 1) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, version = EXCLUDED.version, \
             writes = widget_copy.writes + 1",
        )
        .bind(id)
        .bind(name)
        .bind(version)
        .execute(&mut **tx)
        .await
        .expect("write the copy");
    }
    newer
}

async fn copy(setup: &Setup, id: &str) -> Option<(String, i64, i32)> {
    sqlx::query_as("SELECT name, version, writes FROM widget_copy WHERE id = $1")
        .bind(id)
        .fetch_optional(&setup.consumer)
        .await
        .expect("read the copy")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_reading_never_overwrites_a_newer_one() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    create_copy_table(&setup).await;
    rename(&setup, "w", "first").await;
    let old = current(&setup.producer, "w").await.expect("old reading");
    rename(&setup, "w", "second").await;
    let new = current(&setup.producer, "w").await.expect("new reading");
    assert_eq!((old.1, new.1), (1, 2));

    // The newer reading lands first; the older one arrives late and is ignored.
    let mut tx = setup.consumer.begin().await.expect("begin");
    assert!(store(&mut tx, "w", &new).await);
    tx.commit().await.expect("commit");
    let mut tx = setup.consumer.begin().await.expect("begin");
    assert!(
        !store(&mut tx, "w", &old).await,
        "an old reading is refused"
    );
    tx.commit().await.expect("commit");
    assert_eq!(copy(&setup, "w").await, Some(("second".to_owned(), 2, 1)));

    // Two replicas race: each holds its reading's transaction open while the other tries.
    for (first, second, id) in [
        (&old, &new, "race-old-first"),
        (&new, &old, "race-new-first"),
    ] {
        let mut holder = setup.consumer.begin().await.expect("begin");
        let first_written = store(&mut holder, id, first).await;
        assert!(first_written, "nothing stored yet");
        let pool = setup.consumer.clone();
        let (reading, id_owned) = (second.clone(), id.to_owned());
        let racer = tokio::spawn(async move {
            let mut tx = pool.begin().await.expect("begin");
            let written = store(&mut tx, &id_owned, &reading).await;
            tx.commit().await.expect("commit");
            written
        });
        // The racer waits on the version row until the holder commits.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!racer.is_finished(), "the racer must wait for the row lock");
        holder.commit().await.expect("commit");
        let second_written = racer.await.expect("racer");
        assert_eq!(second_written, second.1 > first.1, "{id}");
        assert_eq!(
            copy(&setup, id)
                .await
                .map(|(name, version, _)| (name, version)),
            Some(("second".to_owned(), 2)),
            "{id}: the newest reading wins"
        );
    }
}

/// Keeps a copy of each widget by reading the producer's current state.
struct Mirror {
    source: PgPool,
    fetches: AtomicU32,
}

impl Handler for Mirror {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        // Cheap skip: the stored copy already reflects this event or a later one.
        if stored_version(tx, SCOPE, &delivery.aggregate)
            .await?
            .is_some_and(|stored| stored >= delivery.aggregate_seq)
        {
            return Ok(());
        }
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let Some(reading) = current(&self.source, delivery.aggregate.id()).await else {
            return Ok(());
        };
        store(tx, delivery.aggregate.id(), &reading).await;
        Ok(())
    }
}

#[tokio::test]
async fn late_and_replayed_events_converge_on_the_current_state() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    create_copy_table(&setup).await;
    for name in ["a", "b", "c"] {
        rename(&setup, "w", name).await;
    }
    let consumer = setup.subscribe(
        ConsumerConfig::new("mirror", &setup.producer_schema).expect("config"),
        Mirror {
            source: setup.producer.clone(),
            fetches: AtomicU32::new(0),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");

    // The first event arrives after the state already moved on: it stores the current state,
    // and the two later events find nothing newer.
    assert_eq!(drain(&consumer, Duration::from_secs(30)).await.delivered, 3);
    assert_eq!(copy(&setup, "w").await, Some(("c".to_owned(), 3, 1)));
    assert_eq!(consumer.handler().fetches.load(Ordering::SeqCst), 1);

    rename(&setup, "w", "d").await;
    assert_eq!(drain(&consumer, Duration::from_secs(30)).await.delivered, 1);
    assert_eq!(copy(&setup, "w").await, Some(("d".to_owned(), 4, 2)));

    // Replaying every old event changes nothing.
    let mut tx = setup.consumer.begin().await.expect("begin");
    outbox::replay_from(&mut tx, "mirror", Position::START, Replay::Reprocess)
        .await
        .expect("replay");
    tx.commit().await.expect("commit");
    assert_eq!(drain(&consumer, Duration::from_secs(30)).await.delivered, 4);
    assert_eq!(copy(&setup, "w").await, Some(("d".to_owned(), 4, 2)));
    assert_eq!(consumer.handler().fetches.load(Ordering::SeqCst), 2);

    // Even with the guard's memory gone (a rebuild that kept the copy), old events re-read
    // the current state and store the same values.
    let mut tx = setup.consumer.begin().await.expect("begin");
    outbox::replay_from(&mut tx, "mirror", Position::START, Replay::Reprocess)
        .await
        .expect("replay");
    sqlx::query("DELETE FROM outbox_version")
        .execute(&mut *tx)
        .await
        .expect("forget versions");
    tx.commit().await.expect("commit");
    drain(&consumer, Duration::from_secs(30)).await;
    let (name, version, _) = copy(&setup, "w").await.expect("copy");
    assert_eq!((name.as_str(), version), ("d", 4));
}
