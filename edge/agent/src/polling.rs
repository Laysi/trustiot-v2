//! Polling one device on a fixed schedule, with bounded waits, reconnect backoff and a watchdog
//! (rebuild checklist F4; Laysi/trustiot#151, Laysi/trustiot#159, Laysi/trustiot#164).
//!
//! [`run`] polls a device with its [`DeviceReader`] until the caller's event handler says stop,
//! or until the caller drops the future:
//!
//! - **Fixed rate.** Poll `k` is due at `start + k × period`, computed from the start and not
//!   from the previous poll, so the schedule does not drift however long polls take. A poll that
//!   runs past the next due time skips the polls it overlapped instead of catching up in a burst
//!   ([`Polled::missed`]).
//! - **Every wait is bounded.** Connecting waits at most the connect timeout and each request at
//!   most the request timeout ([`Settings`]); each request is also bounded by the watchdog, and
//!   between polls the loop only sleeps until the next due poll.
//! - **Backoff.** After a poll in which the connection failed (it could not be opened, it was
//!   lost, the stream was not Modbus, or the watchdog closed it), the next poll waits at least
//!   [`Settings::backoff_initial`], doubling after each such poll up to
//!   [`Settings::backoff_max`]. Only a poll that read values (at least one point read and
//!   decoded) resets it: opening a connection does not, and neither does an exception response,
//!   so a device that accepts connections and then closes them is not reconnected at the poll
//!   rate.
//! - **Watchdog.** A device that keeps its connection open but stops answering (a converter
//!   whose serial side hangs, a peer that vanished without FIN or RST) only times out, and a
//!   connection is kept after a timeout, so polling would never recover by itself. The watchdog
//!   closes the connection once the device has not answered for [`Settings::watchdog`], counted
//!   from the first request it left unanswered on an open connection. The request that ends
//!   unanswered after that point fails as [`ReadError::ConnectionLost`] (a request still waiting
//!   is cut off at the watchdog's deadline), the rest of the poll is not sent, and the next poll,
//!   after the backoff, opens a new connection. A request is never cut off before its poll has
//!   run for [`Settings::minimum_watchdog`], the longest one request can take, so a device polled
//!   less often than the watchdog still gets a whole request each poll.

use std::future::Future;
use std::ops::ControlFlow;
use std::time::Duration;

use protocol::config::{Device, MIN_POLL_INTERVAL_MS};
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::modbus::pdu::Read;
use crate::modbus::tcp::TcpConnection;
use crate::modbus::{
    Clock, Connection, DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, DeviceReader, Exchange,
    Poll, ReadError,
};

/// The shortest request or connect timeout accepted.
pub const MIN_TIMEOUT: Duration = Duration::from_millis(100);
/// The longest request or connect timeout accepted.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a device may stay silent before the watchdog closes its connection, unless the
/// settings say otherwise.
pub const DEFAULT_WATCHDOG: Duration = Duration::from_secs(60);
/// The longest watchdog accepted.
pub const MAX_WATCHDOG: Duration = Duration::from_secs(60 * 60);
/// The first wait after a failed connection, unless the settings say otherwise.
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// The longest wait after failed connections, unless the settings say otherwise.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The shortest first wait after a failed connection accepted.
pub const MIN_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
/// The longest first wait after a failed connection accepted.
pub const MAX_BACKOFF_INITIAL: Duration = Duration::from_secs(60);
/// The longest wait after failed connections accepted.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// Why a poll's remaining requests were not sent after the watchdog fired. [`DeviceReader`]
/// already skips them after the [`ReadError::ConnectionLost`] the watchdog reports; this covers
/// any other caller of the connection.
const NOT_SENT_AFTER_WATCHDOG: &str = "not sent: the watchdog closed the connection";

