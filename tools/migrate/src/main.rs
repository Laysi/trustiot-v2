//! `migrate [run]`: one migration run (the chart's Job). `migrate wait-for-job`: the init
//! container that holds a service's pod until the run has completed. `migrate
//! clickhouse-admin-password`: the bundled ClickHouse's init container, which puts the bootstrap
//! admin password in place before the server starts. Settings come from the
//! environment (crates/service-kit, "設定"); logs are JSON lines on stdout. Exit codes: 0 done,
//! 1 failed, 2 unusable settings or arguments.

use std::process::ExitCode;
use std::time::Duration;

use migrate::Report;
use migrate::config::{ClickHouseAdminConfig, RunConfig, WaitForJobConfig};
use migrate::registry::PRODUCT;
use serde::de::DeserializeOwned;
use service_kit::ServiceInfo;
use service_kit::config::{self, Settings, Validate};
use service_kit::telemetry::{self, Telemetry};

const INFO: ServiceInfo = ServiceInfo::new("migrate", env!("CARGO_PKG_VERSION"));

/// How long buffered telemetry may take to leave at exit (nothing is exported by default).
const TELEMETRY_FLUSH: Duration = Duration::from_secs(2);

fn load<T: DeserializeOwned + Validate>() -> Result<(Settings<T>, Telemetry), ExitCode> {
    let settings = match config::load::<T>(INFO.env_prefix) {
        Ok(settings) => settings,
        Err(error) => {
            telemetry::init_fallback(&INFO);
            tracing::error!(error = %error, "unusable settings");
            return Err(ExitCode::from(2));
        }
    };
    match Telemetry::init(&INFO, &settings.kit.telemetry) {
        Ok(telemetry) => Ok((settings, telemetry)),
        Err(error) => {
            telemetry::init_fallback(&INFO);
            tracing::error!(error = %error, "cannot start logging");
            Err(ExitCode::from(2))
        }
    }
}

async fn run() -> ExitCode {
    let (settings, telemetry) = match load::<RunConfig>() {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };
    let code = match migrate::run(&settings.app, &PRODUCT).await {
        Ok(report) => {
            tracing::info!(
                changes = report.changes.len(),
                roles_without_password = ?report.roles_without_password,
                "migration run finished"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(error = %error, "migration run failed");
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown(TELEMETRY_FLUSH).await;
    code
}

async fn wait_for_job() -> ExitCode {
    let (settings, telemetry) = match load::<WaitForJobConfig>() {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };
    let code = match migrate::kube::wait_for_job(&settings.app.kube).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "stopped waiting for the migration Job");
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown(TELEMETRY_FLUSH).await;
    code
}

async fn clickhouse_admin_password() -> ExitCode {
    let (settings, telemetry) = match load::<ClickHouseAdminConfig>() {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };
    let mut report = Report::default();
    let code = match migrate::clickhouse_admin::generate(&settings.app.admin, &mut report).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "the bootstrap admin password is not in place");
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown(TELEMETRY_FLUSH).await;
    code
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        None | Some("run") => run().await,
        Some("wait-for-job") => wait_for_job().await,
        Some("clickhouse-admin-password") => clickhouse_admin_password().await,
        Some(_) => {
            telemetry::init_fallback(&INFO);
            tracing::error!(
                "unknown command; use `migrate run`, `migrate wait-for-job` or `migrate \
                 clickhouse-admin-password`"
            );
            ExitCode::from(2)
        }
    }
}
