//! Devices behind an RS485 converter, read over RTU-over-TCP against testkit's fake converter
//! (rebuild checklist F2): decoding through RTU frames, one shared connection for every device
//! on the converter with requests in turn, and responses of other units, including ones the
//! converter sends to every connection (Laysi/trustiot#183), never landing on another device.
//! Timeouts follow a clock that moves only when the test advances it
//! (`testkit::clock::manual`), so they fire in order, never because the machine is slow.

use std::collections::BTreeMap;
use std::time::Duration;

use agent::modbus::endpoint::Endpoints;
use agent::modbus::pdu::{Data, Read};
use agent::modbus::tcp::TcpConnection;
use agent::modbus::{Clock, Connection, DEFAULT_CONNECT_TIMEOUT, DeviceReader, Poll, Timestamp};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::health::FailureKind;
use protocol::{ConfigVersion, DeviceId, Message, ModelName, PointKey, UnixMillis, V1, Value};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, FakeModbus, Fault};
use tokio::time::{Instant, advance};

/// The request timeout the readers are given.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The reading time at the start of a test.
const AT: u64 = 1_760_054_400_000;

/// A synchronized clock that reads [`AT`] plus the time the tokio clock moved since `start`.
struct TestClock {
    start: Instant,
}

impl TestClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).expect("a short test");
        Timestamp {
            at: UnixMillis::new(AT + elapsed),
            synced: true,
        }
    }
}

fn point(key: &str, function_code: FunctionCode, address: u16, data_type: DataType) -> DataPoint {
    DataPoint {
        key: PointKey::new(key).expect("a valid key"),
        function_code,
        address,
        data_type,
        byte_order: ByteOrder::Abcd,
        length: None,
        scale: None,
        offset: None,
    }
}

fn holding(key: &str, address: u16) -> DataPoint {
    point(
        key,
        FunctionCode::ReadHoldingRegisters,
        address,
        DataType::U16,
    )
}

fn input(key: &str, address: u16) -> DataPoint {
    point(
        key,
        FunctionCode::ReadInputRegisters,
        address,
        DataType::U16,
    )
}

/// The devices on the converter `endpoint`, each a device ID, its unit and the points of its
/// model, with their readers, after the configuration passed the same validation the gateway
/// applies before polling.
fn devices(
    endpoint: &FakeModbus,
    entries: Vec<(&str, u8, Vec<DataPoint>)>,
) -> Vec<(Device, DeviceReader)> {
    let (models, devices): (Vec<DeviceModel>, Vec<Device>) = entries
        .into_iter()
        .map(|(id, unit, points)| {
            let model = DeviceModel {
                name: ModelName::new(format!("model-{unit}")).expect("a valid model name"),
                points,
            };
            let device = Device {
                id: DeviceId::new(id).expect("a valid device ID"),
                model: model.name.clone(),
                transport: Transport::RtuOverTcp,
                host: Host::new("127.0.0.1").expect("a valid host"),
                port: endpoint.address().port(),
                unit_id: unit,
                poll_interval_ms: 1000,
            };
            (model, device)
        })
        .unzip();
    let config = DesiredConfig {
        v: V1,
        rev: 1,
        version: ConfigVersion::new(1).expect("a nonzero version"),
        models: models.clone(),
        devices: devices.clone(),
    };
    config.validate().expect("the test configuration is valid");
    devices
        .into_iter()
        .zip(&models)
        .map(|(device, model)| {
            let reader = DeviceReader::new(&device, model).expect("a plan for a validated model");
            (device, reader)
        })
        .collect()
}

/// The device and reader of a test with one device.
fn one(mut devices: Vec<(Device, DeviceReader)>) -> (Device, DeviceReader) {
    assert_eq!(devices.len(), 1);
    devices.remove(0)
}

