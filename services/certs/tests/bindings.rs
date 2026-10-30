//! Gateway bindings and tenant statuses (Laysi/trustiot-v2#632; `README.md`, "閘道器的綁定與租戶狀態"),
//! on the product's schema applied by the migration Job (`testkit::product`):
//!
//! - The real binary: its consumer's subscription to the assets service's events exists; a
//!   binding the assets service writes (`assets.gateway_bound` in the tenant's transaction, as
//!   its binding does) reaches `certs.gateway_bindings` with the tenant, the binding ID, when it
//!   was bound and the event's position; the event delivered again and the whole subscription
//!   replayed leave one row, unchanged (acceptance 1).
//! - A consumer of its own (same handler, its own subscription, so its replays and dead letters
//!   are its own): an older binding of the same gateway that arrives after a newer one is stored
//!   but does not become current (acceptance 2); an event with an invalid hardware ID and one
//!   without a tenant are dead-lettered, and the binding after them is stored (acceptance 4).
//! - The tenant statuses: only a newer version replaces the stored one; a tenant without a
//!   status reads as suspended (fail closed since Laysi/trustiot-v2#778), and so do a stored
//!   suspension, a deleted tenant and a state this build does not know (acceptance 3). A deleted
//!   tenant's row never changes again; an unrecognized one takes its own version again. A
//!   tenant's transaction cannot store another tenant's status.
//! - The platform role's reads see every tenant's bindings, take each gateway's latest binding,
//!   leave out an ended one, and each read logs `platform database access` with the certs
//!   service and the purpose (acceptance 5, with `tests/cross_tenant.rs`).
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use certs::bindings::{
    Binding, GatewayBoundHandler, Projection, SUBSCRIPTION, TenantStatus, store_tenant_status,
};
use chrono::{DateTime, Utc};
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{Event, EventEnvelope, EventId, GatewayBindingId, ServiceName, TenantId};
use outbox::{Aggregate, Consumer, ConsumerConfig, Position, Replay};
use protocol::GatewayId;
use serde::{Deserialize, Serialize};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, PlatformActor, PlatformDb, TenantDb};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::{Service, TestCa, wait_for};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// The roles these tests log in with: the certs service's two, and the assets service's, which
/// write the events as its binding does.
const LOGINS: &[&str] = &[
    "assets_app",
    "assets_platform",
    "certs_app",
    "certs_platform",
];

/// The log line of every applied binding (`GatewayBoundHandler`).
const APPLIED: &str = "applied assets.gateway_bound";
/// The INFO line of every platform role transaction (`PlatformDb::begin`).
const PLATFORM_TRANSACTION: &str = "platform database access";

/// The subscription of the real service's consumer, and what it reads.
const SUBSCRIPTION_ROW: &str =
    "SELECT source_schema FROM certs.outbox_subscription WHERE name = $1";
/// Whether a subscription processed an event. `$1` subscription, `$2` event ID.
const INBOX: &str = "SELECT count(*) FROM certs.outbox_inbox
WHERE subscription = $1 AND event_id = $2::uuid";
/// A subscription's dead letter of an event. `$1` subscription, `$2` event ID.
const DEAD_LETTER: &str = "SELECT reason FROM certs.outbox_dead_letter
WHERE subscription = $1 AND event_id = $2::uuid";
/// Every stored binding of a gateway, whole.
const ROWS: &str = "SELECT tenant_id::text, binding_id::text, bound_at, position_tx, position_id,
    unbound_at IS NULL, recorded_at
FROM certs.gateway_bindings WHERE hardware_id = $1 ORDER BY position_tx, position_id";
/// An event's position and time as the assets service's log has them.
const LOGGED: &str = "SELECT tx_order, id, (envelope ->> 'occurred_at')::timestamptz
FROM assets.outbox_event WHERE event_id = $1::uuid";
/// An event written straight into the assets service's log, sorted at `$1` (its `tx_order`):
/// `$2` event ID, `$3` tenant, `$4` binding ID (the aggregate), `$5` the envelope.
const LOG_EVENT: &str = "INSERT INTO assets.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES ($1, $2::uuid, 'assets.gateway_bound', $3::uuid, 'gateway_binding', $4, 1, $5::jsonb)";
/// A stored binding, as the consumer would store it. `$1` tenant, `$2` binding, `$3` hardware
/// ID, `$4` and `$5` its position, `$6` whether it has ended.
const STORE_BINDING: &str = "INSERT INTO certs.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id, unbound_at)
VALUES ($1::uuid, $2::uuid, $3, now() - interval '1 hour', $4, $5,
    CASE WHEN $6 THEN now() END)";
