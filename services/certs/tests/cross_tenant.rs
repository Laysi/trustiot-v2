//! Cross-tenant negative tests (decision 29, crates/testkit): the certs service keeps no tenant
//! data, so the suite's part that applies is the coverage check. Every relation the service role
//! can reach must be declared; each is exempt here with the reason it holds no tenant data, and a
//! new table fails this test until it is declared as a tenant table (with row-level security and
//! the probes) or exempted with a reason. Its only endpoints serve public CA data under
//! `/internal` (tools/openapi lists it in `SERVICES_WITHOUT_API`).
//!
//! The ledger's gateway ids are not tenant data in this issue: a gateway belongs to a tenant
//! through its assignment in the assets service, and certificates are issued before a gateway is
//! assigned (factory registration, Laysi/trustiot-v2#79). If a later issue stores tenant ids
//! here, the tables become tenant tables and get the probes.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

use testkit::isolation::postgres::PgStore;
use testkit::isolation::{Report, Store as _};
use testkit::product::{DATABASE, product_postgres};

#[tokio::test(flavor = "multi_thread")]
async fn every_relation_the_service_reaches_is_declared() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("certs_app", DATABASE),
    )
    .exempt(
        "certs.issued_certificates",
        "the ledger of signed gateway certificates: serial numbers, the issuing CA's key id, the \
         gateway id, validity and the certificate, which are public data of the PKI; no tenant \
         id (a gateway's tenant is the assets service's assignment)",
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
    let mut report = Report::default();
    store.check_coverage(&mut report).await;
    println!("{report}");
    report.assert_isolated();
}