/// How the agent polls every device; the same for all devices. Defaults are those of the rebuild
/// checklist (F4); [`Settings::validate`] states the accepted ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// How long a request waits for its response. Default 5 s, 100 ms–60 s.
    pub request_timeout: Duration,
    /// How long connecting waits, name resolution included; the caller gives it to the
    /// connection ([`TcpConnection::new`]). Default 10 s, 100 ms–60 s.
    pub connect_timeout: Duration,
    /// How long a device may leave requests unanswered on an open connection before the watchdog
    /// closes it. Default 60 s, from [`Settings::minimum_watchdog`] to 1 h.
    pub watchdog: Duration,
    /// The wait after the first poll whose connection failed. Default 1 s, 100 ms–60 s.
    pub backoff_initial: Duration,
    /// The longest wait after polls whose connection failed. Default 60 s, from
    /// `backoff_initial` to 1 h.
    pub backoff_max: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            watchdog: DEFAULT_WATCHDOG,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
        }
    }
}

impl Settings {
    /// The longest one request can take: connecting, a request on the old connection that fails,
    /// and the same request again on a new one ([`TcpConnection`] sends a request once more when
    /// a connection it kept open turns out to be lost). The watchdog may not be shorter, and it
    /// never cuts off a request before its poll has run this long.
    pub fn minimum_watchdog(&self) -> Duration {
        self.connect_timeout
            .saturating_add(self.request_timeout.saturating_mul(2))
    }

    /// Checks every setting against its range; the error names the first setting outside it.
    pub fn validate(&self) -> Result<(), SettingsError> {
        within(
            "request_timeout",
            self.request_timeout,
            MIN_TIMEOUT,
            MAX_TIMEOUT,
        )?;
        within(
            "connect_timeout",
            self.connect_timeout,
            MIN_TIMEOUT,
            MAX_TIMEOUT,
        )?;
        within(
            "watchdog",
            self.watchdog,
            self.minimum_watchdog(),
            MAX_WATCHDOG,
        )?;
        within(
            "backoff_initial",
            self.backoff_initial,
            MIN_BACKOFF_INITIAL,
            MAX_BACKOFF_INITIAL,
        )?;
        within(
            "backoff_max",
            self.backoff_max,
            self.backoff_initial,
            MAX_BACKOFF,
        )
    }
}

fn within(
    setting: &'static str,
    value: Duration,
    min: Duration,
    max: Duration,
) -> Result<(), SettingsError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(SettingsError {
            setting,
            value,
            min,
            max,
        })
    }
}

/// A setting outside its range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsError {
    /// The setting's name, as in [`Settings`].
    pub setting: &'static str,
    /// Its value.
    pub value: Duration,
    /// The shortest value accepted.
    pub min: Duration,
    /// The longest value accepted.
    pub max: Duration,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} is {} ms, outside {}..={} ms",
            self.setting,
            self.value.as_millis(),
            self.min.as_millis(),
            self.max.as_millis()
        )
    }
}

impl std::error::Error for SettingsError {}

/// How often `device` is polled: its `poll_interval_ms`, which a validated configuration keeps
/// within 100 ms–1 day.
pub fn poll_period(device: &Device) -> Duration {
    Duration::from_millis(u64::from(device.poll_interval_ms.max(MIN_POLL_INTERVAL_MS)))
}

/// A connection the watchdog can close, so that the next read opens a new one.
pub trait Disconnect {
    /// Closes the connection if one is open; the next read opens a new one. It must not wait
    /// for the device: the watchdog calls it because the device stopped answering.
    fn disconnect(&mut self) -> impl Future<Output = ()> + Send;
}

impl Disconnect for TcpConnection {
    async fn disconnect(&mut self) {
        self.close();
    }
}

/// What the polling loop reports to its caller.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A poll ended.
    Polled(Polled),
    /// The loop sleeps until the next poll is due.
    Waiting(Waiting),
}

/// One poll, and how it went.
#[derive(Clone, Debug, PartialEq)]
pub struct Polled {
    /// When the poll was due: the start of the loop plus a whole number of periods.
    pub scheduled: Instant,
    /// When it started; at the due time unless the runtime was late.
    pub started: Instant,
    /// When it ended.
    pub ended: Instant,
    /// Due polls skipped right before this one, because the poll before ran past them or the
    /// backoff waited past them.
    pub missed: u64,
    /// What it read.
    pub poll: Poll,
    /// Set when the watchdog fired during this poll and closed the connection.
    pub watchdog: Option<WatchdogFired>,
}