/// The statuses a tenant transaction sees.
const VISIBLE_STATUSES: &str = "SELECT tenant_id::text FROM certs.tenant_statuses";

/// `rows` of [`ROWS`].
type Row = (String, String, DateTime<Utc>, i64, i64, bool, DateTime<Utc>);

/// A hardware ID no other test uses.
fn hardware_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        GatewayBindingId::generate().to_string().replace('-', "")
    )
}

fn gateway(hardware_id: &str) -> GatewayId {
    GatewayId::new(hardware_id).expect("a hardware ID")
}

/// The assets service's role as its binding uses it: a tenant transaction per binding.
fn assets_writer(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("assets_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("the assets service role's pool")
    .1
}

/// The certs service's role: tenant transactions and the consumer's bookkeeping.
fn certs_work(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("certs_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Certs),
    )
    .expect("the certs service role's pool")
    .1
}

/// The certs service's platform role.
fn certs_platform(product: &ProductPostgres) -> PlatformDb {
    PlatformDb::connect_lazy(
        product.postgres.options("certs_platform", DATABASE),
        PgPoolOptions::new().max_connections(2),
    )
    .expect("the certs platform pool")
}

/// Writes `assets.gateway_bound` as the assets service's binding does: in `tenant`'s
/// transaction, the binding's own aggregate. Returns the binding and the event.
async fn bind(
    assets: &BackgroundTenantDb,
    tenant: TenantId,
    hardware_id: &str,
) -> (GatewayBindingId, EventId) {
    let binding_id = GatewayBindingId::generate();
    let event = GatewayBound {
        binding_id,
        hardware_id: gateway(hardware_id),
    };
    (
        binding_id,
        publish(assets, tenant, binding_id, &event).await,
    )
}

/// Writes `event` in `tenant`'s transaction of the assets service's role, in the aggregate of
/// `binding_id`.
async fn publish<E: Event>(
    assets: &BackgroundTenantDb,
    tenant: TenantId,
    binding_id: GatewayBindingId,
    event: &E,
) -> EventId {
    let mut tx = assets
        .begin(tenant, "test-bind")
        .await
        .expect("a tenant transaction of the assets service");
    let aggregate =
        Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding_id.to_string()).expect("an aggregate");
    let published = outbox::publish(tx.transaction(), &aggregate, event, Some(tenant))
        .await
        .expect("write the event");
    tx.commit().await.expect("commit the binding");
    published.event_id
}

/// `assets.gateway_bound` without the rules of its fields, to write what the assets service
/// never would.
#[derive(Serialize, Deserialize)]
struct UncheckedGatewayBound {
    binding_id: String,
    hardware_id: String,
}

