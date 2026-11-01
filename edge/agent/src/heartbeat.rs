//! The heartbeat: a valid [`Heartbeat`] on `up/heartbeat` right after every connection and then
//! every interval, with what the agent knows by itself (rebuild checklist H1,
//! Laysi/trustiot-v2#117):
//!
//! - the agent: its session, release, uptime, starts in this boot, and the configuration
//!   version in use ([`HealthSources::config_version`]);
//! - the gateway ([`crate::host`]): CPU use, memory, the data partition's size and free space,
//!   the SoC temperature, the power and throttling flags, the OS uptime and boot time and the
//!   boot count;
//! - the client certificate's expiry, and the root CRL in use (`root_crl`: its number,
//!   nextUpdate and whether that has passed);
//! - the offline buffer (`buffer`, from [`Stats`]) and whether the clock is synchronized
//!   (`clock_synced`, from [`GatewayClock::synced`], the same state the batches are marked with);
//! - the read statistics of every configured device (`devices`, from the [`DeviceBook`] that
//!   [`crate::health::HealthSink`] fills from the polling loops).
//!
//! A source that cannot be read leaves its field empty or zero; it never keeps the heartbeat
//! from being sent, because the heartbeat is also the liveness signal. That includes a read that
//! never returns: the heartbeat waits a few seconds for the files (`host_read_limit`), then goes
//! out with an empty host sample while at most one read stays stuck on a blocking thread.

use std::time::Duration;

use protocol::health::{Heartbeat, MAX_CRL_NUMBER_DIGITS, RootCrlStatus, SystemStats};
use protocol::topic::Channel;
use protocol::{AgentVersion, ConfigVersion, SessionId, UnixMillis, V1};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use crate::clock::GatewayClock;
use crate::health::DeviceBook;
use crate::host::{BootCounts, CpuMeter, HostPaths, HostSample, ThrottleTracker};
use crate::link::{Link, LinkState, PublishError};
use crate::tls::RootCrlInUse;
use crate::upload::Stats;

/// Where the heartbeat takes what only other parts of the agent and the host know.
#[derive(Clone, Debug)]
pub struct HealthSources {
    /// The files the host's metrics are read from.
    pub host: HostPaths,
    /// How often the OS booted and the agent started, counted when the agent started
    /// ([`crate::host::register_start`]).
    pub boot: BootCounts,
    /// The read statistics of the devices ([`crate::health::HealthSink`] fills it).
    pub devices: DeviceBook,
    /// The configuration version in use ([`crate::desired::Applier::version_watch`]); `None`
    /// before the first is applied.
    pub config_version: watch::Receiver<Option<ConfigVersion>>,
}

impl HealthSources {
    /// Sources that have nothing: no host files, no boots, no devices and no configuration;
    /// for tests that look at something else in the heartbeat.
    pub fn none() -> Self {
        Self {
            host: HostPaths::nowhere(),
            boot: BootCounts::default(),
            devices: DeviceBook::new(),
            config_version: watch::channel(None).1,
        }
    }
}

/// What every heartbeat carries besides the session.
#[derive(Clone, Debug)]
pub struct HeartbeatSettings {
    /// Time between heartbeats; also sent as `interval_s`.
    pub interval: Duration,
    /// This agent's release.
    pub agent_version: AgentVersion,
    /// When the client certificate expires; renewal ([`crate::renewal`]) updates it, and every
    /// heartbeat reports the value it has then.
    pub certificate_not_after: watch::Receiver<UnixMillis>,
    /// The offline buffer's numbers.
    pub buffer: Stats,
    /// The clock: `sent_at` and `clock_synced`.
    pub clock: GatewayClock,
    /// The root CRL in use ([`crate::tls::InnerConfig::root_crl_in_use`]), which the updater
    /// replaces ([`crate::revocation`]): its number and whether its nextUpdate has passed by
    /// [`HeartbeatSettings::clock`] go in `root_crl`; `None` leaves it out.
    pub root_crl: watch::Receiver<Option<RootCrlInUse>>,
    /// The host, the boots, the devices and the configuration.
    pub health: HealthSources,
}

/// What the heartbeats of one run remember from one to the next.
#[derive(Debug, Default)]
struct Carried {
    cpu: CpuMeter,
    throttle: ThrottleTracker,
    host: HostReader,
}

