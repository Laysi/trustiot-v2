//! The telemetry-query service's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("telemetry-query", env!("CARGO_PKG_VERSION")),
        telemetry_query::start,
    )
    .await
}
