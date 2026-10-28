//! Desired configurations from the assets service's events (Laysi/trustiot-v2#719; README.md,
//! "由事件到設定"), on the product's schema applied by the migration Job (`testkit::product`). The
//! events are written as the assets service writes them, and the service's handler runs in a
//! consumer of the service's own subscription in the test's thread:
//!
//! - A device created or updated makes the gateway's next version, which crates/protocol's codec
//!   decodes and validates as the gateway does; a deleted device is gone from the next version;
//!   an older event delivered late and the whole range reprocessed make no version
//!   (acceptance 1 and 2).
//! - A device event of another tenant than the gateway's current binding, and one of the
//!   tenant's ended binding, are in no configuration; a gateway bound to another tenant gets a
//!   higher version without the earlier tenant's devices (acceptance 3).
//! - Ending the binding stores the configuration without devices once (acceptance 2).
//! - The read interface for the delivery (Laysi/trustiot-v2#720) returns the latest version, and
//!   the platform role cannot write a configuration.
//!
//! The tests of this binary share the subscription, as the service's replicas do. Starts a
//! throwaway Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use chrono::Utc;
use contract::{DeviceId, EventId, TenantId};
use gateway_ops::desired::DesiredConfigs;
use sqlx::Connection as _;
use support::{
    Assets, LOGINS, consumer, deliver, device, hardware_id, model, platform, position,
    reprocess_from, versions,
};
use testkit::product::{DATABASE, product_postgres};

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

