//! The assets service's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("assets", env!("CARGO_PKG_VERSION")),
        assets::start,
    )
    .await
}