/// The values of a poll's readings by point; every reading belongs to `device`.
fn values(poll: &Poll, device: &str) -> BTreeMap<String, Value> {
    poll.readings
        .iter()
        .map(|reading| {
            assert_eq!(reading.device.as_str(), device);
            assert!(reading.clock_synced);
            (
                reading.point.as_str().to_owned(),
                reading.sample.value.clone(),
            )
        })
        .collect()
}

fn ints(entries: &[(&str, i64)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), Value::Int(*value)))
        .collect()
}

/// The failed points of a poll by point: the failure kind and exception code.
fn failures(poll: &Poll) -> BTreeMap<String, (FailureKind, Option<u8>)> {
    poll.failed
        .iter()
        .map(|failed| {
            (
                failed.point.as_str().to_owned(),
                (failed.failure.kind.clone(), failed.failure.exception_code),
            )
        })
        .collect()
}

fn timed_out(keys: &[&str]) -> BTreeMap<String, (FailureKind, Option<u8>)> {
    keys.iter()
        .map(|key| ((*key).to_owned(), (FailureKind::Timeout, None)))
        .collect()
}

/// Every request the converter received: unit, function code, start address and count.
fn requests(endpoint: &FakeModbus) -> Vec<(u8, u8, u16, u16)> {
    endpoint
        .requests()
        .iter()
        .map(|request| {
            let (start, count) = request.range.expect("a well-formed read");
            (request.unit, request.function, start, count)
        })
        .collect()
}

/// The readers never sent anything but a read, and every frame carried a correct CRC: no
/// function code above 4 reached the bus.
fn assert_read_only(endpoint: &FakeModbus) {
    let requests = endpoint.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert!(
            (1..=4).contains(&request.function),
            "function code {} was sent: {request:?}",
            request.function
        );
        assert!(request.checksum_ok, "a bad CRC: {request:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn every_function_code_and_type_reads_over_rtu() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_coils(1, 0, &[true, false, true]);
    endpoint.set_discrete_inputs(1, 0, &[true]);
    endpoint.set_holding_registers(
        1,
        0,
        &[
            2301,   // voltage, tenths of a volt
            0xCFC7, // torque, -12345
            0x2800, 0xEE6B, // energy, 4_000_000_000 in cdab order
            0x4366, 0x8000, // power, 230.5
            0x4D53, 0x3330, 0x3000, 0x0000, // model, "MS300"
        ],
    );
    endpoint.set_input_registers(1, 0, &[650]);
    let (device, reader) = one(devices(
        &endpoint,
        vec![(
            "meter-1",
            1,
            vec![
                point("alarm", FunctionCode::ReadCoils, 0, DataType::Bool),
                point("door", FunctionCode::ReadCoils, 1, DataType::Bool),
                point("fan", FunctionCode::ReadCoils, 2, DataType::Bool),
                point("estop", FunctionCode::ReadDiscreteInputs, 0, DataType::Bool),
                DataPoint {
                    scale: Some(0.1),
                    ..holding("voltage", 0)
                },
                point(
                    "torque",
                    FunctionCode::ReadHoldingRegisters,
                    1,
                    DataType::I16,
                ),
                DataPoint {
                    byte_order: ByteOrder::Cdab,
                    ..point(
                        "energy",
                        FunctionCode::ReadHoldingRegisters,
                        2,
                        DataType::U32,
                    )
                },
                point(
                    "power",
                    FunctionCode::ReadHoldingRegisters,
                    4,
                    DataType::F32,
                ),
                DataPoint {
                    length: Some(4),
                    ..point(
                        "model",
                        FunctionCode::ReadHoldingRegisters,
                        6,
                        DataType::String,
                    )
                },
                DataPoint {
                    data_type: DataType::I16,
                    scale: Some(0.1),
                    offset: Some(-40.0),
                    ..input("temperature", 0)
                },
            ],
        )],
    ));
    let mut endpoints = Endpoints::new(DEFAULT_CONNECT_TIMEOUT);
    let mut connection = endpoints.connection(&device);
    let poll = bounded(
        "a poll",
        reader.poll(&mut connection, &TestClock::new(), TIMEOUT),
    )
    .await;

    assert_eq!(failures(&poll), BTreeMap::new());
    assert_eq!(
        values(&poll, "meter-1"),
        BTreeMap::from([
            ("alarm".to_owned(), Value::Bool(true)),
            ("door".to_owned(), Value::Bool(false)),
            ("fan".to_owned(), Value::Bool(true)),
            ("estop".to_owned(), Value::Bool(true)),
            ("voltage".to_owned(), Value::Float(2301.0 * 0.1)),
            ("torque".to_owned(), Value::Int(-12_345)),
            ("energy".to_owned(), Value::Int(4_000_000_000)),
            ("power".to_owned(), Value::Float(230.5)),
            ("model".to_owned(), Value::Text("MS300".to_owned())),
            ("temperature".to_owned(), Value::Float(650.0 * 0.1 - 40.0)),
        ])
    );
    assert_eq!(
        requests(&endpoint),
        [(1, 1, 0, 3), (1, 2, 0, 1), (1, 3, 0, 10), (1, 4, 0, 1)]
    );
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn an_exception_over_rtu_fails_only_its_request() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[100]);
    endpoint.set_holding_registers(1, 100, &[200]);
    endpoint.set_input_registers(1, 0, &[300]);
    endpoint.inject(Fault::exception(0x02).address(100).once());
    let (device, reader) = one(devices(
        &endpoint,
        vec![(
            "meter-1",
            1,
            vec![holding("a", 0), holding("b", 100), input("c", 0)],
        )],
    ));
    let mut connection = Endpoints::new(DEFAULT_CONNECT_TIMEOUT).connection(&device);
    let poll = bounded(
        "a poll",
        reader.poll(&mut connection, &TestClock::new(), TIMEOUT),
    )
    .await;
    assert_eq!(values(&poll, "meter-1"), ints(&[("a", 100), ("c", 300)]));
    assert_eq!(
        failures(&poll),
        BTreeMap::from([("b".to_owned(), (FailureKind::Exception, Some(2)))])
    );
    assert_read_only(&endpoint);
}

