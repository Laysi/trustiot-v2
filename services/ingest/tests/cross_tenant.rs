//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#640 acceptance 3):
//! the ingest service's Postgres role cannot read or change another tenant's gateway bindings,
//! and every relation the role can reach is declared. The schema is the product's, applied by
//! the migration Job (`testkit::product`).
//!
//! The suite probes the service role, which writes bindings in the event's tenant. The platform
//! role reads every tenant's bindings by design (the lookup of a gateway's tenant, through the
//! table's `platform_access` policy, SELECT only); `tests/gateway_bindings.rs` checks that it
//! cannot write one.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One binding: `$2` is the binding id, the canary the gateway's hardware ID (`canary-a-<32
/// hex>` fits the hardware ID rule).
const SEED_BINDING: &str =
    "INSERT INTO ingest.gateway_bindings (tenant_id, binding_id, hardware_id, bound_at)
VALUES ($1::uuid, $2::uuid, $3, now())";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["ingest_app"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("ingest_app", DATABASE),
    )
    .table(pg_table!("ingest.gateway_bindings", seed = SEED_BINDING))
    .exempt("ingest.outbox_subscription", CONSUMER_TABLE)
    .exempt("ingest.outbox_inbox", CONSUMER_TABLE)
    .exempt("ingest.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "ingest.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far",
    )
    .exempt(
        "ingest.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check \
         (Laysi/trustiot-v2#402)",
    );

    Suite::new(ServiceName::Ingest, &issuer, store)
        .without_api(
            "ingest has no HTTP API besides the health endpoints (tools/openapi, \
             SERVICES_WITHOUT_API): it takes telemetry from MQTT (Laysi/trustiot-v2#641) and \
             bindings from the assets service's events",
        )
        .run(None)
        .await
        .assert_isolated();
}
