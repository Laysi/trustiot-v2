//! The gateway's clock: the time readings are stamped with, whether it is synchronized, and
//! the clock that measures how long a batch has been buffered (Laysi/trustiot-v2#90).
//!
//! Timestamps are never rewritten: a reading keeps the time [`GatewayClock`] gave it, and a
//! batch says with `clock_synced` whether the clock was synchronized for all of its samples
//! (Laysi/trustiot#180). The agent never sets the clock either: the unit has `ProtectClock=yes`
//! and no capabilities, and systemd-timesyncd corrects it over NTP.
//!
//! **Synchronized.** Until the first [`TelemetryAck`](protocol::telemetry::TelemetryAck) of
//! this process has been compared with the local clock, the clock counts as synchronized when
//! systemd-timesyncd says so (the file [`TIMESYNC_SYNCHRONIZED`] exists). From then on it is the
//! result of the latest comparison: the ack's `server_time` must lie between the local time of
//! the batch's first publish that the broker accepted on the connection the ack came on minus
//! [`SYNC_MARGIN`] and the local time the ack arrived plus [`SYNC_MARGIN`]. An ack can only
//! answer a publish made on its own connection (every connection starts a clean session) that
//! reached the cloud, so neither a publish on an earlier connection nor one the broker refused
//! widens the window; without an accepted publish there is nothing to compare. The
//! heartbeat's `clock_synced` reads the same state.
//!
//! An ack is compared only when it arrives within [`SYNC_ROUND_TRIP_LIMIT`] of that publish,
//! measured both on the wall clock (the clock the window is on) and on the monotonic clock;
//! otherwise it is not compared and the state stays as it was. The window spans the whole round
//! trip (the wait for the link, the write of the batch, the broker, the cloud's commit, the way
//! back), and a clock off by less than the round trip plus [`SYNC_MARGIN`] can fall inside it:
//! the limit caps that error at [`SYNC_ROUND_TRIP_LIMIT`] + [`SYNC_MARGIN`] (12 s). A wall
//! clock that stepped between the publish and the ack is not compared either: a forward step
//! makes the wall-clock round trip long, and a backward step makes it negative or, by hiding a
//! long round trip, shorter than the monotonic one, which then exceeds the limit.
//!
//! **Batch age.** [`AgeClock`] measures the 30-day limit of the buffer
//! ([`protocol::telemetry::BUFFER_MAX_AGE`]): its time is the larger of the wall clock and a
//! floor plus the monotonic time since the agent started, where the floor is the wall clock at
//! start, raised to the newest formation time of the batches found on disk and to this clock's
//! time as the buffer last saved it there (every minute and at a stop while batches are on
//! disk, [`crate::buffer::CLOCK_SAVE_EVERY`]), and raised again by every reading of the clock
//! so that it never returns less than it returned before plus the monotonic time since. Each
//! reading and each raise happens under one lock, so the values come out in the order the calls
//! take it. A backward step of the wall clock therefore never makes a batch younger, and a
//! forward step counts as elapsed time and stays counted when the wall clock is corrected back,
//! across a restart too: a batch formed during a wrong forward step is still dropped 30 real
//! days later at the latest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use protocol::UnixMillis;
use tokio::time::Instant;

use crate::modbus::{Clock, Timestamp};

/// The file systemd-timesyncd creates once it has synchronized the clock (systemd 239 and
/// later).
pub const TIMESYNC_SYNCHRONIZED: &str = "/run/systemd/timesync/synchronized";

/// How far an acknowledgement's `server_time` may lie outside the local times around it before
/// the clock counts as not synchronized: network delay, the cloud's own clock error, and the
/// millisecond rounding of both clocks.
pub const SYNC_MARGIN: Duration = Duration::from_secs(2);

/// The longest round trip, from right before a batch's publish to the arrival of the cloud's
/// acknowledgement, whose `server_time` is compared with the local clock. A clock off by less
/// than the round trip plus [`SYNC_MARGIN`] can pass the comparison, so this bounds the error
/// of a clock judged synchronized to 12 s. The cloud normally acknowledges within a few seconds;
/// a slower answer (a long wait for the link, a slow uplink, a resend after an acknowledgement
/// timeout) leaves the clock's state as it was.
pub const SYNC_ROUND_TRIP_LIMIT: Duration = Duration::from_secs(10);

