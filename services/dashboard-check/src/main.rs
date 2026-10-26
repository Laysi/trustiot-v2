//! The dashboard check service's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("dashboard-check", env!("CARGO_PKG_VERSION")),
        dashboard_check::start,
    )
    .await
}
