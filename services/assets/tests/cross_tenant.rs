//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#65 acceptance 3):
//! the assets service's role cannot read or change another tenant's rows, its API never shows
//! one tenant another tenant's data, and every relation the role can reach is declared. The
//! schema is the product's, applied by the migration Job (`testkit::product`), and the service is
//! the real binary.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::ServiceName;
use support::Service;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::Endpoint;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One row per tenant: `$1` tenant, `$2` the tenant id again (one_per_tenant), `$3` a canary,
/// which this table has no text column for.
const SEED_TENANT: &str = "INSERT INTO assets.tenants (tenant_id)
SELECT $1::uuid WHERE $2::uuid IS NOT NULL AND $3::text IS NOT NULL";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;

    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("assets_app", DATABASE),
    )
    .table(pg_table!(
        "assets.tenants",
        seed = SEED_TENANT,
        one_per_tenant
    ))
    .exempt("assets.outbox_subscription", CONSUMER_TABLE)
    .exempt("assets.outbox_inbox", CONSUMER_TABLE)
    .exempt("assets.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "assets.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far",
    )
    .exempt(
        "assets.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check \
         (Laysi/trustiot-v2#402)",
    );

    Suite::new(ServiceName::Assets, &issuer, store)
        .openapi(serde_json::to_value(assets::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list("/api/assets/v1/tenant", "assets.tenants"))
        .run(Some(&assets.base_url))
        .await
        .assert_isolated();
}
