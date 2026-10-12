//! The consumer under decision 29's tenant isolation, against a real Postgres with TLS (a
//! throwaway container; skipped without podman or docker unless `CI` is set):
//!
//! - it reads the producer's `outbox_event`, which has row-level security, as the consuming
//!   service's platform role and sees every tenant's events and platform-wide ones;
//! - each event is processed in a transaction of the service role in the event's tenant, so a
//!   handler sees and writes only that tenant's rows, and a platform-wide event's handler none;
//! - a service role that a policy lets read across tenants is refused by service-kit, and a
//!   reader without the read policy stops the consumer instead of reading nothing;
//! - an event whose envelope names another tenant than its log row is never processed.

mod support;

use std::error::Error as _;
use std::time::Duration;

use contract::TenantId;
use outbox::{
    Aggregate, ConsumerConfig, ConsumerError, Delivery, Handler, HandlerError, Outcome, Position,
};
use service_kit::postgres::UnsafeSession;
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
async fn publish(setup: &Setup, aggregate: &str, tenant: Option<TenantId>) -> outbox::Published {
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
        &UnsafeSession::PermissivePolicy(format!("{ps}.outbox_event policy service_read"))
    );
    assert!(error.is_retryable(), "a migration can fix it");
    println!("service role with a cross-tenant read policy: {error}");
}

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
