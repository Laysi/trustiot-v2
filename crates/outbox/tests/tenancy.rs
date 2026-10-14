//! The consumer under decision 29's tenant isolation, against a real Postgres with TLS (a
//! throwaway container; skipped without podman or docker unless `CI` is set):
//!
//! - it reads the producer's `outbox_event`, which has row-level security, as the consuming
//!   service's platform role and sees every tenant's events and platform-wide ones;
//! - each event is processed in a transaction of the service role in the event's tenant, so a
//!   handler sees and writes only that tenant's rows, and a platform-wide event's handler none;
//! - a service role that a policy lets read across tenants is refused by service-kit, and a
//!   reader without the read policy stops the consumer instead of reading nothing;
//! - an event whose envelope names another tenant than its log row is never processed;
//! - the producer's roles reach only their own aggregates in `outbox_aggregate`: another
//!   tenant's sequence can be neither read nor advanced (Laysi/trustiot-v2#413).

mod support;

use std::error::Error as _;
use std::time::Duration;

use contract::{ServiceName, TenantId};
use outbox::{
    Aggregate, ConsumerConfig, ConsumerError, Delivery, Handler, HandlerError, Outcome, Position,
    PublishError, Published,
};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::UnsafeSession;
use service_kit::tenant::{BackgroundTenantDb, PlatformActor, PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Postgres, Transaction};
use support::{Counted, Recorder, Setup, TENANT_A, TENANT_B, database, drain, tenant};

const DRAIN: Duration = Duration::from_secs(30);

/// A third tenant, whose rows no event touches.
const TENANT_C: &str = "01928f6e-7a3c-7d4e-8f00-00000000000c";

/// A tenant table in the consumer's schema, written by the handlers, with service-kit's policy
/// template; `platform_note` and `delivered` hold no tenant data. The table owner (the
/// superuser) adds a row of tenant C that no handler may see.
const CONSUMER_TABLES: &str = "
CREATE TABLE note (tenant_id uuid NOT NULL, event_id uuid NOT NULL, PRIMARY KEY (tenant_id, event_id));
ALTER TABLE note ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON note
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
CREATE TABLE platform_note (event_id uuid PRIMARY KEY);
CREATE TABLE delivered (event_id uuid PRIMARY KEY, tenant_setting text, visible_notes bigint NOT NULL);
INSERT INTO note VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000c', '00000000-0000-7000-8000-0000000000c1');
";

async fn create_tables(setup: &Setup) {
    sqlx::raw_sql(CONSUMER_TABLES)
        .execute(&setup.consumer)
        .await
        .expect("consumer tables");
}

/// Writes `Counted { n }` as an event of `tenant` (platform-wide for `None`), as the superuser.
async fn publish(setup: &Setup, aggregate: &str, tenant: Option<TenantId>) -> Published {
    let mut tx = setup.producer.begin().await.expect("begin");
    let published = outbox::publish(
        &mut tx,
        &Aggregate::new("widget", aggregate).expect("aggregate"),
        &Counted { n: 1 },
        tenant,
    )
    .await
    .expect("publish");
    tx.commit().await.expect("commit");
    published
}

/// The SQLSTATE of the database error behind a handler error.
fn sqlstate(error: &HandlerError) -> Option<String> {
    let source = error.source()?.downcast_ref::<sqlx::Error>()?;
    match source {
        sqlx::Error::Database(error) => error.code().map(|code| code.into_owned()),
        _ => None,
    }
}

/// Notes the event in its tenant (a platform-wide event in `platform_note`) and records the
/// transaction's tenant setting and how many notes the handler can see.
struct TenantWriter;