/// How often a clock that stays off is reported again.
const REPORT_EVERY: Duration = Duration::from_secs(600);

/// A wall clock: milliseconds since the Unix epoch. [`SystemWall`] outside tests.
pub trait WallClock: Send + Sync + 'static {
    /// The time now; 0 for a clock before 1970.
    fn now(&self) -> UnixMillis;
}

/// The system's real-time clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWall;

impl WallClock for SystemWall {
    fn now(&self) -> UnixMillis {
        UnixMillis::from_system_time(SystemTime::now()).unwrap_or(UnixMillis::new(0))
    }
}

/// What the latest comparison with the cloud's time said.
const NOT_COMPARED: u8 = 0;
const WITHIN: u8 = 1;
const OFF: u8 = 2;

/// The gateway's clock and whether it is synchronized; cheap to clone, shared by everything that
/// stamps or reports time. Implements [`Clock`] for the Modbus readers.
#[derive(Clone)]
pub struct GatewayClock {
    inner: Arc<Inner>,
}

struct Inner {
    wall: Box<dyn WallClock>,
    timesync: PathBuf,
    compared: AtomicU8,
    /// When an off clock was last reported, to repeat the warning only every [`REPORT_EVERY`].
    reported: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for GatewayClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayClock")
            .field("timesync", &self.inner.timesync)
            .field("synced", &self.synced())
            .finish_non_exhaustive()
    }
}

/// The result of comparing an acknowledgement's `server_time` with the local clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    /// Within the window: the clock counts as synchronized.
    Within,
    /// Outside: the local clock is ahead of the cloud's by at least this many milliseconds
    /// (negative: behind by at least as many).
    Off {
        /// Local minus cloud: the publish time minus `server_time` when the cloud's time is
        /// before the window, `server_time` minus the arrival time (negated) when after it.
        offset_ms: i64,
    },
    /// Not compared: the round trip was longer than [`SYNC_ROUND_TRIP_LIMIT`] on the wall clock
    /// or on the monotonic clock, or the wall clock ran backward across it. The clock's state
    /// stays as it was.
    NotCompared {
        /// The arrival minus the publish on the wall clock (negative: it ran backward).
        wall_ms: i64,
        /// The monotonic time from the publish to the arrival.
        elapsed_ms: u64,
    },
}

impl GatewayClock {
    /// The system clock, with systemd-timesyncd's flag file.
    pub fn system() -> Self {
        Self::new(SystemWall, PathBuf::from(TIMESYNC_SYNCHRONIZED))
    }

