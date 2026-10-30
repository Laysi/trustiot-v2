//! Cross-tenant negative tests (decision 29, crates/testkit). Every relation the service role can
//! reach must be declared, and so must every operation of its client API: the CA's tables are
//! exempt here with the reason they hold no tenant data, and a new table or operation fails this
//! test until it is declared (a tenant table with row-level security and the probes, an endpoint
//! to attack) or exempted with a reason. Its other endpoints serve public
//! CA data under `/internal`, which the OpenAPI document leaves out because the ingress never
//! routes it.
//!
//! The ledger's gateway ids and workstation names are not tenant data: a gateway belongs to a
//! tenant through its assignment in the assets service, and certificates are issued before a
//! gateway is assigned (factory registration, Laysi/trustiot-v2#79); production workstations
//! belong to the platform, not to a tenant (Laysi/trustiot-v2#534); the MQTT broker's server
//! certificate belongs to the platform's broker (Laysi/trustiot-v2#557), and its issuance is on a
//! port of its own outside the OpenAPI document, which only the broker's workload reaches. The
//! workstation API is for platform administrators only, which `tests/workstation.rs` tests. The
//! registrations themselves (`certs.gateway_registrations`, `certs.workstation_request_nonces`)
//! are platform data the service role cannot reach at all, only the platform role.
//!
//! Since Laysi/trustiot-v2#632 the service keeps which tenant each gateway is bound to
//! (`certs.gateway_bindings`) and each tenant's status (`certs.tenant_statuses`): tenant tables,
//! probed here. The service role inserts bindings and stores statuses in the event's tenant only;
//! the reads across tenants are the platform role's (`tests/bindings.rs` checks that it sees every
//! tenant and that each read is in the audit log). Consuming the assets service's events also gave
//! the schema the outbox consumer's bookkeeping tables, which hold no tenant data.
//!
//! Since the factory registration the service is an outbox producer (`certs.gateway_registered`,
//! platform-wide), so its schema has the outbox's tables with the tenant policies of every
//! producer; the service role's grants on them are probed like any tenant table: it cannot write
//! another tenant's events or advance another tenant's aggregates. So the suite no longer
//! declares the service as one without tenant data (`Suite::without_tenant_data`), which a
//! tenant table would contradict.
//!
//! The renewal endpoint (Laysi/trustiot-v2#81) is a gateway protocol on the public path
//! `/certs/renew`, outside the OpenAPI document like the `/internal` endpoints: a gateway proves
//! who it is with its TPM signature, not a token, and nothing it sends names a tenant
//! (`tests/renewal.rs`). Its challenges (`certs.renewal_challenges`) hold a hardware ID and a
//! nonce, no tenant data; its refusal counts are the platform role's, which the service role
//! cannot reach.
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

/// One binding of the tenant: `$2` is the binding id, the canary the gateway's hardware ID
/// (`canary-a-<32 hex>` fits the hardware ID rule).
const SEED_BINDING: &str = "INSERT INTO certs.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id)
VALUES ($1::uuid, $2::uuid, $3, now(), 1, 1)";

/// The tenant's status, one row per tenant; the table has no column for `$2` (the tenant id
/// again) or the canary.
const SEED_STATUS: &str = "INSERT INTO certs.tenant_statuses (tenant_id, status, version)
SELECT $1::uuid, 'suspended', 1 WHERE $2::uuid IS NOT NULL AND $3::text IS NOT NULL";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

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
    // The service role only inserts bindings (no SELECT, UPDATE or DELETE), in the event's
    // tenant: the probes check that it cannot write one for another tenant.
    .table(pg_table!("certs.gateway_bindings", seed = SEED_BINDING))
    // The service role reads and replaces its tenant's status only (no DELETE).
    .table(pg_table!(
        "certs.tenant_statuses",
        seed = SEED_STATUS,
        one_per_tenant
    ))
    .exempt("certs.outbox_subscription", CONSUMER_TABLE)
    .exempt("certs.outbox_inbox", CONSUMER_TABLE)
    .exempt("certs.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "certs.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far (the tenant statuses keep their own \
         version)",
    )
    .exempt(
        "certs.issued_certificates",
        "the ledger of signed gateway, workstation and MQTT broker certificates: serial \
         numbers, the issuing CA's key id, the gateway id, workstation name or broker name, \
         validity, the certificate and its revocation, which are public data of the PKI; no \
         tenant id (a gateway's tenant is the assets service's assignment, workstations and the \
         broker belong to the platform)",
    )
    .exempt(
        "certs.crls",
        "each issuing CA's current CRL, public data of the PKI; no tenant data",
    )
    .exempt(
        "certs.renewal_challenges",
        "the renewal's single-use challenges (Laysi/trustiot-v2#81): a nonce, the hardware ID \
         it was handed out for, the purpose and the expiry, no tenant data; a challenge looks \
         the same whether or not the gateway is bound to a tenant",
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
        .exempt_endpoint(
            "POST /api/certs/v1/gateways/{hardware_id}/revoke",
            "revokes every valid certificate of a gateway: platform administrators only, by \
             hardware ID, no tenant data (tests/gateway_revocation.rs)",
        )
        .run(None)
        .await;
    report.assert_isolated();
}
