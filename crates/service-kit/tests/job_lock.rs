//! Job locks against a real Postgres. Uses `TEST_DATABASE_URL` when set; otherwise starts a
//! throwaway Postgres container with podman or docker; skipped without either unless `CI` is
//! set. Each "replica" is its own connection pool, as separate pods would have.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use service_kit::job_lock::{JobLock, JobLockError};
use service_kit::shutdown::Shutdown;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use support::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// Postgres for the tests; the version the platform runs is decided with the database
/// deployment, advisory locks behave the same in every supported release.
const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// A database for one test; the container, if any, is removed when dropped.
struct Database {
    url: String,
    _container: Option<Container>,
}

async fn database() -> Option<Database> {
    if let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return Some(Database {
            url,
            _container: None,
        });
    }
    let Some(runtime) = container_runtime() else {
        unavailable("TEST_DATABASE_URL, podman, or docker");
        return None;
    };
    let container = Container {
        runtime,
        name: unique_name("postgres"),
    };
    runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--name",
            &container.name,
            "--env",
            "POSTGRES_PASSWORD=service-kit-test",
            "--publish",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ],
    );
    let address = container.host_address("5432/tcp");
    let url = format!("postgres://postgres:service-kit-test@{address}/postgres");
    // The image runs initdb with networking off and then restarts; wait for the real server.
    let started = Instant::now();
    loop {
        if let Ok(pool) = PgPool::connect(&url).await
            && sqlx::query("SELECT 1").execute(&pool).await.is_ok()
        {
            pool.close().await;
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "Postgres did not start in time"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Some(Database {
        url,
        _container: Some(container),
    })
}

async fn replica(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect a replica's pool")
}

/// The backend holding the advisory lock `key`, if any.
async fn lock_holder(admin: &PgPool, key: i64) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
         AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(key)
    .fetch_optional(admin)
    .await
    .expect("query pg_locks")
}