    /// A clock reading `wall`, which counts as synchronized before the first comparison while
    /// `timesync` exists.
    pub fn new(wall: impl WallClock, timesync: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                wall: Box::new(wall),
                timesync,
                compared: AtomicU8::new(NOT_COMPARED),
                reported: Mutex::new(None),
            }),
        }
    }

    /// The wall clock now.
    pub fn wall(&self) -> UnixMillis {
        self.inner.wall.now()
    }

    /// Whether the clock counts as synchronized now (see the module documentation).
    pub fn synced(&self) -> bool {
        match self.inner.compared.load(Ordering::Relaxed) {
            WITHIN => true,
            OFF => false,
            _ => self.inner.timesync.exists(),
        }
    }

    /// Compares the cloud's `server_time` from an acknowledgement with the local times around
    /// it: `published`, right before the acknowledged batch's first publish that the broker
    /// accepted on this connection, and `arrived`, when the acknowledgement arrived; `elapsed`
    /// is the monotonic time between the two. The result decides [`GatewayClock::synced`]
    /// until the next comparison; a clock that turns off is logged with its offset (and again
    /// every ten minutes while it stays off), one that turns good again too.
    ///
    /// Only an acknowledgement that arrived within [`SYNC_ROUND_TRIP_LIMIT`] of the publish,
    /// on the wall clock (`arrived - published` from 0 to the limit) and on the monotonic
    /// clock (`elapsed` up to the limit), is compared; otherwise the result is
    /// [`Comparison::NotCompared`] and the state stays as it was (see the module
    /// documentation).
    pub fn compare(
        &self,
        server_time: UnixMillis,
        published: UnixMillis,
        arrived: UnixMillis,
        elapsed: Duration,
    ) -> Comparison {
        let limit = i128::try_from(SYNC_ROUND_TRIP_LIMIT.as_millis()).unwrap_or(i128::MAX);
        let wall = i128::from(arrived.get()) - i128::from(published.get());
        if !(0..=limit).contains(&wall) || elapsed > SYNC_ROUND_TRIP_LIMIT {
            let wall_ms = i64::try_from(wall).unwrap_or(if wall < 0 { i64::MIN } else { i64::MAX });
            let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            tracing::debug!(
                server_time = server_time.get(),
                published = published.get(),
                arrived = arrived.get(),
                wall_ms,
                elapsed_ms,
                "the acknowledgement is not compared with the clock: its round trip is over {} s or the wall clock stepped across it",
                SYNC_ROUND_TRIP_LIMIT.as_secs()
            );
            return Comparison::NotCompared {
                wall_ms,
                elapsed_ms,
            };
        }
        let margin = u64::try_from(SYNC_MARGIN.as_millis()).unwrap_or(u64::MAX);
        let earliest = published.get().saturating_sub(margin);
        let latest = arrived.get().saturating_add(margin);
        let server = server_time.get();
        let comparison = if server < earliest {
            Comparison::Off {
                offset_ms: difference(published.get(), server),
            }
        } else if server > latest {
            Comparison::Off {
                offset_ms: -difference(server, arrived.get()),
            }
        } else {
            Comparison::Within
        };
        let state = if comparison == Comparison::Within {
            WITHIN
        } else {
            OFF
        };
        let previous = self.inner.compared.swap(state, Ordering::Relaxed);
        match comparison {
            Comparison::Within if previous == OFF => {
                *self
                    .inner
                    .reported
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                tracing::info!(
                    server_time = server,
                    "the clock agrees with the cloud's time again; readings count as synchronized"
                );
            }
            // Not compared returns before the state changes.
            Comparison::Within | Comparison::NotCompared { .. } => {}
            Comparison::Off { offset_ms } => {
                let mut reported = self
                    .inner
                    .reported
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let due = previous != OFF || reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY);
                if due {
                    *reported = Some(Instant::now());
                    tracing::warn!(
                        offset_ms,
                        server_time = server,
                        published = published.get(),
                        arrived = arrived.get(),
                        "the clock disagrees with the cloud's time by more than {} s (offset_ms > 0: ahead); readings are marked not synchronized until it agrees again",
                        SYNC_MARGIN.as_secs()
                    );
                }
            }
        }
        comparison
    }
}

/// `larger - smaller` as a signed number of milliseconds, saturating.
fn difference(larger: u64, smaller: u64) -> i64 {
    i64::try_from(larger.saturating_sub(smaller)).unwrap_or(i64::MAX)
}

impl Clock for GatewayClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: self.wall(),
            synced: self.synced(),
        }
    }
}

/// The clock that measures a batch's age (see the module documentation): milliseconds on a
/// scale that never runs backward and runs at least as fast as real time. Cheap to clone; the
/// clones share the floor.
///
/// Every reading and every raise of the floor happens under one lock, from reading the clocks
/// to storing the new floor: the values the clock gives out are ordered as the calls take the
/// lock, so no call returns a value computed before a forward step that another call has
/// already returned (linearizable).
#[derive(Clone)]
pub struct AgeClock {
    clock: GatewayClock,
    start: Instant,
    /// The floor: this clock's time is never below it plus the monotonic time since start.
    floor: Arc<Mutex<u64>>,
    /// Tests only: runs between computing a value and storing it, under the lock.
    #[cfg(test)]
    pause: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for AgeClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgeClock")
            .field("floor", &*self.floor())
            .finish_non_exhaustive()
    }
}