/// The watchdog closed the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchdogFired {
    /// When the first request the device left unanswered was sent.
    pub silent_since: Instant,
    /// When the watchdog closed the connection.
    pub at: Instant,
}

/// The loop sleeps until the next due poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// When the next poll starts.
    pub until: Instant,
    /// Polls in a row whose connection failed, since the last poll that read values.
    pub failures: u32,
    /// The backoff after the last poll: zero when its connection did not fail. The loop waits
    /// for the first due poll after it.
    pub backoff: Duration,
}

/// Polls the device of `reader` every `period` (normally [`poll_period`]) through `connection`
/// until `on_event` returns [`ControlFlow::Break`], stamping readings with `clock`. `settings`
/// should have passed [`Settings::validate`]; the connection waits at most its own connect
/// timeout, which the caller set from [`Settings::connect_timeout`].
///
/// Dropping the future stops polling at once; the connection stays usable
/// ([`Connection::read`]).
pub async fn run<C, K, F>(
    reader: &DeviceReader,
    period: Duration,
    connection: &mut C,
    clock: &K,
    settings: &Settings,
    mut on_event: F,
) where
    C: Connection + Disconnect + Send,
    K: Clock,
    F: FnMut(Event) -> ControlFlow<()>,
{
    let start = Instant::now();
    let mut schedule = Schedule::new(start, period);
    let mut backoff = Backoff::new(settings.backoff_initial, settings.backoff_max);
    let mut watch = Watch::new(settings, start);
    let mut earliest = start;
    let mut delay = Duration::ZERO;
    loop {
        let (scheduled, missed) = schedule.next(earliest);
        if scheduled > Instant::now() {
            let waiting = Waiting {
                until: scheduled,
                failures: backoff.failures,
                backoff: delay,
            };
            if on_event(Event::Waiting(waiting)).is_break() {
                return;
            }
            sleep_until(scheduled).await;
        }
        let started = Instant::now();
        watch.begin_poll(started);
        let poll = reader
            .poll(
                &mut Observed {
                    connection: &mut *connection,
                    watch: &mut watch,
                },
                clock,
                settings.request_timeout,
            )
            .await;
        let ended = Instant::now();
        // Values mean at least one point read and decoded: a response whose points all fail to
        // decode does not reset the backoff.
        if !poll.readings.is_empty() {
            backoff.reset();
        }
        delay = if watch.connection_failed {
            backoff.failed()
        } else {
            Duration::ZERO
        };
        earliest = after(ended, delay);
        let polled = Polled {
            scheduled,
            started,
            ended,
            missed,
            poll,
            watchdog: watch.fired,
        };
        if on_event(Event::Polled(polled)).is_break() {
            return;
        }
    }
}

/// Due times at a fixed rate: tick `k` is `start + k × period`.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    start: Instant,
    period: Duration,
    /// The first tick not handed out yet.
    next: u64,
}

impl Schedule {
    fn new(start: Instant, period: Duration) -> Self {
        let period = if period.is_zero() {
            Duration::from_millis(u64::from(MIN_POLL_INTERVAL_MS))
        } else {
            period
        };
        Self {
            start,
            period,
            next: 0,
        }
    }

    fn due(&self, tick: u64) -> Instant {
        let nanos = self.period.as_nanos().saturating_mul(u128::from(tick));
        after(self.start, duration_from_nanos(nanos))
    }

    /// The first tick due at or after `earliest` and not handed out yet, and how many ticks
    /// before it are skipped.
    fn next(&mut self, earliest: Instant) -> (Instant, u64) {
        let elapsed = earliest.saturating_duration_since(self.start).as_nanos();
        let first_due = u64::try_from(elapsed.div_ceil(self.period.as_nanos())).unwrap_or(u64::MAX);
        let tick = first_due.max(self.next);
        let missed = tick - self.next;
        self.next = tick.saturating_add(1);
        (self.due(tick), missed)
    }
}

