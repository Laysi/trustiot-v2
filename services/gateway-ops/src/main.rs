//! The gateway operations service's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("gateway-ops", env!("CARGO_PKG_VERSION")),
        gateway_ops::start,
    )
    .await
}
