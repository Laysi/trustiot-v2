//! Cross-tenant negative tests (decision 29, crates/testkit): the certs service keeps no tenant
//! data of its own, so what applies is the coverage check and the outbox. Every relation the
//! service role can reach must be declared, and so must every operation of its client API: the
//! CA's tables are exempt here with the reason they hold no tenant data, and a new table or
//! operation fails this test until it is declared (a tenant table with row-level security and
//! the probes, an endpoint to attack) or exempted with a reason. Its other endpoints serve public
//! CA data under `/internal`, which the OpenAPI document leaves out because the ingress never
//! routes it.
//!
//! The ledger's gateway ids and workstation names are not tenant data: a gateway belongs to a
//! tenant through its assignment in the assets service, and certificates are issued before a
//! gateway is assigned (factory registration, Laysi/trustiot-v2#79); production workstations
//! belong to the platform, not to a tenant (Laysi/trustiot-v2#534). The registrations themselves
//! (`certs.gateway_registrations`, `certs.workstation_request_nonces`) are platform data the
//! service role cannot reach at all, only the platform role. If a later issue stores tenant ids
//! here, the tables become tenant tables and get the probes.
//!
//! Since the factory registration the service is an outbox producer (`certs.gateway_registered`,
//! platform-wide), so its schema has the outbox's tables with the tenant policies of every
//! producer; the service role's grants on them are probed like any tenant table: it cannot write
//! another tenant's events or advance another tenant's aggregates.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One event of the tenant in the outbox: `$2` is the event id, the canary the aggregate id.
const SEED_EVENT: &str = "INSERT INTO certs.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (1, $2::uuid, 'testkit.canary', $1::uuid, 'testkit', $3, 1, '{}'::jsonb)";

/// One aggregate of the tenant, its sequence in the outbox: the canary is the aggregate id; the
/// table has no column for `$2`.
const SEED_AGGREGATE: &str = "INSERT INTO certs.outbox_aggregate
    (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx)
SELECT 'testkit', $3, $1::uuid, 1, 1 WHERE $2::uuid IS NOT NULL";

#[tokio::test(flavor = "multi_thread")]
async fn every_relation_and_operation_the_service_has_is_declared() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("certs_app", DATABASE),
    )
    // The service role only inserts events (no SELECT or UPDATE): the probes check that it
    // cannot write one for another tenant. The service writes its events as the platform role.
    .table(pg_table!("certs.outbox_event", seed = SEED_EVENT))
    // The service role reads and advances the sequence of its tenant's aggregates only.
    .table(pg_table!("certs.outbox_aggregate", seed = SEED_AGGREGATE))
    .exempt(
        "certs.issued_certificates",
        "the ledger of signed gateway and workstation certificates: serial numbers, the issuing \
         CA's key id, the gateway id or workstation name, validity, the certificate and its \
         revocation, which are public data of the PKI; no tenant id (a gateway's tenant is the \
         assets service's assignment, workstations belong to the platform)",
    )
    .exempt(
        "certs.crls",
        "each issuing CA's current CRL, public data of the PKI; no tenant data",
    )
    .exempt(
        "certs.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check",
    );
    let issuer = TestIssuer::new();
    let document = serde_json::to_value(certs::openapi()).expect("the OpenAPI document as JSON");
    let report = Suite::new(ServiceName::Certs, &issuer, store)
        .openapi(document)
        .exempt_endpoint(
            "POST /api/certs/v1/workstation-certificates",
            "issues a production workstation certificate: platform administrators only, no \
             tenant data (tests/workstation.rs)",
        )
        .exempt_endpoint(
            "POST /api/certs/v1/workstation-certificates/{serial}/revoke",
            "revokes a production workstation certificate: platform administrators only, no \
             tenant data (tests/workstation.rs)",
        )
        .exempt_endpoint(
            "POST /api/certs/v1/gateway-registrations",
            "registers a gateway at the factory: a platform administrator on a production \
             workstation only, before the gateway belongs to any tenant; no tenant data \
             (tests/registration.rs)",
        )
        .run(None)
        .await;
    report.assert_isolated();
}
