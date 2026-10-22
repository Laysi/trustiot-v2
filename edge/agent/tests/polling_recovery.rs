//! The polling loop against testkit's fake Modbus endpoint (rebuild checklist F4): the watchdog
//! recovers a device that keeps its connection open but never answers (Laysi/trustiot#159,
//! Laysi/trustiot#164) and one whose connection went half-open, and the reconnect backoff
//! resets only after a poll read values. Each device reads through the shared connection of its
//! endpoint, as the agent sets it up.
//!
//! The clock moves only when the test advances it (`testkit::clock::manual`), so every timeout
//! fires in order, never because the machine is slow. The test advances it only while the loop
//! waits for a timer: after the endpoint received a request, after the loop reported a poll whose
//! successor is due at once (it sends that request before it yields), or after it reported that
//! it sleeps until the next due poll.

use std::ops::ControlFlow;
use std::time::Duration;

use agent::modbus::endpoint::{Endpoints, SharedConnection};
use agent::modbus::{Clock, DeviceReader, Timestamp};
use agent::polling::{self, Event, Polled, Settings, Waiting};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::health::FailureKind;
use protocol::{ConfigVersion, DeviceId, Message, ModelName, PointKey, UnixMillis, V1, Value};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, Close, ConnectionId, ConnectionState, FakeModbus, Fault};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::{Instant, advance};

/// The device's poll interval.
const PERIOD: Duration = Duration::from_secs(1);
/// The watchdog the tests configure: shorter than the default, still above the minimum.
const WATCHDOG: Duration = Duration::from_secs(30);
/// What the device's register holds.
const VALUE: u16 = 2301;

fn settings() -> Settings {
    let settings = Settings {
        watchdog: WATCHDOG,
        ..Settings::default()
    };
    settings.validate().expect("the test settings are valid");
    settings
}

/// A synchronized clock that always reads the same time.
struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(1_760_054_400_000),
            synced: true,
        }
    }
}