impl Event for UncheckedGatewayBound {
    const TYPE: &'static str = GatewayBound::TYPE;
    const VERSION: u32 = GatewayBound::VERSION;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

async fn logged(admin: &mut PgConnection, event: EventId) -> (Position, DateTime<Utc>) {
    let (tx, id, occurred_at): (i64, i64, DateTime<Utc>) = sqlx::query_as(LOGGED)
        .bind(event.to_string())
        .fetch_one(admin)
        .await
        .expect("read the event in the assets log");
    (Position::new(tx, id), occurred_at)
}

async fn rows(admin: &mut PgConnection, hardware_id: &str) -> Vec<Row> {
    sqlx::query_as(ROWS)
        .bind(hardware_id)
        .fetch_all(admin)
        .await
        .expect("read the stored bindings")
}

async fn processed(admin: &mut PgConnection, subscription: &str, event: EventId) -> bool {
    let count: i64 = sqlx::query_scalar(INBOX)
        .bind(subscription)
        .bind(event.to_string())
        .fetch_one(admin)
        .await
        .expect("read the inbox");
    count == 1
}

async fn dead_letter(
    admin: &mut PgConnection,
    subscription: &str,
    event: EventId,
) -> Option<String> {
    sqlx::query_scalar(DEAD_LETTER)
        .bind(subscription)
        .bind(event.to_string())
        .fetch_optional(admin)
        .await
        .expect("read the dead letters")
}

/// Moves the subscription to `after` (see `outbox::replay_from`).
async fn replay(work: &BackgroundTenantDb, subscription: &str, after: Position, mode: Replay) {
    let mut tx = work
        .begin_without_tenant("test-replay")
        .await
        .expect("a transaction of the certs service role");
    outbox::replay_from(&mut tx, subscription, after, mode)
        .await
        .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
}

/// The service's `applied assets.gateway_bound` lines for `hardware_id`.
fn applied(service: &Service, hardware_id: &str) -> Vec<serde_json::Value> {
    service
        .stdout
        .messages(APPLIED)
        .into_iter()
        .filter(|line| line["fields"]["hardware_id"] == hardware_id)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_reaches_the_service_once_whatever_is_replayed() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let service = Service::start(&product, &ca, &[]).await;
    let mut admin = product.superuser().await;
    let assets = assets_writer(&product);
    let work = certs_work(&product);
    let projection = Projection::new(certs_platform(&product));

    // The consumer's subscription reads the assets service's log.
    let source = wait_for("the consumer's subscription", || async {
        let mut admin = product.superuser().await;
        sqlx::query_scalar::<_, String>(SUBSCRIPTION_ROW)
            .bind(SUBSCRIPTION)
            .fetch_optional(&mut admin)
            .await
            .expect("read the subscriptions")
    })
    .await;
    assert_eq!(source, "assets");

    // 1. A platform administrator binds a gateway: the service stores it with the tenant, the
    //    binding, the event's time and position.
    let tenant = TenantId::generate();
    let hardware = hardware_id("tg-bind");
    let (binding_id, event) = bind(&assets, tenant, &hardware).await;
    wait_for("the binding processed", || async {
        let mut admin = product.superuser().await;
        processed(&mut admin, SUBSCRIPTION, event)
            .await
            .then_some(())
    })
    .await;
    let (position, occurred_at) = logged(&mut admin, event).await;
    let stored = rows(&mut admin, &hardware).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let (row_tenant, row_binding, bound_at, position_tx, position_id, holds, _) = &stored[0];
    assert_eq!(row_tenant, &tenant.to_string());
    assert_eq!(row_binding, &binding_id.to_string());
    assert_eq!(bound_at, &occurred_at);
    assert_eq!((*position_tx, *position_id), (position.tx(), position.id()));
    assert!(holds, "a new binding holds");
    let expected = Binding {
        hardware_id: gateway(&hardware),
        tenant_id: tenant,
        binding_id,
        bound_at: occurred_at,
    };
    assert_eq!(
        projection
            .gateway_tenant(&gateway(&hardware), "test-renewal")
            .await
            .expect("read the gateway's tenant"),
        Some(expected.clone())
    );
    let bound = projection
        .bound_gateways("test-sync")
        .await
        .expect("list the bound gateways");
    assert!(bound.contains(&expected), "{bound:?}");
    let first = wait_for("the service's log line", || async {
        let lines = applied(&service, &hardware);
        (!lines.is_empty()).then_some(lines)
    })
    .await;
    assert_eq!(first[0]["level"], "INFO", "{}", first[0]);
    assert_eq!(first[0]["fields"]["tenant_id"], tenant.to_string());
    println!("evidence: stored binding {stored:?}; log line {}", first[0]);

    // 2. The same event delivered again: one row, unchanged.
    replay(
        &work,
        SUBSCRIPTION,
        position.just_before(),
        Replay::Reprocess,
    )
    .await;
    wait_for("the event handled a second time", || async {
        (applied(&service, &hardware).len() >= 2).then_some(())
    })
    .await;
    assert_eq!(rows(&mut admin, &hardware).await, stored, "delivered again");

    // 3. The whole subscription replayed: still one row, unchanged.
    replay(&work, SUBSCRIPTION, Position::START, Replay::Reprocess).await;
    wait_for("the event handled after the whole replay", || async {
        (applied(&service, &hardware).len() >= 3).then_some(())
    })
    .await;
    wait_for("the event processed after the whole replay", || async {
        let mut admin = product.superuser().await;
        processed(&mut admin, SUBSCRIPTION, event)
            .await
            .then_some(())
    })
    .await;
    assert_eq!(
        rows(&mut admin, &hardware).await,
        stored,
        "after the replay"
    );
    let lines = applied(&service, &hardware);
    // Another consumer of this test binary may have stored it first, so the service may never
    // have created it itself; it created it at most once.
    let created = lines
        .iter()
        .filter(|line| line["fields"]["created"] == true)
        .count();
    assert!(created <= 1, "{lines:?}");
    println!(
        "evidence: {} applications of {hardware}, {created} created a row; rows unchanged",
        lines.len()
    );
}

/// Delivers what is readable to `consumer` until `done` holds.
async fn poll_until<F, Fut>(consumer: &Consumer<GatewayBoundHandler>, what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    wait_for(what, || {
        let check = done();
        async move {
            consumer.poll_once().await.expect("poll the assets log");
            check.await.then_some(())
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn late_and_invalid_bindings_neither_win_nor_block() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = assets_writer(&product);
    let work = certs_work(&product);
    let platform = certs_platform(&product);
    let projection = Projection::new(platform.clone());
    // A subscription of its own, so this test's replays and dead letters are its own.
    let subscription = "certs.test-late-bindings";
    let consumer = Consumer::new(
        &platform,
        work.clone(),
        ConsumerConfig::new(subscription, "assets").expect("the consumer's settings"),
        GatewayBoundHandler,
    );
    consumer
        .ensure_subscription()
        .await
        .expect("create the subscription");

    // 1. The gateway is bound to tenant B.
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());
    let hardware = hardware_id("tg-late");
    let (newer, newer_event) = bind(&assets, tenant_b, &hardware).await;
    poll_until(&consumer, "the newer binding processed", || {
        let product = &product;
        async move {
            let mut admin = product.superuser().await;
            processed(&mut admin, subscription, newer_event).await
        }
    })
    .await;
    let (newer_position, _) = logged(&mut admin, newer_event).await;

    // 2. An older binding of the same gateway to tenant A, sorted before it in the log, arrives
    //    afterwards (a replayed range, as a fixed dead letter is replayed).
    let older = GatewayBindingId::generate();
    let older_event = EventId::generate();
    let mut envelope = EventEnvelope::new(
        &GatewayBound {
            binding_id: older,
            hardware_id: gateway(&hardware),
        },
        Some(tenant_a),
    )
    .expect("an envelope");
    envelope.id = older_event;
    sqlx::query(LOG_EVENT)
        .bind(newer_position.tx() - 1)
        .bind(older_event.to_string())
        .bind(tenant_a.to_string())
        .bind(older.to_string())
        .bind(serde_json::to_string(&envelope).expect("the envelope as JSON"))
        .execute(&mut admin)
        .await
        .expect("write the older binding into the log");
    let (older_position, _) = logged(&mut admin, older_event).await;
    assert!(
        older_position < newer_position,
        "the older event sorts first"
    );
    replay(
        &work,
        subscription,
        older_position.just_before(),
        Replay::SkipProcessed,
    )
    .await;
    poll_until(&consumer, "the older binding processed", || {
        let product = &product;
        async move {
            let mut admin = product.superuser().await;
            processed(&mut admin, subscription, older_event).await
        }
    })
    .await;
    let stored = rows(&mut admin, &hardware).await;
    assert_eq!(stored.len(), 2, "both bindings are stored: {stored:?}");
    let current = projection
        .gateway_tenant(&gateway(&hardware), "test-renewal")
        .await
        .expect("read the gateway's tenant")
        .expect("the gateway is bound");
    assert_eq!(
        (current.tenant_id, current.binding_id),
        (tenant_b, newer),
        "the newer binding stays current"
    );
    let listed: Vec<Binding> = projection
        .bound_gateways("test-sync")
        .await
        .expect("list the bound gateways")
        .into_iter()
        .filter(|binding| binding.hardware_id.as_str() == hardware)
        .collect();
    assert_eq!(listed, std::slice::from_ref(&current));
    println!("evidence: late older binding stored {stored:?}; current {current:?}");

    // 3. An invalid hardware ID, and a binding without a tenant (a platform-wide event, which only
    //    the assets service's platform role can write), then a valid binding.
    let invalid_binding = GatewayBindingId::generate();
    let invalid = publish(
        &assets,
        tenant_a,
        invalid_binding,
        &UncheckedGatewayBound {
            binding_id: invalid_binding.to_string(),
            hardware_id: "not a hardware id!".to_owned(),
        },
    )
    .await;
    let platform_assets = PlatformDb::connect_lazy(
        product.postgres.options("assets_platform", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the assets platform pool");
    let tenantless_binding = GatewayBindingId::generate();
    let mut tx = platform_assets
        .begin(
            PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
            "test-tenantless-binding",
        )
        .await
        .expect("an assets platform transaction");
    let tenantless = outbox::publish(
        tx.transaction(),
        &Aggregate::new(GATEWAY_BINDING_AGGREGATE, tenantless_binding.to_string())
            .expect("an aggregate"),
        &GatewayBound {
            binding_id: tenantless_binding,
            hardware_id: gateway(&hardware_id("tg-tenantless")),
        },
        None,
    )
    .await
    .expect("write a binding without a tenant")
    .event_id;
    tx.commit().await.expect("commit");
    let after = hardware_id("tg-after");
    let (after_binding, after_event) = bind(&assets, tenant_a, &after).await;
    poll_until(
        &consumer,
        "the binding after the invalid ones processed",
        || {
            let product = &product;
            async move {
                let mut admin = product.superuser().await;
                processed(&mut admin, subscription, after_event).await
            }
        },
    )
    .await;
    let invalid_reason = dead_letter(&mut admin, subscription, invalid)
        .await
        .expect("the invalid hardware ID is dead-lettered");
    let tenantless_reason = dead_letter(&mut admin, subscription, tenantless)
        .await
        .expect("the binding without a tenant is dead-lettered");
    assert!(
        tenantless_reason.contains("assets.gateway_bound without a tenant"),
        "{tenantless_reason}"
    );
    assert!(!processed(&mut admin, subscription, invalid).await);
    assert!(!processed(&mut admin, subscription, tenantless).await);
    assert_eq!(
        projection
            .gateway_tenant(&gateway(&after), "test-renewal")
            .await
            .expect("read the gateway's tenant")
            .map(|binding| (binding.tenant_id, binding.binding_id)),
        Some((tenant_a, after_binding))
    );
    println!(
        "evidence: dead letters {invalid_reason:?} and {tenantless_reason:?}; the binding after \
         them is current"
    );
}

/// Each captured event's message and other fields.
type Captured = Vec<(String, BTreeMap<String, String>)>;

/// Captures the events of the current thread.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Captured>>);

impl Capture {
    /// Captures until the guard is dropped. tracing caches per callsite whether any subscriber
    /// wants its events; a second dispatcher kept alive for the whole binary makes it ask every
    /// live dispatcher, so a callsite another test's thread reached first is not cached as
    /// unwanted (services/assistant/tests/support/mod.rs, Laysi/trustiot-v2#581).
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        static KEEPER: OnceLock<tracing::Dispatch> = OnceLock::new();
        KEEPER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    /// The fields of every event whose message is `message`.
    fn messages(&self, message: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .expect("the lock")
            .iter()
            .filter(|(seen, _)| seen == message)
            .map(|(_, fields)| fields.clone())
            .collect()
    }
}

struct Visitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        event.record(&mut Visitor(&mut fields));
        let message = fields.remove("message").unwrap_or_default();
        self.0.lock().expect("the lock").push((message, fields));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn statuses_keep_the_newest_version_and_platform_reads_see_every_tenant() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let work = certs_work(&product);
    let projection = Projection::new(certs_platform(&product));
    let (capture, _guard) = Capture::install();
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());

    // 1. No status yet: suspended (fail closed, Laysi/trustiot-v2#778).
    assert!(
        projection
            .tenant_suspended(tenant_a, "test-renewal")
            .await
            .expect("read the status")
    );
    // 2. Only a newer version replaces the stored one; the writes run in the tenant's
    //    transaction, as a consumer of the tenant's events would run them.
    for (status, version, stored, suspended) in [
        (TenantStatus::Suspended, 2, true, true),
        (TenantStatus::Active, 1, false, true),
        (TenantStatus::Active, 2, false, true),
        (TenantStatus::Active, 3, true, false),
        (TenantStatus::Suspended, 4, true, true),
    ] {
        let mut tx = work
            .begin(tenant_a, "test-status")
            .await
            .expect("a tenant transaction");
        let was_stored = store_tenant_status(&mut tx, tenant_a, status, version)
            .await
            .expect("store the status");
        tx.commit().await.expect("commit");
        assert_eq!(was_stored, stored, "{status:?} at version {version}");
        assert_eq!(
            projection
                .tenant_suspended(tenant_a, "test-renewal")
                .await
                .expect("read the status"),
            suspended,
            "after {status:?} at version {version}"
        );
    }
    assert!(
        projection
            .tenant_suspended(tenant_b, "test-renewal")
            .await
            .expect("read the status"),
        "another tenant, without a status, reads as suspended"
    );
    // 3. A tenant's transaction cannot store another tenant's status, and sees only its own.
    let mut tx = work
        .begin(tenant_a, "test-status")
        .await
        .expect("a tenant transaction");
    let refused = store_tenant_status(&mut tx, tenant_b, TenantStatus::Suspended, 9)
        .await
        .expect_err("another tenant's status is refused");
    let code = refused
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned());
    assert_eq!(code.as_deref(), Some("42501"), "{refused}");
    tx.rollback().await.expect("roll back");
    let mut tx = work
        .begin(tenant_b, "test-status")
        .await
        .expect("a tenant transaction");
    store_tenant_status(&mut tx, tenant_b, TenantStatus::Active, 1)
        .await
        .expect("store tenant B's status");
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_STATUSES)
        .fetch_all(&mut *tx)
        .await
        .expect("read the statuses");
    tx.commit().await.expect("commit");
    assert_eq!(visible, [tenant_b.to_string()]);

    // 4. The platform role reads every tenant's bindings: each gateway's latest binding, none
    //    when it has ended.
    let (moved, ended, kept) = (
        hardware_id("tg-moved"),
        hardware_id("tg-ended"),
        hardware_id("tg-kept"),
    );
    let ids: Vec<GatewayBindingId> = (0..4).map(|_| GatewayBindingId::generate()).collect();
    for (tenant, binding, hardware, position, has_ended) in [
        (tenant_a, ids[0], &moved, (10, 1), false),
        (tenant_b, ids[1], &moved, (20, 1), false),
        (tenant_a, ids[2], &ended, (10, 2), true),
        (tenant_b, ids[3], &kept, (10, 3), false),
    ] {
        sqlx::query(STORE_BINDING)
            .bind(tenant.to_string())
            .bind(binding.to_string())
            .bind(hardware)
            .bind(position.0)
            .bind(position.1)
            .bind(has_ended)
            .execute(&mut admin)
            .await
            .expect("store a binding");
    }
    let tenant_of = |binding: Option<Binding>| binding.map(|b| (b.tenant_id, b.binding_id));
    assert_eq!(
        tenant_of(
            projection
                .gateway_tenant(&gateway(&moved), "test-renewal")
                .await
                .expect("read")
        ),
        Some((tenant_b, ids[1])),
        "the latest binding, in another tenant than the first"
    );
    assert_eq!(
        projection
            .gateway_tenant(&gateway(&ended), "test-renewal")
            .await
            .expect("read"),
        None,
        "an ended binding is no binding"
    );
    assert_eq!(
        projection
            .gateway_tenant(&gateway(&hardware_id("tg-never")), "test-renewal")
            .await
            .expect("read"),
        None,
        "a gateway never bound"
    );
    let listed: Vec<(String, TenantId, GatewayBindingId)> = projection
        .bound_gateways("test-sync")
        .await
        .expect("list")
        .into_iter()
        .filter(|binding| {
            [moved.as_str(), ended.as_str(), kept.as_str()].contains(&binding.hardware_id.as_str())
        })
        .map(|binding| {
            (
                binding.hardware_id.as_str().to_owned(),
                binding.tenant_id,
                binding.binding_id,
            )
        })
        .collect();
    let mut expected = vec![
        (moved.clone(), tenant_b, ids[1]),
        (kept.clone(), tenant_b, ids[3]),
    ];
    expected.sort();
    assert_eq!(listed, expected, "both tenants' gateways, byte order");

    // 5. Every read above is in the audit log: the certs service as the actor, the caller's
    //    purpose. 11 reads: 7 statuses, 3 gateways, 1 listing.
    let lines = capture.messages(PLATFORM_TRANSACTION);
    let purposes: Vec<&str> = lines
        .iter()
        .map(|fields| {
            assert_eq!(fields.get("actor").map(String::as_str), Some("service"));
            assert_eq!(fields.get("service").map(String::as_str), Some("certs"));
            fields.get("purpose").map_or("", String::as_str)
        })
        .collect();
    let count = |purpose: &str| purposes.iter().filter(|seen| **seen == purpose).count();
    assert_eq!(
        (count("test-renewal"), count("test-sync"), purposes.len()),
        (10, 1, 11),
        "{lines:?}"
    );
    println!(
        "evidence: {} audit lines, e.g. {:?}",
        lines.len(),
        lines.first()
    );
}

/// The status a tenant's transaction stores and the platform role reads, as one row.
const STATUS_ROW: &str =
    "SELECT status, version FROM certs.tenant_statuses WHERE tenant_id = $1::uuid";

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_tenant_is_final_and_an_unrecognized_state_takes_its_version_again() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let work = certs_work(&product);
    let projection = Projection::new(certs_platform(&product));
    let (deleted, future) = (TenantId::generate(), TenantId::generate());
    let store = |tenant: TenantId, status: TenantStatus, version: i64| {
        let work = work.clone();
        async move {
            let mut tx = work
                .begin(tenant, "test-status")
                .await
                .expect("a tenant transaction");
            let stored = store_tenant_status(&mut tx, tenant, status, version)
                .await
                .expect("store the status");
            tx.commit().await.expect("commit");
            stored
        }
    };
    let row = |tenant: TenantId| {
        let product = product.clone();
        async move {
            let row: (String, i64) = sqlx::query_as(STATUS_ROW)
                .bind(tenant.to_string())
                .fetch_one(&mut product.superuser().await)
                .await
                .expect("read the row");
            row
        }
    };

