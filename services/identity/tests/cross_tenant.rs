//! Cross-tenant negative tests (decision 29, crates/testkit): the identity service's role
//! cannot read or change another tenant's memberships, tenant, events, event aggregates'
//! sequences or recorded events (the event log, Laysi/trustiot-v2#130), its API never shows one
//! tenant another tenant's operation log, and every relation it can reach is declared. The
//! schema is the product's, applied by the migration Job as in a deployment
//! (`testkit::product`), so the outbox tables the Job creates are covered too, and the service is
//! the real binary.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use std::time::Duration;

use contract::ServiceName;
use serde_json::json;
use support::{KeyFiles, Service, TestKey, signing_keys};
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::Endpoint;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const KID: &str = "2026-10-a";

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

/// One recorded event of the tenant (Laysi/trustiot-v2#130): `$2` is the event id, the canary
/// the device model's id. Every producer in `CONSUMES` is recorded in this one table under the
/// same row-level security; the seeded row is one of assets' device model events, the producer
/// that is not the service itself (tests/event_log.rs records both producers end to end).
const SEED_LOGGED_EVENT: &str = "INSERT INTO identity.event_log
    (tenant_id, event_id, occurred_at, event_type, version, producer, aggregate_type,
     aggregate_id, aggregate_seq, payload)
VALUES ($1::uuid, $2::uuid, now(), 'assets.device_model_created', 1, 'assets',
        'device_model', $3, 1, '{}'::jsonb)";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the event log's consumer reads \
     and writes it in transactions without a tenant";

/// Why the migration records are exempt (Laysi/trustiot-v2#402).
const MIGRATION_RECORDS: &str = "the migration Job's record of the files applied in this schema \
     (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no tenant \
     data; the service role only reads it, for its readiness check";

/// The identity binary on `product`, trusting the test issuer's tokens (and its own signing key,
/// which readiness needs), with the event log on.
fn start(product: &ProductPostgres, issuer: &TestIssuer) -> Service {
    let key = TestKey::new(29);
    let mut published: serde_json::Value =
        serde_json::from_str(&issuer.jwks()).expect("the issuer's key set");
    published["keys"]
        .as_array_mut()
        .expect("keys")
        .push(key.jwk(KID));
    let files = KeyFiles::with(
        "cross-tenant",
        &signing_keys(&[(&key, KID, 1, 0)]),
        &published.to_string(),
    );
    let routes = json!([
        {"host": "app.saas.trustiot.test", "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        (
            "TRUSTIOT__DATABASE__PLATFORM_USERNAME".into(),
            "identity_platform".into(),
        ),
        (
            "TRUSTIOT__DATABASE__PLATFORM_PASSWORD".into(),
            TEST_PASSWORD.into(),
        ),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(Duration::from_secs(60));
    service
}

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let identity = start(&product, &issuer);
    let base_url = format!("http://{}", identity.address);

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
    // The event log: the service role inserts (the consumer) and reads its tenant's recorded
    // events, of every producer it consumes, and may never change one (Laysi/trustiot-v2#130).
    .table(pg_table!("identity.event_log", seed = SEED_LOGGED_EVENT))
    .exempt(
        "identity.platform_event_log",
        "platform-wide events (the envelope has no tenant), so no tenant column: the consumer \
         inserts them in a transaction without a tenant, and the service role cannot read them \
         (services/identity/README.md, 事件層與操作紀錄)",
    )
    .exempt("identity.outbox_subscription", CONSUMER_TABLE)
    .exempt("identity.outbox_inbox", CONSUMER_TABLE)
    .exempt("identity.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "identity.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service",
    )
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
    .exempt("identity.trustiot_migrate_applied", MIGRATION_RECORDS);

    Suite::new(ServiceName::Identity, &issuer, store)
        .openapi(serde_json::to_value(identity::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list(
            "/api/identity/v1/event-log",
            "identity.event_log",
        ))
        .exempt_endpoint(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/event-log",
            "platform administrators only (A2): the back office reads any tenant's log by \
             design, as the platform role; a tenant administrator gets 403 (tests/event_log.rs \
             covers it)",
        )
        .exempt_endpoint(
            "GET /api/identity/v1/platform/event-log",
            "platform administrators only (A2), and it serves platform-wide events, which belong \
             to no tenant (tests/event_log.rs covers it)",
        )
        .exempt_endpoint(
            "POST /api/identity/v1/tenants",
            "platform administrators only, and it creates a new tenant whose ID the service makes: \
             it takes no tenant from the request and serves no existing tenant's data \
             (tests/tenants.rs covers it)",
        )
        .run(Some(&base_url))
        .await
        .assert_isolated();
    drop(identity);
}