impl Handler for TenantWriter {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let event_id = delivery.envelope.id.as_uuid();
        match delivery.tenant() {
            Some(tenant) => {
                sqlx::query("INSERT INTO note (tenant_id, event_id) VALUES ($1, $2)")
                    .bind(tenant.as_uuid())
                    .bind(event_id)
                    .execute(&mut **tx)
                    .await?;
            }
            None => {
                sqlx::query("INSERT INTO platform_note (event_id) VALUES ($1)")
                    .bind(event_id)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        sqlx::query(
            "INSERT INTO delivered (event_id, tenant_setting, visible_notes) \
             SELECT $1, NULLIF(current_setting('trustiot.tenant_id', true), ''), count(*) \
             FROM note",
        )
        .bind(event_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

#[tokio::test]
async fn the_platform_role_reads_every_tenant_and_each_event_runs_in_its_tenant() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    create_tables(&setup).await;
    let a = publish(&setup, "a", Some(tenant(TENANT_A))).await;
    let b = publish(&setup, "b", Some(tenant(TENANT_B))).await;
    let platform_wide = publish(&setup, "p", None).await;

    let consumer = setup.subscribe(
        ConsumerConfig::new("tenant-writer", &setup.producer_schema).expect("config"),
        TenantWriter,
    );
    consumer.ensure_subscription().await.expect("subscription");
    let progress = drain(&consumer, DRAIN).await;
    assert_eq!((progress.delivered, progress.dead_lettered), (3, 0));
    assert_eq!(consumer.measure_lag().await.expect("lag").events, 0);

    // Each handler ran in its event's tenant and saw only that tenant's note: tenant C's row
    // stayed invisible, and the platform-wide event saw no tenant row at all.
    let delivered: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT event_id::text, tenant_setting, visible_notes FROM delivered ORDER BY event_id",
    )
    .fetch_all(&setup.consumer)
    .await
    .expect("delivered");
    let mut expected = vec![
        (a.event_id.to_string(), Some(TENANT_A.to_owned()), 1),
        (b.event_id.to_string(), Some(TENANT_B.to_owned()), 1),
        (platform_wide.event_id.to_string(), None, 0),
    ];
    expected.sort();
    assert_eq!(delivered, expected);
    let notes: Vec<(String, String)> =
        sqlx::query_as("SELECT tenant_id::text, event_id::text FROM note ORDER BY tenant_id")
            .fetch_all(&setup.consumer)
            .await
            .expect("notes");
    assert_eq!(
        notes,
        [
            (TENANT_A.to_owned(), a.event_id.to_string()),
            (TENANT_B.to_owned(), b.event_id.to_string()),
            (
                TENANT_C.to_owned(),
                "00000000-0000-7000-8000-0000000000c1".to_owned()
            ),
        ]
    );
    println!(
        "platform role read 3 events (tenant A, tenant B, platform-wide); each handler ran in \
         its event's tenant: {delivered:?}"
    );
}

/// Writes a note of `other` instead of the event's tenant.
struct Trespasser {
    other: TenantId,
}

impl Handler for Trespasser {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        sqlx::query("INSERT INTO note (tenant_id, event_id) VALUES ($1, $2)")
            .bind(self.other.as_uuid())
            .bind(delivery.envelope.id.as_uuid())
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_handler_cannot_write_another_tenant() {
    let Some(database) = database().await else {
        return;
    };
    for (case, event_tenant) in [
        ("tenant A's event", Some(tenant(TENANT_A))),
        ("a platform-wide event", None),
    ] {
        let setup = database.setup().await;
        create_tables(&setup).await;
        let published = publish(&setup, "w", event_tenant).await;
        let consumer = setup.subscribe(
            ConsumerConfig::new("trespasser", &setup.producer_schema).expect("config"),
            Trespasser {
                other: tenant(TENANT_B),
            },
        );
        consumer.ensure_subscription().await.expect("subscription");
        let progress = consumer.poll_once().await.expect("poll");
        let Outcome::Retry { event_id, error } = progress.outcome else {
            panic!("{case}: expected the write to fail, got {progress:?}");
        };
        assert_eq!(event_id, published.event_id, "{case}");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some("42501"),
            "{case}: row-level security refuses the row: {error:?}"
        );
        // Nothing was written, and the event waits in place.
        let notes: i64 = sqlx::query_scalar("SELECT count(*) FROM note WHERE tenant_id = $1")
            .bind(TENANT_B.parse::<uuid::Uuid>().expect("uuid"))
            .fetch_one(&setup.consumer)
            .await
            .expect("notes");
        assert_eq!(notes, 0, "{case}");
        assert_eq!(
            consumer.position().await.expect("position"),
            Position::START,
            "{case}"
        );
        println!("{case}: a write to tenant B fails with SQLSTATE 42501");
    }
}

#[tokio::test]
async fn a_service_role_that_can_read_across_tenants_is_refused() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let (ps, app) = (&setup.producer_schema, &setup.consumer_role);
    // A migration that opens the log to the consumer's service role instead of its platform
    // role.
    database.grant_log_read(&setup, app, true).await;
    let (platform, work) =
        database.consumer_pools(&setup.consumer_platform, app, Duration::from_secs(2));
    let consumer = outbox::Consumer::new(
        &platform,
        work,
        ConsumerConfig::new("refused", ps).expect("config"),
        Recorder {
            subscription: "refused".to_owned(),
        },
    );
    let error = consumer
        .ensure_subscription()
        .await
        .expect_err("the service role's sessions are refused");
    let ConsumerError::UnsafeSession(problem) = &error else {
        panic!("expected a refused session, got {error:?}");
    };
    assert_eq!(
        problem,
        &UnsafeSession::PermissivePolicy {
            policy: format!("{ps}.outbox_event policy service_read"),
            role: app.clone(),
        }
    );
    assert!(error.is_retryable(), "a migration can fix it");
    println!("service role with a cross-tenant read policy: {error}");
}

/// A reader that cannot see every tenant's events stops the consumer instead of delivering
/// nothing and reporting no lag.
#[tokio::test]
async fn a_reader_without_the_read_policy_stops_the_consumer() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    publish(&setup, "w", Some(tenant(TENANT_A))).await;
    let (ps, app) = (&setup.producer_schema, &setup.consumer_role);
    // The service role may read the table, but only the tenant policy applies to it: without a
    // tenant it sees no event, so as a reader it would deliver nothing and report no lag.
    // Service-kit's session check refuses such a reader's sessions (Laysi/trustiot-v2#366).
    database.grant_log_read(&setup, app, false).await;
    let (platform, work) = database.consumer_pools(app, app, Duration::from_secs(10));
    let consumer = outbox::Consumer::new(
        &platform,
        work,
        ConsumerConfig::new("blind", ps).expect("config"),
        Recorder {
            subscription: "blind".to_owned(),
        },
    );
    let error = consumer
        .ensure_subscription()
        .await
        .expect_err("a reader that sees no tenant");
    let ConsumerError::UnsafeSession(problem) = &error else {
        panic!("expected a refused session, got {error:?}");
    };
    assert_eq!(
        problem,
        &UnsafeSession::PlatformTenantOnly {
            table: format!("{ps}.outbox_event"),
            role: app.clone(),
            command: "SELECT".to_owned(),
        }
    );
    assert!(error.is_retryable(), "a migration can fix it");
    println!("reader with only the tenant policy: {error}");