    // A deleted tenant keeps its row whatever a later version says, and reads as suspended.
    assert!(store(deleted, TenantStatus::Active, 1).await);
    assert!(store(deleted, TenantStatus::Suspended, 2).await);
    assert!(store(deleted, TenantStatus::Deleted, 3).await);
    for (status, version) in [
        (TenantStatus::Active, 4),
        (TenantStatus::Unrecognized, 5),
        (TenantStatus::Deleted, 3),
        (TenantStatus::Active, 3),
    ] {
        assert!(
            !store(deleted, status, version).await,
            "{status:?} at {version} over deleted"
        );
    }
    assert_eq!(row(deleted).await, ("deleted".to_owned(), 3));
    assert!(
        projection
            .tenant_suspended(deleted, "test-renewal")
            .await
            .expect("read")
    );

    // A state this build does not know reads as suspended; the same version stated in a way a
    // newer build knows replaces it (a replay after an upgrade), an older one never does, and a
    // known state is not replaced at its own version.
    assert!(store(future, TenantStatus::Active, 1).await);
    assert!(store(future, TenantStatus::Unrecognized, 2).await);
    assert!(
        projection
            .tenant_suspended(future, "test-renewal")
            .await
            .expect("read")
    );
    assert!(!store(future, TenantStatus::Active, 1).await, "older");
    assert!(
        !store(future, TenantStatus::Unrecognized, 2).await,
        "the same unknown state again writes nothing"
    );
    assert!(
        store(future, TenantStatus::Active, 2).await,
        "same version, known"
    );
    assert_eq!(row(future).await, ("active".to_owned(), 2));
    assert!(
        !store(future, TenantStatus::Suspended, 2).await,
        "a known state at its own version stays"
    );
    assert!(
        !projection
            .tenant_suspended(future, "test-renewal")
            .await
            .expect("read")
    );

    // The table refuses any other value, and a negative version.
    let refused = sqlx::query(
        "INSERT INTO certs.tenant_statuses (tenant_id, status, version) VALUES ($1::uuid, $2, 1)",
    )
    .bind(TenantId::generate().to_string())
    .bind("archived")
    .execute(&mut admin)
    .await
    .expect_err("an unknown status is refused");
    let code = refused
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned());
    assert_eq!(code.as_deref(), Some("23514"), "{refused}");
    println!(
        "evidence: deleted stays (deleted, 3) after active 4, unrecognized 5; unrecognized 2 \
         replaced by active 2, not by active 1; unknown status value refused with 23514"
    );
}