fn duration_from_nanos(nanos: u128) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let seconds = u64::try_from(nanos / NANOS_PER_SECOND).unwrap_or(u64::MAX);
    let subsecond = u32::try_from(nanos % NANOS_PER_SECOND).unwrap_or(0);
    Duration::new(seconds, subsecond)
}

/// The instant `duration` after `at`; a year after it when that does not fit the clock.
fn after(at: Instant, duration: Duration) -> Instant {
    at.checked_add(duration)
        .unwrap_or_else(|| at + Duration::from_secs(365 * 24 * 60 * 60))
}

/// The wait before the next poll after polls whose connection failed: doubling from `initial`
/// up to `max`, back to `initial` once a poll read values.
#[derive(Debug)]
struct Backoff {
    initial: Duration,
    max: Duration,
    /// Polls in a row whose connection failed, since the last poll that read values.
    failures: u32,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            failures: 0,
        }
    }

    /// Counts a poll whose connection failed and returns the wait before the next one.
    fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let doublings = self.failures.saturating_sub(1).min(31);
        self.initial
            .saturating_mul(1_u32 << doublings)
            .min(self.max)
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

/// The watchdog's view of the device, and what the poll in progress has seen so far.
#[derive(Debug)]
struct Watch {
    watchdog: Duration,
    minimum: Duration,
    /// When the first request the device has left unanswered on an open connection, since it
    /// last answered, was sent. `None` while nothing is unanswered.
    silent_since: Option<Instant>,
    poll_started: Instant,
    /// The poll's connection failed, or the watchdog closed it.
    connection_failed: bool,
    /// The watchdog fired during the poll.
    fired: Option<WatchdogFired>,
}

impl Watch {
    fn new(settings: &Settings, now: Instant) -> Self {
        Self {
            watchdog: settings.watchdog,
            minimum: settings.minimum_watchdog(),
            silent_since: None,
            poll_started: now,
            connection_failed: false,
            fired: None,
        }
    }

    fn begin_poll(&mut self, now: Instant) {
        self.poll_started = now;
        self.connection_failed = false;
        self.fired = None;
    }
}

/// The poll's connection, seen by the watchdog: it bounds every request by the watchdog's
/// deadline, notes answers and failed connections, and closes the connection when the device has
/// been silent for too long.
struct Observed<'a, C> {
    connection: &'a mut C,
    watch: &'a mut Watch,
}

impl<C: Connection + Disconnect + Send> Connection for Observed<'_, C> {
    async fn read(&mut self, unit: u8, read: Read, timeout: Duration) -> Exchange {
        let watch = &mut *self.watch;
        if watch.fired.is_some() {
            return Exchange {
                result: Err(ReadError::ConnectionLost(
                    NOT_SENT_AFTER_WATCHDOG.to_owned(),
                )),
                unit_mismatches: 0,
            };
        }
        let silent_since = *watch.silent_since.get_or_insert_with(Instant::now);
        let deadline =
            after(silent_since, watch.watchdog).max(after(watch.poll_started, watch.minimum));
        // Dropping a read that is still under way is safe (`Connection::read`); it cannot be
        // under way past its own timeout unless something without a timeout holds it up.
        let mut exchange = timeout_at(deadline, self.connection.read(unit, read, timeout))
            .await
            .unwrap_or(Exchange {
                result: Err(ReadError::Timeout),
                unit_mismatches: 0,
            });
        match &exchange.result {
            // The device answered, with data or with an exception.
            Ok(_) | Err(ReadError::Exception(_)) => watch.silent_since = None,
            // The connection is gone, so nothing is left to watch on it; the backoff handles it.
            Err(error) if error.ends_poll() => {
                watch.connection_failed = true;
                watch.silent_since = None;
            }
            // A timeout or a response that does not fit: the device is still silent.
            Err(_) => {}
        }
        let now = Instant::now();
        if let Some(since) = watch.silent_since
            && now >= after(since, watch.watchdog)
        {
            self.connection.disconnect().await;
            watch.silent_since = None;
            watch.connection_failed = true;
            watch.fired = Some(WatchdogFired {
                silent_since: since,
                at: now,
            });
            exchange.result = Err(ReadError::ConnectionLost(format!(
                "no answer for {} ms: the watchdog closed the connection",
                now.saturating_duration_since(since).as_millis()
            )));
        }
        exchange
    }
}