impl AgeClock {
    /// An age clock starting now, with the wall clock now as its floor.
    pub fn new(clock: GatewayClock) -> Self {
        let floor = clock.wall().get();
        Self {
            clock,
            start: Instant::now(),
            floor: Arc::new(Mutex::new(floor)),
            #[cfg(test)]
            pause: None,
        }
    }

    fn floor(&self) -> std::sync::MutexGuard<'_, u64> {
        self.floor.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One reading of both clocks: the wall clock and the monotonic time since start.
    fn clocks(&self) -> (u64, u64) {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        (self.clock.wall().get(), elapsed)
    }

    /// This clock's time with `floor` for one reading of the clocks (`wall`, `elapsed`):
    /// `max(wall clock, floor + monotonic time since start)`.
    fn at(floor: u64, (wall, elapsed): (u64, u64)) -> u64 {
        wall.max(floor.saturating_add(elapsed))
    }

    /// This clock's time with `floor`, and the monotonic time since start.
    fn reading(&self, floor: u64) -> (u64, u64) {
        let clocks = self.clocks();
        (Self::at(floor, clocks), clocks.1)
    }

    /// Raises the floor to `at` if it is lower: the newest formation time of the batches found
    /// on disk, so that they never look younger than when they were stored. Returns how far
    /// this moved the clock forward (0 when it did not): batches formed on the clock before
    /// the raise need that much added to their formation time to keep their age.
    pub fn raise_floor(&self, at: u64) -> u64 {
        let mut floor = self.floor();
        // Before and after from one reading of the clocks, so that the result is only what the
        // raise moved: a wall clock step or monotonic time between two readings would be added
        // to the formation time of earlier batches and make them look younger than they are.
        let clocks = self.clocks();
        let before = Self::at(*floor, clocks);
        *floor = (*floor).max(at);
        let after = Self::at(*floor, clocks);
        *floor = (*floor).max(after.saturating_sub(clocks.1));
        after.saturating_sub(before)
    }

