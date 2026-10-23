//! `trustiot-agent`, the gateway agent service (decision 10).
//!
//! ```text
//! trustiot-agent [--config <file>] [--check]
//! ```
//!
//! Start-up loads and checks the configuration, both TLS layers, the client certificate and
//! its key (the TPM in production), then tells systemd it is ready (`READY=1`), connected or
//! not: a gateway without network still runs and keeps trying. `--check` stops after the
//! checks, for operators and update health checks.
//!
//! Threads: the connection (TLS handshakes, MQTT) runs on its own thread with a single-threaded
//! tokio runtime, because a full inner handshake waits about 70 ms for the TPM signature inside
//! rustls' synchronous signing call; that blocks only this thread. Everything else (heartbeat,
//! the systemd watchdog's notifications, signals, and later the Modbus polling of
//! [`agent::polling`], with its own per-device watchdog) runs on a two-thread runtime.
//!
//! The systemd watchdog is fed only while the connection task makes progress and both runtimes
//! run: the connection task declares every wait with its deadline ([`agent::liveness`]), the
//! connection thread counts ticks, and the main runtime sends `WATCHDOG=1` only while the
//! declared wait is within its deadline and the count has moved since the last time.
//! SIGTERM (or SIGINT) publishes `offline`/`shutdown`, disconnects cleanly and exits within
//! [`STOP_LIMIT`].

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent::config::{self, Config};
use agent::heartbeat::{self, HeartbeatSettings};
use agent::identity::utc;
use agent::link::{self, Link, LinkState, Subscriptions};
use agent::liveness::{self, Progress};
use agent::sdnotify::{self, Notification, Notifier, PROGRESS_TICK};
use agent::setup::Setup;
use agent::tls::{RootCrl, Validity};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::Level;

/// How long the agent waits for the connection to close cleanly after SIGTERM; below the unit's
/// `TimeoutStopSec`.
const STOP_LIMIT: Duration = Duration::from_secs(10);

const USAGE: &str = "usage: trustiot-agent [--config <file>] [--check]

  --config <file>  the configuration file (default /etc/trustiot/agent.toml)
  --check          check the configuration, certificate and key, then exit
  --version        print the version
  --help           print this help

The log level is TRUSTIOT_AGENT_LOG (error, warn, info, debug, trace; default info).";

struct Args {
    config: PathBuf,
    check: bool,
}

enum Command {
    Run(Args),
    Help,
    Version,
}

fn parse_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Command, String> {
    let mut parsed = Args {
        config: PathBuf::from(config::DEFAULT_PATH),
        check: false,
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                parsed.config = PathBuf::from(args.next().ok_or("--config needs a file")?);
            }
            Some("--check") => parsed.check = true,
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--version" | "-V") => return Ok(Command::Version),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    Ok(Command::Run(parsed))
}

#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line program answers --help, --version and usage errors on the terminal"
)]
fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("trustiot-agent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("trustiot-agent: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    init_logging();
    let setup = match Config::load(&args.config)
        .map_err(|error| error.to_string())
        .and_then(|config| {
            Setup::new(&config).map_err(|error| format!("{}: {error}", args.config.display()))
        }) {
        Ok(setup) => setup,
        Err(error) => {
            tracing::error!("refusing to start: {error}");
            return ExitCode::FAILURE;
        }
    };
    if setup.file_key {
        tracing::warn!(
            "identity.key.source is \"file\": for development and tests only; production gateways keep the key in the TPM"
        );
    }
    if setup.certificate_validity == Validity::OutsideNow {
        tracing::warn!(
            "by this clock the client certificate is not valid (from {} to {}); starting anyway, because the clock may be wrong, but the broker refuses a certificate outside its validity",
            utc(setup.certificate_not_before),
            utc(setup.certificate_not_after)
        );
    }
    log_root_crl(&setup.root_crl);
    if args.check {
        println!(
            "ok: gateway {}, certificate valid until {}, configuration {}",
            setup.gateway,
            utc(setup.certificate_not_after),
            args.config.display()
        );
        return ExitCode::SUCCESS;
    }
    run(setup)
}

/// Says once at start whether the broker's chain is checked against a root CRL.
fn log_root_crl(root_crl: &RootCrl) {
    match root_crl {
        RootCrl::NotConfigured => tracing::info!(
            "broker.crl is not set: the broker's certificate chain is not checked for revocation"
        ),
        RootCrl::Missing(path) => tracing::info!(
            path = %path.display(),
            "no root CRL yet: the broker's certificate chain is not checked for revocation"
        ),
        RootCrl::Loaded {
            path,
            number,
            next_update,
            expired,
            ..
        } => {
            let next_update = next_update.map_or_else(|| "none".to_owned(), utc);
            tracing::info!(
                path = %path.display(),
                number = number.as_deref().unwrap_or("none"),
                %next_update,
                "root CRL loaded: the broker's issuing CA is checked against it"
            );
            if *expired {
                tracing::warn!(
                    path = %path.display(),
                    %next_update,
                    "the root CRL's nextUpdate has passed; it is still used and the agent still connects, until the root CA signs a new one"
                );
            }
        }
    }
}

