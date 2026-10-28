//! The gateway-ops binary (Laysi/trustiot-v2#719): it starts on the product's schema with the
//! roles the chart gives it, becomes ready, its consumer's subscription to the assets service's
//! events processes a binding and a device into stored versions, and SIGTERM stops it with exit
//! code 0.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use std::time::Duration;

use contract::{DeviceId, TenantId};
use support::{
    Assets, EVENT_LIMIT, LOGINS, Service, device, hardware_id, model, processed, versions, wait_for,
};
use testkit::product::product_postgres;

/// How long the service may take to drain and exit after SIGTERM: its drain delay is 100 ms and
/// its drain timeout 5 s (`Service::start`); the bound is for a loaded host.
const EXIT_LIMIT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn the_service_turns_assets_events_into_versions_and_stops_on_sigterm() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut service = Service::start(&product).await;
    service.wait_ready().await;
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let tenant = TenantId::generate();
    let gateway = hardware_id("go-start");

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
    for event in [bound, model_event, created] {
        wait_for(
            "the service's consumer processing the event",
            EVENT_LIMIT,
            || {
                let product = &product;
                async move {
                    let mut admin = product.superuser().await;
                    processed(&mut admin, event).await.then_some(())
                }
            },
        )
        .await;
    }
    let stored = versions(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert_eq!(stored[1].devices(), [device_id.to_string()]);
    let logged = service.stdout.messages("gateway configuration stored");
    assert!(
        logged
            .iter()
            .any(|line| line["fields"]["hardware_id"] == gateway.as_str()
                && line["fields"]["version"] == 2),
        "the service logs each stored version: {logged:?}"
    );

    let status = service.terminate(EXIT_LIMIT).await;
    assert_eq!(status.code(), Some(0), "stdout:\n{}", service.stdout.text());
    println!(
        "evidence: ready; versions {:?} for the gateway; exit code {:?} after SIGTERM",
        stored.iter().map(|s| s.version).collect::<Vec<_>>(),
        status.code()
    );
}