    /// The time now on this clock: `max(wall clock, floor + monotonic time since start)`. The
    /// floor is then raised so that this clock never returns less than this value plus the
    /// monotonic time since: a wall clock that stepped forward and is corrected back does not
    /// take the age clock back with it.
    pub fn now(&self) -> u64 {
        let mut floor = self.floor();
        let (now, elapsed) = self.reading(*floor);
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause();
        }
        *floor = (*floor).max(now.saturating_sub(elapsed));
        now
    }

    /// Tests only: [`AgeClock::now`] if no other call is under way, `None` if one is (a call
    /// made now would wait for it).
    #[cfg(test)]
    fn now_if_free(&self) -> Option<u64> {
        match self.floor.try_lock() {
            Ok(guard) => {
                drop(guard);
                Some(self.now())
            }
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => Some(self.now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI64;

    use super::*;

    /// A wall clock the test sets.
    struct Manual(Arc<AtomicI64>);

    impl WallClock for Manual {
        fn now(&self) -> UnixMillis {
            UnixMillis::new(u64::try_from(self.0.load(Ordering::Relaxed)).unwrap_or(0))
        }
    }

    fn manual(at: i64, timesync: PathBuf) -> (GatewayClock, Arc<AtomicI64>) {
        let time = Arc::new(AtomicI64::new(at));
        (GatewayClock::new(Manual(Arc::clone(&time)), timesync), time)
    }

    fn missing_flag() -> PathBuf {
        PathBuf::from("/nonexistent/trustiot-agent-test/synchronized")
    }

    /// A monotonic round trip well within [`SYNC_ROUND_TRIP_LIMIT`].
    const PROMPT: Duration = Duration::from_millis(500);

    #[test]
    fn before_any_comparison_the_timesync_flag_decides() {
        let present = std::env::current_exe().unwrap();
        let (clock, _) = manual(1_000, present);
        assert!(clock.synced());
        let (clock, _) = manual(1_000, missing_flag());
        assert!(!clock.synced());
        assert_eq!(
            clock.now(),
            Timestamp {
                at: UnixMillis::new(1_000),
                synced: false
            }
        );
    }

    #[test]
    fn the_latest_comparison_decides_after_the_first() {
        let present = std::env::current_exe().unwrap();
        let (clock, _) = manual(100_000, present);
        let published = UnixMillis::new(100_000);
        let arrived = UnixMillis::new(100_500);
        // Inside the window, its edges included.
        for server in [98_000, 100_200, 102_500] {
            assert_eq!(
                clock.compare(UnixMillis::new(server), published, arrived, PROMPT),
                Comparison::Within,
                "{server}"
            );
            assert!(clock.synced());
        }
        // The cloud is ahead: the local clock is behind, 1 ms beyond the window.
        assert_eq!(
            clock.compare(UnixMillis::new(102_501), published, arrived, PROMPT),
            Comparison::Off { offset_ms: -2_001 }
        );
        assert!(
            !clock.synced(),
            "the flag file no longer decides once compared"
        );
        // The cloud is behind: the local clock is ahead.
        assert_eq!(
            clock.compare(UnixMillis::new(90_000), published, arrived, PROMPT),
            Comparison::Off { offset_ms: 10_000 }
        );
        assert!(!clock.synced());
        assert_eq!(
            clock.compare(UnixMillis::new(100_100), published, arrived, PROMPT),
            Comparison::Within
        );
        assert!(clock.synced());
    }

    /// Codex review before the commit (after the Laysi/trustiot-v2#94 integration): the window
    /// spans the whole round trip, so a slow acknowledgement let a clock off by up to the round
    /// trip pass. Only one that arrives within the limit, on both clocks, is compared; the rest
    /// leave the state as it was, whatever their `server_time`.
    #[test]
    fn only_an_acknowledgement_within_the_round_trip_limit_is_compared() {
        const HOUR: u64 = 3_600_000;
        let (clock, _) = manual(0, missing_flag());
        let published = UnixMillis::new(10 * HOUR);
        let limit = u64::try_from(SYNC_ROUND_TRIP_LIMIT.as_millis()).unwrap();
        // At the limit on both clocks, the edge included: compared.
        assert_eq!(
            clock.compare(
                published,
                published,
                UnixMillis::new(published.get() + limit),
                SYNC_ROUND_TRIP_LIMIT
            ),
            Comparison::Within
        );
        assert!(clock.synced());
        // 1 ms over on the wall clock, or on the monotonic clock: not compared, even with a
        // `server_time` an hour away, and the clock stays synchronized.
        let far = UnixMillis::new(published.get() - HOUR);
        assert_eq!(
            clock.compare(
                far,
                published,
                UnixMillis::new(published.get() + limit + 1),
                SYNC_ROUND_TRIP_LIMIT
            ),
            Comparison::NotCompared {
                wall_ms: 10_001,
                elapsed_ms: 10_000
            }
        );
        assert!(clock.synced());
        let arrived = UnixMillis::new(published.get() + 1_000);
        assert_eq!(
            clock.compare(
                far,
                published,
                arrived,
                SYNC_ROUND_TRIP_LIMIT + Duration::from_millis(1)
            ),
            Comparison::NotCompared {
                wall_ms: 1_000,
                elapsed_ms: 10_001
            }
        );
        assert!(clock.synced());
        // An off clock stays off when a slow acknowledgement would have put it inside.
        assert_eq!(
            clock.compare(far, published, arrived, PROMPT),
            Comparison::Off {
                offset_ms: i64::try_from(HOUR).unwrap()
            }
        );
        assert!(!clock.synced());
        assert_eq!(
            clock.compare(
                published,
                published,
                UnixMillis::new(published.get() + limit + 1),
                Duration::from_millis(limit + 1)
            ),
            Comparison::NotCompared {
                wall_ms: 10_001,
                elapsed_ms: 10_001
            }
        );
        assert!(!clock.synced());
    }

    /// A wall clock that steps between the publish and the acknowledgement: forward, the
    /// window would hold a cloud time the clock has left behind; backward, the wall-clock round
    /// trip is negative, or short while the real one is long. None is compared; each
    /// `server_time` here is the true time and would fall inside the window.
    #[test]
    fn a_wall_clock_step_between_the_publish_and_the_ack_is_never_compared() {
        const HOUR: u64 = 3_600_000;
        let (clock, _) = manual(0, missing_flag());
        let published = UnixMillis::new(10 * HOUR);
        // A 1 s round trip with a step an hour forward: the clock is an hour ahead at the ack.
        let true_arrival = UnixMillis::new(published.get() + 1_000);
        assert_eq!(
            clock.compare(
                true_arrival,
                published,
                UnixMillis::new(true_arrival.get() + HOUR),
                Duration::from_secs(1)
            ),
            Comparison::NotCompared {
                wall_ms: 3_601_000,
                elapsed_ms: 1_000
            }
        );
        // A 1 s round trip with a step 6 s back: the wall clock ran backward across it.
        assert_eq!(
            clock.compare(
                true_arrival,
                published,
                UnixMillis::new(published.get() - 5_000),
                Duration::from_secs(1)
            ),
            Comparison::NotCompared {
                wall_ms: -5_000,
                elapsed_ms: 1_000
            }
        );
        // An hour's round trip with a step back of an hour less 5 s: 5 s on the wall clock, but
        // the clock is nearly an hour behind at the ack. The monotonic clock tells.
        assert_eq!(
            clock.compare(
                UnixMillis::new(published.get() + 100),
                published,
                UnixMillis::new(published.get() + 5_000),
                Duration::from_millis(HOUR)
            ),
            Comparison::NotCompared {
                wall_ms: 5_000,
                elapsed_ms: HOUR
            }
        );
        assert!(!clock.synced(), "nothing was compared: the flag decides");
    }

    #[tokio::test(start_paused = true)]
    async fn a_backward_step_never_makes_the_age_clock_run_backward() {
        let (clock, wall) = manual(10_000_000, missing_flag());
        let age = AgeClock::new(clock);
        assert_eq!(age.now(), 10_000_000);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(age.now(), 10_060_000);
        // The wall clock steps back an hour: the age clock keeps counting real time.
        wall.store(10_060_000 - 3_600_000, Ordering::Relaxed);
        assert_eq!(age.now(), 10_060_000);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(age.now(), 10_061_000);
        // A forward step (a correction) counts as elapsed time.
        wall.store(20_000_000, Ordering::Relaxed);
        assert_eq!(age.now(), 20_000_000);
    }

    /// Independent review of Laysi/trustiot-v2#90 (2026-10-28), finding 2: a wrong time source
    /// steps the wall clock 40 days ahead and it is corrected a minute later. The age clock
    /// must not run backward by 39 days, or a batch formed during the step would look younger
    /// than it is and outlive the 30-day limit.
    #[tokio::test(start_paused = true)]
    async fn a_forward_step_that_is_corrected_never_makes_the_age_clock_run_backward() {
        const DAY: i64 = 86_400_000;
        let start = 1_790_000_000_000;
        let (clock, wall) = manual(start, missing_flag());
        let age = AgeClock::new(clock);
        wall.store(start + 40 * DAY, Ordering::Relaxed);
        let during = age.now();
        assert_eq!(during, u64::try_from(start + 40 * DAY).unwrap());
        tokio::time::advance(Duration::from_secs(60)).await;
        wall.store(start + 60_000, Ordering::Relaxed);
        let after = age.now();
        assert_eq!(
            after,
            during + 60_000,
            "the age clock ran backward by {} ms after the correction",
            i128::from(during) + 60_000 - i128::from(after)
        );
        // A clone shares what the clock has returned.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(age.clone().now(), during + 61_000);
    }

    /// Codex review (fourth round): two callers interleaved around a forward step. Caller A has
    /// read the clocks before the step and is paused before storing its value; meanwhile the
    /// wall clock steps 40 days forward and back. No other call may complete in between and
    /// hand out the stepped value while A then returns the earlier one: a call made while A
    /// is under way waits for it, so the values come out in the order the calls complete.
    #[test]
    fn a_caller_paused_across_a_forward_step_never_returns_an_earlier_value_than_one_already_returned()
     {
        use std::sync::mpsc;
        const DAY: i64 = 86_400_000;
        const AWAIT: Duration = Duration::from_secs(60);
        let start = 1_790_000_000_000;
        let (clock, wall) = manual(start, missing_flag());
        let mut age = AgeClock::new(clock);
        let (paused_tx, paused_rx) = mpsc::channel::<()>();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let armed = std::sync::atomic::AtomicBool::new(true);
        age.pause = Some(Arc::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                paused_tx.send(()).unwrap();
                resume_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(AWAIT)
                    .expect("the test to let caller A go on");
            }
        }));
        let caller = age.clone();
        let a = std::thread::spawn(move || caller.now());
        paused_rx
            .recv_timeout(AWAIT)
            .expect("caller A to read the clocks and pause");
        // A has read the clocks; the wall clock steps 40 days forward, and caller B reads.
        wall.store(start + 40 * DAY, Ordering::SeqCst);
        let b = age.now_if_free();
        // The step is corrected; A stores and returns.
        wall.store(start, Ordering::SeqCst);
        resume_tx.send(()).unwrap();
        let a = a.join().unwrap();
        let c = age.now();
        if let Some(b) = b {
            assert!(
                a >= b,
                "caller B got {b} and completed first, then caller A returned the earlier {a}"
            );
        }
        assert!(c >= a, "a later call got {c}, earlier than {a}");
    }

    /// A wall clock the test sets that steps `step` ms forward right after the read that
    /// follows arming it.
    struct SteppingWall {
        time: Arc<AtomicI64>,
        armed: Arc<std::sync::atomic::AtomicBool>,
        step: i64,
    }

    impl WallClock for SteppingWall {
        fn now(&self) -> UnixMillis {
            let now = self.time.load(Ordering::SeqCst);
            if self.armed.swap(false, Ordering::SeqCst) {
                self.time.store(now + self.step, Ordering::SeqCst);
            }
            UnixMillis::new(u64::try_from(now).unwrap_or(0))
        }
    }

    /// Codex review before the commit (after the move to main `371572a`): `raise_floor` read
    /// the clocks twice, before and after the raise, so a wall clock step between the two
    /// readings (an NTP correction) went into the result, which `Buffer::raise_floor` adds to
    /// the formation time of earlier batches: they looked younger by the step and could outlive
    /// the 30 days. Here the wall clock steps an hour forward right after the first read inside
    /// `raise_floor`, which raises the floor 10 minutes above the clock: the raise moved the
    /// clock 10 minutes, not an hour.
    #[tokio::test(start_paused = true)]
    async fn a_wall_clock_step_during_a_floor_raise_is_not_counted_as_the_raise() {
        const MINUTE: u64 = 60_000;
        const HOUR: i64 = 3_600_000;
        let start: i64 = 1_790_000_000_000;
        let time = Arc::new(AtomicI64::new(start));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock = GatewayClock::new(
            SteppingWall {
                time: Arc::clone(&time),
                armed: Arc::clone(&armed),
                step: HOUR,
            },
            missing_flag(),
        );
        let age = AgeClock::new(clock);
        let start = u64::try_from(start).unwrap();
        armed.store(true, Ordering::SeqCst);
        assert_eq!(
            age.raise_floor(start + 10 * MINUTE),
            10 * MINUTE,
            "only the raise counts, not the wall clock's step"
        );
        assert!(!armed.load(Ordering::SeqCst), "the wall clock was read");
        // The step itself counts as elapsed time for what comes after, as always.
        assert_eq!(age.now(), start + 3_600_000);
    }

    #[tokio::test(start_paused = true)]
    async fn the_floor_rises_to_the_newest_stored_batch() {
        // The wall clock at start is behind the newest batch on disk (a backward step across a
        // restart): the batch's age starts at 0, not at a negative value that would extend it.
        let (clock, _) = manual(5_000, missing_flag());
        let age = AgeClock::new(clock);
        assert_eq!(
            age.raise_floor(9_000),
            4_000,
            "how far the raise moved the clock"
        );
        assert_eq!(age.now(), 9_000);
        assert_eq!(age.raise_floor(1_000), 0);
        assert_eq!(age.now(), 9_000, "the floor never falls");
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(age.clone().now(), 9_500);
    }
}