/// The shortest wait for the host's files.
const HOST_READ_LIMIT_MIN: Duration = Duration::from_secs(1);
/// The longest wait for the host's files.
const HOST_READ_LIMIT_MAX: Duration = Duration::from_secs(5);

/// How long a heartbeat waits for the host's files before it goes out without them: a quarter of
/// the interval, between [`HOST_READ_LIMIT_MIN`] and [`HOST_READ_LIMIT_MAX`]. The files are
/// local and answer in microseconds; a wait that long means a read is stuck (the firmware's
/// mailbox behind `get_throttled`, a failing data partition), and the heartbeat is also the
/// liveness signal, so it must still arrive close to its schedule, a few seconds late at most.
fn host_read_limit(interval: Duration) -> Duration {
    (interval / 4).clamp(HOST_READ_LIMIT_MIN, HOST_READ_LIMIT_MAX)
}

/// Reads the host's files on the blocking pool, at most one read at a time.
///
/// A read that does not finish within the limit is left running (a blocking thread cannot be
/// cancelled) and is the only one: the next heartbeat waits for the same read again instead of
/// starting another, so a stuck file holds one thread, never one per heartbeat. A heartbeat whose
/// read is late goes out with an empty [`HostSample`], which reports zeros and leaves the
/// temperature and the throttling flags out; the read's result is used by the first heartbeat
/// that finds it finished, even though it is older than the heartbeat.
#[derive(Debug, Default)]
struct HostReader {
    reading: Option<JoinHandle<HostSample>>,
    /// Whether the read in progress was already reported as late.
    late: bool,
    /// How many reads were started, for the tests.
    started: u64,
}

impl HostReader {
    async fn sample(&mut self, paths: &HostPaths, limit: Duration) -> HostSample {
        let reading = self.reading.get_or_insert_with(|| {
            self.started += 1;
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || HostSample::read(&paths))
        });
        match timeout(limit, &mut *reading).await {
            Ok(Ok(sample)) => {
                self.reading = None;
                self.late = false;
                sample
            }
            Ok(Err(error)) => {
                self.reading = None;
                self.late = false;
                tracing::warn!(%error, "reading the host's metrics failed");
                HostSample::default()
            }
            Err(_) => {
                if !self.late {
                    self.late = true;
                    tracing::warn!(
                        ?limit,
                        "reading the host's metrics takes too long; sending heartbeats without them until it ends"
                    );
                }
                HostSample::default()
            }
        }
    }
}

/// Sends heartbeats while the link is connected, until `shutdown` is cancelled.
pub async fn run(
    link: Link,
    settings: HeartbeatSettings,
    started: Instant,
    shutdown: CancellationToken,
) {
    let mut state = link.state();
    // Across connections: the CPU is measured between two heartbeats whichever session sent
    // them, and a throttling flag seen stays seen.
    let mut carried = Carried::default();
    loop {
        let session = loop {
            if let LinkState::Connected { session } = &*state.borrow_and_update() {
                break session.clone();
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = state.changed() => if changed.is_err() { return },
            }
        };
        let mut next = Instant::now();
        let mut busy = 0_u64;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = sleep_until(next) => {
                    send(&link, &settings, &session, started, &mut busy, &mut carried).await;
                    next = Instant::now() + settings.interval;
                }
                changed = state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let same = matches!(&*state.borrow(), LinkState::Connected { session: now } if *now == session);
                    if !same {
                        break;
                    }
                }
            }
        }
    }
}

/// Sends one heartbeat. It never waits for room in the queue: a heartbeat that finds the queue
/// full is skipped (the next one comes an interval later). `busy` counts the skips in a row,
/// so only the first of a run is a warning.
async fn send(
    link: &Link,
    settings: &HeartbeatSettings,
    session: &SessionId,
    started: Instant,
    busy: &mut u64,
    carried: &mut Carried,
) {
    // A read that hangs must neither hold up the heartbeat nor start more reads (HostReader).
    let sample = carried
        .host
        .sample(&settings.health.host, host_read_limit(settings.interval))
        .await;
    let heartbeat = build(
        settings,
        session.clone(),
        started.elapsed(),
        &sample,
        carried,
    );
    let payload = match heartbeat.encode() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(%error, "a heartbeat does not encode");
            return;
        }
    };
    match link
        .publish_in_session(session, Channel::Heartbeat, payload)
        .await
    {
        Ok(_) => *busy = 0,
        Err(PublishError::Busy) => {
            *busy += 1;
            if *busy == 1 {
                tracing::warn!("the publish queue is full; skipping heartbeats until it has room");
            } else {
                tracing::debug!(
                    skipped = *busy,
                    "skipped a heartbeat: the publish queue is full"
                );
            }
        }
        Err(error) => tracing::debug!(%error, "a heartbeat was not sent"),
    }
}

