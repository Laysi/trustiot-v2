//! The read statistics of the heartbeat's `devices` under fault injection (rebuild checklist H1,
//! Laysi/trustiot-v2#117): real polls of devices on testkit's fake Modbus endpoint, with every
//! kind of failure the checklist names injected in turn (a device that cannot be reached, one
//! that stays silent, a Modbus exception code, the response of another unit, a value that does
//! not decode, a connection that is lost), and the counters the heartbeat would report checked
//! after each step.
//!
//! The clock moves only when the test advances it (`testkit::clock::manual`): a timeout fires
//! because the test advanced the clock past it, never because the machine is slow.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};
use std::ops::ControlFlow;
use std::sync::Arc;

use agent::desired::fleet::Sink;
use agent::health::{DeviceBook, HealthSink};
use agent::modbus::endpoint::{Endpoints, SharedConnection};
use agent::modbus::{Clock, DeviceReader, Timestamp};
use agent::polling::{self, Event, Polled, Settings};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::health::{DeviceStats, FailureCounts, FailureKind, Heartbeat};
use protocol::{
    AgentVersion, ConfigVersion, DeviceId, Message as _, ModelName, PointKey, SessionId,
    UnixMillis, V1,
};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, Close, FakeModbus, Fault};
use tokio::time::advance;

/// Every reading is stamped with this time.
const NOW: u64 = 1_760_054_400_000;
/// What meter-1's register holds.
const VALUE: u16 = 2301;

struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(NOW),
            synced: true,
        }
    }
}

fn device(id: &str, port: u16, unit_id: u8, data_type: DataType) -> (DeviceReader, Device) {
    let model = DeviceModel {
        name: ModelName::new("meter").expect("a valid model name"),
        points: vec![DataPoint {
            key: PointKey::new("power").expect("a valid key"),
            function_code: FunctionCode::ReadHoldingRegisters,
            address: 0,
            data_type,
            byte_order: ByteOrder::Abcd,
            length: None,
            scale: None,
            offset: None,
        }],
    };
    let device = Device {
        id: DeviceId::new(id).expect("a valid device ID"),
        model: model.name.clone(),
        transport: Transport::ModbusTcp,
        host: Host::new("127.0.0.1").expect("a valid host"),
        port,
        unit_id,
        poll_interval_ms: 1000,
    };
    DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(1).expect("a nonzero version"),
        models: vec![model.clone()],
        devices: vec![device.clone()],
    }
    .validate()
    .expect("the test configuration is valid");
    (
        DeviceReader::new(&device, &model).expect("a plan for a validated model"),
        device,
    )
}

/// One device, as the fleet sets it up: its reader and the connection of its endpoint.
struct Polled1 {
    id: DeviceId,
    reader: DeviceReader,
    connection: SharedConnection,
}

fn polled_device(
    endpoints: &mut Endpoints,
    id: &str,
    port: u16,
    unit_id: u8,
    data_type: DataType,
) -> Polled1 {
    let (reader, device) = device(id, port, unit_id, data_type);
    Polled1 {
        id: device.id.clone(),
        reader,
        connection: endpoints.connection(&device),
    }
}

struct Harness {
    endpoint: FakeModbus,
    settings: Settings,
    sink: HealthSink,
    book: DeviceBook,
}

impl Harness {
    /// Polls `device` once, then reports the poll to the sink the way the fleet does. While the
    /// poll waits for the fake endpoint's answer that never comes, `silent` advances the clock
    /// past the request timeout.
    async fn poll(&self, device: &mut Polled1, silent: bool) {
        let before = self.endpoint.requests().len();
        let polling = polling::run(
            &device.reader,
            std::time::Duration::from_secs(1),
            &mut device.connection,
            &TestClock,
            &self.settings,
            |event| match event {
                Event::Polled(polled) => {
                    let polled: Polled = polled;
                    self.sink.polled(&device.id, polled);
                    ControlFlow::Break(())
                }
                Event::Waiting(_) => ControlFlow::Continue(()),
            },
        );
        let drive = async {
            if silent {
                self.endpoint.wait_for_requests(before + 1).await;
                advance(self.settings.request_timeout).await;
            }
        };
        let ((), ()) = bounded("one poll of a device", async {
            tokio::join!(polling, drive)
        })
        .await;
    }

    fn stats(&self, id: &str) -> DeviceStats {
        self.book
            .snapshot()
            .into_iter()
            .find(|stats| stats.device.as_str() == id)
            .unwrap_or_else(|| panic!("{id} is in the book"))
    }
}

