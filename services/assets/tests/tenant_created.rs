//! A tenant the identity service creates eventually gets its row in the assets service, and a
//! replayed or repeated `identity.tenant_created` never adds a second one (Laysi/trustiot-v2#65,
//! acceptance 1). The assets binary runs as a real process on the product's schema, applied by
//! the migration Job (`testkit::product`); the tenant is created by the identity service's own
//! code (`identity::tenants::TenantStore`), the event's only producer. The API answers for the
//! caller's tenant only.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use contract::events::identity::{TENANT_AGGREGATE, TenantCreated};
use contract::{ServiceName, TenantId, TenantRole};
use identity::tenants::{TenantName, TenantStore};
use outbox::{Aggregate, Position, Replay};
use serde_json::Value;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use support::{EVENT_LIMIT, Service, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const TENANT_ROWS: &str = "SELECT count(*) FROM assets.tenants WHERE tenant_id = $1::uuid";
const CREATED_AT: &str = "SELECT created_at::text FROM assets.tenants WHERE tenant_id = $1::uuid";
/// Whether the subscription processed the event: its ID is in the inbox.
const PROCESSED: &str = "SELECT count(*) FROM assets.outbox_inbox
WHERE subscription = 'assets.identity-events' AND event_id = $1::uuid";
const DEAD_LETTERS: &str = "SELECT count(*) FROM assets.outbox_dead_letter";
/// The event IDs of a tenant's `identity.tenant_created`, oldest first.
const EVENTS_OF: &str = "SELECT event_id::text FROM identity.outbox_event
WHERE event_type = 'identity.tenant_created' AND tenant_id = $1::uuid ORDER BY id";

async fn count(admin: &mut PgConnection, sql: &'static str, id: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(admin)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

/// A platform administrator's verified identity, as the identity service's own API receives it.
fn platform_admin(issuer: &TestIssuer) -> service_kit::identity::UserIdentity {
    let mut claims = TestIssuer::claims(ServiceName::Identity, None);
    claims.platform_admin = true;
    issuer.user(ServiceName::Identity, &claims)
}

/// Creates a tenant through the identity service's store, as `POST /api/identity/v1/tenants`
/// does.
async fn create_tenant(product: &ProductPostgres, issuer: &TestIssuer, name: &str) -> TenantId {
    let store = TenantStore::connect_lazy(product.postgres.options("identity_app", DATABASE))
        .expect("verify-full options");
    let name = TenantName::parse(name).expect("a valid name");
    store
        .create(&platform_admin(issuer), &name)
        .await
        .expect("create the tenant")
        .tenant_id
}

/// Waits until the service has logged `count` applications of `identity.tenant_created` for
/// `tenant`, and returns those log lines in order.
async fn wait_applied(assets: &Service, tenant: &str, count: usize) -> Vec<Value> {
    wait_for(
        &format!("{count} applications of identity.tenant_created for tenant {tenant}"),
        EVENT_LIMIT,
        || async {
            let applied: Vec<Value> = assets
                .stdout
                .messages("applied identity.tenant_created")
                .into_iter()
                .filter(|line| line["fields"]["tenant_id"] == tenant)
                .collect();
            (applied.len() >= count).then_some(applied)
        },
    )
    .await
}

/// Waits until the assets service processed `event`.
async fn wait_processed(product: &ProductPostgres, event: &str) {
    wait_for(
        &format!("the assets service processing event {event}"),
        EVENT_LIMIT,
        || async {
            let mut admin = product.superuser().await;
            (count(&mut admin, PROCESSED, event).await == 1).then_some(())
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_new_tenant_gets_one_row_whatever_is_replayed() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;

    // 1. A new tenant: the service creates its row from the event, and the API shows it to the
    //    tenant's users only.
    let tenant = create_tenant(&product, &issuer, "Plant A").await;
    let other = create_tenant(&product, &issuer, "Plant B").await;
    let token = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);
    let body = wait_for(
        "GET /api/assets/v1/tenant answering 200 for the new tenant",
        EVENT_LIMIT,
        || async {
            let (status, body) = assets.get("/api/assets/v1/tenant", Some(&token)).await;
            (status == 200).then_some(body)
        },
    )
    .await;
    let json: Value = serde_json::from_str(&body).expect("a JSON body");
    assert_eq!(json["tenant_id"], tenant.to_string(), "{body}");
    assert!(
        !body.contains(&other.to_string()),
        "the other tenant leaked: {body}"
    );
    let id = tenant.to_string();
    assert_eq!(count(&mut admin, TENANT_ROWS, &id).await, 1);
    let first_created_at: String = sqlx::query_scalar(CREATED_AT)
        .bind(&id)
        .fetch_one(&mut admin)
        .await
        .expect("read created_at");
    let events: Vec<String> = sqlx::query_scalar(EVENTS_OF)
        .bind(&id)
        .fetch_all(&mut admin)
        .await
        .expect("read the events");
    assert_eq!(events.len(), 1, "one event per created tenant");
    wait_processed(&product, &events[0]).await;

    // 2. Replay from the start with Reprocess: the consumer forgets what it processed and
    //    delivers every event again; the row is not duplicated or replaced.
    let mut as_app = product.postgres.connect("assets_app", DATABASE).await;
    let mut tx = as_app.begin().await.expect("begin");
    outbox::replay_from(
        &mut tx,
        "assets.identity-events",
        Position::START,
        Replay::Reprocess,
    )
    .await
    .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
    wait_applied(&assets, &id, 2).await;
    wait_processed(&product, &events[0]).await;
    assert_eq!(
        count(&mut admin, TENANT_ROWS, &id).await,
        1,
        "the replay added a row"
    );
    let replayed_created_at: String = sqlx::query_scalar(CREATED_AT)
        .bind(&id)
        .fetch_one(&mut admin)
        .await
        .expect("read created_at");
    assert_eq!(
        replayed_created_at, first_created_at,
        "the row was replaced"
    );

    // 3. The same event type again for the same tenant (a new event ID, so the inbox does not
    //    stop it): the insert does nothing.
    let (_requests, background) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("verify-full options");
    let mut tx = background
        .begin(tenant, "test-repeat-tenant-created")
        .await
        .expect("a transaction of the tenant");
    let aggregate = Aggregate::new(TENANT_AGGREGATE, id.clone()).expect("aggregate");
    let repeated = outbox::publish(
        tx.transaction(),
        &aggregate,
        &TenantCreated {},
        Some(tenant),
    )
    .await
    .expect("write the event again");
    tx.commit().await.expect("commit");
    wait_processed(&product, &repeated.event_id.to_string()).await;
    assert_eq!(
        count(&mut admin, TENANT_ROWS, &id).await,
        1,
        "a repeated event added a row"
    );
    let applied = wait_applied(&assets, &id, 3).await;
    assert_eq!(
        applied
            .iter()
            .map(|line| line["fields"]["created"].as_bool())
            .collect::<Vec<_>>(),
        [Some(true), Some(false), Some(false)],
        "created once, then found by the replay and the repeated event: {applied:?}"
    );
    let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
        .fetch_one(&mut admin)
        .await
        .expect("count dead letters");
    assert_eq!(dead, 0, "no event was set aside");

    // 4. The API: no token 401, no tenant 403, a tenant the service has not seen 404, and a
    //    wrong audience 401.
    let (status, body) = assets.get("/api/assets/v1/tenant", None).await;
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("\"unauthenticated\""), "{body}");
    let no_tenant = issuer.token(&TestIssuer::claims(ServiceName::Assets, None));
    let (status, body) = assets.get("/api/assets/v1/tenant", Some(&no_tenant)).await;
    assert_eq!(status, 403, "{body}");
    let unknown = issuer.tenant_token(ServiceName::Assets, TenantId::generate(), TenantRole::Admin);
    let (status, body) = assets.get("/api/assets/v1/tenant", Some(&unknown)).await;
    assert_eq!(status, 404, "{body}");
    let for_identity = issuer.tenant_token(ServiceName::Identity, tenant, TenantRole::Admin);
    let (status, _) = assets
        .get("/api/assets/v1/tenant", Some(&for_identity))
        .await;
    assert_eq!(status, 401, "a token for another service is refused");
    admin.close().await.expect("close");
}
