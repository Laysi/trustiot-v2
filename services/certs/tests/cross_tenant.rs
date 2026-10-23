//! Cross-tenant negative tests (decision 29, crates/testkit): the certs service keeps no tenant
//! data, so the suite's part that applies is the coverage check. Every relation the service role
//! can reach must be declared, and so must every operation of its client API: each is exempt
//! here with the reason it holds no tenant data, and a new table or operation fails this test
//! until it is declared (a tenant table with row-level security and the probes, an endpoint to
//! attack) or exempted with a reason. Its other endpoints serve public CA data under `/internal`,
//! which the OpenAPI document leaves out because the ingress never routes it.
//!
//! The ledger's gateway ids and workstation names are not tenant data: a gateway belongs to a
//! tenant through its assignment in the assets service, and certificates are issued before a
//! gateway is assigned (factory registration, Laysi/trustiot-v2#79); production workstations
//! belong to the platform, not to a tenant (Laysi/trustiot-v2#534). The workstation API is for
//! platform administrators only, which `tests/workstation.rs` tests. If a later issue stores
//! tenant ids here, the tables become tenant tables and get the probes.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::product::{DATABASE, product_postgres};

#[tokio::test(flavor = "multi_thread")]
async fn every_relation_and_operation_the_service_has_is_declared() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("certs_app", DATABASE),
    )
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
        .without_tenant_data(
            "the CA's ledger and CRLs are public PKI data of gateways and the platform's \
             workstations, with no tenant id",
        )
        .run(None)
        .await;
    report.assert_isolated();
}
