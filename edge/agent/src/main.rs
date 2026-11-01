//! `trustiot-agent`, the gateway agent service (decision 10).
//!
//! ```text
//! trustiot-agent [--config <file>] [--check]
//! trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]
//! trustiot-agent provision install --hardware-id <id>
//! ```
//!
//! Start-up loads and checks the configuration, both TLS layers, the client certificate and
//! its key (the TPM in production), then tells systemd it is ready (`READY=1`), connected or
//! not: a gateway without network still runs and keeps trying. `--check` stops after the
//! checks, for operators and update health checks. `provision` and `provision install` are the
//! production workstation's subcommands ([`agent::provision`],
//! [`agent::provision::install`]): they do not start the service.
//!
//! Threads: the connection (TLS handshakes, MQTT) runs on its own thread with a single-threaded
//! tokio runtime, because a full inner handshake waits about 70 ms for the TPM signature inside
//! rustls' synchronous signing call; that blocks only this thread. Everything else (heartbeat,
//! certificate renewal ([`agent::renewal`], whose TPM signature runs on tokio's blocking
//! threads), the systemd watchdog's notifications, signals, the dispatch of `down` messages,
//! the Modbus polling of [`agent::polling`] with its own per-device watchdog, one task per
//! device of the configuration [`agent::desired`] applies, and the telemetry pipeline of
//! [`agent::upload`] that takes the polls' readings, with its offline buffer, whose file I/O
//! runs on tokio's blocking threads, and the live values of [`agent::live`], which publishes
//! the samples of leased devices on `up/live`) runs on a two-thread runtime.
//!
//! The systemd watchdog is fed only while the connection task makes progress and both runtimes
//! run: the connection task declares every wait with its deadline ([`agent::liveness`]), the
//! connection thread counts ticks, and the main runtime sends `WATCHDOG=1` only while the
//! declared wait is within its deadline and the count has moved since the last time.
//! SIGTERM (or SIGINT) publishes `offline`/`shutdown` and disconnects cleanly while the polling
//! stops and then the pipeline writes the batches in memory, the last polls' readings among
//! them, to disk, and exits within [`STOP_LIMIT`]. When the connection thread, the
//! configuration side or the pipeline ends unexpectedly, the others stop the same way (the
//! pipeline's batches in memory still go to disk), and the exit code is 1.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent::clock::GatewayClock;
use agent::config::{self, Config};
use agent::desired::fleet::Fleet;
use agent::desired::store::{self, Store};
use agent::desired::{self, Applier, Deliveries};
use agent::dispatch::{self, Handlers};
use agent::heartbeat::{self, HeartbeatSettings};
use agent::identity::utc;
use agent::link::{self, Incoming, Link, LinkState};
use agent::live;
use agent::liveness::{self, Progress};
use agent::modbus::endpoint::Endpoints;
use agent::polling;
use agent::sdnotify::{self, Notification, Notifier, PROGRESS_TICK};
use agent::setup::Setup;
use agent::tls::{RootCrl, Validity};
use agent::tpm::TpmTransport;
use agent::upload;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::Level;

/// How long a stop waits for the connection to close cleanly and for the pipeline to write the
/// batches in memory to disk; below the unit's `TimeoutStopSec`.
const STOP_LIMIT: Duration = Duration::from_secs(10);

/// How long a stop waits for the polling to stop (every device's read in flight to end) before
/// the pipeline stops anyway; the readings of a poll that ends later are lost. Polling stops
/// first so that the readings of the polls that end during the stop still go into the
/// pipeline's last window.
const POLLING_STOP_LIMIT: Duration = Duration::from_secs(2);

// The pipeline writes for at most FLUSH_LIMIT from its stop, which comes at most
// POLLING_STOP_LIMIT after the agent's, and then syncs once: all of it within STOP_LIMIT.
const _: () = assert!(
    POLLING_STOP_LIMIT.as_secs() + upload::FLUSH_LIMIT.as_secs() < STOP_LIMIT.as_secs(),
    "the polling's stop and the flush must fit in the stop limit with time for the last sync"
);