#[cfg(test)]
mod tests {
    use protocol::config::{
        ByteOrder, DataPoint, DataType, DeviceModel, FunctionCode, Host, Transport,
    };
    use protocol::{DeviceId, ModelName, PointKey, UnixMillis};

    use super::*;
    use crate::modbus::Timestamp;
    use crate::modbus::pdu::Data;

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp {
                at: UnixMillis::new(1_760_054_400_000),
                synced: true,
            }
        }
    }

    /// A reader for one device with one holding register, so one request per poll.
    fn reader() -> DeviceReader {
        let model = DeviceModel {
            name: ModelName::new("meter").unwrap(),
            points: vec![DataPoint {
                key: PointKey::new("power").unwrap(),
                function_code: FunctionCode::ReadHoldingRegisters,
                address: 0,
                data_type: DataType::U16,
                byte_order: ByteOrder::Abcd,
                length: None,
                scale: None,
                offset: None,
            }],
        };
        let device = Device {
            id: DeviceId::new("meter-1").unwrap(),
            model: model.name.clone(),
            transport: Transport::ModbusTcp,
            host: Host::new("192.0.2.10").unwrap(),
            port: 502,
            unit_id: 1,
            poll_interval_ms: 1000,
        };
        DeviceReader::new(&device, &model).unwrap()
    }

    /// Answers every read with a value after the next of `durations`, in a cycle.
    struct Timed {
        durations: std::iter::Cycle<std::vec::IntoIter<Duration>>,
    }

    impl Connection for Timed {
        async fn read(&mut self, _unit: u8, _read: Read, _timeout: Duration) -> Exchange {
            tokio::time::sleep(self.durations.next().unwrap_or_default()).await;
            Exchange {
                result: Ok(Data::Registers(vec![1])),
                unit_mismatches: 0,
            }
        }
    }

    impl Disconnect for Timed {
        async fn disconnect(&mut self) {}
    }

    /// Rebuild checklist F4: the schedule does not drift. One simulated hour on the paused tokio
    /// clock (no sockets, so the clock may advance by itself), with polls of varied length, some
    /// longer than the period.
    #[tokio::test(start_paused = true)]
    async fn the_schedule_does_not_drift_over_an_hour() {
        const PERIOD: Duration = Duration::from_secs(1);
        const HOUR: Duration = Duration::from_secs(60 * 60);
        // The drift allowed: the tokio timer's resolution, the most a sleep can wake after its
        // deadline. It does not grow with the number of polls.
        const MAX_DRIFT: Duration = Duration::from_millis(1);
        let durations = [130, 470, 999, 1000, 1500, 20, 2600, 0, 640]
            .map(Duration::from_millis)
            .to_vec();
        let mut connection = Timed {
            durations: durations.into_iter().cycle(),
        };
        let start = Instant::now();
        let mut polls: Vec<Polled> = Vec::new();
        run(
            &reader(),
            PERIOD,
            &mut connection,
            &FixedClock,
            &Settings::default(),
            |event| {
                if let Event::Polled(polled) = event {
                    let done = polled.scheduled >= start + HOUR;
                    polls.push(polled);
                    if done {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            },
        )
        .await;

        let mut ticks = 0;
        let mut previous_end = start;
        for polled in &polls {
            let offset = polled.scheduled - start;
            assert_eq!(
                offset.as_nanos() % PERIOD.as_nanos(),
                0,
                "a poll due off the schedule, {offset:?} after the start"
            );
            assert!(
                polled.started - polled.scheduled <= MAX_DRIFT,
                "the poll due {offset:?} after the start began {:?} late",
                polled.started - polled.scheduled
            );
            assert!(polled.started >= previous_end, "polls never overlap");
            assert_eq!(polled.poll.readings.len(), 1);
            previous_end = polled.ended;
            ticks += 1 + polled.missed;
        }
        let last = polls.last().unwrap();
        let last_tick =
            u64::try_from((last.scheduled - start).as_nanos() / PERIOD.as_nanos()).unwrap();
        assert!(last_tick >= 3600, "the polls cover one hour");
        assert_eq!(ticks, last_tick + 1, "every due poll ran or was skipped");
        assert!(
            polls.iter().any(|polled| polled.missed > 0),
            "polls longer than the period skipped due polls"
        );
    }

    #[test]
    fn settings_have_defaults_and_ranges() {
        let defaults = Settings::default();
        assert_eq!(
            defaults,
            Settings {
                request_timeout: Duration::from_secs(5),
                connect_timeout: Duration::from_secs(10),
                watchdog: Duration::from_secs(60),
                backoff_initial: Duration::from_secs(1),
                backoff_max: Duration::from_secs(60),
            }
        );
        assert_eq!(defaults.validate(), Ok(()));
        assert_eq!(defaults.minimum_watchdog(), Duration::from_secs(20));
        let shortest = Settings {
            watchdog: Duration::from_secs(20),
            backoff_max: defaults.backoff_initial,
            ..defaults
        };
        assert_eq!(shortest.validate(), Ok(()));

        let millis = Duration::from_millis;
        let outside = [
            (
                "request_timeout",
                Settings {
                    request_timeout: millis(99),
                    ..defaults
                },
            ),
            (
                "request_timeout",
                Settings {
                    request_timeout: millis(60_001),
                    ..defaults
                },
            ),
            (
                "connect_timeout",
                Settings {
                    connect_timeout: Duration::ZERO,
                    ..defaults
                },
            ),
            (
                "connect_timeout",
                Settings {
                    connect_timeout: millis(60_001),
                    ..defaults
                },
            ),
            (
                "watchdog",
                Settings {
                    watchdog: millis(19_999),
                    ..defaults
                },
            ),
            (
                "watchdog",
                Settings {
                    watchdog: millis(3_600_001),
                    ..defaults
                },
            ),
            (
                "backoff_initial",
                Settings {
                    backoff_initial: millis(99),
                    ..defaults
                },
            ),
            (
                "backoff_initial",
                Settings {
                    backoff_initial: millis(60_001),
                    ..defaults
                },
            ),
            (
                "backoff_max",
                Settings {
                    backoff_max: millis(999),
                    ..defaults
                },
            ),
            (
                "backoff_max",
                Settings {
                    backoff_max: millis(3_600_001),
                    ..defaults
                },
            ),
        ];
        for (setting, settings) in outside {
            let error = settings.validate().unwrap_err();
            assert_eq!(error.setting, setting, "{error}");
        }
        assert_eq!(
            Settings {
                watchdog: millis(19_999),
                ..defaults
            }
            .validate()
            .unwrap_err()
            .to_string(),
            "watchdog is 19999 ms, outside 20000..=3600000 ms"
        );
    }

    #[test]
    fn the_backoff_doubles_up_to_its_maximum() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let delays: Vec<u64> = (0..8).map(|_| backoff.failed().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
        backoff.reset();
        assert_eq!(backoff.failed(), Duration::from_secs(1));
        backoff.failures = u32::MAX;
        assert_eq!(backoff.failed(), Duration::from_secs(60), "no overflow");
    }

    #[test]
    fn due_times_are_whole_periods_from_the_start() {
        let start = Instant::now();
        let mut schedule = Schedule::new(start, Duration::from_millis(1500));
        assert_eq!(schedule.next(start), (start, 0));
        // Asked again at the same time: the next tick, never the same one twice.
        assert_eq!(
            schedule.next(start),
            (start + Duration::from_millis(1500), 0)
        );
        // A poll that ended 4.2 s in: the tick at 3 s is skipped, 4.5 s is the first due after it.
        assert_eq!(
            schedule.next(start + Duration::from_millis(4200)),
            (start + Duration::from_millis(4500), 1)
        );
        // Ended exactly on a tick: that tick runs at once.
        assert_eq!(
            schedule.next(start + Duration::from_secs(6)),
            (start + Duration::from_secs(6), 0)
        );
        let zero = Schedule::new(start, Duration::ZERO);
        assert_eq!(zero.period, Duration::from_millis(100));
    }
}
