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

use contract::ServiceName;
use support::start_on_product;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::Endpoint;
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

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let identity = start_on_product(&product, &issuer, "cross-tenant");
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
    .exempt(
        "identity.preferences",
        "a user's own settings (language, AI answer language, time zone, theme), not tenant \
         data: no tenant column; every query names the user of the request's token \
         (services/identity/README.md, 偏好設定; tests/account.rs covers that users see and \
         change only their own)",
    )
    .exempt(
        "identity.login_codes",
        "a login code belongs to a user and is checked before any session or tenant exists; it \
         holds a hash, a count and times, no tenant data (services/identity/README.md, 登入)",
    )
    .exempt("identity.trustiot_migrate_applied", MIGRATION_RECORDS);

    Suite::new(ServiceName::Identity, &issuer, store)
        .openapi(serde_json::to_value(identity::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list(
            "/api/identity/v1/event-log",
            "identity.event_log",
        ))
        // The current session shows the token's tenant with its name, read under row-level
        // security, and the user's own memberships, read with the platform role for the
        // token's user only (Laysi/trustiot-v2#497): never another tenant's name or members.
        .endpoint(Endpoint::list(
            "/api/identity/v1/sessions/current",
            "identity.tenants",
        ))
        // The members' read of their tenant's plan and entitlements (Laysi/trustiot-v2#111): the
        // token's tenant's row, read under row-level security; never another tenant's.
        .endpoint(Endpoint::list(
            "/api/identity/v1/entitlements",
            "identity.tenants",
        ))
        .exempt_endpoint(
            "PUT /api/identity/v1/sessions/current/tenant",
            "switches the caller's own session, found by its cookie and the token's session \
             claim, which a probe's token cannot name; its answer shows only the tenants the \
             caller is a member of (the same as GET /api/identity/v1/sessions/current, declared \
             above), and a tenant the user is not a member of is refused with the session \
             unchanged (tests/account.rs covers it, across users, sessions and tenants)",
        )
        .exempt_endpoint(
            "GET /api/identity/v1/preferences",
            "the caller's own settings, not tenant data: the user comes from the token, never \
             from the request (tests/account.rs covers it)",
        )
        .exempt_endpoint(
            "PATCH /api/identity/v1/preferences",
            "changes the caller's own settings, not tenant data: the user comes from the token, \
             never from the request (tests/account.rs covers it)",
        )
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
            "GET /api/identity/v1/platform/ai/model-roles",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#608): it reads the \
             AI gateway's model roles, which belong to no tenant; every tenant user and every \
             exchanged token get 403 before anything is sent (tests/ai_model_roles.rs covers it)",
        )
        .exempt_endpoint(
            "PUT /api/identity/v1/platform/ai/model-roles/{role}",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#608): it changes a \
             platform-wide AI model role, no tenant data; every tenant user and every exchanged \
             token get 403 before anything is sent (tests/ai_model_roles.rs covers it)",
        )
        .exempt_endpoint(
            "DELETE /api/identity/v1/platform/ai/model-roles/{role}",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#608): it resets a \
             platform-wide AI model role, no tenant data; every tenant user and every exchanged \
             token get 403 before anything is sent (tests/ai_model_roles.rs covers it)",
        )
        .exempt_endpoint(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/members",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#583): it sets a \
             membership in the tenant the path names, by design; every tenant user, the tenant's \
             own administrator and another tenant's included, and every exchanged token get 403 \
             before anything is read, and nothing of either tenant changes (tests/members.rs \
             covers it, with the service role's new grants on memberships kept to the \
             transaction's tenant; the probes above attack those grants too)",
        )
        .exempt_endpoint(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/state",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#776): it reads the \
             state of the tenant the path names, by design; every tenant user, the tenant's own \
             administrator and another tenant's included, and every exchanged token get 403 \
             before anything is read (tests/tenant_state.rs covers it)",
        )
        .exempt_endpoint(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/state",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#776): it suspends \
             or resumes the tenant the path names, by design; every tenant user, the tenant's own \
             administrator and another tenant's included, and every exchanged token get 403 \
             before anything is read, and nothing of either tenant changes (tests/tenant_state.rs \
             covers it, with the service role's new UPDATE grant on the tenant's state columns \
             kept to the transaction's tenant; the probes above attack that grant too)",
        )
        .exempt_endpoint(
            "GET /api/identity/v1/platform/tenants/{tenant_id}/entitlements",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#111): it reads the \
             plan and entitlements of the tenant the path names, by design; every tenant user, \
             the tenant's own administrator and another tenant's included, and every exchanged \
             token get 403 before anything is read (tests/entitlements.rs covers it)",
        )
        .exempt_endpoint(
            "PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements",
            "platform administrators' own requests only (A2, Laysi/trustiot-v2#111): it sets the \
             plan and overrides of the tenant the path names, by design; every tenant user, the \
             tenant's own administrator and another tenant's included, and every exchanged token \
             get 403 before anything is read, and nothing of either tenant changes \
             (tests/entitlements.rs covers it, with the service role's new UPDATE grant on the \
             entitlement columns kept to the transaction's tenant; the probes above attack that \
             grant too)",
        )
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
        .run(Some(&base_url))
        .await
        .assert_isolated();
    drop(identity);
}