/// Acceptance criterion 2: several devices on one converter are all polled correctly.
#[tokio::test(start_paused = true)]
async fn devices_on_one_converter_share_one_connection_and_take_turns() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    for unit in 1..=3_u8 {
        endpoint.set_holding_registers(unit, 0, &[u16::from(unit) * 100 + 1]);
        endpoint.set_input_registers(unit, 0, &[u16::from(unit) * 100 + 2]);
    }
    // Two reads per device: holding 0, then input 0.
    let devices = devices(
        &endpoint,
        vec![
            ("meter-1", 1, vec![holding("power", 0), input("energy", 0)]),
            ("meter-2", 2, vec![holding("power", 0), input("energy", 0)]),
            ("meter-3", 3, vec![holding("power", 0), input("energy", 0)]),
        ],
    );
    let mut endpoints = Endpoints::new(DEFAULT_CONNECT_TIMEOUT);
    let [(one, first), (two, second), (three, third)] = &devices[..] else {
        panic!("three devices");
    };
    let mut connections = [
        endpoints.connection(one),
        endpoints.connection(two),
        endpoints.connection(three),
    ];
    assert_eq!(endpoints.len(), 1);
    assert!(connections[0].same(&connections[1]) && connections[0].same(&connections[2]));
    let clock = TestClock::new();

    for round in 0..2 {
        let [c1, c2, c3] = &mut connections;
        // `biased` starts the polls in this order: meter-1 takes the connection first, and the
        // other two queue for it in turn.
        let (p1, p2, p3) = bounded("three polls at the same time", async {
            tokio::join!(
                biased;
                first.poll(c1, &clock, TIMEOUT),
                second.poll(c2, &clock, TIMEOUT),
                third.poll(c3, &clock, TIMEOUT),
            )
        })
        .await;
        for (poll, device, unit) in [
            (&p1, "meter-1", 1),
            (&p2, "meter-2", 2),
            (&p3, "meter-3", 3),
        ] {
            assert_eq!(failures(poll), BTreeMap::new(), "round {round}, {device}");
            assert_eq!(
                values(poll, device),
                ints(&[("power", unit * 100 + 1), ("energy", unit * 100 + 2)]),
                "round {round}"
            );
            assert!(
                poll.requests
                    .iter()
                    .all(|request| request.unit_mismatches == 0),
                "round {round}, {device}: {:?}",
                poll.requests
            );
        }
    }

    // One request each in turn, not one device's whole poll after another.
    let turns = [
        (1, 3, 0, 1),
        (2, 3, 0, 1),
        (3, 3, 0, 1),
        (1, 4, 0, 1),
        (2, 4, 0, 1),
        (3, 4, 0, 1),
    ];
    assert_eq!(requests(&endpoint), [turns, turns].concat());
    assert_eq!(
        endpoint.connections().len(),
        1,
        "one connection to the converter"
    );
    assert_read_only(&endpoint);
}

