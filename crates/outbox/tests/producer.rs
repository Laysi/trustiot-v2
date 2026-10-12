//! Writing events in the caller's transaction, against a real Postgres with TLS (a throwaway
//! container; skipped without podman or docker unless `CI` is set).

mod support;

use std::time::Duration;

use contract::EventEnvelope;
use outbox::{Aggregate, ConsumerConfig, PublishError};
use support::{Counted, Recorder, database, drain};

#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_event() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let widget = Aggregate::new("widget", "w-1").expect("aggregate");

    // Explicit rollback after the business write and the event.
    let mut tx = setup.producer.begin().await.expect("begin");
    sqlx::query("INSERT INTO widget (id, name) VALUES ('w-1', 'pump')")
        .execute(&mut *tx)
        .await
        .expect("business write");
    outbox::publish(&mut tx, &widget, &Counted { n: 1 }, None)
        .await
        .expect("publish");
    tx.rollback().await.expect("rollback");

    // A transaction dropped without commit, as on an error path or a panic.
    {
        let mut tx = setup.producer.begin().await.expect("begin");
        outbox::publish(&mut tx, &widget, &Counted { n: 2 }, None)
            .await
            .expect("publish");
    }

    // A transaction that fails after the event: the commit cannot succeed.
    let mut tx = setup.producer.begin().await.expect("begin");
    outbox::publish(&mut tx, &widget, &Counted { n: 3 }, None)
        .await
        .expect("publish");
    let failed = sqlx::query("INSERT INTO widget (id, name) VALUES ('w-1', NULL)")
        .execute(&mut *tx)
        .await;
    assert!(failed.is_err(), "the NOT NULL violation must fail");
    // Postgres answers COMMIT of an aborted transaction with ROLLBACK, not an error.
    let _ = tx.commit().await;

    assert_eq!(
        setup.event_count().await,
        0,
        "no event may survive a rollback"
    );
    let widgets: i64 = sqlx::query_scalar("SELECT count(*) FROM widget")
        .fetch_one(&setup.producer)
        .await
        .expect("count widgets");
    assert_eq!(widgets, 0);
    let aggregates: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_aggregate")
        .fetch_one(&setup.producer)
        .await
        .expect("count aggregates");
    assert_eq!(aggregates, 0, "the sequence rolls back with the event");

    // A consumer finds nothing; then a committed write is delivered, numbered 1.
    let consumer = setup.subscribe(
        ConsumerConfig::new("rollback", &setup.producer_schema).expect("config"),
        Recorder {
            subscription: "rollback".to_owned(),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    assert_eq!(drain(&consumer, Duration::from_secs(10)).await.delivered, 0);

    let mut tx = setup.producer.begin().await.expect("begin");
    sqlx::query("INSERT INTO widget (id, name) VALUES ('w-1', 'pump')")
        .execute(&mut *tx)
        .await
        .expect("business write");
    let published = outbox::publish(&mut tx, &widget, &Counted { n: 4 }, None)
        .await
        .expect("publish");
    tx.commit().await.expect("commit");
    assert_eq!(published.aggregate_seq, 1);
    assert_eq!(drain(&consumer, Duration::from_secs(10)).await.delivered, 1);
    let seen: (String, i64) = sqlx::query_as("SELECT event_id::text, n FROM seen")
        .fetch_one(&setup.consumer)
        .await
        .expect("the delivered event");
    assert_eq!(seen, (published.event_id.to_string(), 4));
}

#[tokio::test]
async fn events_are_numbered_per_aggregate() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let a = Aggregate::new("widget", "a").expect("aggregate");
    let b = Aggregate::new("widget", "b").expect("aggregate");

    let mut tx = setup.producer.begin().await.expect("begin");
    let first = outbox::publish(&mut tx, &a, &Counted { n: 1 }, None)
        .await
        .expect("publish");
    let second = outbox::publish(&mut tx, &a, &Counted { n: 2 }, None)
        .await
        .expect("publish");
    let other = outbox::publish(&mut tx, &b, &Counted { n: 3 }, None)
        .await
        .expect("publish");
    tx.commit().await.expect("commit");
    let third = setup.publish("a", 4).await;
    assert_eq!(
        [
            first.aggregate_seq,
            second.aggregate_seq,
            other.aggregate_seq,
            third.aggregate_seq
        ],
        [1, 2, 1, 3]
    );

    let mut conn = setup.producer.acquire().await.expect("connection");
    assert_eq!(
        outbox::aggregate_version(&mut conn, &a)
            .await
            .expect("version"),
        Some(3)
    );
    assert_eq!(
        outbox::aggregate_version(&mut conn, &b)
            .await
            .expect("version"),
        Some(1)
    );
    let unknown = Aggregate::new("widget", "never").expect("aggregate");
    assert_eq!(
        outbox::aggregate_version(&mut conn, &unknown)
            .await
            .expect("version"),
        None
    );

    // The stored row carries the whole envelope and the tenant column for row-level security.
    let (event_type, envelope, tenant): (String, String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT event_type, envelope::text, tenant_id FROM outbox_event WHERE event_id = $1",
    )
    .bind(first.event_id.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .expect("stored event");
    assert_eq!(event_type, "example.counted");
    assert_eq!(tenant, None);
    let envelope: EventEnvelope = serde_json::from_str(&envelope).expect("an envelope");
    assert_eq!(envelope.id, first.event_id);
    assert_eq!(
        envelope.decode::<Counted>().expect("decode"),
        Counted { n: 1 }
    );
}

#[tokio::test]
async fn an_envelope_is_written_once() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let widget = Aggregate::new("widget", "w").expect("aggregate");
    let tenant = contract::TenantId::generate();
    let envelope = EventEnvelope::new(&Counted { n: 1 }, Some(tenant)).expect("envelope");

    let mut tx = setup.producer.begin().await.expect("begin");
    outbox::write(&mut tx, &widget, &envelope)
        .await
        .expect("first write");
    tx.commit().await.expect("commit");

    let mut tx = setup.producer.begin().await.expect("begin");
    let error = outbox::write(&mut tx, &widget, &envelope)
        .await
        .expect_err("the same event ID twice");
    let PublishError::Database(sqlx::Error::Database(error)) = error else {
        panic!("expected a database error, got {error:?}");
    };
    assert_eq!(error.code().as_deref(), Some("23505"));
    drop(tx);

    let stored: Option<uuid::Uuid> = sqlx::query_scalar("SELECT tenant_id FROM outbox_event")
        .fetch_one(&setup.producer)
        .await
        .expect("stored tenant");
    assert_eq!(stored, Some(*tenant.as_uuid()));
}
