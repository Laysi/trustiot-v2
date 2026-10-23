//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#65 acceptance 3,
//! Laysi/trustiot-v2#99 acceptance 3, Laysi/trustiot-v2#101 acceptance 4): the assets service's
//! role cannot read or change another tenant's rows (tenants, device models and their points,
//! gateway bindings, events and their aggregates' sequences), its API never shows one tenant another tenant's data or writes into another
//! tenant, and every relation the role can reach and every API operation is declared. The schema
//! is the product's, applied by the migration Job (`testkit::product`), and the service is the
//! real binary.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

mod support;

use contract::ServiceName;
use serde_json::json;
use support::Service;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One row per tenant: `$1` tenant, `$2` the tenant id again (one_per_tenant), `$3` a canary,
/// which this table has no text column for.
const SEED_TENANT: &str = "INSERT INTO assets.tenants (tenant_id)
SELECT $1::uuid WHERE $2::uuid IS NOT NULL AND $3::text IS NOT NULL";

/// One device model: `$2` is the model id, which the name repeats; the canary does not fit the
/// naming rule, so no column holds it. The tenant's row must exist (the foreign key); it does
/// when `assets.tenants` was seeded first, and the CTE adds it otherwise.
const SEED_MODEL: &str = "WITH tenant AS (
    INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid) ON CONFLICT (tenant_id) DO NOTHING
)
INSERT INTO assets.device_models (tenant_id, model_id, name)
SELECT $1::uuid, $2::uuid, 'm' || replace($2::text, '-', '') WHERE $3::text IS NOT NULL";

/// One data point of a new model: `$2` is the model id, the canary the point's description.
const SEED_POINT: &str = "WITH tenant AS (
    INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid) ON CONFLICT (tenant_id) DO NOTHING
), model AS (
    INSERT INTO assets.device_models (tenant_id, model_id, name)
    VALUES ($1::uuid, $2::uuid, 'p' || replace($2::text, '-', ''))
    RETURNING tenant_id, model_id
)
INSERT INTO assets.device_model_points
    (tenant_id, model_id, position, key, function_code, address, data_type, description)
SELECT tenant_id, model_id, 0, 'canary', 3, 0, 'u16', $3 FROM model";

/// One event of the tenant in the outbox: `$2` is the event id, the canary the aggregate id.
const SEED_EVENT: &str = "INSERT INTO assets.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (1, $2::uuid, 'testkit.canary', $1::uuid, 'testkit', $3, 1, '{}'::jsonb)";

/// One aggregate of the tenant, its sequence in the outbox: the canary is the aggregate id; the
/// table has no column for `$2`.
const SEED_AGGREGATE: &str = "INSERT INTO assets.outbox_aggregate
    (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx)
SELECT 'testkit', $3, $1::uuid, 1, 1 WHERE $2::uuid IS NOT NULL";

/// One binding of a new gateway: `$2` is the binding id, the canary the gateway's hardware ID
/// (`canary-a-<32 hex>` fits the hardware ID rule). The gateway enters the pool and the tenant's
/// row is added in the same statement (the foreign keys).
const SEED_BINDING: &str = "WITH tenant AS (
    INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid) ON CONFLICT (tenant_id) DO NOTHING
), gateway AS (
    INSERT INTO assets.gateway_pool (hardware_id, registered_at, workstation)
    VALUES ($3, now(), 'testkit')
    RETURNING hardware_id
)
INSERT INTO assets.gateway_bindings (tenant_id, binding_id, hardware_id)
SELECT $1::uuid, $2::uuid, hardware_id FROM gateway";

/// Why the platform routes of the device pool are not probed here.
const PLATFORM_ROUTE: &str = "platform administrators only (Laysi/trustiot-v2#101): every tenant \
     user, the tenant admins this suite probes with included, gets 403 before anything is read, \
     and the platform role reads across tenants by design; tests/gateway_pool.rs checks the \
     refusals, and that a binding is written into the tenant of its body only";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

