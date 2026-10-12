//! Independent verification of Laysi/trustiot-v2#51: the advisory-lock tool under failure.
//! Uses `TEST_DATABASE_URL` when set, otherwise a throwaway Postgres container shared by the
//! tests, each with a database of its own; skipped without either unless `CI` is set (like
//! `job_lock.rs`).
//!
//! Adds to `job_lock.rs`: a stress run in which many replicas contend for one job and the number
//! of concurrent runs is counted, the lock holder's backend killed mid-job (the job stops, the
//! lock does not leak, another replica takes over), a panicking job, and a job future dropped by
//! a timeout.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use service_kit::job_lock::{JobLock, JobLockError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use support::postgres::test_database;

async fn replica(url: &str, connections: u32) -> PgPool {
    PgPoolOptions::new()
        .max_connections(connections)
        .connect(url)
        .await
        .expect("connect a replica's pool")
}

/// Process ids of the backends that hold an advisory lock with `key`.
async fn holders(admin: &PgPool, key: i64) -> Vec<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(key)
    .fetch_all(admin)
    .await
    .expect("query pg_locks")
}

async fn any_advisory_locks(admin: &PgPool) -> i64 {
    // Advisory locks belong to a database, and the tests of a binary share one server.
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(admin)
    .await
    .expect("count advisory locks")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_replicas_contending_never_run_a_job_twice_at_once() {
    let Some(database) = test_database().await else {
        return;
    };
    let admin = replica(&database.url, 2).await;
    let running = Arc::new(AtomicUsize::new(0));
    let highest = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = replica(&database.url, 2).await;
        let lock =
            JobLock::new(pool, "verify", "stress").with_heartbeat(Duration::from_millis(100));
        let (running, highest, runs, skipped) = (
            Arc::clone(&running),
            Arc::clone(&highest),
            Arc::clone(&runs),
            Arc::clone(&skipped),
        );
        tasks.push(tokio::spawn(async move {
            for _ in 0..60 {
                let outcome = lock
                    .run_exclusive(|| async {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        highest.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .expect("lock");
                match outcome {
                    Some(()) => runs.fetch_add(1, Ordering::SeqCst),
                    None => skipped.fetch_add(1, Ordering::SeqCst),
                };
            }
        }));
    }
    for task in tasks {
        task.await.expect("replica task");
    }
    eprintln!(
        "8 replicas x 60 rounds: {} runs, {} skipped, highest concurrency {}",
        runs.load(Ordering::SeqCst),
        skipped.load(Ordering::SeqCst),
        highest.load(Ordering::SeqCst)
    );
    assert_eq!(
        highest.load(Ordering::SeqCst),
        1,
        "two replicas ran at once"
    );
    assert!(runs.load(Ordering::SeqCst) > 0);
    assert!(skipped.load(Ordering::SeqCst) > 0, "no contention happened");
    assert_eq!(any_advisory_locks(&admin).await, 0, "a lock leaked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_lock_session_stops_the_job_reports_lost_and_leaks_nothing() {
    let Some(database) = test_database().await else {
        return;
    };
    let admin = replica(&database.url, 2).await;
    let holder = JobLock::new(replica(&database.url, 2).await, "verify", "killed")
        .with_heartbeat(Duration::from_millis(200));
    let other = JobLock::new(replica(&database.url, 2).await, "verify", "killed");
    let key = holder.key();

    let ticks = Arc::new(AtomicUsize::new(0));
    let job_ticks = Arc::clone(&ticks);
    let run = tokio::spawn(async move {
        holder
            .run_exclusive(|| async move {
                loop {
                    job_ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
    });
    // Wait for the lock, then kill the holder's session the way a failover or an admin would.
    let started = Instant::now();
    let pid = loop {
        if let Some(pid) = holders(&admin, key).await.first().copied() {
            break pid;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "never locked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // A second replica is refused while the holder lives.
    assert!(other.try_acquire().await.expect("try").is_none());
    let killed = Instant::now();
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&admin)
        .await
        .expect("terminate the holder's backend");

    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the run ends after its session died")
        .expect("no panic");
    let stopped_after = killed.elapsed();
    assert!(
        matches!(result, Err(JobLockError::Lost { .. })),
        "{result:?}"
    );
    // The job really stopped: its counter stops moving.
    let frozen = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), frozen, "the job kept running");
    // Detected within about one heartbeat (200 ms), with generous slack for a loaded machine.
    assert!(stopped_after < Duration::from_secs(2), "{stopped_after:?}");
    eprintln!("killed session detected after {stopped_after:?} (heartbeat 200 ms)");

    assert!(holders(&admin, key).await.is_empty(), "the lock leaked");
    let guard = other
        .try_acquire()
        .await
        .expect("try")
        .expect("another replica takes over");
    guard.release().await.expect("release");
    assert_eq!(any_advisory_locks(&admin).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicking_job_and_a_dropped_run_release_the_lock() {
    let Some(database) = test_database().await else {
        return;
    };
    let admin = replica(&database.url, 2).await;
    let lock = JobLock::new(replica(&database.url, 2).await, "verify", "panic");
    let key = lock.key();

    let panicking = lock.clone();
    let joined = tokio::spawn(async move {
        let _ = panicking
            .run_exclusive(|| async { panic!("job failed on purpose") })
            .await;
    })
    .await;
    assert!(joined.is_err(), "the panic propagates");
    let started = Instant::now();
    while !holders(&admin, key).await.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "lock leaked after a panic"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A run dropped by a timeout (cancelled future) while the job holds the lock.
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        lock.run_exclusive(std::future::pending::<()>),
    )
    .await;
    assert!(result.is_err(), "the timeout cancelled the run");
    let started = Instant::now();
    while !holders(&admin, key).await.is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "lock leaked after a cancel"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The replica still works: the pool replaced the closed connections.
    let guard = lock
        .try_acquire()
        .await
        .expect("try")
        .expect("lock is free");
    guard.release().await.expect("release");
    assert_eq!(any_advisory_locks(&admin).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releasing_a_guard_whose_session_died_reports_an_error_not_success() {
    let Some(database) = test_database().await else {
        return;
    };
    let admin = replica(&database.url, 2).await;
    let lock = JobLock::new(replica(&database.url, 2).await, "verify", "dead-guard");
    let key = lock.key();
    let guard = lock.try_acquire().await.expect("try").expect("lock");
    let pid = holders(&admin, key).await[0];
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&admin)
        .await
        .expect("terminate");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        guard.release().await.is_err(),
        "releasing a dead session must not claim success"
    );
    assert_eq!(any_advisory_locks(&admin).await, 0);
}