    // A read policy for the platform role that opens less than every tenant passes the session
    // check; ensure_subscription refuses it.
    database.narrow_log_read(&setup).await;
    let consumer = setup.subscribe(
        ConsumerConfig::new("narrow", ps).expect("config"),
        Recorder {
            subscription: "narrow".to_owned(),
        },
    );
    let error = consumer
        .ensure_subscription()
        .await
        .expect_err("a reader that sees platform-wide events only");
    assert!(
        matches!(&error, ConsumerError::CannotReadAllTenants { source_schema } if source_schema == ps),
        "{error:?}"
    );
    assert!(error.is_retryable());
    println!("reader without consumer_read: {error}");
}

#[tokio::test]
async fn an_event_whose_envelope_names_another_tenant_is_dead_lettered() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let forged = publish(&setup, "w", Some(tenant(TENANT_A))).await;
    // The log row says tenant B (what the producer's row-level security checked); the envelope
    // still says tenant A.
    sqlx::query("UPDATE outbox_event SET tenant_id = $1 WHERE event_id = $2")
        .bind(TENANT_B.parse::<uuid::Uuid>().expect("uuid"))
        .bind(forged.event_id.as_uuid())
        .execute(&setup.producer)
        .await
        .expect("forge the row");
    let consistent = publish(&setup, "w", Some(tenant(TENANT_B))).await;

    let consumer = setup.subscribe(
        ConsumerConfig::new("forged", &setup.producer_schema).expect("config"),
        Recorder {
            subscription: "forged".to_owned(),
        },
    );
    consumer.ensure_subscription().await.expect("subscription");
    let progress = drain(&consumer, DRAIN).await;
    assert_eq!((progress.delivered, progress.dead_lettered), (1, 1));
    let dead: (String, String) =
        sqlx::query_as("SELECT event_id::text, reason FROM outbox_dead_letter")
            .fetch_one(&setup.consumer)
            .await
            .expect("dead letter");
    assert_eq!(
        dead,
        (
            forged.event_id.to_string(),
            "envelope tenant differs from the log row".to_owned()
        )
    );
    let seen: Vec<String> = sqlx::query_scalar("SELECT event_id::text FROM seen")
        .fetch_all(&setup.consumer)
        .await
        .expect("seen");
    assert_eq!(seen, [consistent.event_id.to_string()]);
}

