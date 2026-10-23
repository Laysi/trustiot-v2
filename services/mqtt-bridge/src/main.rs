//! The MQTT bridge's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("mqtt-bridge", env!("CARGO_PKG_VERSION"))
            .with_log_ceilings(mqtt_bridge::LOG_CEILINGS),
        mqtt_bridge::start,
    )
    .await
}
