//! The basic heartbeat: a valid [`Heartbeat`] on `up/heartbeat` right after every connection
//! and then every interval, with what the agent knows by itself: its session, version and
//! uptime, the OS uptime and boot time, the memory of the gateway and the client certificate's
//! expiry. Device statistics, the offline buffer, CPU, disk, temperature, throttling and boot
//! counts are Laysi/trustiot-v2#117's and stay empty or zero until then; the clock's sync
//! state is Laysi/trustiot-v2#90's and reads `false` (not known to be synchronized).

use std::time::{Duration, SystemTime};

use protocol::health::{BufferStats, Heartbeat, SystemStats};
use protocol::topic::Channel;
use protocol::{AgentVersion, SessionId, UnixMillis, V1};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::link::{Link, LinkState, PublishError};

/// What every heartbeat carries besides the session.
#[derive(Clone, Debug)]
pub struct HeartbeatSettings {
    /// Time between heartbeats; also sent as `interval_s`.
    pub interval: Duration,
    /// This agent's release.
    pub agent_version: AgentVersion,
    /// When the client certificate expires.
    pub certificate_not_after: UnixMillis,
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
    let now = UnixMillis::from_system_time(SystemTime::now()).unwrap_or(UnixMillis::new(0));
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
        buffer: BufferStats {
            pending_records: 0,
            pending_batches: 0,
            oldest_pending: None,
            disk_bytes: 0,
            dropped_records: 0,
        },
        clock_synced: false,
        cert_not_after: settings.certificate_not_after,
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
            certificate_not_after: UnixMillis::new(1_900_000_000_000),
        }
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