/// The heartbeat for `session`, from what the host `sample` found (a source that could not be
/// read is empty, which reports zeros or leaves the field out) and what `carried` remembers.
fn build(
    settings: &HeartbeatSettings,
    session: SessionId,
    agent_uptime: Duration,
    sample: &HostSample,
    carried: &mut Carried,
) -> Heartbeat {
    let now = settings.clock.wall();
    let uptime = os_uptime(&sample.uptime);
    let booted_at = UnixMillis::new(
        now.get()
            .saturating_sub(u64::try_from(uptime.as_millis()).unwrap_or(u64::MAX)),
    );
    Heartbeat {
        v: V1,
        session,
        sent_at: now,
        interval_s: u32::try_from(settings.interval.as_secs())
            .unwrap_or(u32::MAX)
            .max(1),
        agent_version: settings.agent_version.clone(),
        config_version: *settings.health.config_version.borrow(),
        system: SystemStats {
            cpu_percent: carried.cpu.percent(&sample.stat),
            memory_total_bytes: meminfo_bytes(&sample.meminfo, "MemTotal:"),
            memory_available_bytes: meminfo_bytes(&sample.meminfo, "MemAvailable:"),
            data_total_bytes: sample.data.map_or(0, |data| data.total_bytes),
            data_available_bytes: sample.data.map_or(0, |data| data.available_bytes),
            soc_temp_celsius: sample
                .temperature
                .as_deref()
                .and_then(crate::host::temperature_celsius),
            throttling: carried.throttle.observe(sample.throttled.as_deref()),
            booted_at,
            uptime_s: uptime.as_secs(),
            boot_count: settings.health.boot.boot_count,
            agent_uptime_s: agent_uptime.as_secs(),
            agent_starts_since_boot: settings.health.boot.agent_starts_since_boot,
        },
        buffer: settings.buffer.snapshot(),
        clock_synced: settings.clock.synced(),
        cert_not_after: *settings.certificate_not_after.borrow(),
        root_crl: settings
            .root_crl
            .borrow()
            .as_ref()
            .map(|in_use| RootCrlStatus {
                // A number the protocol does not carry (more digits than a CRL number has) is
                // left out rather than making the heartbeat invalid.
                number: in_use.number.clone().filter(|number| {
                    number.len() <= MAX_CRL_NUMBER_DIGITS
                        && !number.is_empty()
                        && number.bytes().all(|byte| byte.is_ascii_digit())
                }),
                next_update: in_use.next_update,
                // An expired root CRL is still used, and the agent still connects
                // (crate::tls::RootCrl::Loaded::expired).
                expired: in_use.next_update.is_some_and(|next| next < now),
            }),
        devices: settings.health.devices.snapshot(),
    }
}

/// A `/proc/meminfo` line such as `MemTotal:  1919956 kB`, in bytes; 0 when missing.
fn meminfo_bytes(meminfo: &str, key: &str) -> u64 {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse::<u64>().ok())
        .map_or(0, |kib| kib.saturating_mul(1024))
}

