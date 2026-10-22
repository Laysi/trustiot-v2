//! The web service's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("web", env!("CARGO_PKG_VERSION")),
        web::start,
    )
    .await
}
