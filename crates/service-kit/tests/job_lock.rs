//! Job locks against a real Postgres. Uses `TEST_DATABASE_URL` when set; otherwise starts a
//! throwaway Postgres container with podman or docker, one for the whole binary with a database
//! per test (`support::postgres::test_database`); skipped without either unless `CI` is set.
//! Each "replica" is its own connection pool, as separate pods would have. The partition
//! tests put a TCP proxy between a replica and Postgres and turn it into a black hole.

mod support;

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use service_kit::job_lock::{JobLock, JobLockError};
use service_kit::shutdown::Shutdown;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use support::postgres::test_database;
use support::unique_name;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

async fn replica(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect a replica's pool")
}

/// A replica's pool that connects through `proxy`.
async fn replica_behind(proxy: &Proxy, pool: PgPoolOptions) -> PgPool {
    pool.max_connections(4)
        .connect_with(proxy.options.clone())
        .await
        .expect("connect a replica's pool through the proxy")
}

/// The backend holding the advisory lock `key`, if any.
async fn lock_holder(admin: &PgPool, key: i64) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
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
    let Some(database) = test_database().await else {
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
    let Some(database) = test_database().await else {
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
    let Some(database) = test_database().await else {
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
    let Some(database) = test_database().await else {
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
    let Some(database) = test_database().await else {
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

/// Heartbeat and lock timeout of the partition tests: short, so a run takes seconds.
const PARTITION_HEARTBEAT: Duration = Duration::from_millis(200);
const PARTITION_TIMEOUT: Duration = Duration::from_secs(1);
/// Allowance on top of the lock timeout for timer and scheduling delay on a loaded machine.
const SCHEDULING_SLACK: Duration = Duration::from_millis(150);
/// How long the proxy holds back what Postgres says (see [`Proxy::delay_replies`]). It is well
/// above the time a test needs to see the lock on the server and cut the network, so the client
/// has certainly not heard of the lock yet when the partition begins, and well below
/// `PARTITION_TIMEOUT`, so the lock's own queries still finish in time.
const REPLY_DELAY: Duration = Duration::from_millis(300);

/// A TCP proxy to Postgres that can turn into a black hole: it then drops every byte in both
/// directions and keeps both connections open, never sending a FIN or a reset. That is what a
/// network partition looks like to both ends, which notice only once TCP gives up (on Linux,
/// after about 15 minutes of retransmissions).
struct Proxy {
    /// How to connect to the database through this proxy.
    options: PgConnectOptions,
    blackhole: Arc<AtomicBool>,
    /// For each connection through the proxy, how long in milliseconds replies from Postgres are
    /// held back; see `delay_replies`.
    reply_delays: Arc<Mutex<Vec<Arc<AtomicU64>>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(database_url: &str) -> Self {
        let direct = PgConnectOptions::from_str(database_url).expect("parse the database URL");
        let target = tokio::net::lookup_host((direct.get_host(), direct.get_port()))
            .await
            .expect("resolve the database host")
            .next()
            .expect("the database host has an address");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let address = listener.local_addr().expect("the proxy's address");
        let blackhole = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&blackhole);
        let reply_delays = Arc::new(Mutex::new(Vec::new()));
        let delays = Arc::clone(&reply_delays);
        let accept = tokio::spawn(async move {
            // Dropped, and so aborted, together with the accept task.
            let mut connections = JoinSet::new();
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(target).await else {
                    continue;
                };
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                let delay = Arc::new(AtomicU64::new(0));
                delays
                    .lock()
                    .expect("reply delays")
                    .push(Arc::clone(&delay));
                connections.spawn(forward(client_read, server_write, Arc::clone(&flag), None));
                connections.spawn(forward(
                    server_read,
                    client_write,
                    Arc::clone(&flag),
                    Some(delay),
                ));
            }
        });
        Self {
            options: direct.host(&address.ip().to_string()).port(address.port()),
            blackhole,
            reply_delays,
            accept,
        }
    }

    /// From now on, holds back every reply of Postgres on the connections that are open now for
    /// `delay` before it is passed on, in order. A partition that begins in that time drops the
    /// reply: the server has done what the client asked, the client has not heard of it, and
    /// no amount of waiting changes that. It is what a slow or loaded machine does by chance,
    /// made certain. Connections opened later are not held back: opening one takes several
    /// replies, and a replica whose first attempt failed must be able to connect again.
    fn delay_replies(&self, delay: Duration) {
        let millis = u64::try_from(delay.as_millis()).expect("a delay that fits in u64");
        for connection in self.reply_delays.lock().expect("reply delays").iter() {
            connection.store(millis, Ordering::SeqCst);
        }
    }

    /// From now on, drops all traffic in both directions without closing anything.
    fn partition(&self) {
        self.blackhole.store(true, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Copies one direction of a proxied connection; in black-hole mode it reads and drops. With
/// `delay_ms`, it holds each chunk back for that long first, and drops it if the partition began
/// meanwhile, as a network drops what is still in flight.
async fn forward(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    blackhole: Arc<AtomicBool>,
    delay_ms: Option<Arc<AtomicU64>>,
) {
    let mut buffer = vec![0_u8; 8192];
    loop {
        match from.read(&mut buffer).await {
            Ok(read) if read > 0 => {
                if blackhole.load(Ordering::SeqCst) {
                    continue;
                }
                if let Some(delay_ms) = &delay_ms {
                    let delay = Duration::from_millis(delay_ms.load(Ordering::SeqCst));
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    if blackhole.load(Ordering::SeqCst) {
                        continue;
                    }
                }
                if to.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    if blackhole.load(Ordering::SeqCst) {
        // Dropping `to` would send a FIN that tells the other end the connection ended, which a
        // partition never does. Hold it until the proxy is dropped.
        std::future::pending::<()>().await;
        drop(to);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partitioned_holder_stops_its_job_within_the_lock_timeout() {
    let Some(database) = test_database().await else {
        return;
    };
    let job = unique_name("partition");
    let proxy = Proxy::start(&database.url).await;
    let admin = replica(&database.url).await;
    let a = JobLock::new(
        replica_behind(&proxy, PgPoolOptions::new()).await,
        "example-service",
        &job,
    )
    .with_heartbeat(PARTITION_HEARTBEAT)
    .with_timeout(PARTITION_TIMEOUT);
    proxy.delay_replies(REPLY_DELAY);
    let b = JobLock::new(replica(&database.url).await, "example-service", &job);
    let key = a.key();
    let ticks = Arc::new(AtomicUsize::new(0));
    let last_tick = Arc::new(Mutex::new(None::<Instant>));
    let running = {
        let ticks = Arc::clone(&ticks);
        let last_tick = Arc::clone(&last_tick);
        tokio::spawn(async move {
            loop {
                let (job_ticks, job_last_tick) = (Arc::clone(&ticks), Arc::clone(&last_tick));
                let outcome = a
                    .run_exclusive(|| async move {
                        loop {
                            job_ticks.fetch_add(1, Ordering::SeqCst);
                            *job_last_tick.lock().expect("tick lock") = Some(Instant::now());
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await;
                // A loaded machine can make the first attempt miss its lock timeout (the lock
                // query is then cancelled and the job never runs); the test needs a job that
                // runs, so it tries again, on a fresh connection.
                if outcome.is_err() && last_tick.lock().expect("tick lock").is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                return outcome;
            }
        })
    };
    // The job running is what shows that the holder has the lock. The server holding the lock
    // shows nothing of the kind: it grants the lock before the holder hears of it, and a
    // partition that begins in between drops that reply, so the job would never run. The
    // proxy holds every reply back, to leave that gap open for the whole test.
    wait_for(&ticks, 1, "the job to start under its lock").await;
    let holder = lock_holder(&admin, key)
        .await
        .expect("the running job's session holds the lock on the server");

    // The network between the holder and Postgres starts dropping packets. Then the server ends
    // the holder's session, as a failover, an idle timeout, TCP keepalive or an admin would: the
    // server releases the lock, and the holder hears nothing of it.
    proxy.partition();
    let partitioned = Instant::now();
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .fetch_one(&admin)
        .await
        .expect("terminate the holder's session");
    assert!(terminated);
    let guard = loop {
        if let Some(guard) = b.try_acquire().await.expect("try the lock") {
            break guard;
        }
        assert!(
            partitioned.elapsed() < Duration::from_secs(5),
            "the server never released the lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let taken_over = Instant::now();

    let wait = PARTITION_TIMEOUT * 5;
    let Ok(outcome) = tokio::time::timeout(wait, running).await else {
        panic!(
            "replica a still ran its job {wait:?} after replica b took the lock ({} ticks so \
             far): two replicas run the job at once",
            ticks.load(Ordering::SeqCst)
        );
    };
    let stopped_after = partitioned.elapsed();
    let outcome = outcome.expect("job task");
    assert!(
        matches!(outcome, Err(JobLockError::Lost { .. })),
        "{outcome:?}"
    );
    let last_tick = last_tick.lock().expect("tick lock").expect("the job ran");
    let overlap = last_tick.saturating_duration_since(taken_over);
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!(
        "lock timeout {PARTITION_TIMEOUT:?}, heartbeat {PARTITION_HEARTBEAT:?}: replica b took \
         the lock {:?} after the partition; replica a stopped its job {stopped_after:?} after \
         the partition; both ran the job for {overlap:?}",
        taken_over - partitioned
    );
    // The holder's last confirmed ping went out before the partition, and it stops at the latest
    // one timeout after that ping, so the overlap and the detection are each within the timeout.
    assert!(
        overlap <= PARTITION_TIMEOUT + SCHEDULING_SLACK,
        "both replicas ran the job for {overlap:?}"
    );
    assert!(
        stopped_after <= PARTITION_TIMEOUT + SCHEDULING_SLACK,
        "the holder noticed the partition only after {stopped_after:?}"
    );
    // The job really stopped.
    let frozen = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), frozen, "the job kept running");
    guard.release().await.expect("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn taking_and_releasing_the_lock_give_up_within_the_timeout_in_a_partition() {
    let Some(database) = test_database().await else {
        return;
    };
    let job = unique_name("partition-queries");
    let proxy = Proxy::start(&database.url).await;
    // Each pool keeps the connection it opened on connect. The first two hand it out without
    // pinging it first, so the lock's own query is what meets the partition; a default pool
    // pings it before handing it out, and that ping meets the partition.
    let untested = || PgPoolOptions::new().test_before_acquire(false);
    let pools = [
        replica_behind(&proxy, untested()).await,
        replica_behind(&proxy, untested()).await,
        replica_behind(&proxy, PgPoolOptions::new()).await,
    ];
    for pool in &pools {
        settle(pool).await;
    }
    let [releasing, taking, pinging] = pools
        .map(|pool| JobLock::new(pool, "example-service", &job).with_timeout(PARTITION_TIMEOUT));
    // The guard in hand is what shows that this replica has heard of the lock; the partition
    // begins only after that.
    let guard = releasing
        .try_acquire()
        .await
        .expect("try the lock")
        .expect("the lock is free");
    proxy.partition();
    let partitioned = Instant::now();
    let wait = PARTITION_TIMEOUT * 5;
    let (released, taken, pinged) = tokio::join!(
        tokio::time::timeout(wait, async {
            (guard.release().await, partitioned.elapsed())
        }),
        tokio::time::timeout(wait, async {
            (
                taking.try_acquire().await.map(|guard| guard.is_some()),
                partitioned.elapsed(),
            )
        }),
        tokio::time::timeout(wait, async {
            (
                pinging.try_acquire().await.map(|guard| guard.is_some()),
                partitioned.elapsed(),
            )
        }),
    );
    eprintln!(
        "lock timeout {PARTITION_TIMEOUT:?}, after the partition: release {released:?}; \
         try_acquire {taken:?}; try_acquire through a pinging pool {pinged:?}"
    );
    let (released, released_after) = released.expect("release stops waiting for the database");
    let (taken, taken_after) = taken.expect("try_acquire stops waiting for the database");
    let (pinged, pinged_after) =
        pinged.expect("try_acquire through a pinging pool stops waiting for the database");
    assert!(
        matches!(released, Err(JobLockError::Lost { .. })),
        "{released:?}"
    );
    assert!(matches!(taken, Err(JobLockError::Lost { .. })), "{taken:?}");
    assert!(
        matches!(
            pinged,
            Err(JobLockError::Database {
                source: sqlx::Error::PoolTimedOut,
                ..
            })
        ),
        "{pinged:?}"
    );
    for elapsed in [released_after, taken_after, pinged_after] {
        assert!(
            elapsed <= PARTITION_TIMEOUT + SCHEDULING_SLACK,
            "{elapsed:?}"
        );
    }
}

/// Collects the events logged on the test's thread: level and `name=value` text.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl Captured {
    fn events(&self) -> Vec<(tracing::Level, String)> {
        self.0.lock().expect("captured events").clone()
    }
}

struct EventText<'a>(&'a mut String);

impl tracing::field::Visit for EventText<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut text = String::new();
        event.record(&mut EventText(&mut text));
        self.0
            .lock()
            .expect("captured events")
            .push((*event.metadata().level(), text));
    }
}

/// Starts capturing this thread's log events; the capture ends when the guard drops. The test
/// must run on a current-thread runtime so that the scheduled task logs on this thread.
fn capture_logs() -> (Captured, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    (captured, tracing::subscriber::set_default(subscriber))
}

/// What a panicking job must not leak: the panic message, which can carry data.
const SECRET: &str = "secret-customer-data-4711";

/// Upper bound for waiting on something that must happen but not at a fixed moment, such as a
/// job being called or an event being logged. Only a failure ever waits this long: it is not a
/// time any test expects to take, and none asserts on how long such a wait lasted.
const EVENT_WAIT: Duration = Duration::from_secs(10);

/// Waits until `counter` reaches `at_least`; `what` names what is awaited, for the failure.
async fn wait_for(counter: &AtomicUsize, at_least: usize, what: &str) {
    let started = Instant::now();
    while counter.load(Ordering::SeqCst) < at_least {
        assert!(
            started.elapsed() < EVENT_WAIT,
            "waited {EVENT_WAIT:?} for {what}: the counter is at {} of {at_least}",
            counter.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A job that panics keeps its schedule (the chosen behaviour, see the README): later periods
/// run it again, one ERROR line per panic names the job but not the panic message, and the lock
/// is free for another replica afterwards. The job panics on its first two calls.
async fn assert_panics_keep_the_schedule<F, Fut>(name: &str, calls: Arc<AtomicUsize>, job: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let Some(database) = test_database().await else {
        return;
    };
    let (captured, _logs) = capture_logs();
    let job_name = unique_name(name);
    let lock = JobLock::new(replica(&database.url).await, "example-service", &job_name);
    let other = JobLock::new(replica(&database.url).await, "example-service", &job_name);
    let admin = replica(&database.url).await;
    let shutdown = Shutdown::new();
    let runner = tokio::spawn(lock.clone().run_periodically(
        Duration::from_millis(40),
        shutdown.clone(),
        job,
    ));

    // The schedule must reach the fifth call although the first two panicked.
    wait_for(&calls, 5, "the periodic job to be called five times").await;
    assert!(
        !runner.is_finished(),
        "a panicking job ended the schedule of its replica"
    );
    shutdown.stop();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("the runner stops when the service stops")
        .expect("the runner task did not panic");
    assert_eq!(lock_holder(&admin, lock.key()).await, None);
    eventually_acquires(&other).await;

    let events = captured.events();
    let panics: Vec<_> = events
        .iter()
        .filter(|(level, text)| {
            *level == tracing::Level::ERROR && text.contains(&job_name) && text.contains("panicked")
        })
        .collect();
    assert_eq!(panics.len(), 2, "one ERROR per panic: {events:?}");
    // Only the scheduler's own lines (they carry the job name) are checked: the process-wide
    // panic hook, which this test does not install, logs the message by design (README).
    for (level, text) in events.iter().filter(|(_, text)| text.contains(&job_name)) {
        assert!(
            !text.contains(SECRET),
            "{level} event leaks the payload: {text}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_panicking_job_keeps_its_schedule_and_logs_no_payload() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    assert_panics_keep_the_schedule("panicking", calls, move || {
        let call = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if call < 2 {
                panic!("{SECRET} in call {call}");
            }
            tokio::task::yield_now().await;
            Ok(())
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_job_that_panics_before_returning_its_future_keeps_its_schedule_too() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    assert_panics_keep_the_schedule("panicking-builder", calls, move || {
        let call = counted.fetch_add(1, Ordering::SeqCst);
        assert!(call >= 2, "{SECRET} in call {call}");
        async { Ok(()) }
    })
    .await;
}

/// Waits until a captured event at or after position `from` satisfies `matches`, for at most
/// `within`; `what` names the event for the failure message. Returns the event's position and
/// when it was first seen.
async fn wait_for_event(
    captured: &Captured,
    what: &str,
    within: Duration,
    from: usize,
    matches: impl Fn(&(tracing::Level, String)) -> bool,
) -> (usize, Instant) {
    let started = Instant::now();
    loop {
        let events = captured.events();
        if let Some(offset) = events.iter().skip(from).position(&matches) {
            return (from + offset, Instant::now());
        }
        assert!(
            started.elapsed() < within,
            "waited {within:?} for {what} (looking from event {from}); the events: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A periodic job that panics while its replica is cut off from Postgres: the panic is caught
/// and logged although the release that follows fails, that release gives up within the lock
/// timeout instead of waiting for TCP, and the schedule goes on (its next rounds fail to take
/// the lock, also within the timeout) until the service stops. The lock timeout and the panic
/// handling compose.
#[tokio::test(flavor = "current_thread")]
async fn a_job_that_panics_in_a_partition_gives_up_its_lock_within_the_timeout() {
    let Some(database) = test_database().await else {
        return;
    };
    let (captured, _logs) = capture_logs();
    let job = unique_name("partition-panic");
    let proxy = Proxy::start(&database.url).await;
    let admin = replica(&database.url).await;
    let a = JobLock::new(
        replica_behind(&proxy, PgPoolOptions::new()).await,
        "example-service",
        &job,
    )
    .with_heartbeat(PARTITION_HEARTBEAT)
    .with_timeout(PARTITION_TIMEOUT);
    let b = JobLock::new(replica(&database.url).await, "example-service", &job);
    let key = a.key();
    // The proxy holds Postgres's replies back, so the server grants the lock a while before the
    // replica hears of it. Whatever a test does at the server cannot tell it that the job runs.
    proxy.delay_replies(REPLY_DELAY);
    let calls = Arc::new(AtomicUsize::new(0));
    // The job's first run cuts the network and fails in the same poll, so that it is certain to
    // fail while its replica is partitioned and before the heartbeat can declare the lock lost.
    // Cutting the network from the test, after the job has started, would race with that: the
    // runtime may poll the heartbeat first, and the job would then be dropped, not panic.
    let partition = Arc::clone(&proxy.blackhole);
    let shutdown = Shutdown::new();
    let runner = {
        let calls = Arc::clone(&calls);
        tokio::spawn(
            a.run_periodically(Duration::from_millis(100), shutdown.clone(), move || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let partition = Arc::clone(&partition);
                async move {
                    if call == 0 {
                        partition.store(true, Ordering::SeqCst);
                        panic!("the first run fails while its replica is partitioned");
                    }
                    Ok::<(), String>(())
                }
            }),
        )
    };
    // A round that does not hear the server's answer in time gives up and the next one tries
    // again, so wait for the job's panic, however many rounds that takes.
    let (panic_event, panicked) = wait_for_event(
        &captured,
        "the ERROR event for the job's panic",
        EVENT_WAIT,
        0,
        |(level, text)| {
            *level == tracing::Level::ERROR && text.contains(&job) && text.contains("panicked")
        },
    )
    .await;
    let wait = PARTITION_TIMEOUT * 5;
    let (_, released) = wait_for_event(
        &captured,
        "the WARN event that the lock was lost after the panic",
        wait,
        panic_event + 1,
        |(level, text)| {
            *level == tracing::Level::WARN
                && text.contains(&job)
                && text.contains("scheduled job lock failed")
                && text.contains("lost the lock")
        },
    )
    .await;
    let gave_up_after = released - panicked;
    // Evidence for the test report, shown with `--nocapture`.
    eprintln!(
        "lock timeout {PARTITION_TIMEOUT:?}: the release after the panic gave up after \
         {gave_up_after:?}"
    );
    assert!(
        gave_up_after <= PARTITION_TIMEOUT + SCHEDULING_SLACK,
        "the release after the panic waited {gave_up_after:?}"
    );

    // The schedule goes on: later rounds try again, and each gives up within the timeout too.
    // Counted from the panic on: the failure that ended the panicked run is the first of three.
    let lock_failures = |captured: &Captured| {
        captured
            .events()
            .iter()
            .skip(panic_event)
            .filter(|(level, text)| {
                *level == tracing::Level::WARN
                    && text.contains(&job)
                    && text.contains("scheduled job lock failed")
            })
            .count()
    };
    let started = Instant::now();
    while lock_failures(&captured) < 3 {
        assert!(
            started.elapsed() < wait,
            "the schedule stalled after the panic: {:?}",
            captured.events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!runner.is_finished(), "the panic ended the schedule");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the job ran without its lock"
    );
    // The cut-off session still holds the lock on the server, which has heard nothing from the
    // replica. The server ends that session, as a failover, an idle timeout, TCP keepalive or an
    // admin would; the lock is then free for another replica.
    let holder = lock_holder(&admin, key)
        .await
        .expect("the cut-off session still holds the lock on the server");
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .fetch_one(&admin)
        .await
        .expect("terminate the holder's session");
    assert!(terminated);
    eventually_acquires(&b).await;

    shutdown.stop();
    tokio::time::timeout(PARTITION_TIMEOUT + Duration::from_secs(2), runner)
        .await
        .expect("the runner stops when the service stops")
        .expect("the runner task did not panic");
}
