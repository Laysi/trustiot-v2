//! Cross-tenant negative tests (decision 29, crates/testkit): the identity service's role
//! cannot read or change another tenant's memberships, and every relation it can reach is
//! declared. The schema is the service's own migration, applied by the migration role as in a
//! deployment.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::postgres::tls_postgres;

/// The roles of decision 29 for this service. The identity service has no platform role yet.
const ROLES: &str = "
CREATE ROLE identity_owner LOGIN PASSWORD 'testkit-postgres';
CREATE ROLE identity_app LOGIN PASSWORD 'testkit-postgres';
";

const DATABASE: &str = "identity";
const CREATE_DATABASE: &str = "CREATE DATABASE identity OWNER identity_owner";
const DATABASE_TEMPLATE: &str = include_str!("../../../crates/service-kit/sql/database.sql");
const MIGRATION: &str = include_str!("../migrations/postgres/0001_identity.sql");

/// One membership: `$1` tenant, `$2` the row id, kept as the member's user id (a new user, so
/// the foreign key holds), `$3` the canary, which this table has no text column for.
const SEED_MEMBERSHIP: &str = "WITH member AS (
    INSERT INTO identity.users (id) VALUES ($2::uuid) RETURNING id
)
INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
SELECT $1::uuid, member.id, 'viewer' FROM member WHERE $3::text IS NOT NULL";

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(postgres) = tls_postgres(ROLES).await else {
        return;
    };
    let mut admin = postgres.connect("postgres", "postgres").await;
    sqlx::raw_sql(CREATE_DATABASE)
        .execute(&mut admin)
        .await
        .expect("create the database");
    let mut owner = postgres.connect("identity_owner", DATABASE).await;
    for sql in [DATABASE_TEMPLATE, MIGRATION] {
        sqlx::raw_sql(sql)
            .execute(&mut owner)
            .await
            .expect("apply the schema as the migration role");
    }

    let store = PgStore::new(
        postgres.options("identity_owner", DATABASE),
        postgres.options("identity_app", DATABASE),
    )
    .table(pg_table!("identity.memberships", seed = SEED_MEMBERSHIP))
    .exempt(
        "identity.users",
        "users span tenants: the platform-wide user record, no tenant data (services/identity/README.md)",
    )
    .exempt(
        "identity.sessions",
        "a session belongs to a user and is found by its cookie hash before any tenant is known; \
         the tenant role is always read from memberships under row-level security \
         (services/identity/README.md)",
    );

    let issuer = TestIssuer::new();
    Suite::new(ServiceName::Identity, &issuer, store)
        .without_api(
            "only the ingress gateway's ext_authz check under /internal, which serves no tenant \
             data; the login API arrives with Laysi/trustiot-v2#103",
        )
        .run(None)
        .await
        .assert_isolated();
}
