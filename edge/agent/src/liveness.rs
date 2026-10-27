//! Whether the connection task makes progress, for the systemd watchdog.
//!
//! A thread that runs is not enough: the connection task can wait in an `await` that never
//! ends while its runtime keeps going. So the connection task declares every wait before it
//! starts, with the latest moment the wait may legitimately end ([`Progress::enter`]): opening
//! the connection, a write, CONNACK, the main loop until its next timer, a backoff delay, the
//! stop. The watchdog task ([`watchdog`]) sends `WATCHDOG=1` only while the declared wait is
//! within its deadline (plus [`SLACK`]) and the connection thread keeps counting ticks. A wait
//! that overstays stops the notifications, so systemd restarts the agent, and `STATUS=` names
//! the wait and how long it has lasted.
//!
//! Long but legitimate waits keep the watchdog fed, because their deadline says so: a backoff
//! of 60 s on a site without network never gets the agent restarted.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::sdnotify::{Notification, Notifier};

/// Added to every deadline before a wait counts as stuck: timers fire late on a loaded
/// gateway, and the code between two declarations takes some time too.
pub const SLACK: Duration = Duration::from_secs(10);
/// How long the connection task may take to declare its first wait.
pub const STARTUP_LIMIT: Duration = Duration::from_secs(10);
/// How long a received message may wait for the application to take it from
/// [`crate::link::Incoming`]. The connection does not read meanwhile; an application that takes
/// longer is stuck.
pub const DELIVERY_LIMIT: Duration = Duration::from_secs(300);

/// What the connection task is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    /// The connection task has not started yet.
    Starting,
    /// Opening a connection: TCP, outer TLS, the WebSocket upgrade, the inner mTLS.
    Connecting,
    /// Writing a packet to the broker.
    Writing,
    /// Waiting for CONNACK.
    AwaitConnack,
    /// After CONNACK, until `online` is acknowledged and the SUBACK (if the link subscribes to
    /// anything) grants every subscription.
    SettingUp,
    /// Connected: waiting for the next packet, publish request or timer.
    Connected,
    /// Waiting for the application to take a received message.
    Delivering,
    /// Stopping: waiting for the PUBACK of `offline`/`shutdown`.
    Stopping,
    /// Sending DISCONNECT and closing the stream.
    Closing,
    /// Waiting to reconnect.
    Backoff,
    /// The connection task has ended.
    Stopped,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting the connection task",
            Self::Connecting => "opening the connection",
            Self::Writing => "writing to the broker",
            Self::AwaitConnack => "waiting for CONNACK",
            Self::SettingUp => "waiting for the PUBACK of online or the SUBACK",
            Self::Connected => "connected",
            Self::Delivering => "waiting for the application to take a message",
            Self::Stopping => "stopping",
            Self::Closing => "closing the connection",
            Self::Backoff => "waiting to reconnect",
            Self::Stopped => "stopped",
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Declared {
    phase: Phase,
    since: Instant,
    /// `None`: no limit (the task has ended, or a test turned the keepalive off).
    deadline: Option<Instant>,
}

/// The wait the connection task declared last; shared with the watchdog task.
#[derive(Debug)]
pub struct Progress(Mutex<Declared>);

impl Default for Progress {
    fn default() -> Self {
        let now = Instant::now();
        Self(Mutex::new(Declared {
            phase: Phase::Starting,
            since: now,
            deadline: Some(now + STARTUP_LIMIT),
        }))
    }
}

/// What [`Progress::check`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The declared wait is within its deadline (and [`SLACK`]).
    Alive,
    /// The declared wait has passed its deadline by more than [`SLACK`].
    Overstayed {
        /// The wait.
        phase: Phase,
        /// How long it has lasted.
        lasted: Duration,
        /// How long it was allowed to last.
        allowed: Duration,
    },
}

impl Progress {
    fn lock(&self) -> MutexGuard<'_, Declared> {
        // Only plain values are written under the lock, so a poisoned one is still consistent.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Declares a wait that starts now and may last until `deadline` (`None`: no limit).
    pub fn enter(&self, phase: Phase, deadline: Option<Instant>) {
        self.enter_since(phase, Instant::now(), deadline);
    }