/// The heartbeat as the gateway would send it with these device statistics, through the
/// protocol's validation and back.
fn heartbeat_with(devices: Vec<DeviceStats>) -> Heartbeat {
    let heartbeat = Heartbeat {
        v: V1,
        session: SessionId::new("s-1").expect("a valid session"),
        sent_at: UnixMillis::new(NOW),
        interval_s: 60,
        agent_version: AgentVersion::new("0.1.0-test").expect("a valid version"),
        config_version: ConfigVersion::new(1),
        system: protocol::health::SystemStats {
            cpu_percent: 0.0,
            memory_total_bytes: 0,
            memory_available_bytes: 0,
            data_total_bytes: 0,
            data_available_bytes: 0,
            soc_temp_celsius: None,
            throttling: None,
            booted_at: UnixMillis::new(NOW),
            uptime_s: 0,
            boot_count: 1,
            agent_uptime_s: 0,
            agent_starts_since_boot: 1,
        },
        buffer: agent::upload::Stats::default().snapshot(),
        clock_synced: true,
        cert_not_after: UnixMillis::new(NOW),
        root_crl: None,
        devices,
    };
    Heartbeat::decode(&heartbeat.encode().expect("the heartbeat encodes"))
        .expect("the heartbeat decodes")
}

/// The sink the health sink hands every poll on to.
struct Forwarded(std::sync::Mutex<Vec<String>>);

impl Sink for Forwarded {
    fn started(&self, device: &DeviceId) {
        self.0
            .lock()
            .expect("the log lock")
            .push(format!("started {device}"));
    }

    fn polled(&self, device: &DeviceId, _: Polled) {
        self.0
            .lock()
            .expect("the log lock")
            .push(format!("polled {device}"));
    }

    fn removed(&self, device: &DeviceId) {
        self.0
            .lock()
            .expect("the log lock")
            .push(format!("removed {device}"));
    }
}