const USAGE: &str = "usage: trustiot-agent [--config <file>] [--check]
       trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]
       trustiot-agent provision install --hardware-id <id>

  --config <file>  the configuration file (default /etc/trustiot/agent.toml; provision install
                   always reads that one)
  --check          check the configuration, certificate and key, then exit
  --version        print the version
  --help           print this help

The log level is TRUSTIOT_AGENT_LOG (error, warn, info, debug, trace; default info).
`trustiot-agent provision --help` and `trustiot-agent provision install --help` explain the
production workstation's subcommands.";

struct Args {
    config: PathBuf,
    check: bool,
}

enum Command {
    Run(Args),
    Help,
    Version,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
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
    let mut args = std::env::args_os().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|first| first == agent::provision::SUBCOMMAND)
    {
        let _subcommand = args.next();
        return provision(args);
    }
    let args = match parse_args(args) {
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
        })
        .and_then(|setup| {
            let directory = std::env::var_os(store::STATE_DIRECTORY_VARIABLE);
            store::state_directory(directory.as_deref()).map(|directory| (setup, directory))
        }) {
        Ok(setup) => setup,
        Err(error) => {
            tracing::error!("refusing to start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (setup, state_directory) = setup;
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
    run(setup, state_directory)
}

/// `trustiot-agent provision`: the report on stdout, everything else (usage errors, logs, why it
/// failed) on stderr, and the exit status of [`agent::provision::Exit`].
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line program answers --help and usage errors on the terminal"
)]
fn provision(args: impl Iterator<Item = OsString>) -> ExitCode {
    use agent::provision::{self, Command, Exit};
    let mut args = args.peekable();
    if args
        .peek()
        .is_some_and(|first| first == provision::install::SUBCOMMAND)
    {
        let _subcommand = args.next();
        return install(args);
    }
    let options = match provision::parse_args(args) {
        Ok(Command::Provision(options)) => options,
        Ok(Command::Help) => {
            println!("{}", provision::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("trustiot-agent provision: {error}\n\n{}", provision::USAGE);
            return ExitCode::from(Exit::Usage.code());
        }
    };
    init_logging();
    let device = options.device.clone();
    let exit = provision::run(
        &options,
        &mut || TpmTransport::Device(device.clone()),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(exit.code())
}

/// `trustiot-agent provision install`: the certificates from stdin, one line per file on stdout,
/// everything else on stderr, and the exit status of [`agent::provision::install::Exit`].
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line program answers --help and usage errors on the terminal"
)]
fn install(args: impl Iterator<Item = OsString>) -> ExitCode {
    use agent::provision::install::{self, Command, Exit, Owners};
    let options = match install::parse_args(args) {
        Ok(Command::Install(options)) => options,
        Ok(Command::Help) => {
            println!("{}", install::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!(
                "trustiot-agent provision install: {error}\n\n{}",
                install::USAGE
            );
            return ExitCode::from(Exit::Usage.code());
        }
    };
    init_logging();
    let owners = match Owners::system() {
        Ok(owners) => owners,
        Err(error) => {
            eprintln!("trustiot-agent provision install: {error}");
            return ExitCode::from(Exit::Configuration.code());
        }
    };
    let exit = install::run(
        &options,
        &owners,
        &mut |device| TpmTransport::Device(device.to_owned()),
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(exit.code())
}

/// Says once at start whether the broker's chain is checked against a root CRL.
fn log_root_crl(root_crl: &RootCrl) {
    match root_crl {
        RootCrl::NotConfigured => tracing::info!(
            "broker.crl is not set: the broker's certificate chain is not checked for revocation"
        ),
        RootCrl::Missing(path) => tracing::info!(
            path = %path.display(),
            "no root CRL yet: the broker's certificate chain is not checked for revocation until one is fetched"
        ),
        RootCrl::Withheld { path, number, .. } => tracing::warn!(
            path = %path.display(),
            highest = number.as_deref().unwrap_or("unknown"),
            "the root CRL file is gone or older than the highest one this gateway accepted: no connection until a root CRL at least that new is fetched"
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

fn run(setup: Setup, state_directory: PathBuf) -> ExitCode {
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
    // Renewal updates the certificate's expiry the heartbeat reports, the root CRL updater the
    // root CRL.
    let certificate_not_after = setup.renewal.certificate_not_after();
    let root_crl_in_use = setup.inner.root_crl_in_use();
    let renewal = setup.renewal;
    // One clock stamps the readings, compares the cloud's time in its acknowledgements and
    // reports in the heartbeat, so a batch's `clock_synced` is what the uploader found.
    let clock = GatewayClock::system();
    let gateway = setup.gateway.clone();
    // The link subscribes only to the down channels the dispatcher has a handler for
    // (dispatch.rs, where each later issue adds its channel with its handler).
    let mut link_settings = setup.link;
    link_settings.subscriptions = dispatch::subscriptions();
    let agent_version = link_settings.agent_version.clone();
    let (link, incoming, supervisor) = link::link(link_settings, setup.connector, shutdown.clone());
    // Certificate renewal (Laysi/trustiot-v2#83): on schedule, and when the broker refuses the
    // certificate. Its TPM signature runs on the blocking pool.
    runtime.spawn(renewal.run(
        link.state(),
        link.attempts(),
        link.certificate_refused(),
        shutdown.clone(),
    ));
    // The root CRL (Laysi/trustiot-v2#563): fetched from the gateway host at start and every few
    // hours; a newer one starts the connection over with a full handshake. Its file writes run
    // on the blocking pool.
    if let Some(updater) = setup.root_crl_updater {
        runtime.spawn(updater.run(shutdown.clone()));
    }
    let pipeline_stop = CancellationToken::new();
    let mut upload_settings = upload::Settings::new(state_directory.join(upload::BUFFER_DIRECTORY));
    upload_settings.memory_limit = setup.buffer.memory_limit;
    upload_settings.disk_limit = setup.buffer.disk_limit;
    upload_settings.upload_interval = setup.buffer.upload_interval;
    let (handles, pipeline) = match upload::pipeline(
        upload_settings,
        link.clone(),
        clock.clone(),
        pipeline_stop.clone(),
    ) {
        Ok(created) => created,
        Err(error) => {
            tracing::error!(%error, "cannot start the telemetry pipeline");
            return ExitCode::FAILURE;
        }
    };
    let (config_inbox, deliveries) = desired::inbox();
    let poll_settings = polling::Settings::default();
    // The live leases (down/live-control) make the fleet poll leased devices faster and send
    // their samples to up/live at once; every regular poll's readings go on into the pipeline's
    // window (`Readings`).
    let (leases, watcher) = live::leases();
    let (sink, live_publisher) = live::publisher(
        link.clone(),
        watcher.clone(),
        Arc::new(handles.readings.clone()),
    );
    let fleet = Fleet::new(
        Endpoints::new(poll_settings.connect_timeout),
        poll_settings,
        Arc::new(clock.clone()),
        Arc::new(sink),
    )
    .with_live(watcher);
    let services = Services {
        incoming,
        handlers: Handlers {
            config: config_inbox,
            telemetry_ack: handles.acks.clone(),
            live: leases,
        },
        applier: Applier::new(fleet, Store::new(state_directory)),
        deliveries,
        pipeline,
        pipeline_stop,
        live_publisher,
    };
    let heartbeat_settings = HeartbeatSettings {
        interval: setup.heartbeat_interval,
        agent_version,
        certificate_not_after,
        buffer: handles.stats,
        clock,
        root_crl: root_crl_in_use,
    };
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
        services,
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

/// What `serve` runs besides the heartbeat and the watchdog: the link's incoming messages and
/// their handlers, the configuration side (the applier, which drives the polling, and its
/// deliveries), the telemetry pipeline (collector, offline buffer, uploader) with the token
/// that stops it, and the publisher of the live values.
struct Services {
    incoming: Incoming,
    handlers: Handlers,
    applier: Applier,
    deliveries: Deliveries,
    pipeline: upload::Pipeline,
    pipeline_stop: CancellationToken,
    live_publisher: live::Publisher,
}

/// What ended the agent's run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// SIGTERM or SIGINT.
    Signal,
    /// The connection thread, unexpectedly.
    Connection,
    /// The configuration side (`desired::run`), unexpectedly.
    Configuration,
    /// The telemetry pipeline, unexpectedly.
    Pipeline,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the pieces main wires together; a struct would only rename them"
)]
async fn serve(
    link: Link,
    connection: std::thread::JoinHandle<()>,
    services: Services,
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
            services.pipeline_stop.cancel();
            return ExitCode::FAILURE;
        }
    };
    let Services {
        incoming,
        handlers,
        applier,
        deliveries,
        pipeline,
        pipeline_stop,
        live_publisher,
    } = services;
    // Every message the cloud sends is taken at once and handed to its handler.
    tokio::spawn(dispatch::run(incoming, handlers));
    // Publishes the live values the polls of leased devices hand over; it ends with the fleet's
    // sink, and while the link stops it drops what it gets.
    tokio::spawn(live_publisher.run());
    // The pipeline opens the disk buffer in its own task, the file I/O on a blocking thread, so
    // a slow or failing data partition never delays READY=1 or the polling.
    let mut pipeline = tokio::spawn(pipeline.run());
    // Polls the saved configuration at once, before and without a connection, then the
    // cloud's; stops polling once `shutdown` is cancelled.
    let mut configuration = tokio::spawn(desired::run(
        applier,
        link.clone(),
        deliveries,
        shutdown.clone(),
    ));
    // A failed READY=1 is only logged: the agent keeps running, and however it stops later
    // (systemd's start timeout sends SIGTERM), it stops through the path below, which ends the
    // polling and flushes the pipeline.
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
    // Until the link stops, none of the three ends by itself.
    let ended = tokio::select! {
        _ = terminate.recv() => Ended::Signal,
        _ = interrupt.recv() => Ended::Signal,
        _ = &mut connection => {
            tracing::error!("the connection thread ended unexpectedly");
            Ended::Connection
        }
        _ = &mut configuration => {
            tracing::error!("the configuration side ended unexpectedly; no device is polled");
            Ended::Configuration
        }
        _ = &mut pipeline => {
            tracing::error!("the telemetry pipeline ended unexpectedly");
            Ended::Pipeline
        }
    };
    tracing::info!("stopping");
    let _ = notifier.notify(&[Notification::Stopping]);
    // Whatever ended the wait, everything that still runs stops within STOP_LIMIT: the link
    // sends offline/shutdown and disconnects, and the configuration side stops polling.
    shutdown.cancel();
    let stopped = tokio::time::timeout(STOP_LIMIT, async {
        let connection = async {
            if ended != Ended::Connection {
                let _ = connection.await;
            }
        };
        let telemetry = async {
            // The polling stops first, so that the readings of the polls that end during the
            // stop still go into the pipeline's last window; bounded, so that the pipeline
            // keeps its time.
            if ended != Ended::Configuration
                && tokio::time::timeout(POLLING_STOP_LIMIT, &mut configuration)
                    .await
                    .is_err()
            {
                tracing::warn!(
                    "the polling did not stop within {POLLING_STOP_LIMIT:?}; the readings of the polls that end later are lost"
                );
            }
            // Then the pipeline closes its window and writes the batches in memory to disk (for
            // at most upload::FLUSH_LIMIT from its stop, then one last sync). Returning without
            // waiting for it would leave those batches to the runtime's teardown, which drops
            // them.
            pipeline_stop.cancel();
            if ended != Ended::Pipeline && pipeline.await.is_err() {
                tracing::error!("the telemetry pipeline failed while stopping");
            }
        };
        tokio::join!(connection, telemetry);
    })
    .await;
    match stopped {
        Ok(()) => tracing::info!("stopped"),
        Err(_) => tracing::warn!(
            "the connection or the telemetry pipeline did not stop within {STOP_LIMIT:?}; exiting anyway"
        ),
    }
    if ended == Ended::Signal {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
        parse_args(args.iter().map(OsString::from))
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

    /// A link that never connects.
    struct Nowhere;

    impl link::Connector for Nowhere {
        type Stream = tokio::io::DuplexStream;

        async fn connect(&self) -> Result<Self::Stream, link::ConnectError> {
            std::future::pending().await
        }
    }

    /// The agent as `run` wires it, with the state directory `state`, a link that never
    /// connects, and devices reached through `endpoints`.
    struct Wired {
        link: Link,
        services: Services,
        heartbeat_settings: HeartbeatSettings,
        progress: Arc<Progress>,
        shutdown: CancellationToken,
        stats: upload::Stats,
        readings: upload::Readings,
    }

    fn wire(state: &std::path::Path, endpoints: Endpoints) -> Wired {
        let version = protocol::AgentVersion::new("0.1.0-test").unwrap();
        let shutdown = CancellationToken::new();
        let mut link_settings =
            link::LinkSettings::new(protocol::GatewayId::new("gw-1").unwrap(), version.clone());
        link_settings.subscriptions = dispatch::subscriptions();
        let (link, incoming, supervisor) = link::link(link_settings, Nowhere, shutdown.clone());
        let progress = supervisor.progress();
        drop(supervisor);
        let clock = GatewayClock::new(
            agent::clock::SystemWall,
            PathBuf::from("/nonexistent/trustiot-agent-test/synchronized"),
        );
        let pipeline_stop = CancellationToken::new();
        let (handles, pipeline) = upload::pipeline(
            upload::Settings::new(state.join(upload::BUFFER_DIRECTORY)),
            link.clone(),
            clock.clone(),
            pipeline_stop.clone(),
        )
        .unwrap();
        let (config_inbox, deliveries) = desired::inbox();
        let (leases, watcher) = live::leases();
        let (sink, live_publisher) = live::publisher(
            link.clone(),
            watcher.clone(),
            Arc::new(handles.readings.clone()),
        );
        let fleet = Fleet::new(
            endpoints,
            polling::Settings::default(),
            Arc::new(clock.clone()),
            Arc::new(sink),
        )
        .with_live(watcher);
        Wired {
            services: Services {
                incoming,
                handlers: Handlers {
                    config: config_inbox,
                    telemetry_ack: handles.acks.clone(),
                    live: leases,
                },
                applier: Applier::new(fleet, Store::new(state)),
                deliveries,
                pipeline,
                pipeline_stop,
                live_publisher,
            },
            heartbeat_settings: HeartbeatSettings {
                interval: Duration::from_secs(60),
                agent_version: version,
                certificate_not_after: tokio::sync::watch::channel(protocol::UnixMillis::new(0)).1,
                buffer: handles.stats.clone(),
                clock,
                root_crl: tokio::sync::watch::channel(None).1,
            },
            link,
            progress,
            shutdown,
            stats: handles.stats,
            readings: handles.readings,
        }
    }

    impl Wired {
        /// `serve` with a connection thread that ends at once: the same stop as SIGTERM's,
        /// with exit code 1.
        async fn serve_until_the_connection_ends(self) -> ExitCode {
            serve(
                self.link,
                std::thread::spawn(|| {}),
                self.services,
                self.heartbeat_settings,
                Arc::new(Notifier::disabled()),
                Arc::new(AtomicU64::new(0)),
                self.progress,
                self.shutdown,
                "gw-1".to_owned(),
            )
            .await
        }
    }

    /// The sizes of the buffer's segment files.
    fn segments(state: &std::path::Path) -> Vec<u64> {
        std::fs::read_dir(state.join(upload::BUFFER_DIRECTORY))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap())
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
                    .map(|entry| entry.metadata().unwrap().len())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Codex review before the commit (seventh round): when the connection thread ends
    /// unexpectedly, the pipeline still writes the readings in memory to disk before `serve`
    /// returns, and the exit code says it failed. It used to return at once, and the runtime's
    /// teardown dropped them.
    #[tokio::test]
    async fn an_unexpected_end_of_the_connection_still_writes_the_batches_in_memory_to_disk() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-main");
        let wired = wire(scratch.path(), Endpoints::new(Duration::from_secs(1)));
        wired.readings.push([protocol::telemetry::Reading {
            device: protocol::DeviceId::new("device-0").unwrap(),
            point: protocol::PointKey::new("point-0").unwrap(),
            sample: protocol::telemetry::Sample {
                at: wired.heartbeat_settings.clock.wall(),
                value: protocol::Value::Int(1),
            },
            clock_synced: false,
        }]);
        let code = wired.serve_until_the_connection_ends().await;
        assert_eq!(code, ExitCode::FAILURE);
        let segments = segments(scratch.path());
        assert!(
            segments.iter().any(|len| *len > 16),
            "no batch on disk when serve returned: segment sizes {segments:?}"
        );
    }

    /// A stop lets the polls in flight end before the pipeline closes its last window, so
    /// their readings go to disk with the rest: here the only poll's answer is held at the
    /// device until the stop has begun, and the pipeline is not asked to stop before it is in.
    /// If the pipeline stopped at the same time as the polling, its window could close before
    /// the reading arrived, and the reading would be lost without a count. The clock stands still (`testkit::clock::manual`), so neither the
    /// request's timeout nor [`POLLING_STOP_LIMIT`] can end the wait for the answer however
    /// slow the machine is.
    #[tokio::test(start_paused = true)]
    async fn a_stop_lets_the_polls_in_flight_end_and_writes_their_readings_to_disk() {
        use protocol::config::{
            ByteOrder, ConfigReport, DataPoint, DataType, DesiredConfig, Device, DeviceModel,
            FunctionCode, Host, Transport,
        };
        use testkit::modbus::{self as fake, FakeModbus, Fault};

        let _manual = testkit::clock::manual();
        let scratch = testkit::mqtt::ScratchDir::new("agent-main-stop");
        let device = FakeModbus::start(fake::Transport::ModbusTcp).await;
        device.add_unit(1);
        device.set_holding_registers(1, 0, &[2301]);
        device.inject(Fault::hold().once());
        let mut wired = wire(
            scratch.path(),
            Endpoints::new(Duration::from_secs(10))
                .exempt_for_tests([std::net::Ipv4Addr::LOCALHOST.into()]),
        );
        let model = DeviceModel {
            name: protocol::ModelName::new("meter").unwrap(),
            points: vec![DataPoint {
                key: protocol::PointKey::new("power").unwrap(),
                function_code: FunctionCode::ReadHoldingRegisters,
                address: 0,
                data_type: DataType::U16,
                byte_order: ByteOrder::Abcd,
                length: None,
                scale: None,
                offset: None,
            }],
        };
        let config = DesiredConfig {
            v: protocol::V1,
            rev: 1,
            version: protocol::ConfigVersion::new(1).unwrap(),
            devices: vec![Device {
                id: protocol::DeviceId::new("meter-1").unwrap(),
                model: model.name.clone(),
                transport: Transport::ModbusTcp,
                host: Host::new("127.0.0.1").unwrap(),
                port: device.address().port(),
                unit_id: 1,
                poll_interval_ms: 60_000,
            }],
            models: vec![model],
        }
        .encode()
        .unwrap();
        // Applied before `serve` (which finds it saved and keeps it), so that its first poll is
        // under way when the stop comes.
        assert_eq!(
            wired.services.applier.receive(config).await,
            Some(ConfigReport::applied(
                protocol::ConfigVersion::new(1).unwrap()
            ))
        );
        // The first poll's request waits at the device for its answer.
        device.wait_for_requests(1).await;
        // Once the stop has begun, the device answers; until then the poll is in flight, and the
        // pipeline must not have been asked to stop.
        let stopping = wired.shutdown.clone();
        let pipeline_stop = wired.services.pipeline_stop.clone();
        let release = tokio::spawn(async move {
            stopping.cancelled().await;
            let pipeline_stopped_first = pipeline_stop.is_cancelled();
            device.release(0);
            (device, pipeline_stopped_first)
        });
        let stats = wired.stats.clone();
        let code = wired.serve_until_the_connection_ends().await;
        assert_eq!(code, ExitCode::FAILURE);
        let (_device, pipeline_stopped_first) = release.await.unwrap();
        assert!(
            !pipeline_stopped_first,
            "the pipeline was asked to stop while a poll was still in flight"
        );
        let stats = stats.snapshot();
        assert_eq!(
            (stats.pending_batches, stats.pending_records),
            (1, 1),
            "the reading is in a batch, not left in a window nobody closes: {stats:?}"
        );
        let segments = segments(scratch.path());
        assert!(
            segments.iter().any(|len| *len > 16),
            "no batch on disk when serve returned: segment sizes {segments:?}"
        );
    }
}