fn id_text(device: DeviceId) -> String {
    device.to_string()
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test(flavor = "multi_thread")]
async fn device_changes_make_versions_and_replays_make_none() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, _) = consumer(&product).await;
    let tenant = TenantId::generate();
    let gateway = hardware_id("go-versions");

    // 1. Bound: the first version, without devices.
    let (binding, bound) = assets.bind(tenant, &gateway).await;
    deliver(&product, &consumer, &[bound]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert!(stored[0].bound && stored[0].devices().is_empty());
    assert_eq!(
        (stored[0].tenant_id, stored[0].binding_id),
        (tenant, binding)
    );

    // 2. A model no device uses yet changes no configuration.
    let meter = model("meter");
    let (meter_id, model_event) = assets.create_model(tenant, &meter).await;
    deliver(&product, &consumer, &[model_event]).await;
    assert_eq!(versions(&mut admin, &gateway).await.len(), 1);

    // 3. Two devices, each a version; the payload decodes and validates as the gateway's.
    let (first, second) = (DeviceId::generate(), DeviceId::generate());
    let first_device = device(first, &meter, 1);
    let created_first = assets
        .create_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            first,
            &first_device,
        )
        .await;
    deliver(&product, &consumer, &[created_first]).await;
    let created_second = assets
        .create_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            second,
            &device(second, &meter, 2),
        )
        .await;
    deliver(&product, &consumer, &[created_second]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(
        stored.iter().map(|s| s.version).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(stored[1].devices(), [id_text(first)]);
    assert_eq!(stored[1].event_id, created_first);
    assert_eq!(
        stored[2].devices(),
        sorted(vec![id_text(first), id_text(second)])
    );
    assert_eq!(stored[2].device_count, 2);
    assert_eq!(
        stored[2].config.models,
        std::slice::from_ref(&meter),
        "each model once"
    );

    // 4. An update is a version; the same content again (a later revision that changes nothing
    //    the gateway polls) and an older revision delivered late are not.
    let mut changed = first_device.clone();
    changed.poll_interval_ms = 30_000;
    let updated = assets
        .update_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            first,
            2,
            &changed,
        )
        .await;
    deliver(&product, &consumer, &[updated]).await;
    let same_again = assets
        .update_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            first,
            3,
            &changed,
        )
        .await;
    let late = assets
        .update_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            first,
            1,
            &first_device,
        )
        .await;
    deliver(&product, &consumer, &[same_again, late]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 4, "{stored:?}");
    let polled = stored[3]
        .config
        .devices
        .iter()
        .find(|d| d.id.as_str() == id_text(first))
        .expect("the updated device");
    assert_eq!(polled.poll_interval_ms, 30_000, "the newest revision wins");

    // 5. Deleted: the next version no longer lists it.
    let deleted = assets
        .delete_device(tenant, &gateway, meter_id, Some(binding), second, 1)
        .await;
    deliver(&product, &consumer, &[deleted]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[4].devices(), [id_text(first)]);
    assert_eq!(stored[4].device_count, 1);

    // 6. Every event of this test reprocessed (an operator's replay of the range): no version.
    let all: Vec<EventId> = vec![
        bound,
        model_event,
        created_first,
        created_second,
        updated,
        same_again,
        late,
        deleted,
    ];
    reprocess_from(&product, position(&mut admin, bound).await).await;
    deliver(&product, &consumer, &all).await;
    let replayed = versions(&mut admin, &gateway).await;
    assert_eq!(replayed.len(), 5, "a replay makes no version: {replayed:?}");
    assert_eq!(replayed[4].devices(), [id_text(first)]);
    println!(
        "evidence: versions {:?}; devices per version {:?}; 5 versions after reprocessing {} \
         events",
        replayed.iter().map(|s| s.version).collect::<Vec<_>>(),
        replayed.iter().map(|s| s.device_count).collect::<Vec<_>>(),
        all.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn other_tenants_and_ended_bindings_stay_out() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, _) = consumer(&product).await;
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());
    let gateway = hardware_id("go-tenants");

    // 1. Bound to tenant A, with one of A's devices.
    let (binding_a, bound_a) = assets.bind(tenant_a, &gateway).await;
    let meter = model("meter");
    let (meter_a, model_a) = assets.create_model(tenant_a, &meter).await;
    let device_a = DeviceId::generate();
    let created_a = assets
        .create_device(
            tenant_a,
            &gateway,
            meter_a,
            Some(binding_a),
            device_a,
            &device(device_a, &meter, 1),
        )
        .await;
    deliver(&product, &consumer, &[bound_a, model_a, created_a]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].devices(), [id_text(device_a)]);

    // 2. Tenant B writes a device on the same hardware ID while A holds it, once without a
    //    binding (an event written before 2026-10-29) and once with a binding that is not the
    //    gateway's: neither reaches any configuration.
    let (meter_b, model_b) = assets.create_model(tenant_b, &meter).await;
    let (stray, legacy) = (DeviceId::generate(), DeviceId::generate());
    let stray_event = assets
        .create_device(
            tenant_b,
            &gateway,
            meter_b,
            Some(contract::GatewayBindingId::generate()),
            stray,
            &device(stray, &meter, 2),
        )
        .await;
    let legacy_event = assets
        .create_device(
            tenant_b,
            &gateway,
            meter_b,
            None,
            legacy,
            &device(legacy, &meter, 3),
        )
        .await;
    deliver(&product, &consumer, &[model_b, stray_event, legacy_event]).await;
    assert_eq!(
        versions(&mut admin, &gateway).await.len(),
        2,
        "B's devices ignored"
    );

    // 3. The gateway is bound to tenant B now: the next version is higher, in tenant B, and
    //    lists neither A's device nor B's device of another binding. B's device written without
    //    a binding is B's and stays in (README.md, "綁定與租戶").
    let (binding_b, bound_b) = assets.bind(tenant_b, &gateway).await;
    deliver(&product, &consumer, &[bound_b]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 3);
    assert_eq!(
        (stored[2].version, stored[2].tenant_id, stored[2].binding_id),
        (3, tenant_b, binding_b)
    );
    assert_eq!(stored[2].devices(), [id_text(legacy)]);

    // 4. A's device changes after the gateway left A: no version. B's new device of the
    //    current binding: the next version.
    let mut moved = device(device_a, &meter, 1);
    moved.port = 502;
    let late_a = assets
        .update_device(
            tenant_a,
            &gateway,
            meter_a,
            Some(binding_a),
            device_a,
            2,
            &moved,
        )
        .await;
    let device_b = DeviceId::generate();
    let created_b = assets
        .create_device(
            tenant_b,
            &gateway,
            meter_b,
            Some(binding_b),
            device_b,
            &device(device_b, &meter, 4),
        )
        .await;
    deliver(&product, &consumer, &[late_a, created_b]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(
        stored.iter().map(|s| s.version).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        stored[3].devices(),
        sorted(vec![id_text(legacy), id_text(device_b)])
    );
    for version in &stored {
        let listed = version.devices();
        assert!(!listed.contains(&id_text(stray)), "{version:?}");
        if version.tenant_id == tenant_b {
            assert!(!listed.contains(&id_text(device_a)), "{version:?}");
        }
    }
    println!(
        "evidence: versions {:?} in tenants {:?}; tenant B's device of another binding is in \
         none; tenant A's device is in none of tenant B's",
        stored.iter().map(|s| s.version).collect::<Vec<_>>(),
        stored
            .iter()
            .map(|s| if s.tenant_id == tenant_a { "A" } else { "B" })
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ending_the_binding_leaves_a_configuration_without_devices() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, handler) = consumer(&product).await;
    let work = support::work(&product);
    let tenant = TenantId::generate();
    let gateway = hardware_id("go-unbind");

    let (binding, bound) = assets.bind(tenant, &gateway).await;
    let meter = model("meter");
    let (meter_id, model_event) = assets.create_model(tenant, &meter).await;
    let device_id = DeviceId::generate();
    let created = assets
        .create_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            device_id,
            &device(device_id, &meter, 1),
        )
        .await;
    deliver(&product, &consumer, &[bound, model_event, created]).await;
    assert_eq!(versions(&mut admin, &gateway).await.len(), 2);

    // What the unbinding event of Laysi/trustiot-v2#115 will do, in its delivery's transaction:
    // twice, as a repeated delivery would.
    for _ in 0..2 {
        let mut tx = work
            .begin(tenant, "test-unbind")
            .await
            .expect("a tenant transaction");
        handler
            .apply_unbinding(&mut tx, tenant, binding, Utc::now(), EventId::generate())
            .await
            .expect("end the binding");
        tx.commit().await.expect("commit");
    }
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 3, "stored once: {stored:?}");
    assert!(!stored[2].bound && stored[2].devices().is_empty());
    assert_eq!(stored[2].device_count, 0);
    assert_eq!(
        (stored[2].tenant_id, stored[2].binding_id),
        (tenant, binding)
    );

    // A device event after the binding ended changes nothing.
    let mut changed = device(device_id, &meter, 1);
    changed.unit_id = 9;
    let late = assets
        .update_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            device_id,
            2,
            &changed,
        )
        .await;
    deliver(&product, &consumer, &[late]).await;
    assert_eq!(versions(&mut admin, &gateway).await.len(), 3);

    // An unbinding this service never recorded is a no-op.
    let mut tx = work
        .begin(tenant, "test-unbind")
        .await
        .expect("a tenant transaction");
    handler
        .apply_unbinding(
            &mut tx,
            tenant,
            contract::GatewayBindingId::generate(),
            Utc::now(),
            EventId::generate(),
        )
        .await
        .expect("nothing to end");
    tx.commit().await.expect("commit");
    assert_eq!(versions(&mut admin, &gateway).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_delivery_reads_the_latest_version_and_cannot_write() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, _) = consumer(&product).await;
    let (tenant_a, tenant_b) = (TenantId::generate(), TenantId::generate());
    let (gateway_a, gateway_b, unknown) = (
        hardware_id("go-read-a"),
        hardware_id("go-read-b"),
        hardware_id("go-read-none"),
    );
    let (binding_a, bound_a) = assets.bind(tenant_a, &gateway_a).await;
    let (_, bound_b) = assets.bind(tenant_b, &gateway_b).await;
    let meter = model("meter");
    let (meter_id, model_event) = assets.create_model(tenant_a, &meter).await;
    let device_id = DeviceId::generate();
    let created = assets
        .create_device(
            tenant_a,
            &gateway_a,
            meter_id,
            Some(binding_a),
            device_id,
            &device(device_id, &meter, 1),
        )
        .await;
    deliver(
        &product,
        &consumer,
        &[bound_a, bound_b, model_event, created],
    )
    .await;

    let platform = platform(&product);
    let reads = DesiredConfigs::new(&platform);
    let current = reads
        .current(&gateway_a)
        .await
        .expect("read")
        .expect("gateway A has a configuration");
    let stored = versions(&mut admin, &gateway_a).await;
    let latest = stored.last().expect("a version");
    assert_eq!(
        i64::try_from(current.version.get()).expect("a version"),
        latest.version
    );
    assert_eq!(
        (current.tenant_id, current.binding_id),
        (tenant_a, binding_a)
    );
    assert!(current.bound);
    assert_eq!(current.device_count, 1);
    assert_eq!(
        protocol::config::DesiredConfig::decode(&current.payload).expect("valid"),
        latest.config,
        "the payload exactly as stored"
    );
    assert!(reads.current(&unknown).await.expect("read").is_none());

    let all = reads.latest_versions().await.expect("read");
    let of = |gateway: &protocol::GatewayId| {
        all.iter()
            .find(|latest| &latest.hardware_id == gateway)
            .map(|latest| latest.version.get())
    };
    assert_eq!(of(&gateway_a), Some(2));
    assert_eq!(of(&gateway_b), Some(1));
    assert_eq!(of(&unknown), None);

    // The platform role reads only: its transactions are READ ONLY, and it has no INSERT.
    let mut connection = sqlx::PgConnection::connect_with(
        &product.postgres.options("gateway_ops_platform", DATABASE),
    )
    .await
    .expect("connect as the platform role");
    let refused = sqlx::query(
        "INSERT INTO gateway_ops.desired_configs
            (tenant_id, hardware_id, version, binding_id, bound, payload, device_count, event_id)
         VALUES ($1::uuid, $2, 99, $3::uuid, true, '\\x01', 0, $3::uuid)",
    )
    .bind(tenant_a.to_string())
    .bind(gateway_a.as_str())
    .bind(binding_a.to_string())
    .execute(&mut connection)
    .await
    .expect_err("the platform role cannot write a configuration");
    assert_eq!(
        refused
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{refused}"
    );

    // The service role in tenant B sees none of tenant A's configurations.
    let work = support::work(&product);
    let mut tx = work.begin(tenant_b, "test-read").await.expect("tenant B");
    let seen: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway_ops.desired_configs WHERE hardware_id = $1",
    )
    .bind(gateway_a.as_str())
    .fetch_one(&mut *tx)
    .await
    .expect("read as tenant B");
    tx.commit().await.expect("commit");
    assert_eq!(seen, 0, "tenant B reads none of gateway A's configurations");
}
