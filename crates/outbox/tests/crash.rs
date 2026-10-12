//! Crashes of a real `outbox-example` process (SIGKILL: no shutdown code runs), against a real
//! Postgres with TLS (a throwaway container; skipped without podman or docker unless `CI` is
//! set). The consumer reads as its platform role and processes as its service role in the
//! event's tenant (decision 29).

mod support;

use std::time::Duration;

use support::{Example, TENANT_A, TENANT_B, database, eventually};

/// `(name, applied, tenant)` of the consumer's copy of `id`; `tenant` is the tenant the
/// delivery's transaction was in.
async fn copied(pool: &sqlx::PgPool, id: &str) -> Option<(String, i32, Option<String>)> {
    sqlx::query_as("SELECT name, applied, tenant FROM widget_name WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("read the copy")
}

#[tokio::test]
async fn an_event_committed_before_the_producer_is_killed_is_delivered_after_restart() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;

    // The consumer in this process would only start processing in an hour: the event is
    // committed but certainly not delivered when the process dies.
    let example = Example::start(
        &database,
        &setup,
        &[("TRUSTIOT__CONSUMER__PROCESS_AFTER", "1h")],
    );
    let (status, body) = example.rename("w-1", "pump A", Some(TENANT_A));
    assert_eq!(status, 200, "{body}");
    let event_id = body["event_id"].as_str().expect("event id").to_owned();
    example.kill();

    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_event WHERE event_id::text = $1")
            .bind(&event_id)
            .fetch_one(&setup.producer)
            .await
            .expect("count");
    assert_eq!(stored, 1, "the committed event survives the crash");
    assert_eq!(
        copied(&setup.consumer, "w-1").await,
        None,
        "not delivered yet"
    );

    // Restarted, the service delivers it.
    let _restarted = Example::start(&database, &setup, &[]);
    let copy = eventually(Duration::from_secs(30), "the event delivered", || async {
        copied(&setup.consumer, "w-1").await
    })
    .await;
    assert_eq!(copy, ("pump A".to_owned(), 1, Some(TENANT_A.to_owned())));
    let inbox: Vec<String> = sqlx::query_scalar("SELECT event_id::text FROM outbox_inbox")
        .fetch_all(&setup.consumer)
        .await
        .expect("inbox");
    assert_eq!(inbox, [event_id]);
}

#[tokio::test]
async fn a_consumer_killed_mid_delivery_processes_the_event_once_after_restart() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;

    // The handler writes the copy, then keeps the transaction open for an hour.
    let example = Example::start(&database, &setup, &[("TRUSTIOT__CONSUMER__HOLD", "1h")]);
    let (status, body) = example.rename("w-2", "valve", Some(TENANT_B));
    assert_eq!(status, 200, "{body}");
    let event_id = body["event_id"].as_str().expect("event id").to_owned();
    let holding = example
        .stdout
        .wait_for_message(
            "holding the delivery transaction open",
            Duration::from_secs(30),
        )
        .expect("the handler ran");
    assert_eq!(holding["fields"]["event_id"], event_id.as_str());
    example.kill();

    // The open transaction died with the connection: no copy, no inbox entry, no progress.
    eventually(
        Duration::from_secs(30),
        "the server to roll back",
        || async {
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE state LIKE 'idle in transaction%' \
             AND query LIKE '%widget_name%'",
            )
            .fetch_one(&setup.consumer)
            .await
            .expect("activity");
            (open == 0).then_some(())
        },
    )
    .await;
    assert_eq!(copied(&setup.consumer, "w-2").await, None);
    let inbox: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_inbox")
        .fetch_one(&setup.consumer)
        .await
        .expect("inbox");
    assert_eq!(inbox, 0);

    // Restarted without the hold: delivered again, applied exactly once.
    let restarted = Example::start(&database, &setup, &[]);
    let copy = eventually(Duration::from_secs(30), "the event delivered", || async {
        copied(&setup.consumer, "w-2").await
    })
    .await;
    assert_eq!(copy, ("valve".to_owned(), 1, Some(TENANT_B.to_owned())));
    // Later polls do not apply it again.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        copied(&setup.consumer, "w-2").await,
        Some(("valve".to_owned(), 1, Some(TENANT_B.to_owned())))
    );
    drop(restarted);
}