/// A device on `endpoint` with one holding register, after the configuration passed the
/// validation the gateway applies: its reader, and the connection of its endpoint.
fn device(endpoint: &FakeModbus, settings: &Settings) -> (DeviceReader, SharedConnection) {
    let model = DeviceModel {
        name: ModelName::new("meter").expect("a valid model name"),
        points: vec![DataPoint {
            key: PointKey::new("power").expect("a valid key"),
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
        id: DeviceId::new("meter-1").expect("a valid device ID"),
        model: model.name.clone(),
        transport: Transport::ModbusTcp,
        host: Host::new("127.0.0.1").expect("a valid host"),
        port: endpoint.address().port(),
        unit_id: 1,
        poll_interval_ms: 1000,
    };
    let config = DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(1).expect("a nonzero version"),
        models: vec![model.clone()],
        devices: vec![device.clone()],
    };
    config.validate().expect("the test configuration is valid");
    assert_eq!(polling::poll_period(&device), PERIOD);
    let reader = DeviceReader::new(&device, &model).expect("a plan for a validated model");
    let connection = Endpoints::new(settings.connect_timeout).connection(&device);
    (reader, connection)
}

async fn next_event(events: &mut UnboundedReceiver<Event>) -> Event {
    bounded("the next event of the polling loop", events.recv())
        .await
        .expect("the polling loop still runs")
}

async fn next_polled(events: &mut UnboundedReceiver<Event>) -> Polled {
    match next_event(events).await {
        Event::Polled(polled) => polled,
        other => panic!("expected a poll, got {other:?}"),
    }
}

async fn next_waiting(events: &mut UnboundedReceiver<Event>) -> Waiting {
    match next_event(events).await {
        Event::Waiting(waiting) => waiting,
        other => panic!("expected the loop to wait, got {other:?}"),
    }
}

/// The failure kinds of a poll's points.
fn failure_kinds(polled: &Polled) -> Vec<FailureKind> {
    polled
        .poll
        .failed
        .iter()
        .map(|failed| failed.failure.kind.clone())
        .collect()
}

fn values(polled: &Polled) -> Vec<Value> {
    polled
        .poll
        .readings
        .iter()
        .map(|reading| reading.sample.value.clone())
        .collect()
}

/// What happened from the start until the device answered again.
struct Recovery {
    start: Instant,
    /// The polls the device did not answer; the last one is where the watchdog fired.
    silent: Vec<Polled>,
    /// The backoff after the watchdog.
    waiting: Waiting,
    /// The first poll after it.
    recovered: Polled,
}

/// Polls a device whose first connection gets `fault` for every request, until a poll after
/// the watchdog fired reads its value again.
async fn recover_from(endpoint: &FakeModbus, fault: Fault) -> Recovery {
    endpoint.set_holding_registers(1, 0, &[VALUE]);
    endpoint.inject(fault.connection(ConnectionId(0)));
    let settings = settings();
    let (reader, mut connection) = device(endpoint, &settings);
    let (sender, mut events) = unbounded_channel();
    let mut fired = false;
    let start = Instant::now();
    let polling = polling::run(
        &reader,
        PERIOD,
        &mut connection,
        &TestClock,
        &settings,
        |event| {
            let recovered = match &event {
                Event::Polled(polled) => {
                    let recovered = fired && !polled.poll.readings.is_empty();
                    fired |= polled.watchdog.is_some();
                    recovered
                }
                Event::Waiting(_) => false,
            };
            sender.send(event).expect("the test receives every event");
            if recovered {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    let drive = async {
        // The first poll opens the connection; once its request arrived, its timeout runs.
        endpoint.wait_for_requests(1).await;
        let mut silent = Vec::new();
        loop {
            advance(settings.request_timeout).await;
            let polled = next_polled(&mut events).await;
            let watchdog = polled.watchdog.is_some();
            silent.push(polled);
            if watchdog {
                break;
            }
            assert!(silent.len() < 20, "the watchdog never fired");
        }
        let waiting = next_waiting(&mut events).await;
        advance(waiting.until - Instant::now()).await;
        let recovered = next_polled(&mut events).await;
        (silent, waiting, recovered)
    };
    let ((), (silent, waiting, recovered)) =
        bounded("polling until the device answers again", async {
            tokio::join!(polling, drive)
        })
        .await;
    Recovery {
        start,
        silent,
        waiting,
        recovered,
    }
}

/// What every recovery has in common: the polls time out back to back, the watchdog closes
/// the connection exactly `WATCHDOG` after the first unanswered request, and after the first
/// backoff a poll on a new connection reads the value.
fn assert_recovered(endpoint: &FakeModbus, recovery: &Recovery) {
    let settings = settings();
    let Recovery {
        start,
        silent,
        waiting,
        recovered,
    } = recovery;
    let (fired_poll, timed_out) = silent.split_last().expect("at least one poll");
    for (index, polled) in (0_u32..).zip(timed_out) {
        assert_eq!(
            polled.started,
            *start + settings.request_timeout * index,
            "each poll starts when the one before timed out"
        );
        assert_eq!(failure_kinds(polled), [FailureKind::Timeout]);
        assert_eq!(polled.watchdog, None);
    }
    let fired = fired_poll.watchdog.expect("the watchdog fired");
    assert_eq!(fired.silent_since, *start, "silent since the first request");
    assert_eq!(fired.at - fired.silent_since, WATCHDOG);
    assert!(
        fired.at - fired.silent_since <= WATCHDOG + settings.minimum_watchdog(),
        "the documented bound"
    );
    assert_eq!(failure_kinds(fired_poll), [FailureKind::Connect]);
    let detail = fired_poll.poll.failed[0].failure.detail.as_deref();
    assert_eq!(
        detail,
        Some("no answer for 30000 ms: the watchdog closed the connection")
    );

    assert_eq!(waiting.until, fired.at + settings.backoff_initial);
    assert_eq!(
        (waiting.failures, waiting.backoff),
        (1, settings.backoff_initial)
    );
    assert_eq!(recovered.started, waiting.until);
    assert_eq!(values(recovered), [Value::Int(i64::from(VALUE))]);
    assert!(recovered.poll.failed.is_empty());
    assert!(
        recovered.started - fired.at <= settings.backoff_initial + PERIOD,
        "polling resumes with the first due poll after the backoff"
    );

    let requests = endpoint.requests();
    let last = requests.last().expect("requests were sent");
    assert_eq!(
        last.connection,
        ConnectionId(1),
        "the value came on a new connection"
    );
    assert_eq!(endpoint.connections().len(), 2);
    assert!(
        requests.iter().all(|request| request.function == 3),
        "reads only"
    );
}

/// Rebuild checklist F4 acceptance: a device that keeps the connection open but never answers
/// (fault injection) is polled again within the configured watchdog.
#[tokio::test(start_paused = true)]
async fn the_watchdog_recovers_a_device_that_stays_connected_but_never_answers() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    let recovery = recover_from(&endpoint, Fault::no_response()).await;
    assert_recovered(&endpoint, &recovery);
    assert_eq!(recovery.silent.len(), 6, "30 s of 5 s timeouts");
    // Every request reached the device on the first connection, which the agent then closed.
    assert_eq!(
        endpoint
            .requests()
            .iter()
            .filter(|request| request.connection == ConnectionId(0))
            .count(),
        6
    );
    endpoint
        .wait_for_connection(ConnectionId(0), ConnectionState::Closed)
        .await;
}

/// Rebuild checklist F4 acceptance: a half-open connection (the device forgot it and sends
/// neither FIN nor RST, so writes keep succeeding) is detected and replaced.
#[tokio::test(start_paused = true)]
async fn a_half_open_connection_is_detected_and_replaced() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    let recovery = recover_from(&endpoint, Fault::half_open()).await;
    assert_recovered(&endpoint, &recovery);
    assert_eq!(recovery.silent.len(), 6);
    // The device read only the first request on it before it forgot the connection.
    assert_eq!(
        endpoint.connections().get(&ConnectionId(0)),
        Some(&ConnectionState::Forgotten)
    );
}

/// Rebuild checklist F4 acceptance: only a poll that read values resets the reconnect backoff.
/// The device accepts every connection and then resets it, so connecting succeeds and still
/// counts as a failure; an exception answer does not reset the backoff either.
#[tokio::test(start_paused = true)]
async fn only_a_poll_that_read_values_resets_the_backoff() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[VALUE]);
    let reset = || Fault::disconnect(Close::Rst);
    // Poll 0: the first connection is reset.
    endpoint.inject(reset().once());
    // What the next polls get, injected while the loop waits for each.
    let next_faults = [
        // Poll 1: a new connection, reset again.
        Some(reset().once()),
        // Poll 2: a new connection, and an exception answer: the device answered, no values.
        Some(Fault::exception(4).once()),
        // Poll 3: the kept connection is reset, and so is the new one the request is retried on.
        Some(reset().times(2)),
        // Poll 4: a new connection, and the value.
        None,
        // Poll 5: the same as poll 3.
        Some(reset().times(2)),
    ];
    let settings = settings();
    let (reader, mut connection) = device(&endpoint, &settings);
    let (sender, mut events) = unbounded_channel();
    let mut waits = 0;
    let start = Instant::now();
    let polling = polling::run(
        &reader,
        PERIOD,
        &mut connection,
        &TestClock,
        &settings,
        |event| {
            if matches!(event, Event::Waiting(_)) {
                waits += 1;
            }
            sender.send(event).expect("the test receives every event");
            if waits == 6 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    let drive = async {
        let mut log = Vec::new();
        let mut faults = next_faults.into_iter();
        loop {
            let polled = next_polled(&mut events).await;
            let waiting = next_waiting(&mut events).await;
            log.push((polled, waiting));
            let Some(fault) = faults.next() else {
                break log;
            };
            if let Some(fault) = fault {
                endpoint.inject(fault);
            }
            advance(waiting.until - Instant::now()).await;
        }
    };
    let ((), log) = bounded("six polls and their backoff", async {
        tokio::join!(polling, drive)
    })
    .await;

    // (when the poll started, failed connections in a row, the backoff after it), in seconds.
    let backoff: Vec<(u64, u32, u64)> = log
        .iter()
        .map(|(polled, waiting)| {
            (
                (polled.started - start).as_secs(),
                waiting.failures,
                waiting.backoff.as_secs(),
            )
        })
        .collect();
    assert_eq!(
        backoff,
        [
            (0, 1, 1), // reset: 1 s
            (1, 2, 2), // reset again: doubled
            (3, 2, 0), // an exception answer: no backoff, and no reset
            (4, 3, 4), // reset: doubled from where it was
            (8, 0, 0), // values: reset
            (9, 1, 1), // reset: back to the first backoff
        ]
    );
    let kinds: Vec<Vec<FailureKind>> = log
        .iter()
        .map(|(polled, _)| failure_kinds(polled))
        .collect();
    assert_eq!(
        kinds,
        [
            vec![FailureKind::Connect],
            vec![FailureKind::Connect],
            vec![FailureKind::Exception],
            vec![FailureKind::Connect],
            vec![],
            vec![FailureKind::Connect],
        ]
    );
    assert_eq!(values(&log[4].0), [Value::Int(i64::from(VALUE))]);
    assert!(log.iter().all(|(polled, _)| polled.watchdog.is_none()));
    // Every failed poll had connected: the device accepted all six connections.
    assert_eq!(endpoint.connections().len(), 6);
    assert!(
        endpoint
            .requests()
            .iter()
            .all(|request| request.function == 3),
        "reads only"
    );
}
