//! Two migration runs never run at the same time (issue acceptance: "兩個 migration Job 同時啟動時
//! 只有一個執行"). The chart's Job runs one pod at a time; this is the lock behind it, which also
//! covers two Jobs of different release revisions.
//!
//! Needs podman or docker; skipped without one unless `CI` is set. One test: roles are
//! cluster-wide, and this binary has its own Postgres.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use migrate::Error;
use migrate::registry::PRODUCT;
use sqlx::Connection;
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, random_hex};

const CREATE: &str = "CREATE DATABASE migrate_concurrent OWNER trustiot_owner";
const HOLD_LOCK: &str = "SELECT pg_advisory_lock($1, $2)";
const RELEASE_LOCK: &str = "SELECT pg_advisory_unlock($1, $2)";
const BOOKKEEPING_EXISTS: &str = "SELECT to_regclass('trustiot_migrate.applied') IS NOT NULL";

/// How long the test holds the lock while a run waits; the run's own bound is far longer.
const HELD_FOR: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread")]
async fn only_one_run_at_a_time() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_concurrent").await;
    db.set_password_file("identity_app", &random_hex());
    let (class, object) = migrate::postgres::LOCK_KEY;

    // 1. While another session holds the lock, a run waits and gives up at its bound, having
    //    changed nothing.
    let mut holder = db.connect(MIGRATOR).await;
    sqlx::query(HOLD_LOCK)
        .bind(class)
        .bind(object)
        .execute(&mut holder)
        .await
        .expect("hold the migration lock");
    let mut short = db.config(MIGRATOR);
    short.wait.lock = Duration::from_secs(2);
    let error = migrate::run(&short, &PRODUCT)
        .await
        .expect_err("the lock is held");
    assert!(matches!(error, Error::LockTimeout(_)), "{error}");
    let mut check = db.connect(MIGRATOR).await;
    let exists: bool = sqlx::query_scalar(BOOKKEEPING_EXISTS)
        .fetch_one(&mut check)
        .await
        .expect("look for the bookkeeping table");
    assert!(
        !exists,
        "a run that did not get the lock must not touch the database"
    );

    // 2. A run that starts while the lock is held finishes only after it is released.
    let released = Arc::new(AtomicBool::new(false));
    let waiting = {
        let config = db.config(MIGRATOR);
        let released = Arc::clone(&released);
        tokio::spawn(async move {
            let report = migrate::run(&config, &PRODUCT).await;
            (report, released.load(Ordering::SeqCst))
        })
    };
    tokio::time::sleep(HELD_FOR).await;
    assert!(
        !waiting.is_finished(),
        "the run must wait while the lock is held"
    );
    released.store(true, Ordering::SeqCst);
    sqlx::query(RELEASE_LOCK)
        .bind(class)
        .bind(object)
        .execute(&mut holder)
        .await
        .expect("release the lock");
    let (report, after_release) = waiting.await.expect("join the waiting run");
    let report = report.expect("the run that waited");
    assert!(
        after_release,
        "the run finished before the lock was released"
    );
    assert!(
        !report.changes.is_empty(),
        "the first run that got the lock does the work"
    );
    holder.close().await.expect("close");

    // 3. Two runs started together: both succeed, and only one applies anything; the other
    //    waits for the lock and then finds everything in place. Run on a database whose
    //    bookkeeping is reset, so there is work to do.
    let mut owner = db.connect(MIGRATOR).await;
    sqlx::raw_sql("DELETE FROM trustiot_migrate.applied WHERE component = 'database'")
        .execute(&mut owner)
        .await
        .expect("forget database.sql, which can run again");
    owner.close().await.expect("close");
    let (one, two) = (db.config(MIGRATOR), db.config(MIGRATOR));
    let (first, second) = tokio::join!(migrate::run(&one, &PRODUCT), migrate::run(&two, &PRODUCT));
    let (first, second) = (first.expect("first run"), second.expect("second run"));
    let mut counts = [first.changes.len(), second.changes.len()];
    counts.sort_unstable();
    assert_eq!(
        counts,
        [0, 1],
        "exactly one run re-applies database.sql: {:?} / {:?}",
        first.changes,
        second.changes
    );
    check.close().await.expect("close");
}