/// Logs to stderr, which systemd hands to the journal; the journal adds its own timestamps.
fn init_logging() {
    let level = match std::env::var("TRUSTIOT_AGENT_LOG").as_deref() {
        Ok("error") => Level::ERROR,
        Ok("warn") => Level::WARN,
        Ok("debug") => Level::DEBUG,
        Ok("trace") => Level::TRACE,
        _ => Level::INFO,
    };
    let builder = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(level);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

fn run(setup: Setup) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("agent")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    let notifier = Arc::new(Notifier::from_env().unwrap_or_else(|error| {
        tracing::warn!(%error, "NOTIFY_SOCKET is unusable; not notifying systemd");
        Notifier::disabled()
    }));
    let shutdown = CancellationToken::new();
    let heartbeat_settings = HeartbeatSettings {
        interval: setup.heartbeat_interval,
        agent_version: setup.link.agent_version.clone(),
        certificate_not_after: setup.certificate_not_after,
    };
    let gateway = setup.gateway.clone();
    // The link subscribes only to the down channels something handles, and nothing handles one
    // yet, so it subscribes to none and nobody takes `incoming`. Each later issue adds its
    // channel here together with the handler that takes its messages from `incoming`: config
    // (Laysi/trustiot-v2#94), telemetry-ack (Laysi/trustiot-v2#90), log-request
    // (Laysi/trustiot-v2#185) and live-control (Laysi/trustiot-v2#121).
    let mut link_settings = setup.link;
    link_settings.subscriptions = Subscriptions::none();
    let (link, incoming, supervisor) = link::link(link_settings, setup.connector, shutdown.clone());
    drop(incoming);
    let progress = supervisor.progress();
    let ticks = Arc::new(AtomicU64::new(0));
    let connection = {
        let ticks = Arc::clone(&ticks);
        std::thread::Builder::new()
            .name("connection".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "cannot start the connection runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let ticker = tokio::spawn(async move {
                        let mut interval = tokio::time::interval(PROGRESS_TICK);
                        loop {
                            interval.tick().await;
                            ticks.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    supervisor.run().await;
                    ticker.abort();
                });
            })
    };
    let connection = match connection {
        Ok(connection) => connection,
        Err(error) => {
            tracing::error!(%error, "cannot start the connection thread");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(serve(
        link,
        connection,
        heartbeat_settings,
        notifier,
        ticks,
        progress,
        shutdown,
        gateway.to_string(),
    ));
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}

#[allow(
    clippy::too_many_arguments,
    reason = "the pieces main wires together; a struct would only rename them"
)]
async fn serve(
    link: Link,
    connection: std::thread::JoinHandle<()>,
    heartbeat_settings: HeartbeatSettings,
    notifier: Arc<Notifier>,
    ticks: Arc<AtomicU64>,
    progress: Arc<Progress>,
    shutdown: CancellationToken,
    gateway: String,
) -> ExitCode {
    let (mut terminate, mut interrupt) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(terminate), Ok(interrupt)) => (terminate, interrupt),
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!(%error, "cannot handle signals");
            shutdown.cancel();
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = notifier.notify(&[
        Notification::Ready,
        Notification::Status(&format!("gateway {gateway}: connecting")),
    ]) {
        tracing::warn!(%error, "could not notify systemd");
    }
    tracing::info!(%gateway, version = env!("CARGO_PKG_VERSION"), "trustiot-agent started");
    tokio::spawn(heartbeat::run(
        link.clone(),
        heartbeat_settings,
        tokio::time::Instant::now(),
        shutdown.clone(),
    ));
    tokio::spawn(report_state(link, Arc::clone(&notifier), gateway.clone()));
    if let Some(interval) = sdnotify::watchdog_interval_from_env() {
        tokio::spawn(liveness::watchdog(
            Arc::clone(&notifier),
            interval,
            ticks,
            progress,
            gateway,
        ));
    }

    let mut connection = tokio::task::spawn_blocking(move || connection.join());
    let code = tokio::select! {
        _ = terminate.recv() => None,
        _ = interrupt.recv() => None,
        _ = &mut connection => {
            tracing::error!("the connection thread ended unexpectedly");
            Some(ExitCode::FAILURE)
        }
    };
    if let Some(code) = code {
        shutdown.cancel();
        return code;
    }
    tracing::info!("stopping");
    let _ = notifier.notify(&[Notification::Stopping]);
    shutdown.cancel();
    match tokio::time::timeout(STOP_LIMIT, connection).await {
        Ok(_) => tracing::info!("stopped"),
        Err(_) => {
            tracing::warn!("the connection did not close within {STOP_LIMIT:?}; exiting anyway")
        }
    }
    ExitCode::SUCCESS
}

/// Shows the link state in `systemctl status`.
async fn report_state(link: Link, notifier: Arc<Notifier>, gateway: String) {
    let mut state = link.state();
    while state.changed().await.is_ok() {
        let status = match &*state.borrow_and_update() {
            LinkState::Connecting => format!("gateway {gateway}: connecting"),
            LinkState::Connected { session } => {
                format!("gateway {gateway}: connected, session {session}")
            }
            LinkState::Disconnected { error, retry_in } => {
                format!(
                    "gateway {gateway}: disconnected ({error}); retrying in {} s",
                    retry_in.as_secs()
                )
            }
            LinkState::Stopped => format!("gateway {gateway}: stopped"),
            _ => continue,
        };
        let _ = notifier.notify(&[Notification::Status(&status)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn arguments() {
        let Ok(Command::Run(args)) = parse(&[]) else {
            panic!("no arguments run the agent")
        };
        assert_eq!(args.config, PathBuf::from("/etc/trustiot/agent.toml"));
        assert!(!args.check);
        let Ok(Command::Run(args)) = parse(&["--config", "/tmp/a.toml", "--check"]) else {
            panic!("--config and --check")
        };
        assert_eq!(args.config, PathBuf::from("/tmp/a.toml"));
        assert!(args.check);
        assert!(matches!(parse(&["--help"]), Ok(Command::Help)));
        assert!(matches!(parse(&["--version"]), Ok(Command::Version)));
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--verbose"]).is_err());
    }
}
