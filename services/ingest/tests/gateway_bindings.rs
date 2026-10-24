//! The gateway binding projection (Laysi/trustiot-v2#640, acceptance 2; services/ingest/README.md,
//! "閘道器綁定對照表"): after a platform administrator binds a gateway, ingest's table has the
//! gateway's tenant and binding ID and the lookup answers them; the same event delivered again
//! and a replay of the whole subscription leave one row; an older binding's event that arrives
//! late does not replace the newer binding, and a newer one does; an event that can never be
//! recorded is dead-lettered; the platform role only reads.
//!
//! The ingest binary runs as a real process (its consumer) on the product's schema, applied by the
//! migration Job (`testkit::product`). The binding is made by the assets service's own code
//! (`assets::gateways::binding::GatewayBinder`), the event's only producer; the events that
//! model a late or a broken delivery are written into the assets service's outbox with the
//! outbox's own writer, in the tenant's transaction of `assets_app`, as the producer would. The
//! ClickHouse address points nowhere: the consumer does not need it, and readiness is
//! `tests/startup.rs`'s.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database. Run with `--nocapture` to see the
//! table after each step.
#![allow(
    clippy::print_stdout,
    reason = "prints the table after each step as the test's evidence"
)]

mod support;

use assets::gateways::binding::{GatewayBinder, Outcome};
use chrono::TimeDelta;
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{EventEnvelope, GatewayBindingId, ServiceName, TenantId};
use ingest::bindings::{GatewayBindings, GatewayTenant, SUBSCRIPTION};
use outbox::{Aggregate, Position, Replay};
use protocol::GatewayId;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use support::{ClickHouseTarget, EVENT_LIMIT, Service, closed_port, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)";
const INSERT_POOL: &str =
    "INSERT INTO assets.gateway_pool (hardware_id, registered_at, workstation)
VALUES ($1, now(), 'ingest-test')";

/// ingest's rows of a gateway, oldest binding first: tenant, binding, unbound.
const ROWS_OF: &str = "SELECT tenant_id::text, binding_id::text, unbound_at IS NOT NULL
FROM ingest.gateway_bindings WHERE hardware_id = $1 ORDER BY bound_at, binding_id";

/// Whether the subscription processed the event: its ID is in the inbox.
const PROCESSED: &str = "SELECT count(*) FROM ingest.outbox_inbox
WHERE subscription = 'ingest.assets-events' AND event_id = $1::uuid";

/// The dead-lettered events of the subscription, with their reason.
const DEAD_LETTERS: &str = "SELECT event_id::text, reason FROM ingest.outbox_dead_letter
WHERE subscription = 'ingest.assets-events'";

/// The event IDs of `assets.gateway_bound` for one binding, oldest first.
const EVENTS_OF: &str = "SELECT event_id::text FROM assets.outbox_event
WHERE event_type = 'assets.gateway_bound' AND aggregate_id = $1 ORDER BY id";

/// A write the platform role must not be able to make.
const PLATFORM_INSERT: &str =
    "INSERT INTO ingest.gateway_bindings (tenant_id, binding_id, hardware_id, bound_at)
VALUES ($1::uuid, $2::uuid, 'platform-write', now())";

type Row = (String, String, bool);

async fn rows_of(admin: &mut PgConnection, hardware_id: &str) -> Vec<Row> {
    sqlx::query_as(ROWS_OF)
        .bind(hardware_id)
        .fetch_all(admin)
        .await
        .expect("read ingest's bindings")
}

/// Waits until the ingest service processed `event`.
async fn wait_processed(product: &ProductPostgres, event: &str) {
    wait_for(
        &format!("the ingest service processing event {event}"),
        EVENT_LIMIT,
        || async {
            let mut admin = product.superuser().await;
            let count: i64 = sqlx::query_scalar(PROCESSED)
                .bind(event)
                .fetch_one(&mut admin)
                .await
                .expect("read the inbox");
            (count == 1).then_some(())
        },
    )
    .await;
}

/// Waits until the service has logged `count` applications of `binding`'s event, and returns
/// those log lines in order.
async fn wait_applied(
    service: &Service,
    binding: GatewayBindingId,
    count: usize,
) -> Vec<serde_json::Value> {
    let binding = binding.to_string();
    wait_for(
        &format!("{count} applications of assets.gateway_bound for binding {binding}"),
        EVENT_LIMIT,
        || async {
            let applied: Vec<_> = service
                .stdout
                .messages("applied assets.gateway_bound")
                .into_iter()
                .filter(|line| line["fields"]["binding_id"] == binding.as_str())
                .collect();
            (applied.len() >= count).then_some(applied)
        },
    )
    .await
}