    /// Declares a wait that started at `since` (a wait resumed after other work).
    pub fn enter_since(&self, phase: Phase, since: Instant, deadline: Option<Instant>) {
        *self.lock() = Declared {
            phase,
            since,
            deadline,
        };
    }

    /// The wait declared last.
    pub fn phase(&self) -> Phase {
        self.lock().phase
    }

    /// Whether the declared wait is still within its deadline at `now`.
    pub fn check(&self, now: Instant) -> Liveness {
        let declared = *self.lock();
        match declared.deadline {
            Some(deadline) if now > deadline + SLACK => Liveness::Overstayed {
                phase: declared.phase,
                lasted: now.saturating_duration_since(declared.since),
                allowed: deadline.saturating_duration_since(declared.since),
            },
            _ => Liveness::Alive,
        }
    }
}

/// Sends `WATCHDOG=1` every `interval` while the connection task's declared wait is within its
/// deadline ([`Progress::check`]) and the connection thread's tick count moved since the last
/// time (a blocked thread stops counting). Otherwise it withholds the notification, so systemd
/// restarts the agent; an overstayed wait also goes to `STATUS=`. This task runs on the main
/// runtime, so a stuck main runtime withholds it too.
pub async fn watchdog(
    notifier: Arc<Notifier>,
    interval: Duration,
    ticks: Arc<AtomicU64>,
    progress: Arc<Progress>,
    gateway: String,
) {
    let mut last = ticks.load(Ordering::Relaxed);
    let mut timer = tokio::time::interval_at(Instant::now() + interval, interval);
    loop {
        timer.tick().await;
        let now = ticks.load(Ordering::Relaxed);
        let thread_moved = now != last;
        last = now;
        if !thread_moved {
            tracing::error!(
                "the connection thread made no progress; withholding the watchdog notification"
            );
            continue;
        }
        match progress.check(Instant::now()) {
            Liveness::Alive => {
                if let Err(error) = notifier.notify(&[Notification::Watchdog]) {
                    tracing::warn!(%error, "could not notify the systemd watchdog");
                }
            }
            Liveness::Overstayed {
                phase,
                lasted,
                allowed,
            } => {
                let status = format!(
                    "gateway {gateway}: stuck {phase} for {} s (allowed {} s); withholding the watchdog",
                    lasted.as_secs(),
                    allowed.as_secs() + SLACK.as_secs()
                );
                tracing::error!(%phase, lasted_s = lasted.as_secs(), allowed_s = allowed.as_secs(), "the connection task is stuck; withholding the watchdog notification");
                if let Err(error) = notifier.notify(&[Notification::Status(&status)]) {
                    tracing::warn!(%error, "could not notify systemd");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::UnixDatagram;

    use testkit::mqtt::ScratchDir;

    const INTERVAL: Duration = Duration::from_secs(15);

    /// A watchdog task that notifies a socket the test reads.
    struct Fed {
        _dir: ScratchDir,
        receiver: UnixDatagram,
        ticks: Arc<AtomicU64>,
        progress: Arc<Progress>,
        started: Instant,
        task: tokio::task::JoinHandle<()>,
    }

    impl Fed {
        async fn start() -> Self {
            let dir = ScratchDir::new("liveness");
            let path = dir.path().join("notify.sock");
            let receiver = UnixDatagram::bind(&path).unwrap();
            receiver.set_nonblocking(true).unwrap();
            let notifier = Arc::new(Notifier::to_path(&path).unwrap());
            let ticks = Arc::new(AtomicU64::new(0));
            let progress = Arc::new(Progress::default());
            progress.enter(Phase::Connecting, None);
            let started = Instant::now();
            let task = tokio::spawn(watchdog(
                notifier,
                INTERVAL,
                Arc::clone(&ticks),
                Arc::clone(&progress),
                "gw-test-1".to_owned(),
            ));
            // Let it read the starting count.
            tokio::task::yield_now().await;
            Self {
                _dir: dir,
                receiver,
                ticks,
                progress,
                started,
                task,
            }
        }

        /// Just past notification `round` (1, 2, ...): what the watchdog task sent for it.
        async fn after(&self, round: u32, thread_moved: bool) -> Vec<String> {
            if thread_moved {
                self.ticks.fetch_add(1, Ordering::Relaxed);
            }
            tokio::time::sleep_until(self.started + INTERVAL * round + Duration::from_millis(1))
                .await;
            let mut sent = Vec::new();
            let mut buffer = [0_u8; 512];
            loop {
                match self.receiver.recv(&mut buffer) {
                    Ok(len) => sent.push(String::from_utf8_lossy(&buffer[..len]).into_owned()),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return sent,
                    Err(error) => panic!("{error}"),
                }
            }
        }
    }

    impl Drop for Fed {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_backoff_keeps_the_watchdog_fed() {
        let fed = Fed::start().await;
        // The longest backoff, declared at once: every notification within it goes out.
        fed.progress.enter(
            Phase::Backoff,
            Some(Instant::now() + Duration::from_secs(60)),
        );
        for round in 1..=4 {
            assert_eq!(fed.after(round, true).await, ["WATCHDOG=1\n"], "{round}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_past_its_deadline_stops_the_watchdog_and_says_why() {
        let fed = Fed::start().await;
        let declared = Instant::now();
        fed.progress.enter(
            Phase::AwaitConnack,
            Some(declared + Duration::from_secs(30)),
        );
        // Deadline 30 s, plus SLACK 10 s: fed at 15 s and 30 s, not at 45 s or later.
        assert_eq!(fed.after(1, true).await, ["WATCHDOG=1\n"]);
        assert_eq!(fed.after(2, true).await, ["WATCHDOG=1\n"]);
        assert_eq!(
            fed.after(3, true).await,
            [
                "STATUS=gateway gw-test-1: stuck waiting for CONNACK for 45 s (allowed 40 s); withholding the watchdog\n"
            ]
        );
        assert_eq!(fed.after(4, true).await.len(), 1, "still stuck");
        // A new wait within its deadline feeds it again.
        fed.progress.enter(
            Phase::Backoff,
            Some(Instant::now() + Duration::from_secs(20)),
        );
        assert_eq!(fed.after(5, true).await, ["WATCHDOG=1\n"]);
        fed.progress.enter(
            Phase::Backoff,
            Some(Instant::now() + Duration::from_secs(1)),
        );
        assert_eq!(
            fed.progress.check(Instant::now() + Duration::from_secs(12)),
            Liveness::Overstayed {
                phase: Phase::Backoff,
                lasted: Duration::from_secs(12),
                allowed: Duration::from_secs(1)
            }
        );
        // Without a deadline (the task ended) nothing is stuck.
        fed.progress.enter(Phase::Stopped, None);
        assert_eq!(
            fed.progress
                .check(Instant::now() + Duration::from_secs(86_400)),
            Liveness::Alive
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_connection_thread_stops_the_watchdog() {
        let fed = Fed::start().await;
        fed.progress.enter(
            Phase::Backoff,
            Some(Instant::now() + Duration::from_secs(600)),
        );
        let moved = [true, false, true, true, false];
        let mut fed_rounds = Vec::new();
        for (round, thread_moved) in (1_u32..).zip(moved) {
            fed_rounds.push(fed.after(round, thread_moved).await == ["WATCHDOG=1\n"]);
        }
        assert_eq!(fed_rounds, moved);
    }

    #[test]
    fn the_connection_task_gets_a_deadline_before_it_starts() {
        let progress = Progress::default();
        assert_eq!(progress.phase(), Phase::Starting);
        let late = Instant::now() + STARTUP_LIMIT + SLACK + Duration::from_millis(1);
        assert!(matches!(
            progress.check(late),
            Liveness::Overstayed {
                phase: Phase::Starting,
                ..
            }
        ));
    }
}
