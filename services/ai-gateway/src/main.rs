//! The AI gateway's executable; see the library and `README.md`.

use std::process::ExitCode;

use service_kit::ServiceInfo;

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(
        ServiceInfo::new("ai-gateway", env!("CARGO_PKG_VERSION"))
            .with_env_prefix(ai_gateway::ENV_PREFIX),
        ai_gateway::start,
    )
    .await
}