/// The first number of `/proc/uptime` (seconds since boot, with a fraction).
fn os_uptime(uptime: &str) -> Duration {
    uptime
        .split_whitespace()
        .next()
        .and_then(|secs| secs.parse::<f64>().ok())
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use protocol::health::{FailureCounts, FailureKind, LastFailure, ThrottleFlags, Throttling};
    use protocol::telemetry::{Reading, Sample};
    use protocol::{DeviceId, PointKey, Value};

    use super::*;
    use crate::host::DiskSpace;
    use crate::modbus::pdu::Read;
    use crate::modbus::{PointFailure, Poll, RequestReport};

    fn settings() -> HeartbeatSettings {
        HeartbeatSettings {
            interval: Duration::from_secs(60),
            agent_version: AgentVersion::new("0.1.0").unwrap(),
            certificate_not_after: watch::channel(UnixMillis::new(1_900_000_000_000)).1,
            buffer: Stats::default(),
            clock: GatewayClock::new(
                crate::clock::SystemWall,
                std::path::PathBuf::from("/nonexistent/trustiot-agent-test/synchronized"),
            ),
            root_crl: watch::channel(None).1,
            health: HealthSources::none(),
        }
    }

    /// A heartbeat built from nothing the host could tell.
    fn bare(settings: &HeartbeatSettings, session: SessionId) -> Heartbeat {
        build(
            settings,
            session,
            Duration::ZERO,
            &HostSample::default(),
            &mut Carried::default(),
        )
    }

    fn sample(meminfo: &str, uptime: &str) -> HostSample {
        HostSample {
            meminfo: meminfo.to_owned(),
            uptime: uptime.to_owned(),
            ..HostSample::default()
        }
    }

    #[test]
    fn heartbeats_report_the_root_crl_in_use_and_whether_it_expired() {
        let (in_use, receiver) = watch::channel(None);
        let settings = HeartbeatSettings {
            root_crl: receiver,
            ..settings()
        };
        let session = SessionId::new("s-1").unwrap();
        // No root CRL: the field is left out.
        let none = bare(&settings, session.clone());
        assert_eq!(none.root_crl, None);
        let now = settings.clock.wall().get();
        for (next_update, expired) in [
            (Some(now + 86_400_000), false),
            (Some(now - 1_000), true),
            (None, false),
        ] {
            in_use.send_replace(Some(RootCrlInUse {
                number: Some("12".to_owned()),
                next_update: next_update.map(UnixMillis::new),
            }));
            let heartbeat = bare(&settings, session.clone());
            let decoded = Heartbeat::decode(&heartbeat.encode().unwrap()).unwrap();
            assert_eq!(
                decoded.root_crl,
                Some(RootCrlStatus {
                    number: Some("12".to_owned()),
                    next_update: next_update.map(UnixMillis::new),
                    expired,
                }),
                "{next_update:?}"
            );
        }
        // A number longer than any CRL number is left out; the heartbeat stays valid.
        in_use.send_replace(Some(RootCrlInUse {
            number: Some("9".repeat(MAX_CRL_NUMBER_DIGITS + 1)),
            next_update: None,
        }));
        let heartbeat = bare(&settings, session);
        assert!(heartbeat.encode().is_ok());
        assert_eq!(heartbeat.root_crl.and_then(|status| status.number), None);
    }

    #[test]
    fn the_buffer_and_the_clock_state_come_from_their_sources() {
        let mut settings = settings();
        let heartbeat = bare(&settings, SessionId::new("s-1").unwrap());
        assert_eq!(heartbeat.buffer, Stats::default().snapshot());
        assert_eq!(heartbeat.buffer.pending_batches, 0);
        assert!(!heartbeat.clock_synced, "timesyncd has not synchronized");
        // A file that exists stands for timesyncd's flag; then a comparison with the cloud's
        // time decides.
        settings.clock =
            GatewayClock::new(crate::clock::SystemWall, std::env::current_exe().unwrap());
        let heartbeat = bare(&settings, SessionId::new("s-1").unwrap());
        assert!(heartbeat.clock_synced);
        let now = settings.clock.wall();
        settings.clock.compare(
            UnixMillis::new(now.get() + 3_600_000),
            now,
            now,
            Duration::ZERO,
        );
        let heartbeat = bare(&settings, SessionId::new("s-1").unwrap());
        assert!(
            !heartbeat.clock_synced,
            "the cloud's time says the clock is off"
        );
    }

    #[test]
    fn heartbeats_are_valid_messages_with_what_the_agent_knows() {
        let meminfo = "MemTotal:        1919956 kB\nMemFree:          100000 kB\nMemAvailable:    1500000 kB\n";
        let heartbeat = build(
            &settings(),
            SessionId::new("s-1").unwrap(),
            Duration::from_secs(42),
            &sample(meminfo, "3600.25 7000.00\n"),
            &mut Carried::default(),
        );
        let payload = heartbeat.encode().unwrap();
        let decoded = Heartbeat::decode(&payload).unwrap();
        assert_eq!(decoded, heartbeat);
        assert_eq!(decoded.interval_s, 60);
        assert_eq!(decoded.system.memory_total_bytes, 1_919_956 * 1024);
        assert_eq!(decoded.system.memory_available_bytes, 1_500_000 * 1024);
        assert_eq!(decoded.system.uptime_s, 3600);
        assert_eq!(decoded.system.agent_uptime_s, 42);
        assert_eq!(
            decoded.sent_at.get() - decoded.system.booted_at.get(),
            3_600_250
        );
        assert_eq!(decoded.cert_not_after.get(), 1_900_000_000_000);
        assert!(decoded.devices.is_empty());
    }

    #[test]
    fn heartbeats_report_the_expiry_of_the_renewed_certificate() {
        let (expiry, receiver) = watch::channel(UnixMillis::new(1_900_000_000_000));
        let settings = HeartbeatSettings {
            certificate_not_after: receiver,
            ..settings()
        };
        let session = SessionId::new("s-1").unwrap();
        let before = bare(&settings, session.clone());
        expiry.send_replace(UnixMillis::new(1_950_000_000_000));
        let after = bare(&settings, session);
        assert_eq!(before.cert_not_after.get(), 1_900_000_000_000);
        assert_eq!(after.cert_not_after.get(), 1_950_000_000_000);
    }

    #[test]
    fn unreadable_proc_files_report_zeros_and_still_encode() {
        let heartbeat = build(
            &settings(),
            SessionId::new("s-1").unwrap(),
            Duration::ZERO,
            &sample("", "garbage"),
            &mut Carried::default(),
        );
        assert_eq!(heartbeat.system.memory_total_bytes, 0);
        assert_eq!(heartbeat.system.uptime_s, 0);
        // What the hardware or the partition does not tell is empty, not invented.
        assert_eq!(heartbeat.system.cpu_percent, 0.0);
        assert_eq!(heartbeat.system.soc_temp_celsius, None);
        assert_eq!(heartbeat.system.throttling, None);
        assert_eq!(heartbeat.system.data_total_bytes, 0);
        assert!(heartbeat.encode().is_ok());
        assert_eq!(meminfo_bytes("MemTotal: 12 MB", "MemTotal:"), 0);
        assert_eq!(os_uptime("-5 1"), Duration::ZERO);
    }

    fn reading(device: &str, at: u64) -> Reading {
        Reading {
            device: DeviceId::new(device).unwrap(),
            point: PointKey::new("power").unwrap(),
            sample: Sample {
                at: UnixMillis::new(at),
                value: Value::Int(1),
            },
            clock_synced: true,
        }
    }

    /// Rebuild checklist H1 acceptance (the gateway side): the heartbeat carries every field the
    /// checklist lists, each from its own source, and the result is a valid message.
    #[test]
    fn a_heartbeat_carries_every_operating_metric_of_the_checklist() {
        let (version, config_version) = watch::channel(ConfigVersion::new(7));
        let devices = DeviceBook::new();
        let id = DeviceId::new("meter-1").unwrap();
        devices.add(&id);
        let failure = crate::modbus::ReadError::Exception(2).failure(UnixMillis::new(1_999));
        devices.record(
            &id,
            &Poll {
                readings: vec![reading("meter-1", 1_998)],
                failed: vec![PointFailure {
                    point: PointKey::new("energy").unwrap(),
                    failure: failure.clone(),
                }],
                requests: vec![
                    RequestReport {
                        read: Read::new(protocol::config::FunctionCode::ReadHoldingRegisters, 0, 1)
                            .unwrap(),
                        failure: None,
                        unit_mismatches: 0,
                    },
                    RequestReport {
                        read: Read::new(protocol::config::FunctionCode::ReadHoldingRegisters, 9, 1)
                            .unwrap(),
                        failure: Some(failure),
                        unit_mismatches: 0,
                    },
                ],
                undecodable: 0,
            },
        );
        let settings = HeartbeatSettings {
            health: HealthSources {
                host: HostPaths::nowhere(),
                boot: BootCounts {
                    boot_count: 12,
                    agent_starts_since_boot: 3,
                },
                devices,
                config_version,
            },
            ..settings()
        };
        let host = HostSample {
            meminfo: "MemTotal: 2048 kB\nMemAvailable: 1024 kB\n".to_owned(),
            uptime: "100.0 0.0\n".to_owned(),
            stat: "cpu  100 0 100 800 0 0 0 0\n".to_owned(),
            temperature: Some("48500\n".to_owned()),
            throttled: Some("50005\n".to_owned()),
            data: Some(DiskSpace {
                total_bytes: 16_000_000,
                available_bytes: 9_000_000,
            }),
        };
        let heartbeat = build(
            &settings,
            SessionId::new("s-1").unwrap(),
            Duration::from_secs(30),
            &host,
            &mut Carried::default(),
        );
        let decoded = Heartbeat::decode(&heartbeat.encode().unwrap()).unwrap();
        assert_eq!(decoded, heartbeat);
        // The agent and the configuration.
        assert_eq!(decoded.agent_version.as_str(), "0.1.0");
        assert_eq!(decoded.config_version, ConfigVersion::new(7));
        version.send_replace(ConfigVersion::new(8));
        let next = bare(&settings, SessionId::new("s-1").unwrap());
        assert_eq!(next.config_version, ConfigVersion::new(8));
        // The host.
        let system = &decoded.system;
        assert!((system.cpu_percent - 20.0).abs() < 1e-9);
        assert_eq!(
            (system.memory_total_bytes, system.memory_available_bytes),
            (2048 * 1024, 1024 * 1024)
        );
        assert_eq!(
            (system.data_total_bytes, system.data_available_bytes),
            (16_000_000, 9_000_000)
        );
        assert_eq!(system.soc_temp_celsius, Some(48.5));
        let both = ThrottleFlags {
            under_voltage: true,
            throttled: true,
            ..ThrottleFlags::default()
        };
        assert_eq!(
            system.throttling,
            Some(Throttling {
                now: both,
                since_boot: both
            })
        );
        assert_eq!(system.uptime_s, 100);
        assert_eq!(system.boot_count, 12);
        assert_eq!(system.agent_uptime_s, 30);
        assert_eq!(system.agent_starts_since_boot, 3);
        // The buffer, the clock and the certificate.
        assert_eq!(decoded.buffer, Stats::default().snapshot());
        assert!(!decoded.clock_synced);
        assert_eq!(decoded.cert_not_after.get(), 1_900_000_000_000);
        // The device.
        assert_eq!(decoded.devices.len(), 1);
        let device = &decoded.devices[0];
        assert_eq!(device.device, id);
        assert_eq!(device.last_success, Some(UnixMillis::new(1_998)));
        assert_eq!((device.points_total, device.points_failed), (2, 1));
        assert_eq!(device.requests, 2);
        assert_eq!(
            device.failures,
            FailureCounts {
                exception: 1,
                ..FailureCounts::default()
            }
        );
        assert_eq!(device.exception_codes.get(&2), Some(&1));
        assert_eq!(
            device.last_failure,
            Some(LastFailure {
                kind: FailureKind::Exception,
                at: UnixMillis::new(1_999),
                exception_code: Some(2),
                detail: None,
            })
        );
    }

    #[test]
    fn the_cpu_is_measured_between_heartbeats_and_throttling_flags_stay_seen() {
        let settings = settings();
        let mut carried = Carried::default();
        let session = SessionId::new("s-1").unwrap();
        let mut host = HostSample {
            stat: "cpu  100 0 0 100 0 0 0 0\n".to_owned(),
            throttled: Some("1\n".to_owned()),
            ..HostSample::default()
        };
        let first = build(
            &settings,
            session.clone(),
            Duration::ZERO,
            &host,
            &mut carried,
        );
        assert!((first.system.cpu_percent - 50.0).abs() < 1e-9, "since boot");
        // Between the two: 100 busy and 300 idle ticks; the throttling flag cleared.
        host.stat = "cpu  200 0 0 400 0 0 0 0\n".to_owned();
        host.throttled = Some("0\n".to_owned());
        let second = build(&settings, session, Duration::ZERO, &host, &mut carried);
        assert!((second.system.cpu_percent - 25.0).abs() < 1e-9);
        let throttling = second.system.throttling.unwrap();
        assert!(!throttling.now.under_voltage);
        assert!(
            throttling.since_boot.under_voltage,
            "seen earlier in this run"
        );
    }

    #[test]
    fn heartbeats_report_the_configured_devices_before_their_first_poll() {
        let settings = settings();
        settings
            .health
            .devices
            .add(&DeviceId::new("meter-1").unwrap());
        let heartbeat = bare(&settings, SessionId::new("s-1").unwrap());
        let decoded = Heartbeat::decode(&heartbeat.encode().unwrap()).unwrap();
        assert_eq!(decoded.devices.len(), 1);
        assert_eq!(decoded.devices[0].requests, 0);
    }

    /// A named pipe nobody writes to: opening it for reading blocks until a writer opens it, like
    /// a sysfs file whose firmware call never returns.
    fn blocking_file(directory: &std::path::Path) -> std::path::PathBuf {
        let fifo = directory.join("temperature");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo {}", fifo.display());
        fifo
    }

    /// Lets the blocked read through: a writer that opens and closes the pipe gives it an empty
    /// file. The open waits for a reader, which is there once the read has started.
    fn release(fifo: &std::path::Path) {
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .open(fifo)
                .expect("a reader opens the pipe"),
        );
    }

    /// Runs `future` while the paused clock moves on in steps of `step`, so that a timeout fires
    /// because the test advanced the clock past it, never because the machine is slow.
    async fn with_clock<F: Future>(step: Duration, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => return output,
                () = tokio::time::advance(step) => {}
            }
        }
    }

    /// Laysi/trustiot-v2#117, security review L1: a host file whose read never returns holds the
    /// heartbeat for the limit at most, and never more than one blocking thread.
    #[tokio::test(start_paused = true)]
    async fn a_host_read_that_never_returns_delays_the_heartbeat_by_the_limit_and_holds_one_thread()
    {
        let scratch = testkit::mqtt::ScratchDir::new("agent-heartbeat-blocked");
        let fifo = blocking_file(scratch.path());
        let paths = HostPaths {
            temperature: fifo.clone(),
            ..HostPaths::nowhere()
        };
        let limit = host_read_limit(Duration::from_secs(60));
        assert_eq!(limit, HOST_READ_LIMIT_MAX);
        let mut reader = HostReader::default();
        for beat in 0..4 {
            let sample = testkit::clock::bounded(
                format!("heartbeat {beat}'s host read to time out"),
                with_clock(limit, reader.sample(&paths, limit)),
            )
            .await;
            assert_eq!(sample, HostSample::default(), "heartbeat {beat}");
        }
        assert_eq!(
            reader.started, 1,
            "the stuck read is waited for again, not started again"
        );
        assert!(reader.reading.is_some());

        // The files answer after all: the first heartbeat that finds the read finished uses it,
        // and the next heartbeat starts a new one.
        release(&fifo);
        let sample = testkit::clock::bounded("the released read to finish", async {
            loop {
                let sample = with_clock(limit, reader.sample(&paths, limit)).await;
                if reader.reading.is_none() {
                    return sample;
                }
            }
        })
        .await;
        assert_eq!(sample.temperature.as_deref(), Some(""));
        assert_eq!(reader.started, 1);
        let late = testkit::clock::bounded(
            "the next read to time out",
            with_clock(limit, reader.sample(&paths, limit)),
        )
        .await;
        assert_eq!(late, HostSample::default());
        assert_eq!(reader.started, 2);

        // Let the last read go, or the runtime would wait for its thread when the test ends.
        release(&fifo);
        testkit::clock::bounded("the last read to finish", async {
            while reader.reading.is_some() {
                with_clock(limit, reader.sample(&paths, limit)).await;
            }
        })
        .await;
    }

    #[test]
    fn the_wait_for_the_host_is_a_quarter_of_the_interval_within_bounds() {
        for (interval, limit) in [(1, 1), (4, 1), (8, 2), (20, 5), (30, 5), (3600, 5)] {
            assert_eq!(
                host_read_limit(Duration::from_secs(interval)),
                Duration::from_secs(limit),
                "{interval} s"
            );
        }
    }
}