/// Every aggregate as the superuser sees it: kind/id, tenant, last sequence number and ordering
/// transaction.
const ALL_AGGREGATES: &str = "SELECT aggregate_type || '/' || aggregate_id, tenant_id::text, \
     last_seq, last_tx FROM outbox_aggregate ORDER BY 1";
/// The aggregates a role sees.
const VISIBLE_AGGREGATES: &str =
    "SELECT aggregate_type || '/' || aggregate_id FROM outbox_aggregate ORDER BY 1";
/// Rewinds an aggregate's sequence, as a service role that wanted to reorder another tenant's
/// later events would.
const REWIND_AGGREGATE: &str = "UPDATE outbox_aggregate SET last_seq = 0, last_tx = 0 \
     WHERE aggregate_type = $1 AND aggregate_id = $2";
/// Creates an aggregate of the tenant `$1` (or of the platform) directly, without an event.
const PLANT_AGGREGATE: &str = "INSERT INTO outbox_aggregate \
     (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx) \
     VALUES ('widget', 'planted', $1, 1, 0)";
/// Moves every aggregate the role sees into the tenant `$1`.
const MOVE_AGGREGATES: &str = "UPDATE outbox_aggregate SET tenant_id = $1";
/// The ordering transactions of an aggregate's events, in sequence order.
const EVENT_ORDER: &str = "SELECT tx_order FROM outbox_event \
     WHERE aggregate_type = $1 AND aggregate_id = $2 ORDER BY aggregate_seq";

/// One aggregate as [`ALL_AGGREGATES`] reads it.
type AggregateRow = (String, Option<String>, i64, i64);

/// The SQLSTATE of a database error.
fn code(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// The SQLSTATE of the database error behind a failed publish.
fn publish_code(error: &PublishError) -> Option<String> {
    match error {
        PublishError::Database(error) => code(error),
        _ => None,
    }
}

/// Publishes `Counted { n }` for `aggregate` in a transaction of the producer's service role in
/// `tenant`; commits on success and rolls back on failure.
async fn publish_in(
    service: &BackgroundTenantDb,
    tenant: TenantId,
    aggregate: &Aggregate,
    n: i64,
) -> Result<Published, PublishError> {
    let mut tx = service.begin(tenant, "publish-test").await.expect("begin");
    match outbox::publish(tx.transaction(), aggregate, &Counted { n }, Some(tenant)).await {
        Ok(published) => {
            tx.commit().await.expect("commit");
            Ok(published)
        }
        Err(error) => {
            tx.rollback().await.expect("rollback");
            Err(error)
        }
    }
}

/// Publishes `Counted { n }` for `aggregate` with `tenant` as the producer's platform role, which
/// sets no tenant; commits on success and rolls back on failure.
async fn publish_as_platform(
    platform: &PlatformDb,
    aggregate: &Aggregate,
    tenant: Option<TenantId>,
    n: i64,
) -> Result<Published, PublishError> {
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assets));
    let mut tx = platform.begin(actor, "publish-test").await.expect("begin");
    match outbox::publish(tx.transaction(), aggregate, &Counted { n }, tenant).await {
        Ok(published) => {
            tx.commit().await.expect("commit");
            Ok(published)
        }
        Err(error) => {
            tx.rollback().await.expect("rollback");
            Err(error)
        }
    }
}

