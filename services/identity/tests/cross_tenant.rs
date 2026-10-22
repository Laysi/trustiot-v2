//! Cross-tenant negative tests (decision 29, crates/testkit): the identity service's role
//! cannot read or change another tenant's memberships, tenant, events or event aggregates'
//! sequences, and every relation it can reach is declared. The schema is the product's, applied
//! by the migration Job as in a deployment (`testkit::product`), so the outbox tables the Job
//! creates are covered too.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One membership: `$1` tenant, `$2` the row id, kept as the member's user id (a new user, so
/// the foreign key holds), `$3` the canary, which this table has no text column for.
const SEED_MEMBERSHIP: &str = "WITH member AS (
    INSERT INTO identity.users (id) VALUES ($2::uuid) RETURNING id
)
INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
SELECT $1::uuid, member.id, 'viewer' FROM member WHERE $3::text IS NOT NULL";

/// The tenant itself, one row per tenant: `$2` is the tenant id again (one_per_tenant), the
/// canary is the name.
const SEED_TENANT: &str = "INSERT INTO identity.tenants (tenant_id, name)
SELECT $1::uuid, $3 WHERE $2::uuid IS NOT NULL";

/// One event of the tenant in the outbox: `$2` is the event id, the canary the aggregate id.
const SEED_EVENT: &str = "INSERT INTO identity.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (1, $2::uuid, 'testkit.canary', $1::uuid, 'testkit', $3, 1, '{}'::jsonb)";

/// One aggregate of the tenant, its sequence in the outbox (Laysi/trustiot-v2#413): the canary
/// is the aggregate id; the table has no column for `$2`.
const SEED_AGGREGATE: &str = "INSERT INTO identity.outbox_aggregate
    (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx)
SELECT 'testkit', $3, $1::uuid, 1, 1 WHERE $2::uuid IS NOT NULL";

/// Why the migration records are exempt (Laysi/trustiot-v2#402).
const MIGRATION_RECORDS: &str = "the migration Job's record of the files applied in this schema \
     (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no tenant \
     data; the service role only reads it, for its readiness check";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };

    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("identity_app", DATABASE),
    )
    .table(pg_table!("identity.memberships", seed = SEED_MEMBERSHIP))
    .table(pg_table!(
        "identity.tenants",
        seed = SEED_TENANT,
        one_per_tenant
    ))
    // The service role only inserts events (no SELECT or UPDATE): the probes check that it
    // cannot write one for another tenant.
    .table(pg_table!("identity.outbox_event", seed = SEED_EVENT))
    // The service role reads and advances the sequence of its tenant's aggregates only: the
    // probes check that it neither sees nor changes another tenant's (Laysi/trustiot-v2#413).
    .table(pg_table!("identity.outbox_aggregate", seed = SEED_AGGREGATE))
    .exempt(
        "identity.users",
        "users span tenants: the platform-wide user record, no tenant data (services/identity/README.md)",
    )
    .exempt(
        "identity.sessions",
        "a session belongs to a user and is found by its cookie hash before any tenant is known; \
         the tenant role is always read from memberships under row-level security \
         (services/identity/README.md)",
    )
    .exempt(
        "identity.login_codes",
        "a login code belongs to a user and is checked before any session or tenant exists; it \
         holds a hash, a count and times, no tenant data (services/identity/README.md, 登入)",
    )
    .exempt("identity.trustiot_migrate_applied", MIGRATION_RECORDS);

    let issuer = TestIssuer::new();
    Suite::new(ServiceName::Identity, &issuer, store)
        .openapi(serde_json::to_value(identity::openapi()).expect("the OpenAPI document"))
        .exempt_endpoint(
            "POST /api/identity/v1/tenants",
            "platform administrators only, and it creates a new tenant whose ID the service makes: \
             it takes no tenant from the request and serves no existing tenant's data \
             (tests/tenants.rs covers it)",
        )
        .exempt_endpoint(
            "POST /auth/login",
            "logging in happens before there is a session or a tenant: it takes an email address \
             and a code, and the session it creates acts in no tenant the user is not a member of \
             (tests/login.rs covers it)",
        )
        .exempt_endpoint(
            "DELETE /api/identity/v1/sessions/current",
            "revokes the caller's own session, found by its cookie and its user: no tenant data \
             (tests/login.rs covers it)",
        )
        .exempt_endpoint(
            "DELETE /api/identity/v1/sessions",
            "revokes the caller's own sessions, found by their user: no tenant data \
             (tests/login.rs covers it)",
        )
        .run(None)
        .await
        .assert_isolated();
}
