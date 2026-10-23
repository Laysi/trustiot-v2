//! The assistant's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("assistant", env!("CARGO_PKG_VERSION")),
        assistant::start,
    )
    .await
}
