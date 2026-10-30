//! The scheduled purge (Laysi/trustiot-v2#645) under a lock on the whole conversations table:
//! a round never waits for it without a bound, and it does not go on through the expired backlog
//! while every deletion is blocked. The product's schema applied by the migration Job
//! (`testkit::product`), the purge's platform role `assistant_platform` through `PlatformDb`, as
//! the service uses it; a superuser session holds the table lock.
//!
//! - `LOCK TABLE ... IN SHARE MODE` blocks every DELETE but not the read of the keys: the batch
//!   gives up, the one-at-a-time deletion of its conversations deletes none, and the round stops
//!   there, with every one of them skipped, instead of running a second batch.
//! - `LOCK TABLE ... IN ACCESS EXCLUSIVE MODE` blocks the read of the keys too: it gives up after
//!   the purge's lock timeout, and the round ends while the lock is still held.
//! - Each round logs one WARN line with the counts only; once the lock is gone, the next round
//!   purges everything, at INFO.
//!
//! Its own test binary, so its own Postgres: a table lock in the database another test binary
//! shares would hold every other test there. The two tests here take turns ([`TABLE`]). No step
//! waits a fixed time. Starts a throwaway Postgres with TLS (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::sync::Arc;
use std::time::Duration;

use assistant::purge::{self, Round};
use service_kit::tenant::PlatformDb;
use sqlx::Connection;
use sqlx::postgres::PgPoolOptions;
use support::{Capture, Event};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::sync::Mutex;
use uuid::Uuid;

/// The superuser stores `$3` conversations of user `$2` in tenant `$1`, deleted 31, 32, ... days
/// ago, so every one is expired. Returns their IDs.
const SEED_EXPIRED: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl, created_at, last_activity_at, deleted_at)
SELECT $1::uuid, gen_random_uuid(), $2::uuid, 'seed', '', '', '[]', 'medium', '5m',
    now() - make_interval(days => 40), now() - make_interval(days => 40),
    now() - make_interval(days => 30 + n)
FROM generate_series(1, $3) AS n
RETURNING conversation_id::text";

/// How many conversations tenant `$1` has stored.
const STORED: &str = "SELECT count(*) FROM assistant.conversations WHERE tenant_id = $1::uuid";

/// Blocks every DELETE of the table, not its reads.
const LOCK_SHARE: &str = "LOCK TABLE assistant.conversations IN SHARE MODE";

/// Blocks every statement on the table, its reads included.
const LOCK_EXCLUSIVE: &str = "LOCK TABLE assistant.conversations IN ACCESS EXCLUSIVE MODE";

/// What `PlatformDb::begin` logs for every platform transaction: the round's transactions,
/// counted.
const PLATFORM_ACCESS: &str = "platform database access";

/// The longest a round may take while the table stays exclusively locked: it configured at most
/// `store::PURGE_LOCK_TIMEOUT` for the batch and again for the read of its keys (10 s), and the
/// bound leaves room for a loaded machine. Without the read's own lock timeout the round waits
/// until the lock goes, so this bound is what ends the test then.
const ROUND_LIMIT: Duration = Duration::from_secs(120);

/// The tests take turns: each holds a lock on the whole table, and each round of either purges
/// the expired conversations of every tenant.
static TABLE: Mutex<()> = Mutex::const_new(());

struct Db {
    product: Arc<ProductPostgres>,
    platform: PlatformDb,
}

/// The platform role's pool, with one session that every round reuses ([`Db::warm`]).
async fn db() -> Option<Db> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let platform = PlatformDb::connect_lazy(
        product.postgres.options("assistant_platform", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the platform role's pool verifies the server");
    Some(Db { product, platform })
}

impl Db {
    /// Opens the pool's one session before a test locks the table. A new session runs
    /// service-kit's session check, which prints the table's policies (`pg_get_expr`) and so
    /// waits behind an ACCESS EXCLUSIVE lock until the pool's acquire timeout: that is a failure of
    /// `PlatformDb::begin`, not of the round's statements, which these tests are about. The
    /// service's pool normally has an open session to reuse: readiness pings it.
    async fn warm(&self) {
        self.platform
            .ping()
            .await
            .expect("open the platform session");
    }

    /// Stores `count` expired conversations of a new tenant; returns the tenant and their IDs.
    async fn expired(&self, count: i32) -> (Uuid, Vec<Uuid>) {
        let tenant = Uuid::now_v7();
        let mut admin = self.product.superuser().await;
        let ids: Vec<String> = sqlx::query_scalar(SEED_EXPIRED)
            .bind(tenant.to_string())
            .bind(Uuid::now_v7().to_string())
            .bind(count)
            .fetch_all(&mut admin)
            .await
            .expect("store the expired conversations");
        let ids = ids.iter().map(|id| id.parse().expect("a UUID")).collect();
        (tenant, ids)
    }

    async fn stored(&self, tenant: Uuid) -> i64 {
        let mut admin = self.product.superuser().await;
        sqlx::query_scalar(STORED)
            .bind(tenant.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("count the tenant's conversations")
    }
}

/// The round's own lines (target `assistant::purge`).
fn own(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|event| event.target == "assistant::purge")
        .collect()
}

/// The platform transactions the round opened.
fn transactions(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| event.message == PLATFORM_ACCESS)
        .count()
}

