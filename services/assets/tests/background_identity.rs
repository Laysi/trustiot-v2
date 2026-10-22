//! Background work needs no user's identity (Laysi/trustiot-v2#106, decision 27): the consumer of
//! `identity.tenant_created` works as the service's own identity (`ServiceIdentity`, its own
//! database roles), so it does its work even when no request can carry a user. The assets binary
//! runs with a key set that verifies no token: every request, a genuine user's token included, is
//! refused, readiness fails on the keys, and still a tenant the identity service creates gets its
//! row here.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use contract::{ServiceName, TenantId, TenantRole};
use identity::tenants::{TenantName, TenantStore};
use serde_json::Value;
use support::{EVENT_LIMIT, Service, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// A key set with no key: the service has nothing to verify a token with (service-kit's
/// `PublishedKeys` treats it as unusable and keeps none).
const NO_KEYS: &str = r#"{"keys":[]}"#;

const TENANT_ROWS: &str = "SELECT count(*) FROM assets.tenants WHERE tenant_id = $1::uuid";

/// Creates a tenant through the identity service's store, as `POST /api/identity/v1/tenants`
/// does for a platform administrator.
async fn create_tenant(product: &ProductPostgres, issuer: &TestIssuer, name: &str) -> TenantId {
    let store = TenantStore::connect_lazy(product.postgres.options("identity_app", DATABASE))
        .expect("verify-full options");
    let mut claims = TestIssuer::claims(ServiceName::Identity, None);
    claims.platform_admin = true;
    let admin = issuer.user(ServiceName::Identity, &claims);
    let name = TenantName::parse(name).expect("a valid name");
    store
        .create(&admin, &name)
        .await
        .expect("create the tenant")
        .tenant_id
}

#[tokio::test(flavor = "multi_thread")]
async fn the_consumer_does_its_work_while_no_request_can_carry_a_user() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::spawn(&product, NO_KEYS).await;
    // Not ready: no usable key set (both database roles and the migrations are fine).
    assets.wait_readiness(503).await;

    let tenant = create_tenant(&product, &issuer, "Plant without users").await;
    let id = tenant.to_string();

    // No user reaches the service: a genuine user's token for it, of the new tenant, is refused
    // because nothing can verify it.
    let token = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);
    let (status, body) = assets.get("/api/assets/v1/tenant", Some(&token)).await;
    assert_eq!(status, 503, "{body}");
    let error: Value = serde_json::from_str(&body).expect("an M1 error body");
    assert_eq!(error["code"], "unavailable", "{body}");

    // The background work does not need one: the tenant gets its row from the event.
    let applied = wait_for(
        &format!("the consumer applying identity.tenant_created for tenant {id}"),
        EVENT_LIMIT,
        || async {
            assets
                .stdout
                .messages("applied identity.tenant_created")
                .into_iter()
                .find(|line| line["fields"]["tenant_id"] == id.as_str())
        },
    )
    .await;
    assert_eq!(applied["fields"]["created"], true, "{applied}");
    let mut admin = product.superuser().await;
    let rows: i64 = sqlx::query_scalar(TENANT_ROWS)
        .bind(&id)
        .fetch_one(&mut admin)
        .await
        .expect("count the tenant's rows");
    assert_eq!(rows, 1, "the consumer did not create the tenant's row");

    // Still no user: the request is refused after the work is done, too.
    let (status, _) = assets.get("/api/assets/v1/tenant", Some(&token)).await;
    assert_eq!(status, 503);
    assert_eq!(assets.readiness().await, Some(503));
}
