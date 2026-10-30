//! Cross-tenant negative tests (decision 29, crates/testkit; Laysi/trustiot-v2#719 acceptance 5,
//! Laysi/trustiot-v2#720): the gateway operations service's Postgres role cannot read or change
//! another tenant's bindings, models, devices, configurations or their publishes and reports,
//! and every relation the role can reach is declared. The schema is the product's, applied by the migration Job (`testkit::product`).
//!
//! The suite probes the service role, which keeps the projection and writes configurations in
//! the event's tenant. The platform role reads every tenant's bindings and configurations by
//! design (the consumer's reads of a gateway's binding and latest version, and the delivery's
//! reads, through the tables' `platform_access` policies, SELECT only);
//! `tests/desired_config.rs` checks that it cannot write a configuration.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use contract::ServiceName;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::postgres::PgStore;
use testkit::pg_table;
use testkit::product::{DATABASE, product_postgres};

/// One binding: `$2` is the binding ID, the canary the gateway's hardware ID (`canary-a-<32
/// hex>` fits the hardware ID rule). The log position is a placeholder.
const SEED_BINDING: &str = "INSERT INTO gateway_ops.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id)
VALUES ($1::uuid, $2::uuid, $3, now(), 1, 1)";

/// One model: `$2` is the model ID, the canary its name.
const SEED_MODEL: &str = "INSERT INTO gateway_ops.device_models
    (tenant_id, model_id, name, revision, model)
VALUES ($1::uuid, $2::uuid, $3, 1, jsonb_build_object('name', $3::text))";

/// One device: `$2` is the device ID, the canary its gateway's hardware ID.
const SEED_DEVICE: &str = "INSERT INTO gateway_ops.devices
    (tenant_id, device_id, hardware_id, model_id, revision, device)
VALUES ($1::uuid, $2::uuid, $3, gen_random_uuid(), 1, jsonb_build_object('id', $2::text))";

/// One configuration: `$2` is its binding ID, the canary its gateway's hardware ID.
const SEED_CONFIG: &str = "INSERT INTO gateway_ops.desired_configs
    (tenant_id, hardware_id, version, binding_id, bound, payload, device_count, event_id)
VALUES ($1::uuid, $3, 1, $2::uuid, true, '\\x01'::bytea, 0, gen_random_uuid())";

/// One configuration's publish: its configuration (the foreign key's parent) is written in the
/// same statement. `$2` is the configuration's binding ID, the canary its gateway's hardware ID.
const SEED_DELIVERY: &str = "WITH config AS (
    INSERT INTO gateway_ops.desired_configs
        (tenant_id, hardware_id, version, binding_id, bound, payload, device_count, event_id)
    VALUES ($1::uuid, $3, 1, $2::uuid, true, '\\x01'::bytea, 0, gen_random_uuid())
    RETURNING tenant_id, hardware_id, version
)
INSERT INTO gateway_ops.config_deliveries
    (tenant_id, hardware_id, version, first_published_at, last_published_at)
SELECT tenant_id, hardware_id, version, now(), now() FROM config";

const CONSUMER_TABLE: &str = "the outbox consumer's bookkeeping (crates/outbox): subscription \
     positions and processed or failed event IDs, no tenant data; the consumer reads and writes \
     it in transactions without a tenant";

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated() {
    let Some(product) = product_postgres(&["gateway_ops_app"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let store = PgStore::new(
        product.postgres.superuser(DATABASE),
        product.postgres.options("gateway_ops_app", DATABASE),
    )
    .table(pg_table!(
        "gateway_ops.gateway_bindings",
        seed = SEED_BINDING
    ))
    .table(pg_table!("gateway_ops.device_models", seed = SEED_MODEL))
    .table(pg_table!("gateway_ops.devices", seed = SEED_DEVICE))
    .table(pg_table!("gateway_ops.desired_configs", seed = SEED_CONFIG))
    .table(pg_table!(
        "gateway_ops.config_deliveries",
        seed = SEED_DELIVERY
    ))
    .exempt("gateway_ops.outbox_subscription", CONSUMER_TABLE)
    .exempt("gateway_ops.outbox_inbox", CONSUMER_TABLE)
    .exempt("gateway_ops.outbox_dead_letter", CONSUMER_TABLE)
    .exempt(
        "gateway_ops.outbox_version",
        "the versions notification-mode consumers stored per aggregate (crates/outbox), no \
         tenant data; unused by this service so far",
    )
    .exempt(
        "gateway_ops.trustiot_migrate_applied",
        "the migration Job's record of the files applied in this schema \
         (crates/service-kit/sql/migration-records.sql): file names and SHA-256 hashes, no \
         tenant data; the service role only reads it, for its readiness check \
         (Laysi/trustiot-v2#402)",
    );

    Suite::new(ServiceName::GatewayOps, &issuer, store)
        .without_api(
            "gateway-ops has no HTTP API besides the health endpoints so far (tools/openapi, \
             SERVICES_WITHOUT_API): it takes models, devices and bindings from the assets \
             service's events and delivers configurations over MQTT (Laysi/trustiot-v2#720)",
        )
        .run(None)
        .await
        .assert_isolated();
}
