//! The gateway-ops binary (Laysi/trustiot-v2#719, Laysi/trustiot-v2#720): it starts on the
//! product's schema with the roles the chart gives it and a broker, becomes ready, its consumer
//! turns a binding and a device into stored versions and the delivery publishes the latest one
//! retained, readiness fails while the broker is down and recovers with a republish when it is
//! back, and SIGTERM stops it with exit code 0. No log line carries the broker password.
//!
//! Starts a throwaway Postgres with TLS and a Mosquitto (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::time::Duration;

use contract::{DeviceId, TenantId};
use protocol::ConfigVersion;
use support::broker::{Broker, Gateway};
use support::{
    Assets, EVENT_LIMIT, LOGINS, START_LIMIT, Service, device, hardware_id, model, processed,
    versions, wait_for,
};
use testkit::product::product_postgres;

/// How long the service may take to drain and exit after SIGTERM: its drain delay is 100 ms and
/// its drain timeout 5 s (`Service::start`); the bound is for a loaded host.
const EXIT_LIMIT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn the_service_delivers_versions_retained_follows_the_broker_and_stops_on_sigterm() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let mqtt = broker.service_config();
    let mut service = Service::start(&product, &mqtt, "1s").await;
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
    let latest = ConfigVersion::new(2);
    let logged = service.stdout.messages("gateway configuration stored");
    assert!(
        logged
            .iter()
            .any(|line| line["fields"]["hardware_id"] == gateway.as_str()
                && line["fields"]["version"] == 2),
        "the service logs each stored version: {logged:?}"
    );

    // The consumer announced the versions: the delivery publishes the latest after the throttle
    // (logged after the PUBACK), and a subscriber that comes later gets it retained.
    let published = wait_for("the latest version's publish logged", START_LIMIT, || {
        let lines = service.stdout.messages("gateway configuration published");
        let found = lines.into_iter().find(|line| {
            line["fields"]["hardware_id"] == gateway.as_str() && line["fields"]["version"] == 2
        });
        async move { found }
    })
    .await;
    assert_eq!(published["fields"]["trigger"], "change", "{published}");
    let mut newcomer = Gateway::connect(&broker).await;
    let retained = newcomer
        .wait("the retained version", &gateway, |received| received.retain)
        .await;
    newcomer.close().await;
    assert_eq!(retained.version, latest);

    // Readiness follows the broker connection.
    broker.stop();
    let not_ready = wait_for("/readyz failing without the broker", START_LIMIT, || {
        let service = &service;
        async move {
            let (status, body) = service.readiness().await;
            (status != 200).then_some((status, body))
        }
    })
    .await;
    let failed: Vec<_> = service
        .stdout
        .messages("readiness check failed")
        .into_iter()
        .map(|line| line["fields"]["check"].clone())
        .collect();
    assert!(
        failed.iter().any(|check| check == "mqtt"),
        "the failing check is mqtt: {failed:?}"
    );
    broker.resume();
    service.wait_ready().await;
    // One republish at the first connection, one after the restart.
    let runs = wait_for(
        "the republish after the restart logged",
        START_LIMIT,
        || {
            let lines = service
                .stdout
                .messages("republished every gateway's configuration");
            async move { (lines.len() >= 2).then_some(lines.len()) }
        },
    )
    .await;
    // The restarted broker kept no retained message: this one is the republish.
    let mut newcomer = Gateway::connect(&broker).await;
    let republished = newcomer
        .wait("the republished version", &gateway, |received| {
            received.retain
        })
        .await;
    newcomer.close().await;
    assert_eq!(republished.version, latest);

    let status = service.terminate(EXIT_LIMIT).await;
    assert_eq!(status.code(), Some(0), "stdout:\n{}", service.stdout.text());
    let password = std::fs::read_to_string(&mqtt.password_file).expect("read the password file");
    for output in [service.stdout.text(), service.stderr.text()] {
        assert!(
            !output.contains(password.trim()),
            "the broker password is never logged"
        );
    }
    println!(
        "evidence: ready; versions {:?} for the gateway; a new subscriber got version {:?} \
         retained (QoS {}); /readyz without the broker: {} {} (failed checks {failed:?}); \
         after the restart: ready, \
         {runs} republish runs logged, a new subscriber got version {:?} retained; exit code \
         {:?} after SIGTERM",
        stored.iter().map(|s| s.version).collect::<Vec<_>>(),
        retained.version.map(ConfigVersion::get),
        retained.qos,
        not_ready.0,
        not_ready.1,
        republished.version.map(ConfigVersion::get),
        status.code()
    );
}