/// Acceptance criterion 1: when the converter sends responses to every connection, no device's
/// values end up on another device.
#[tokio::test(start_paused = true)]
async fn broadcast_responses_never_land_on_another_device() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[11, 12]);
    endpoint.set_holding_registers(2, 0, &[21]);
    endpoint.set_holding_registers(3, 0, &[31]);
    // Every response goes to every connection, and only once the test releases it: requests
    // reach the bus in the order they arrive, and a held one holds up the ones behind it.
    endpoint.inject(Fault::broadcast().held());
    let devices = devices(
        &endpoint,
        vec![
            ("meter-1", 1, vec![holding("a", 0), holding("b", 1)]),
            ("drive-2", 2, vec![holding("speed", 0)]),
        ],
    );
    let [(meter, meter_reader), (drive, drive_reader)] = &devices[..] else {
        panic!("two devices");
    };
    let mut endpoints = Endpoints::new(DEFAULT_CONNECT_TIMEOUT);
    let mut meter_connection = endpoints.connection(meter);
    let mut drive_connection = endpoints.connection(drive);
    // Another client of the same converter (a field SCADA, say), on its own connection. It
    // reads unit 1 with another count than the meter's, and unit 3.
    let mut foreign = TcpConnection::new(
        Transport::RtuOverTcp,
        "127.0.0.1",
        endpoint.address().port(),
        DEFAULT_CONNECT_TIMEOUT,
    );
    let clock = TestClock::new();
    let read = |start, count| {
        Read::new(FunctionCode::ReadHoldingRegisters, start, count).expect("a valid range")
    };

    let ours = async {
        tokio::join!(
            async {
                // Request 1, behind the foreign client's request 0.
                endpoint.wait_for_requests(1).await;
                meter_reader
                    .poll(&mut meter_connection, &clock, TIMEOUT)
                    .await
            },
            async {
                // Starts while the meter's request waits, so the drive waits for its turn.
                endpoint.wait_for_requests(2).await;
                drive_reader
                    .poll(&mut drive_connection, &clock, TIMEOUT)
                    .await
            },
        )
    };
    let theirs = async {
        // Request 0: unit 1, one register. Its answer goes out while the meter's request for
        // two registers of unit 1 waits on the shared connection.
        let (first, ()) = tokio::join!(foreign.read(1, read(0, 1), TIMEOUT), async {
            endpoint.wait_for_requests(2).await;
            endpoint.release(0);
        });
        // Request 2: unit 3, sent before the meter's answer goes out.
        let (second, ()) = tokio::join!(foreign.read(3, read(0, 1), TIMEOUT), async {
            endpoint.wait_for_requests(3).await;
            endpoint.release(1); // the meter's answer
            // Request 3, the drive's, now waits behind request 2.
            endpoint.wait_for_requests(4).await;
            endpoint.release(2); // unit 3's answer, while the drive waits
            endpoint.release(3);
        });
        (first, second)
    };
    let ((meter_poll, drive_poll), (first, second)) =
        bounded("our polls and the foreign client's reads", async {
            tokio::join!(ours, theirs)
        })
        .await;

    // The foreign answer of unit 1 had the meter's unit but not its byte count: not taken.
    assert_eq!(failures(&meter_poll), BTreeMap::new());
    assert_eq!(
        values(&meter_poll, "meter-1"),
        ints(&[("a", 11), ("b", 12)])
    );
    assert_eq!(meter_poll.requests[0].unit_mismatches, 0);
    // Unit 3's answer reached the drive's request and was discarded as another unit's.
    assert_eq!(failures(&drive_poll), BTreeMap::new());
    assert_eq!(values(&drive_poll, "drive-2"), ints(&[("speed", 21)]));
    assert_eq!(drive_poll.requests[0].unit_mismatches, 1);
    // The foreign client got its own values too, and discarded the meter's answer.
    assert_eq!(first.result, Ok(Data::Registers(vec![11])));
    assert_eq!(first.unit_mismatches, 0);
    assert_eq!(second.result, Ok(Data::Registers(vec![31])));
    assert_eq!(second.unit_mismatches, 1);

    let log: Vec<(usize, u8, u16)> = endpoint
        .requests()
        .iter()
        .map(|request| {
            let (_, count) = request.range.expect("a well-formed read");
            (request.connection.0, request.unit, count)
        })
        .collect();
    assert_eq!(log, [(0, 1, 1), (1, 1, 2), (0, 3, 1), (1, 2, 1)]);
    assert_eq!(endpoint.connections().len(), 2, "ours and the foreign one");
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_silent_device_holds_the_others_up_by_one_timeout_per_request() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[101]);
    endpoint.set_input_registers(1, 0, &[102]);
    endpoint.set_holding_registers(2, 0, &[201]);
    endpoint.set_holding_registers(3, 0, &[301]);
    // Unit 1 is on the bus but never answers.
    endpoint.inject(Fault::no_response().unit(1));
    let devices = devices(
        &endpoint,
        vec![
            ("meter-1", 1, vec![holding("power", 0), input("energy", 0)]),
            ("meter-2", 2, vec![holding("power", 0)]),
            ("meter-3", 3, vec![holding("power", 0)]),
        ],
    );
    let [(one, first), (two, second), (three, third)] = &devices[..] else {
        panic!("three devices");
    };
    let mut endpoints = Endpoints::new(DEFAULT_CONNECT_TIMEOUT);
    let (mut c1, mut c2, mut c3) = (
        endpoints.connection(one),
        endpoints.connection(two),
        endpoints.connection(three),
    );
    let clock = TestClock::new();

    let (p1, p2, p3, ()) = bounded("three polls and the clock", async {
        tokio::join!(
            biased;
            first.poll(&mut c1, &clock, TIMEOUT),
            second.poll(&mut c2, &clock, TIMEOUT),
            third.poll(&mut c3, &clock, TIMEOUT),
            async {
                endpoint.wait_for_requests(1).await;
                advance(TIMEOUT).await; // meter-1's first request times out
                endpoint.wait_for_requests(4).await;
                advance(TIMEOUT).await; // and its second
            },
        )
    })
    .await;

    assert_eq!(values(&p1, "meter-1"), BTreeMap::new());
    assert_eq!(failures(&p1), timed_out(&["power", "energy"]));
    let at: Vec<u64> = p1
        .failed
        .iter()
        .map(|failed| failed.failure.at.get() - AT)
        .collect();
    assert_eq!(at, [5_000, 10_000]);
    // The others were sent once meter-1's first request had timed out, before its second, and
    // their own timeouts did not run while they waited for their turn.
    for (poll, device, value) in [(&p2, "meter-2", 201), (&p3, "meter-3", 301)] {
        assert_eq!(values(poll, device), ints(&[("power", value)]));
        assert_eq!(poll.readings[0].sample.at, UnixMillis::new(AT + 5_000));
    }
    let units: Vec<u8> = requests(&endpoint).iter().map(|(unit, ..)| *unit).collect();
    assert_eq!(units, [1, 2, 3, 1]);
    assert_eq!(endpoint.connections().len(), 1);
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_siblings_late_answer_is_a_unit_mismatch_not_a_value() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[100]);
    endpoint.set_holding_registers(2, 0, &[200]);
    // Unit 1 answers 1 s after its request timed out, while unit 2's request of the same shape
    // waits on the shared connection (and on the bus behind it).
    endpoint.inject(
        Fault::delay(TIMEOUT + Duration::from_secs(1))
            .unit(1)
            .once(),
    );
    let devices = devices(
        &endpoint,
        vec![
            ("meter-1", 1, vec![holding("power", 0)]),
            ("meter-2", 2, vec![holding("power", 0)]),
        ],
    );
    let [(one, first), (two, second)] = &devices[..] else {
        panic!("two devices");
    };
    let mut endpoints = Endpoints::new(DEFAULT_CONNECT_TIMEOUT);
    let (mut c1, mut c2) = (endpoints.connection(one), endpoints.connection(two));
    let clock = TestClock::new();

    let (p1, p2, ()) = bounded("two polls and the clock", async {
        tokio::join!(
            biased;
            first.poll(&mut c1, &clock, TIMEOUT),
            second.poll(&mut c2, &clock, TIMEOUT),
            async {
                endpoint.wait_for_requests(1).await;
                advance(TIMEOUT).await;
                endpoint.wait_for_requests(2).await;
                advance(Duration::from_secs(1)).await;
            },
        )
    })
    .await;

    assert_eq!(failures(&p1), timed_out(&["power"]));
    assert_eq!(p1.requests[0].unit_mismatches, 0);
    // 200, not unit 1's late 100.
    assert_eq!(values(&p2, "meter-2"), ints(&[("power", 200)]));
    assert_eq!(p2.requests[0].unit_mismatches, 1);
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_late_answer_of_another_shape_is_not_taken_for_the_next_request() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[11, 12]);
    endpoint.set_holding_registers(1, 100, &[13]);
    // The answer to holding 0..=1 comes 1 s after its request timed out, while the request for
    // holding 100 waits. RTU has no transaction identifier: only its byte count (4, not 2) tells
    // it is not the answer.
    endpoint.inject(
        Fault::delay(TIMEOUT + Duration::from_secs(1))
            .address(0)
            .once(),
    );
    let (device, reader) = one(devices(
        &endpoint,
        vec![(
            "meter-1",
            1,
            vec![holding("a", 0), holding("b", 1), holding("c", 100)],
        )],
    ));
    let mut connection = Endpoints::new(DEFAULT_CONNECT_TIMEOUT).connection(&device);
    let clock = TestClock::new();
    let (poll, ()) = bounded("a poll and the clock", async {
        tokio::join!(reader.poll(&mut connection, &clock, TIMEOUT), async {
            endpoint.wait_for_requests(1).await;
            advance(TIMEOUT).await;
            endpoint.wait_for_requests(2).await;
            advance(Duration::from_secs(1)).await;
        })
    })
    .await;
    assert_eq!(failures(&poll), timed_out(&["a", "b"]));
    assert_eq!(values(&poll, "meter-1"), ints(&[("c", 13)]));
    assert_eq!(poll.requests[1].failure, None);
    assert_eq!(
        poll.requests[1].unit_mismatches, 0,
        "the same unit's late answer is not a unit mismatch"
    );
    assert_eq!(requests(&endpoint), [(1, 3, 0, 2), (1, 3, 100, 1)]);
    assert_read_only(&endpoint);
}