/// Rebuild checklist H1 acceptance: with faults injected, the count of each error kind (cannot
/// connect, timeout, Modbus exception code, unit mismatch, undecodable value) is right, and so
/// are the requests, the points of the latest poll and the last success and failure.
#[tokio::test(start_paused = true)]
async fn injected_faults_are_counted_under_their_kind() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[VALUE]);
    // Unit 2 holds a float that is not a number: its request succeeds, its point does not decode.
    endpoint.set_holding_registers(2, 0, &[0x7FC0, 0x0000]);
    // A port nobody listens on: connecting is refused.
    let closed_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind a loopback port")
        .local_addr()
        .expect("its address")
        .port();
    let settings = Settings::default();
    let mut endpoints =
        Endpoints::new(settings.connect_timeout).exempt_for_tests([Ipv4Addr::LOCALHOST.into()]);
    let port = endpoint.address().port();
    let mut meter = polled_device(&mut endpoints, "meter-1", port, 1, DataType::U16);
    let mut nan = polled_device(&mut endpoints, "meter-2", port, 2, DataType::F32);
    let mut ghost = polled_device(&mut endpoints, "ghost", closed_port, 3, DataType::U16);

    let forwarded = Arc::new(Forwarded(std::sync::Mutex::new(Vec::new())));
    let book = DeviceBook::new();
    let harness = Harness {
        endpoint,
        settings,
        sink: HealthSink::new(book.clone(), forwarded.clone()),
        book,
    };
    for device in [&meter, &nan, &ghost] {
        harness.sink.started(&device.id);
    }
    // Known before the first poll, with nothing counted.
    for id in ["ghost", "meter-1", "meter-2"] {
        let stats = harness.stats(id);
        assert_eq!((stats.requests, stats.last_success), (0, None), "{id}");
        assert_eq!(stats.failures, FailureCounts::default(), "{id}");
    }

    // A healthy read.
    harness.poll(&mut meter, false).await;
    let stats = harness.stats("meter-1");
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.last_success, Some(UnixMillis::new(NOW)));
    assert_eq!((stats.points_total, stats.points_failed), (1, 0));
    assert_eq!(stats.failures, FailureCounts::default());
    assert_eq!(stats.last_failure, None);

    // Modbus exceptions, by code: 2 (illegal data address), 11 (gateway target device failed
    // to respond; what a converter answers for a dead device), and 2 again.
    for code in [2, 11, 2] {
        harness.endpoint.inject(Fault::exception(code).once());
        harness.poll(&mut meter, false).await;
    }
    let stats = harness.stats("meter-1");
    assert_eq!(stats.requests, 4);
    assert_eq!(stats.failures.exception, 3);
    assert_eq!(stats.exception_codes, BTreeMap::from([(2, 2), (11, 1)]));
    assert_eq!((stats.points_total, stats.points_failed), (1, 1));
    let last = stats.last_failure.expect("a failure");
    assert_eq!(
        (last.kind, last.exception_code),
        (FailureKind::Exception, Some(2))
    );

    // A device that stays silent: the request times out.
    harness.endpoint.inject(Fault::no_response().once());
    harness.poll(&mut meter, true).await;
    let stats = harness.stats("meter-1");
    assert_eq!((stats.requests, stats.failures.timeout), (5, 1));
    assert_eq!(
        stats
            .last_failure
            .as_ref()
            .map(|failure| failure.kind.clone()),
        Some(FailureKind::Timeout)
    );

    // The response of another unit (a converter that shares responses among connections) is
    // discarded and counted; the request waits for its own answer and times out.
    harness.endpoint.inject(Fault::unit_mismatch(9).once());
    harness.poll(&mut meter, true).await;
    let stats = harness.stats("meter-1");
    assert_eq!((stats.requests, stats.failures.timeout), (6, 2));
    assert_eq!(stats.failures.unit_mismatch, 1);

    // A healthy read again moves the last success, and the last failure stays.
    harness.poll(&mut meter, false).await;
    let stats = harness.stats("meter-1");
    assert_eq!((stats.points_total, stats.points_failed), (1, 0));
    assert_eq!(
        stats
            .last_failure
            .as_ref()
            .map(|failure| failure.kind.clone()),
        Some(FailureKind::Timeout)
    );

    // A connection that is lost, and again on the retry the agent makes on a new one.
    harness
        .endpoint
        .inject(Fault::disconnect(Close::Rst).times(2));
    harness.poll(&mut meter, false).await;
    let stats = harness.stats("meter-1");
    assert_eq!(stats.requests, 8);
    assert_eq!(
        stats.failures,
        FailureCounts {
            connect: 1,
            timeout: 2,
            exception: 3,
            unit_mismatch: 1,
            invalid_value: 0,
        }
    );
    assert_eq!(stats.exception_codes, BTreeMap::from([(2, 2), (11, 1)]));
    assert_eq!(
        stats
            .last_failure
            .as_ref()
            .map(|failure| failure.kind.clone()),
        Some(FailureKind::Connect)
    );
    assert_eq!(stats.last_success, Some(UnixMillis::new(NOW)));
    // The sum of the kinds that fail a request stays below the requests: 1 - 6 / 8.
    assert!(
        stats.failures.connect + stats.failures.timeout + stats.failures.exception < stats.requests
    );

    // A value that does not decode: the request succeeded, the point failed.
    harness.poll(&mut nan, false).await;
    let stats = harness.stats("meter-2");
    assert_eq!(stats.requests, 1);
    assert_eq!(
        stats.failures,
        FailureCounts {
            invalid_value: 1,
            ..FailureCounts::default()
        }
    );
    assert_eq!((stats.points_total, stats.points_failed), (1, 1));
    assert_eq!(stats.last_success, None, "nothing was ever read");
    assert_eq!(
        stats
            .last_failure
            .as_ref()
            .map(|failure| failure.kind.clone()),
        Some(FailureKind::InvalidValue)
    );

    // A device nobody answers for: connecting is refused.
    harness.poll(&mut ghost, false).await;
    harness.poll(&mut ghost, false).await;
    let stats = harness.stats("ghost");
    assert_eq!(stats.requests, 2);
    assert_eq!(
        stats.failures,
        FailureCounts {
            connect: 2,
            ..FailureCounts::default()
        }
    );
    assert_eq!((stats.points_total, stats.points_failed), (1, 1));
    assert_eq!(stats.last_success, None);

    // Every statistic is a valid heartbeat's, and survives the encoding.
    let heartbeat = heartbeat_with(harness.book.snapshot());
    let ids: Vec<&str> = heartbeat
        .devices
        .iter()
        .map(|stats| stats.device.as_str())
        .collect();
    assert_eq!(ids, ["ghost", "meter-1", "meter-2"]);
    assert_eq!(heartbeat.devices[1], harness.stats("meter-1"));

    // Every poll, start and removal went on to the next sink; a removed device is forgotten.
    harness.sink.removed(&ghost.id);
    assert!(
        harness
            .book
            .snapshot()
            .iter()
            .all(|stats| stats.device != ghost.id)
    );
    let seen = forwarded.0.lock().expect("the log lock");
    assert_eq!(
        seen.iter()
            .filter(|line| line.starts_with("polled"))
            .count(),
        11
    );
    assert_eq!(
        seen.iter()
            .filter(|line| line.starts_with("started"))
            .count(),
        3
    );
    assert_eq!(seen.last().map(String::as_str), Some("removed ghost"));
}
