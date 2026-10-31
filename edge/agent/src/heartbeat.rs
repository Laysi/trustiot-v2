//! The heartbeat: a valid [`Heartbeat`] on `up/heartbeat` right after every connection and then
//! every interval, with what the agent knows by itself: its session, version and uptime, the OS
//! uptime and boot time, the memory of the gateway, the client certificate's expiry, the root
//! CRL in use (`root_crl`: its number, nextUpdate and whether that has passed), the
//! offline buffer (`buffer`, from [`Stats`]) and whether the clock is synchronized
//! (`clock_synced`, from [`GatewayClock::synced`], the same state the batches are marked with).
//! Device statistics, CPU, disk, temperature, throttling and boot counts are
//! Laysi/trustiot-v2#117's and stay empty or zero until then.

use std::time::Duration;

use protocol::health::{Heartbeat, MAX_CRL_NUMBER_DIGITS, RootCrlStatus, SystemStats};
use protocol::topic::Channel;
use protocol::{AgentVersion, SessionId, UnixMillis, V1};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::clock::GatewayClock;
use crate::link::{Link, LinkState, PublishError};
use crate::tls::RootCrlInUse;
use crate::upload::Stats;

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
}

/// Sends heartbeats while the link is connected, until `shutdown` is cancelled.
pub async fn run(
    link: Link,
    settings: HeartbeatSettings,
    started: Instant,
    shutdown: CancellationToken,
) {
    let mut state = link.state();
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
                    send(&link, &settings, &session, started, &mut busy).await;
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
) {
    let heartbeat = build(
        settings,
        session.clone(),
        started.elapsed(),
        &read_proc("/proc/meminfo"),
        &read_proc("/proc/uptime"),
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

fn read_proc(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The heartbeat for `session`, from the contents of `/proc/meminfo` and `/proc/uptime`
/// (empty when unreadable, which reports zeros).
pub(crate) fn build(
    settings: &HeartbeatSettings,
    session: SessionId,
    agent_uptime: Duration,
    meminfo: &str,
    uptime: &str,
) -> Heartbeat {
    let now = settings.clock.wall();
    let uptime = os_uptime(uptime);
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
        config_version: None,
        system: SystemStats {
            cpu_percent: 0.0,
            memory_total_bytes: meminfo_bytes(meminfo, "MemTotal:"),
            memory_available_bytes: meminfo_bytes(meminfo, "MemAvailable:"),
            data_total_bytes: 0,
            data_available_bytes: 0,
            soc_temp_celsius: None,
            throttling: None,
            booted_at,
            uptime_s: uptime.as_secs(),
            boot_count: 0,
            agent_uptime_s: agent_uptime.as_secs(),
            agent_starts_since_boot: 0,
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
        devices: Vec::new(),
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
    use super::*;

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
        let none = build(&settings, session.clone(), Duration::ZERO, "", "");
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
            let heartbeat = build(&settings, session.clone(), Duration::ZERO, "", "");
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
        let heartbeat = build(&settings, session, Duration::ZERO, "", "");
        assert!(heartbeat.encode().is_ok());
        assert_eq!(heartbeat.root_crl.and_then(|status| status.number), None);
    }

    #[test]
    fn the_buffer_and_the_clock_state_come_from_their_sources() {
        let mut settings = settings();
        let heartbeat = build(
            &settings,
            SessionId::new("s-1").unwrap(),
            Duration::ZERO,
            "",
            "",
        );
        assert_eq!(heartbeat.buffer, Stats::default().snapshot());
        assert_eq!(heartbeat.buffer.pending_batches, 0);
        assert!(!heartbeat.clock_synced, "timesyncd has not synchronized");
        // A file that exists stands for timesyncd's flag; then a comparison with the cloud's
        // time decides.
        settings.clock =
            GatewayClock::new(crate::clock::SystemWall, std::env::current_exe().unwrap());
        let heartbeat = build(
            &settings,
            SessionId::new("s-1").unwrap(),
            Duration::ZERO,
            "",
            "",
        );
        assert!(heartbeat.clock_synced);
        let now = settings.clock.wall();
        settings.clock.compare(
            UnixMillis::new(now.get() + 3_600_000),
            now,
            now,
            Duration::ZERO,
        );
        let heartbeat = build(
            &settings,
            SessionId::new("s-1").unwrap(),
            Duration::ZERO,
            "",
            "",
        );
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
            meminfo,
            "3600.25 7000.00\n",
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
        let before = build(&settings, session.clone(), Duration::ZERO, "", "");
        expiry.send_replace(UnixMillis::new(1_950_000_000_000));
        let after = build(&settings, session, Duration::ZERO, "", "");
        assert_eq!(before.cert_not_after.get(), 1_900_000_000_000);
        assert_eq!(after.cert_not_after.get(), 1_950_000_000_000);
    }

    #[test]
    fn unreadable_proc_files_report_zeros_and_still_encode() {
        let heartbeat = build(
            &settings(),
            SessionId::new("s-1").unwrap(),
            Duration::ZERO,
            "",
            "garbage",
        );
        assert_eq!(heartbeat.system.memory_total_bytes, 0);
        assert_eq!(heartbeat.system.uptime_s, 0);
        assert!(heartbeat.encode().is_ok());
        assert_eq!(meminfo_bytes("MemTotal: 12 MB", "MemTotal:"), 0);
        assert_eq!(os_uptime("-5 1"), Duration::ZERO);
    }
}