/// Laysi/trustiot-v2#413: `outbox_aggregate` holds each aggregate's sequence and ordering
/// transaction. Under service-kit's `sql/outbox-aggregate.sql`, the producer's service role in a
/// tenant transaction neither reads nor changes another tenant's aggregate, through `publish`,
/// `aggregate_version` or plain SQL, and the producer's platform role reaches platform
/// aggregates only. The refused attempts leave every row as it was, and the owner tenant's
/// sequence and order go on where they were.
#[tokio::test]
async fn the_producer_roles_reach_only_their_own_aggregates() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let (_, service) = TenantDb::connect_lazy_with_background(
        database.postgres.options(&setup.producer_role),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    let platform = PlatformDb::connect_lazy(
        database.postgres.options(&setup.producer_platform),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let (a, b) = (tenant(TENANT_A), tenant(TENANT_B));
    let of_a = Aggregate::new("widget", "of-a").expect("aggregate");
    let of_b = Aggregate::new("widget", "of-b").expect("aggregate");
    let catalog = Aggregate::new("catalog", "shared").expect("aggregate");

    // Each aggregate's first event: tenant A's, tenant B's, and a platform-wide one.
    let first = publish_in(&service, a, &of_a, 1).await.expect("A's event");
    assert_eq!(first.aggregate_seq, 1);
    publish_in(&service, b, &of_b, 2).await.expect("B's event");
    publish_as_platform(&platform, &catalog, None, 3)
        .await
        .expect("a platform-wide event");
    let before: Vec<AggregateRow> = sqlx::query_as(ALL_AGGREGATES)
        .fetch_all(&setup.producer)
        .await
        .expect("read the aggregates");
    assert_eq!(
        before
            .iter()
            .map(|(name, tenant, ..)| (name.as_str(), tenant.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("catalog/shared", None),
            ("widget/of-a", Some(TENANT_A)),
            ("widget/of-b", Some(TENANT_B)),
        ],
        "each aggregate belongs to the tenant of its first event, or to the platform"
    );

    // Tenant B's transaction sees its own aggregate only, reads the others' versions as absent,
    // and changes nothing of A's or the platform's.
    let mut tx = service.begin(b, "probe").await.expect("begin");
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_all(&mut *tx)
        .await
        .expect("read as B");
    assert_eq!(visible, ["widget/of-b"], "B sees its own aggregate only");
    for aggregate in [&of_a, &catalog] {
        let version = outbox::aggregate_version(&mut tx, aggregate)
            .await
            .expect("aggregate version as B");
        assert_eq!(version, None, "B reads {aggregate}'s version as absent");
        let rewound = sqlx::query(REWIND_AGGREGATE)
            .bind(aggregate.kind())
            .bind(aggregate.id())
            .execute(&mut *tx)
            .await
            .expect("an update that finds no row");
        assert_eq!(rewound.rows_affected(), 0, "B cannot rewind {aggregate}");
    }
    tx.rollback().await.expect("rollback");
    for (case, owner) in [("tenant A", Some(a)), ("the platform", None)] {
        let mut tx = service.begin(b, "probe").await.expect("begin");
        let error = sqlx::query(PLANT_AGGREGATE)
            .bind(owner.map(|owner| *owner.as_uuid()))
            .execute(&mut *tx)
            .await
            .expect_err("B plants an aggregate");
        assert_eq!(code(&error).as_deref(), Some("42501"), "{case}: {error}");
        tx.rollback().await.expect("rollback");
        println!("tenant B creating an aggregate of {case}: refused ({error})");
    }
    let mut tx = service.begin(b, "probe").await.expect("begin");
    let error = sqlx::query(MOVE_AGGREGATES)
        .bind(a.as_uuid())
        .execute(&mut *tx)
        .await
        .expect_err("B moves its aggregate into A");
    assert_eq!(code(&error).as_deref(), Some("42501"), "{error}");
    tx.rollback().await.expect("rollback");
    println!("tenant B moving its aggregate into tenant A: refused ({error})");
    for aggregate in [&of_a, &catalog] {
        let error = publish_in(&service, b, aggregate, 4)
            .await
            .expect_err("B's event for an aggregate that is not B's");
        assert_eq!(
            publish_code(&error).as_deref(),
            Some("42501"),
            "{aggregate}: {error}"
        );
        println!("tenant B publishing for {aggregate}: refused ({error})");
    }

    // Outside a tenant, the service role sees no aggregate at all.
    let mut tx = service
        .begin_without_tenant("probe")
        .await
        .expect("begin without a tenant");
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_all(&mut *tx)
        .await
        .expect("read without a tenant");
    assert!(visible.is_empty(), "without a tenant: {visible:?}");
    tx.rollback().await.expect("rollback");

    // The platform role sees and writes platform aggregates only.
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assets));
    let mut tx = platform.begin(actor, "probe").await.expect("begin");
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_all(&mut *tx)
        .await
        .expect("read as the platform role");
    assert_eq!(visible, ["catalog/shared"], "the platform role");
    let version = outbox::aggregate_version(&mut tx, &of_a)
        .await
        .expect("aggregate version as the platform role");
    assert_eq!(
        version, None,
        "the platform role reads A's version as absent"
    );
    tx.rollback().await.expect("rollback");
    let error = publish_as_platform(&platform, &of_a, None, 5)
        .await
        .expect_err("a platform-wide event for A's aggregate");
    assert_eq!(publish_code(&error).as_deref(), Some("42501"), "{error}");
    println!("the platform role publishing for widget/of-a: refused ({error})");
    let new = Aggregate::new("widget", "new").expect("aggregate");
    let error = publish_as_platform(&platform, &new, Some(a), 6)
        .await
        .expect_err("the platform role writes no tenant's aggregate");
    assert_eq!(publish_code(&error).as_deref(), Some("42501"), "{error}");
    println!("the platform role publishing a tenant A event: refused ({error})");

    // Every refused attempt left every aggregate as it was.
    let after: Vec<AggregateRow> = sqlx::query_as(ALL_AGGREGATES)
        .fetch_all(&setup.producer)
        .await
        .expect("read the aggregates");
    assert_eq!(after, before, "the refused attempts changed no aggregate");

    // Tenant A's sequence and order go on where they were.
    let second = publish_in(&service, a, &of_a, 7).await.expect("A's event");
    assert_eq!(second.aggregate_seq, 2, "A's sequence goes on at 2");
    let mut tx = service.begin(a, "probe").await.expect("begin");
    let version = outbox::aggregate_version(&mut tx, &of_a)
        .await
        .expect("aggregate version as A");
    assert_eq!(version, Some(2), "A reads its own version");
    tx.rollback().await.expect("rollback");
    let order: Vec<i64> = sqlx::query_scalar(EVENT_ORDER)
        .bind(of_a.kind())
        .bind(of_a.id())
        .fetch_all(&setup.producer)
        .await
        .expect("read A's events");
    assert_eq!(order.len(), 2, "two events of A's aggregate");
    assert!(
        order[0] <= order[1],
        "A's events keep their order: {order:?}"
    );
    println!(
        "outbox_aggregate: B saw widget/of-b only, the platform role catalog/shared only, a \
         session without a tenant nothing; A's sequence went on to 2"
    );
}