async fn eventually_acquires(lock: &JobLock) {
    let started = Instant::now();
    loop {
        if let Some(guard) = lock.try_acquire().await.expect("try the lock") {
            guard.release().await.expect("release");
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the lock was never released"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_replica_runs_a_job_scheduled_on_both_at_once() {
    let Some(database) = database().await else {
        return;
    };
    let job = unique_name("simultaneous");
    let a = JobLock::new(replica(&database.url).await, "example-service", &job);
    let b = JobLock::new(replica(&database.url).await, "example-service", &job);
    let running = Arc::new(AtomicUsize::new(0));
    let mut runs_by_replica = [0_u32; 2];
    for _round in 0..10 {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        // How many replicas have finished their attempt in this round.
        let (finished, attempts) = tokio::sync::watch::channel(0_usize);
        let finished = Arc::new(finished);
        let attempt = |lock: JobLock| {
            let barrier = Arc::clone(&barrier);
            let running = Arc::clone(&running);
            let finished = Arc::clone(&finished);
            let mut attempts = attempts.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let outcome = lock
                    .run_exclusive(|| async move {
                        let concurrent = running.fetch_add(1, Ordering::SeqCst) + 1;
                        // Hold the lock until the other replica has made its attempt, so the
                        // attempts overlap however long either takes to reach the database. If
                        // both got the lock, neither attempt finishes and the wait times out.
                        let _ = tokio::time::timeout(
                            Duration::from_secs(10),
                            attempts.wait_for(|finished| *finished >= 1),
                        )
                        .await;
                        running.fetch_sub(1, Ordering::SeqCst);
                        concurrent
                    })
                    .await
                    .expect("the lock works");
                finished.send_modify(|finished| *finished += 1);
                outcome
            })
        };
        let first = attempt(a.clone());
        let second = attempt(b.clone());
        let outcomes = [
            first.await.expect("replica a"),
            second.await.expect("replica b"),
        ];
        let ran: Vec<usize> = outcomes.iter().flatten().copied().collect();
        assert_eq!(ran, [1], "exactly one replica ran, alone: {outcomes:?}");
        for (replica, outcome) in outcomes.iter().enumerate() {
            if outcome.is_some() {
                runs_by_replica[replica] += 1;
            }
        }
    }
    assert_eq!(runs_by_replica.iter().sum::<u32>(), 10);
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!("10 rounds; runs by replica a and b: {runs_by_replica:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_runs_on_two_replicas_never_overlap() {
    let Some(database) = database().await else {
        return;
    };
    let job = unique_name("periodic");
    let shutdown = Shutdown::new();
    let intervals = Arc::new(Mutex::new(Vec::<(Instant, Instant)>::new()));
    let mut runners = Vec::new();
    for _ in 0..2 {
        let lock = JobLock::new(replica(&database.url).await, "example-service", &job);
        let intervals = Arc::clone(&intervals);
        let shutdown = shutdown.clone();
        runners.push(tokio::spawn(lock.run_periodically(
            Duration::from_millis(40),
            shutdown,
            move || {
                let intervals = Arc::clone(&intervals);
                async move {
                    let start = Instant::now();
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    intervals
                        .lock()
                        .expect("intervals lock")
                        .push((start, Instant::now()));
                    Ok::<(), String>(())
                }
            },
        )));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    shutdown.stop();
    for runner in runners {
        tokio::time::timeout(Duration::from_secs(2), runner)
            .await
            .expect("runners stop when the service stops")
            .expect("runner task");
    }
    let mut intervals = intervals.lock().expect("intervals lock").clone();
    intervals.sort();
    assert!(intervals.len() >= 10, "only {} runs", intervals.len());
    for pair in intervals.windows(2) {
        assert!(
            pair[0].1 <= pair[1].0,
            "runs overlapped: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_session_cancels_the_job_and_frees_the_lock() {
    let Some(database) = database().await else {
        return;
    };
    let job = unique_name("lost");
    let admin = replica(&database.url).await;
    let a = JobLock::new(replica(&database.url).await, "example-service", &job)
        .with_heartbeat(Duration::from_millis(100));
    let b = JobLock::new(replica(&database.url).await, "example-service", &job);
    let key = a.key();
    let finished = Arc::new(AtomicBool::new(false));
    let finished_flag = Arc::clone(&finished);
    let running = tokio::spawn(async move {
        a.run_exclusive(|| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            finished_flag.store(true, Ordering::SeqCst);
        })
        .await
    });

    let started = Instant::now();
    let holder = loop {
        if let Some(pid) = lock_holder(&admin, key).await {
            break pid;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "never locked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(b.try_acquire().await.expect("try the lock").is_none());

    // A network partition or a database failover ends the session the same way.
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .fetch_one(&admin)
        .await
        .expect("terminate the holder's session");
    assert!(terminated);
    let outcome = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .expect("the job is cancelled soon after its session ends")
        .expect("job task");
    assert!(
        matches!(outcome, Err(JobLockError::Lost { .. })),
        "{outcome:?}"
    );
    assert!(!finished.load(Ordering::SeqCst), "the job was cancelled");
    eventually_acquires(&b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_guards_and_panicking_jobs_release_the_lock() {
    let Some(database) = database().await else {
        return;
    };
    let job = unique_name("released");
    let a = JobLock::new(replica(&database.url).await, "example-service", &job);
    let b = JobLock::new(replica(&database.url).await, "example-service", &job);

    let guard = a.try_acquire().await.expect("try").expect("free lock");
    assert!(b.try_acquire().await.expect("try").is_none());
    drop(guard);
    eventually_acquires(&b).await;

    let panicking = a.clone();
    let crashed = tokio::spawn(async move {
        panicking
            .run_exclusive(|| async { panic!("job failed hard") })
            .await
    })
    .await;
    assert!(crashed.is_err(), "the job task panicked");
    eventually_acquires(&b).await;

    // A cancelled run (for example cut off by the drain timeout) releases it too.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(200),
        a.run_exclusive(|| tokio::time::sleep(Duration::from_secs(30))),
    )
    .await;
    assert!(cancelled.is_err(), "the run was cancelled");
    eventually_acquires(&b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_locks_return_their_connection_to_the_pool() {
    let Some(database) = database().await else {
        return;
    };
    let job = unique_name("pooled");
    let admin = replica(&database.url).await;
    let pool = replica(&database.url).await;
    let lock = JobLock::new(pool.clone(), "example-service", &job);
    let mut holders = Vec::new();
    for _ in 0..5 {
        // Connecting the pool, like every release, hands a connection back on a background
        // task; an acquire that comes first opens another connection.
        settle(&pool).await;
        let holder = lock
            .run_exclusive(|| lock_holder(&admin, lock.key()))
            .await
            .expect("run")
            .expect("the lock was free");
        holders.push(holder.expect("the run held the lock"));
    }
    settle(&pool).await;
    // One session served all five runs: released sessions go back to the pool, not closed.
    assert!(
        holders.windows(2).all(|pair| pair[0] == pair[1]),
        "{holders:?}"
    );
    assert_eq!(pool.size(), 1);
    assert_eq!(pool.num_idle(), 1);
    // And it holds no advisory lock while idle in the pool.
    assert_eq!(lock_holder(&admin, lock.key()).await, None);
}

/// Waits until every connection of `pool` is back in it.
async fn settle(pool: &PgPool) {
    let started = Instant::now();
    while u32::try_from(pool.num_idle()).ok() != Some(pool.size()) {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "connections did not return to the pool"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
