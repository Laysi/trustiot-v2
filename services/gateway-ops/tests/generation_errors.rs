//! Configurations that cannot be made (Laysi/trustiot-v2#719 acceptance 4; README.md, "錯誤"):
//! a device whose model the projection does not have, a configuration crates/protocol's rules
//! refuse, and a model deleted while devices use it store no version, log an ERROR naming the
//! gateway and the reason, and leave the gateway's previous version as the latest; the event
//! that repairs the projection makes the next version. A device event of another tenant is
//! logged at WARN.
//!
//! One test in its own binary: the log is captured in the test's thread, where its consumer
//! runs the handler, and no other test's consumer may take the deliveries. Starts a throwaway
//! Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use contract::{DeviceId, DeviceModelId, TenantId};
use protocol::config::Transport;
use support::{Assets, Capture, LOGINS, consumer, deliver, device, hardware_id, model, versions};
use testkit::product::product_postgres;

/// The ERROR line of a configuration not made (`projection::AssetsEventsHandler::report`).
const NOT_GENERATED: &str =
    "gateway configuration not generated; the gateway keeps its previous version";
/// The line of a device event that changed no configuration.
const IGNORED: &str = "device event ignored for the gateway configuration";

#[tokio::test(flavor = "multi_thread")]
async fn a_configuration_that_cannot_be_made_keeps_the_previous_version() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let (capture, _guard) = Capture::install();
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, _) = consumer(&product).await;
    let tenant = TenantId::generate();
    let gateway = hardware_id("go-errors");
    let failures = |reason: &str| {
        capture
            .messages(NOT_GENERATED)
            .into_iter()
            .filter(|fields| {
                fields.get("hardware_id").map(String::as_str) == Some(gateway.as_str())
                    && fields.get("reason").map(String::as_str) == Some(reason)
            })
            .collect::<Vec<_>>()
    };

    let (binding, bound) = assets.bind(tenant, &gateway).await;
    deliver(&product, &consumer, &[bound]).await;
    assert_eq!(versions(&mut admin, &gateway).await.len(), 1);

    // 1. A device of a model the projection never received: no version, an ERROR.
    let meter = model("meter");
    let meter_id = DeviceModelId::generate();
    let first = DeviceId::generate();
    let created = assets
        .create_device(
            tenant,
            &gateway,
            meter_id,
            Some(binding),
            first,
            &device(first, &meter, 1),
        )
        .await;
    deliver(&product, &consumer, &[created]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(
        stored.len(),
        1,
        "no version for an unknown model: {stored:?}"
    );
    let unknown = failures("unknown_model");
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    assert_eq!(unknown[0]["level"], "ERROR");
    assert_eq!(unknown[0]["tenant_id"], tenant.to_string());
    assert!(
        unknown[0]["error"].contains(&meter_id.to_string()),
        "the error names the model: {unknown:?}"
    );

    // 2. The model arrives: the device is in the next version.
    let model_event = assets.create_model_as(tenant, meter_id, &meter).await;
    deliver(&product, &consumer, &[model_event]).await;
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].devices(), [first.to_string()]);

    // 3. A second device on the same converter with another transport breaks crates/protocol's
    //    rules: no version, an ERROR; version 2 stays the latest, unchanged.
    let second = DeviceId::generate();
    let mut mixed = device(second, &meter, 2);
    mixed.transport = Transport::ModbusTcp;
    let invalid = assets
        .create_device(tenant, &gateway, meter_id, Some(binding), second, &mixed)
        .await;
    deliver(&product, &consumer, &[invalid]).await;
    let after = versions(&mut admin, &gateway).await;
    assert_eq!(after.len(), 2, "no version for an invalid configuration");
    assert_eq!(
        after[1].config, stored[1].config,
        "the previous version is unchanged"
    );
    let refused = failures("invalid");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        !refused[0]["error"].contains("192.0.2.10"),
        "the log names issue codes and paths, not connection settings: {refused:?}"
    );

    // 4. The invalid device removed, then the model deleted under the remaining device: the
    //    removal is a version, the deletion is not.
    let removed = assets
        .delete_device(tenant, &gateway, meter_id, Some(binding), second, 1)
        .await;
    deliver(&product, &consumer, &[removed]).await;
    assert_eq!(
        versions(&mut admin, &gateway).await.len(),
        2,
        "same content as version 2"
    );
    let model_deleted = assets.delete_model(tenant, meter_id, &meter, 2).await;
    deliver(&product, &consumer, &[model_deleted]).await;
    let last = versions(&mut admin, &gateway).await;
    assert_eq!(last.len(), 2, "no version without the model");
    assert_eq!(last[1].devices(), [first.to_string()]);
    assert_eq!(failures("unknown_model").len(), 2);

    // 5. Another tenant's device on this gateway: a WARN, no version.
    let other = TenantId::generate();
    let stray = DeviceId::generate();
    let stray_event = assets
        .create_device(
            other,
            &gateway,
            DeviceModelId::generate(),
            None,
            stray,
            &device(stray, &meter, 3),
        )
        .await;
    deliver(&product, &consumer, &[stray_event]).await;
    assert_eq!(versions(&mut admin, &gateway).await.len(), 2);
    let ignored: Vec<_> = capture
        .messages(IGNORED)
        .into_iter()
        .filter(|fields| fields.get("device_id") == Some(&stray.to_string()))
        .collect();
    assert_eq!(ignored.len(), 1, "{ignored:?}");
    assert_eq!(ignored[0]["level"], "WARN");
    assert_eq!(ignored[0]["reason"], "other_tenant");
    println!(
        "evidence: unknown_model {:?}; invalid {:?}; other tenant {:?}; versions stay at 2",
        failures("unknown_model")[0],
        refused[0],
        ignored[0]
    );
}
