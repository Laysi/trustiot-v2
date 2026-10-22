//! Cross-tenant negative tests (decision 29, crates/testkit): the telemetry-query service's
//! Postgres role cannot read or change another tenant's rows, and every relation the role can
//! reach is declared. The schema is the product's, applied by the migration Job
//! (`testkit::product`).
//!
//! The two API operations serve ClickHouse data, which the suite's Postgres store cannot seed;
//! `tests/tenant_accounts.rs` (each tenant's account against the PoC's attack types) and
//! `tests/history.rs` (tenant B's readings under the same device ID never reach tenant A, and
//! the reverse) cover them by hand.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::ServiceName;
use support::ROLES;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One row per tenant: `$1` tenant, `$2` the tenant id again (one_per_tenant), `$3` a canary,
/// which this table has no text column for.
const SEED_TENANT: &str = "INSERT INTO telemetry_query.tenant_accounts (tenant_id)
SELECT $1::uuid WHERE $2::uuid IS NOT NULL AND $3::text IS NOT NULL";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

const CLICKHOUSE_API: &str = "serves ClickHouse readings as the caller's tenant account, which \
     the Postgres store cannot seed; tests/tenant_accounts.rs and tests/history.rs cover it";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&ROLES).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("telemetry_query_app", DATABASE),
    )
    .table(pg_table!(
        "telemetry_query.tenant_accounts",
        seed = SEED_TENANT,
        one_per_tenant
    ))
    .exempt("telemetry_query.outbox_subscription", CONSUMER_TABLE)
    .exempt("telemetry_query.outbox_inbox", CONSUMER_TABLE)
    .exempt("telemetry_query.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "telemetry_query.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far",
    )
    .exempt(
        "telemetry_query.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check \
         (Laysi/trustiot-v2#402)",
    );

    Suite::new(ServiceName::TelemetryQuery, &issuer, store)
        .openapi(serde_json::to_value(telemetry_query::openapi()).expect("the OpenAPI document"))
        .exempt_endpoint(
            "GET /api/telemetry/v1/devices/{device_id}/series",
            CLICKHOUSE_API,
        )
        .exempt_endpoint(
            "GET /api/telemetry/v1/devices/{device_id}/latest",
            CLICKHOUSE_API,
        )
        .run(None)
        .await
        .assert_isolated();
}