/// Writes `assets.gateway_bound` for `binding` of `hardware_id` in `tenant`, with `occurred_at`
/// set to `at`, as the assets service would in the tenant's transaction; returns the event ID.
async fn write_bound(
    assets: &BackgroundTenantDb,
    tenant: TenantId,
    binding: GatewayBindingId,
    hardware_id: &GatewayId,
    at: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut envelope = EventEnvelope::new(
        &GatewayBound {
            binding_id: binding,
            hardware_id: hardware_id.clone(),
        },
        Some(tenant),
    )
    .expect("an envelope");
    envelope.occurred_at = at;
    let aggregate = Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding.to_string()).expect("ok");
    let mut tx = assets
        .begin(tenant, "test-write-gateway-bound")
        .await
        .expect("a transaction of the tenant");
    let published = outbox::write(tx.transaction(), &aggregate, &envelope)
        .await
        .expect("write the event");
    tx.commit().await.expect("commit");
    published.event_id.to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario on one shared database, step by step"
)]
async fn bindings_are_projected_once_and_newest_first() {
    let Some(product) = product_postgres(&["assets_app", "ingest_app", "ingest_platform"]).await
    else {
        return;
    };
    let service = Service::start(
        &product,
        None,
        &ClickHouseTarget {
            url: format!("http://127.0.0.1:{}", closed_port()),
            user: "trustiot_test_ingest".to_owned(),
            password: "unused".to_owned(),
        },
    )
    .await;
    let mut admin = product.superuser().await;
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());
    let gateway =
        GatewayId::new(format!("gw-{}", uuid::Uuid::new_v4().simple())).expect("a hardware ID");
    let unknown = GatewayId::new("gw-never-bound").expect("a hardware ID");
    for tenant in [tenant_a, tenant_b] {
        sqlx::query(INSERT_TENANT)
            .bind(tenant.to_string())
            .execute(&mut admin)
            .await
            .expect("the tenant is known to the assets service");
    }
    sqlx::query(INSERT_POOL)
        .bind(gateway.as_str())
        .execute(&mut admin)
        .await
        .expect("the gateway is in the device pool");

    let (_requests, assets) = TenantDb::connect_lazy_with_background(
        product.postgres.options("assets_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    let platform = PlatformDb::connect_lazy(
        product.postgres.options("ingest_platform", DATABASE),
        PgPoolOptions::new().max_connections(2),
    )
    .expect("verify-full options");
    let lookup = GatewayBindings::new(&platform);
    assert_eq!(
        lookup.gateway_tenant(&gateway).await.expect("look up"),
        None,
        "a gateway nobody bound has no tenant"
    );

    // 1. A platform administrator binds the gateway to tenant A through the assets service's
    //    code: ingest records the binding in tenant A, and the lookup answers it.
    let issuer = TestIssuer::new();
    let mut claims = TestIssuer::claims(ServiceName::Assets, None);
    claims.platform_admin = true;
    let administrator = issuer.user(ServiceName::Assets, &claims);
    let Outcome::Bound(bound) = GatewayBinder::new(assets.clone())
        .bind(&administrator, &gateway, tenant_a)
        .await
        .expect("bind the gateway")
    else {
        panic!("the gateway was bound already");
    };
    let first = bound.binding_id;
    let events: Vec<String> = sqlx::query_scalar(EVENTS_OF)
        .bind(first.to_string())
        .fetch_all(&mut admin)
        .await
        .expect("read the event");
    assert_eq!(events.len(), 1, "one event per binding");
    wait_processed(&product, &events[0]).await;
    let expected_a = vec![(tenant_a.to_string(), first.to_string(), false)];
    assert_eq!(rows_of(&mut admin, gateway.as_str()).await, expected_a);
    let current_a = Some(GatewayTenant {
        tenant_id: tenant_a,
        binding_id: first,
    });
    assert_eq!(
        lookup.gateway_tenant(&gateway).await.expect("look up"),
        current_a
    );
    assert_eq!(
        lookup.gateway_tenant(&unknown).await.expect("look up"),
        None,
        "another gateway still has no tenant"
    );
    println!("bound: {:?}", rows_of(&mut admin, gateway.as_str()).await);

    // 2. The whole subscription replayed with Reprocess: the consumer forgets what it processed
    //    and delivers the same event again; the row is neither duplicated nor replaced.
    let recorded_at: String = sqlx::query_scalar(
        "SELECT recorded_at::text FROM ingest.gateway_bindings WHERE binding_id = $1::uuid",
    )
    .bind(first.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("read recorded_at");
    let mut as_app = product.postgres.connect("ingest_app", DATABASE).await;
    let mut tx = as_app.begin().await.expect("begin");
    outbox::replay_from(&mut tx, SUBSCRIPTION, Position::START, Replay::Reprocess)
        .await
        .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
    let applied = wait_applied(&service, first, 2).await;
    assert_eq!(applied[1]["fields"]["created"], false, "{applied:?}");
    wait_processed(&product, &events[0]).await;
    assert_eq!(rows_of(&mut admin, gateway.as_str()).await, expected_a);
    let replayed_at: String = sqlx::query_scalar(
        "SELECT recorded_at::text FROM ingest.gateway_bindings WHERE binding_id = $1::uuid",
    )
    .bind(first.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("read recorded_at");
    assert_eq!(replayed_at, recorded_at, "the row was replaced");

    // 3. The same event once more under a new event ID (so the inbox does not stop it), as a
    //    producer retry would write it: the insert does nothing.
    let repeated = write_bound(&assets, tenant_a, first, &gateway, bound.bound_at).await;
    wait_processed(&product, &repeated).await;
    let applied = wait_applied(&service, first, 3).await;
    assert_eq!(applied[2]["fields"]["created"], false, "{applied:?}");
    assert_eq!(rows_of(&mut admin, gateway.as_str()).await, expected_a);
    assert_eq!(
        lookup.gateway_tenant(&gateway).await.expect("look up"),
        current_a
    );

    // 4. An older binding's event arrives late (written after, bound an hour before): it is
    //    recorded in its own tenant, but the newer binding stays the gateway's tenant.
    let older = GatewayBindingId::generate();
    let late = write_bound(
        &assets,
        tenant_b,
        older,
        &gateway,
        bound.bound_at - TimeDelta::hours(1),
    )
    .await;
    wait_processed(&product, &late).await;
    assert_eq!(rows_of(&mut admin, gateway.as_str()).await.len(), 2);
    assert_eq!(
        lookup.gateway_tenant(&gateway).await.expect("look up"),
        current_a,
        "an older binding that arrived late replaced the newer one"
    );
    println!(
        "after a late older binding: {:?}",
        rows_of(&mut admin, gateway.as_str()).await
    );

    // 5. A newer binding (as after an unbinding, Laysi/trustiot-v2#115): it becomes the
    //    gateway's tenant.
    let newer = GatewayBindingId::generate();
    let next = write_bound(
        &assets,
        tenant_b,
        newer,
        &gateway,
        bound.bound_at + TimeDelta::minutes(1),
    )
    .await;
    wait_processed(&product, &next).await;
    assert_eq!(
        lookup.gateway_tenant(&gateway).await.expect("look up"),
        Some(GatewayTenant {
            tenant_id: tenant_b,
            binding_id: newer,
        })
    );
    println!(
        "after a newer binding: {:?}",
        rows_of(&mut admin, gateway.as_str()).await
    );

    // 6. An event that can never be recorded (a recorded binding of tenant A named with another
    //    gateway) is dead-lettered, and the subscription moves on; nothing changes.
    let other = GatewayId::new("gw-other").expect("a hardware ID");
    let broken = write_bound(&assets, tenant_a, first, &other, bound.bound_at).await;
    let dead: (String, String) = wait_for(
        &format!("the ingest service dead-lettering event {broken}"),
        EVENT_LIMIT,
        || async {
            let mut admin = product.superuser().await;
            let rows: Vec<(String, String)> = sqlx::query_as(DEAD_LETTERS)
                .fetch_all(&mut admin)
                .await
                .expect("read the dead letters");
            rows.into_iter().find(|(event, _)| *event == broken)
        },
    )
    .await;
    println!("dead-lettered: {dead:?}");
    assert!(
        dead.1.contains("another gateway"),
        "the reason names the problem: {dead:?}"
    );
    assert!(rows_of(&mut admin, other.as_str()).await.is_empty());
    let after_broken = GatewayBindingId::generate();
    let unaffected =
        GatewayId::new(format!("gw-{}", uuid::Uuid::new_v4().simple())).expect("a hardware ID");
    let moved_on = write_bound(&assets, tenant_a, after_broken, &unaffected, bound.bound_at).await;
    wait_processed(&product, &moved_on).await;
    assert_eq!(
        lookup.gateway_tenant(&unaffected).await.expect("look up"),
        Some(GatewayTenant {
            tenant_id: tenant_a,
            binding_id: after_broken,
        }),
        "the event after the dead letter is applied"
    );

    // 7. The platform role reads across tenants but cannot write a binding.
    let mut as_platform = product.postgres.connect("ingest_platform", DATABASE).await;
    let error = sqlx::query(PLATFORM_INSERT)
        .bind(tenant_a.to_string())
        .bind(GatewayBindingId::generate().to_string())
        .execute(&mut as_platform)
        .await
        .expect_err("the platform role has no INSERT");
    let code = error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned());
    assert_eq!(code.as_deref(), Some("42501"), "{error}");
}