const MODELS: &str = "/api/assets/v1/device-models";
const MODEL: &str = "/api/assets/v1/device-models/{model_id}";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;

    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("assets_app", DATABASE),
    )
    .table(pg_table!(
        "assets.tenants",
        seed = SEED_TENANT,
        one_per_tenant
    ))
    .table(pg_table!("assets.device_models", seed = SEED_MODEL))
    .table(pg_table!("assets.device_model_points", seed = SEED_POINT))
    // The service role reads its tenant's bindings and inserts new ones; it has no UPDATE or
    // DELETE, and the unique index across tenants tells it only that a gateway is taken.
    .table(pg_table!("assets.gateway_bindings", seed = SEED_BINDING))
    // The service role only inserts events (no SELECT or UPDATE): the probes check that it
    // cannot write one for another tenant.
    .table(pg_table!("assets.outbox_event", seed = SEED_EVENT))
    // The service role reads and advances the sequence of its tenant's aggregates only.
    .table(pg_table!("assets.outbox_aggregate", seed = SEED_AGGREGATE))
    .exempt(
        "assets.gateway_pool",
        "the device pool (Laysi/trustiot-v2#101): gateways registered at the factory, which \
         belong to no tenant; no tenant_id. The service role may only insert (the consumer of \
         certs.gateway_registered) and cannot read it; which tenant holds a gateway is in \
         assets.gateway_bindings",
    )
    .exempt("assets.outbox_subscription", CONSUMER_TABLE)
    .exempt("assets.outbox_inbox", CONSUMER_TABLE)
    .exempt("assets.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "assets.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far",
    )
    .exempt(
        "assets.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check \
         (Laysi/trustiot-v2#402)",
    );

    // A valid model whose point's description is the canary; `{id}` makes the name unique and
    // keeps it within the naming rule (a letter, then the UUID's groups).
    let points = json!([{"key": "p", "function_code": 3, "address": 0, "data_type": "u16",
                         "description": "{canary}"}]);
    Suite::new(ServiceName::Assets, &issuer, store)
        .openapi(serde_json::to_value(assets::openapi()).expect("the OpenAPI document"))
        .endpoint(Endpoint::list("/api/assets/v1/tenant", "assets.tenants"))
        .endpoint(Endpoint::list(MODELS, "assets.device_models"))
        .endpoint(Endpoint::get(MODEL, "assets.device_models"))
        .endpoint(Endpoint::get(
            "/api/assets/v1/device-models/{model_id}/export",
            "assets.device_models",
        ))
        .endpoint(
            // Each write probe gets a seeded row of its own, still at revision 1.
            Endpoint::item(Method::PUT, MODEL, "assets.device_models")
                .body(json!({"revision": 1, "points": points})),
        )
        .endpoint(Endpoint::item(
            Method::DELETE,
            MODEL,
            "assets.device_models",
        ))
        .endpoint(
            Endpoint::create(Method::POST, MODELS, "assets.device_models")
                .body(json!({"name": "c{id}", "points": points})),
        )
        .endpoint(
            Endpoint::create(
                Method::POST,
                "/api/assets/v1/device-models/import",
                "assets.device_models",
            )
            .body(
                json!({"format": "trustiot.device_model", "format_version": 1,
                         "name": "i{id}", "points": points}),
            ),
        )
        .exempt_endpoint("GET /api/assets/v1/platform/gateways", PLATFORM_ROUTE)
        .exempt_endpoint(
            "GET /api/assets/v1/platform/gateways/{hardware_id}",
            PLATFORM_ROUTE,
        )
        .exempt_endpoint(
            "PUT /api/assets/v1/platform/gateways/{hardware_id}/binding",
            PLATFORM_ROUTE,
        )
        .run(Some(&assets.base_url))
        .await
        .assert_isolated();
}