/// Fails unless the round's one line is `level` `message` with these counts, and nothing in the
/// events names the tenant or a conversation.
fn expect_line(
    events: &[Event],
    level: tracing::Level,
    message: &str,
    (purged, skipped): (u64, u64),
    tenant: Uuid,
    ids: &[Uuid],
) {
    let own = own(events);
    assert_eq!(own.len(), 1, "one line per round: {events:?}");
    assert_eq!(own[0].level, level, "{own:?}");
    assert_eq!(own[0].message, message, "{own:?}");
    assert_eq!(
        (
            own[0].fields.get("job").map(String::as_str),
            own[0].fields.get("purged").cloned(),
            own[0].fields.get("skipped").cloned(),
        ),
        (
            Some(purge::JOB),
            Some(purged.to_string()),
            Some(skipped.to_string())
        ),
        "{own:?}"
    );
    let logged = format!("{events:?}");
    assert!(
        !logged.contains(&tenant.to_string()),
        "no tenant in the log"
    );
    for id in ids {
        assert!(!logged.contains(&id.to_string()), "no ID in the log");
    }
}

/// A SHARE lock on the table blocks every DELETE but not the read of the keys. The batch gives
/// up, its conversations are tried one at a time, none can be deleted, and the round stops
/// there: four platform transactions plus one per conversation (the batch, the read of the keys,
/// each deletion), where going on would have run a second batch and a second read. Every one of
/// them counts as skipped, the line is the WARN of a round a lock stopped early, and nothing is
/// deleted. Once the lock is gone, the next round purges them all.
#[tokio::test(flavor = "multi_thread")]
async fn a_table_lock_ends_the_round_after_one_fallback_batch() {
    let Some(db) = db().await else { return };
    let _turn = TABLE.lock().await;
    let (tenant, ids) = db.expired(3).await;
    db.warm().await;
    let mut admin = db.product.superuser().await;
    let mut holding = admin.begin().await.expect("a superuser transaction");
    sqlx::raw_sql(LOCK_SHARE)
        .execute(&mut *holding)
        .await
        .expect("lock the table");

    let (capture, guard) = Capture::install();
    let round = purge::round(&db.platform)
        .await
        .expect("a round that meets a table lock ends without an error");
    drop(guard);
    let events = capture.events();
    assert_eq!(
        round,
        Round {
            purged: 0,
            skipped: 3
        }
    );
    assert_eq!(
        transactions(&events),
        2 + ids.len(),
        "the batch, the read of its keys and one deletion per conversation, then the round \
         stopped: {events:?}"
    );
    expect_line(
        &events,
        tracing::Level::WARN,
        purge::STOPPED_EARLY,
        (0, 3),
        tenant,
        &ids,
    );
    assert_eq!(db.stored(tenant).await, 3, "nothing deleted under the lock");

    holding.rollback().await.expect("release the table lock");
    let (capture, guard) = Capture::install();
    let round = purge::round(&db.platform).await.expect("the next round");
    drop(guard);
    assert_eq!(
        round,
        Round {
            purged: 3,
            skipped: 0
        }
    );
    expect_line(
        &capture.events(),
        tracing::Level::INFO,
        "purged expired conversations",
        (3, 0),
        tenant,
        &ids,
    );
    assert_eq!(db.stored(tenant).await, 0, "purged once the lock is gone");
}

/// An ACCESS EXCLUSIVE lock on the table (an `ALTER TABLE`, a `VACUUM FULL`) blocks the read of
/// the keys too. The batch gives up after the purge's lock timeout, and so does the read: the
/// round returns while the lock is still held, deletes nothing and logs the WARN of a round a
/// lock stopped early. Once the lock is gone, the next round purges.
#[tokio::test(flavor = "multi_thread")]
async fn an_exclusive_table_lock_does_not_hold_the_round() {
    let Some(db) = db().await else { return };
    let _turn = TABLE.lock().await;
    let (tenant, ids) = db.expired(2).await;
    db.warm().await;
    let mut admin = db.product.superuser().await;
    let mut holding = admin.begin().await.expect("a superuser transaction");
    sqlx::raw_sql(LOCK_EXCLUSIVE)
        .execute(&mut *holding)
        .await
        .expect("lock the table");

    let (capture, guard) = Capture::install();
    let round = tokio::time::timeout(ROUND_LIMIT, purge::round(&db.platform))
        .await
        .unwrap_or_else(|_| {
            panic!("the round did not end while the table stayed exclusively locked")
        })
        .expect("a round that meets an exclusive table lock ends without an error");
    drop(guard);
    let events = capture.events();
    assert_eq!(
        round,
        Round {
            purged: 0,
            skipped: 0
        }
    );
    assert_eq!(
        transactions(&events),
        2,
        "the batch and the read of its keys: {events:?}"
    );
    expect_line(
        &events,
        tracing::Level::WARN,
        purge::STOPPED_EARLY,
        (0, 0),
        tenant,
        &ids,
    );

    // The round ended while the lock was held; only now does it go.
    holding.rollback().await.expect("release the table lock");
    assert_eq!(db.stored(tenant).await, 2, "nothing deleted under the lock");
    let (capture, guard) = Capture::install();
    let round = purge::round(&db.platform).await.expect("the next round");
    drop(guard);
    assert_eq!(
        round,
        Round {
            purged: 2,
            skipped: 0
        }
    );
    expect_line(
        &capture.events(),
        tracing::Level::INFO,
        "purged expired conversations",
        (2, 0),
        tenant,
        &ids,
    );
    assert_eq!(db.stored(tenant).await, 0, "purged once the lock is gone");
}
