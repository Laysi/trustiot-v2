//! The Modbus reader against testkit's fake Modbus TCP endpoint (rebuild checklist F1, F5):
//! every data type and byte order, merged reads, scale and offset, and each fault the endpoint
//! injects. Timeouts follow a clock that moves only when the test advances it
//! (`testkit::clock::manual`), so they fire in order, never because the machine is slow.

use std::collections::BTreeMap;
use std::time::Duration;

use agent::modbus::tcp::TcpConnection;
use agent::modbus::{Clock, DEFAULT_CONNECT_TIMEOUT, DeviceReader, Poll, Timestamp};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::health::FailureKind;
use protocol::{ConfigVersion, DeviceId, Message, ModelName, PointKey, UnixMillis, V1, Value};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, Close, ConnectionId, ConnectionState, FakeModbus, Fault};
use tokio::time::advance;

/// The request timeout the reader is given.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Every reading's time: the test clock does not move.
const AT: u64 = 1_760_054_400_000;

/// A synchronized clock that always reads [`AT`].
struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(AT),
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

fn holding(key: &str, address: u16, data_type: DataType) -> DataPoint {
    point(key, FunctionCode::ReadHoldingRegisters, address, data_type)
}

fn ordered(point: DataPoint, byte_order: ByteOrder) -> DataPoint {
    DataPoint {
        byte_order,
        ..point
    }
}

/// The reader for one device on `endpoint`, after the configuration passed the same
/// validation the gateway applies before polling.
fn reader(endpoint: &FakeModbus, device: &str, unit: u8, points: Vec<DataPoint>) -> DeviceReader {
    let model = DeviceModel {
        name: ModelName::new(format!("model-{unit}")).expect("a valid model name"),
        points,
    };
    let device = Device {
        id: DeviceId::new(device).expect("a valid device ID"),
        model: model.name.clone(),
        transport: Transport::ModbusTcp,
        host: Host::new("127.0.0.1").expect("a valid host"),
        port: endpoint.address().port(),
        unit_id: unit,
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
    DeviceReader::new(&device, &model).expect("a plan for a validated model")
}

fn connection(endpoint: &FakeModbus) -> TcpConnection {
    TcpConnection::new(
        Transport::ModbusTcp,
        "127.0.0.1",
        endpoint.address().port(),
        DEFAULT_CONNECT_TIMEOUT,
    )
}

/// Polls once while `meanwhile` moves the clock or watches the endpoint.
async fn poll_with(
    reader: &DeviceReader,
    connection: &mut TcpConnection,
    meanwhile: impl Future<Output = ()>,
) -> Poll {
    let poll = reader.poll(connection, &TestClock, TIMEOUT);
    let (poll, ()) = bounded("a poll", async { tokio::join!(poll, meanwhile) }).await;
    poll
}

/// The values of a poll's readings by point; every reading belongs to `device` and carries
/// the test clock's time.
fn values(poll: &Poll, device: &str) -> BTreeMap<String, Value> {
    poll.readings
        .iter()
        .map(|reading| {
            assert_eq!(reading.device.as_str(), device);
            assert_eq!(reading.sample.at, UnixMillis::new(AT));
            assert!(reading.clock_synced);
            (
                reading.point.as_str().to_owned(),
                reading.sample.value.clone(),
            )
        })
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

/// Every request the endpoint received: function code, start address and count.
fn requests(endpoint: &FakeModbus) -> Vec<(u8, u16, u16)> {
    endpoint
        .requests()
        .iter()
        .map(|request| {
            let (start, count) = request.range.expect("a well-formed read");
            (request.function, start, count)
        })
        .collect()
}

/// The reader never sent anything but a read: no function code above 4 reached the device.
fn assert_read_only(endpoint: &FakeModbus) {
    let requests = endpoint.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert!(
            (1..=4).contains(&request.function),
            "function code {} was sent: {request:?}",
            request.function
        );
    }
}

/// The registers a device stores `value` (most significant byte first) in, for `order`, from
/// the definitions in `ByteOrder`'s documentation.
fn stored(value: &[u8], order: ByteOrder) -> Vec<u16> {
    let mut registers: Vec<u16> = value
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| match order {
            ByteOrder::Abcd | ByteOrder::Cdab => u16::from_be_bytes([high, low]),
            ByteOrder::Badc | ByteOrder::Dcba => u16::from_be_bytes([low, high]),
        })
        .collect();
    if matches!(order, ByteOrder::Cdab | ByteOrder::Dcba) {
        registers.reverse();
    }
    registers
}

const ORDERS: [(ByteOrder, &str); 4] = [
    (ByteOrder::Abcd, "abcd"),
    (ByteOrder::Badc, "badc"),
    (ByteOrder::Cdab, "cdab"),
    (ByteOrder::Dcba, "dcba"),
];

#[tokio::test(start_paused = true)]
async fn every_type_and_byte_order_reads_from_the_fake_endpoint() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    // A merged read also reads the addresses between points, which a device answers only when
    // they exist; give every address up to 60 a value.
    endpoint.set_holding_registers(1, 0, &[0; 61]);
    endpoint.set_coils(1, 0, &[false; 6]);

    let mut points = Vec::new();
    let mut expected = BTreeMap::new();
    let mut put = |point: DataPoint, registers: &[u16], value: Value| {
        endpoint.set_holding_registers(1, point.address, registers);
        expected.insert(point.key.as_str().to_owned(), value);
        points.push(point);
    };

    // An MS300-style register that holds tenths of a volt (rebuild checklist F5).
    put(
        DataPoint {
            scale: Some(0.1),
            ..holding("voltage", 0, DataType::U16)
        },
        &[2301],
        Value::Float(2301.0 * 0.1),
    );
    put(
        holding("torque", 1, DataType::I16),
        &[0xCFC7],
        Value::Int(-12_345),
    );
    // 32-bit types in every byte order: addresses 2..=17.
    for (index, (order, name)) in (0_u16..).zip(ORDERS) {
        let energy = ordered(
            holding(&format!("energy_{name}"), 2 + index * 2, DataType::U32),
            order,
        );
        put(
            energy,
            &stored(&4_000_000_000_u32.to_be_bytes(), order),
            Value::Int(4_000_000_000),
        );
        let power = ordered(
            holding(&format!("power_{name}"), 10 + index * 2, DataType::F32),
            order,
        );
        put(
            power,
            &stored(&230.5_f32.to_be_bytes(), order),
            Value::Float(230.5),
        );
    }
    put(
        ordered(holding("delta", 18, DataType::I32), ByteOrder::Cdab),
        &stored(&(-123_456_789_i32).to_be_bytes(), ByteOrder::Cdab),
        Value::Int(-123_456_789),
    );
    // 64-bit types: addresses 20..=31.
    put(
        ordered(holding("counter", 20, DataType::U64), ByteOrder::Dcba),
        &stored(&0x0102_0304_0506_0708_u64.to_be_bytes(), ByteOrder::Dcba),
        Value::Int(0x0102_0304_0506_0708),
    );
    put(
        ordered(holding("balance", 24, DataType::I64), ByteOrder::Badc),
        &stored(&(-9_876_543_210_123_i64).to_be_bytes(), ByteOrder::Badc),
        Value::Int(-9_876_543_210_123),
    );
    put(
        ordered(holding("precise", 28, DataType::F64), ByteOrder::Cdab),
        &stored(&(-0.000_123_456_789_f64).to_be_bytes(), ByteOrder::Cdab),
        Value::Float(-0.000_123_456_789),
    );
    // Strings after a gap of 8 unread registers (32..=39), still in the same read.
    put(
        DataPoint {
            length: Some(4),
            ..holding("model", 40, DataType::String)
        },
        &[0x4D53, 0x3330, 0x3000, 0x0000], // "MS300"
        Value::Text("MS300".to_owned()),
    );
    put(
        DataPoint {
            length: Some(4),
            ..ordered(holding("serial", 44, DataType::String), ByteOrder::Badc)
        },
        &[0x4241, 0x3231, 0x2020, 0x0000], // "AB12" padded with spaces
        Value::Text("AB12".to_owned()),
    );
    put(
        holding("running", 48, DataType::Bool),
        &[1],
        Value::Bool(true),
    );
    // 11 unread registers (49..=59): a read of its own.
    put(
        DataPoint {
            scale: Some(0.01),
            offset: Some(-1.0),
            ..holding("frequency", 60, DataType::U16)
        },
        &[6100],
        Value::Float(6100.0 * 0.01 - 1.0),
    );

    // Input registers, coils and discrete inputs.
    endpoint.set_input_registers(1, 0, &[650]);
    points.push(DataPoint {
        scale: Some(0.1),
        offset: Some(-40.0),
        ..point(
            "temperature",
            FunctionCode::ReadInputRegisters,
            0,
            DataType::I16,
        )
    });
    expected.insert("temperature".to_owned(), Value::Float(650.0 * 0.1 - 40.0));
    endpoint.set_coils(1, 0, &[true, false]);
    endpoint.set_coils(1, 5, &[true]);
    endpoint.set_coils(1, 100, &[true]);
    for (key, address, value) in [
        ("alarm", 0, true),
        ("door", 1, false),
        ("fan", 5, true),
        ("remote", 100, true),
    ] {
        points.push(point(key, FunctionCode::ReadCoils, address, DataType::Bool));
        expected.insert(key.to_owned(), Value::Bool(value));
    }
    endpoint.set_discrete_inputs(1, 3, &[true]);
    points.push(point(
        "estop",
        FunctionCode::ReadDiscreteInputs,
        3,
        DataType::Bool,
    ));
    expected.insert("estop".to_owned(), Value::Bool(true));

    let reader = reader(&endpoint, "meter-1", 1, points);
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {}).await;

    assert_eq!(failures(&poll), BTreeMap::new());
    assert_eq!(values(&poll, "meter-1"), expected);
    assert!(
        poll.requests
            .iter()
            .all(|request| request.failure.is_none())
    );
    // Merged as planned: coils 0..=5 and 100 apart, then the discrete input, holding 0..=48 in
    // one read and 60 alone, then the input register.
    assert_eq!(
        requests(&endpoint),
        [
            (1, 0, 6),
            (1, 100, 1),
            (2, 3, 1),
            (3, 0, 49),
            (3, 60, 1),
            (4, 0, 1)
        ]
    );
    assert_read_only(&endpoint);
}

/// Three reads: holding 0 ("a", 100), holding 100 ("b", 200), input 0 ("c", 300).
async fn three_reads() -> (FakeModbus, DeviceReader) {
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[100]);
    endpoint.set_holding_registers(1, 100, &[200]);
    endpoint.set_input_registers(1, 0, &[300]);
    let reader = reader(
        &endpoint,
        "meter-1",
        1,
        vec![
            holding("a", 0, DataType::U16),
            holding("b", 100, DataType::U16),
            point("c", FunctionCode::ReadInputRegisters, 0, DataType::U16),
        ],
    );
    (endpoint, reader)
}

fn all_read() -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("a".to_owned(), Value::Int(100)),
        ("b".to_owned(), Value::Int(200)),
        ("c".to_owned(), Value::Int(300)),
    ])
}

fn only(keys: &[&str]) -> BTreeMap<String, Value> {
    all_read()
        .into_iter()
        .filter(|(key, _)| keys.contains(&key.as_str()))
        .collect()
}

fn failed(
    entries: &[(&str, FailureKind, Option<u8>)],
) -> BTreeMap<String, (FailureKind, Option<u8>)> {
    entries
        .iter()
        .map(|(key, kind, code)| ((*key).to_owned(), (kind.clone(), *code)))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_exception_fails_only_its_request() {
    let _clock = clock::manual();
    let (endpoint, reader) = three_reads().await;
    endpoint.inject(Fault::exception(0x02).address(100).once());
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {}).await;
    assert_eq!(values(&poll, "meter-1"), only(&["a", "c"]));
    assert_eq!(
        failures(&poll),
        failed(&[("b", FailureKind::Exception, Some(2))])
    );
    assert_eq!(poll.requests.len(), 3);
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_silent_device_times_out_and_the_connection_stays() {
    let _clock = clock::manual();
    let (endpoint, reader) = three_reads().await;
    // Connected but never answering this request (Laysi/trustiot#159, #164).
    endpoint.inject(Fault::no_response().function(3).address(0).once());
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {
        endpoint.wait_for_requests(1).await;
        advance(TIMEOUT).await;
    })
    .await;
    assert_eq!(values(&poll, "meter-1"), only(&["b", "c"]));
    assert_eq!(
        failures(&poll),
        failed(&[("a", FailureKind::Timeout, None)])
    );
    // The same connection carried the later requests and carries the next poll.
    let poll = poll_with(&reader, &mut connection, async {}).await;
    assert_eq!(values(&poll, "meter-1"), all_read());
    assert_eq!(endpoint.connections().len(), 1);
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_late_answer_is_never_taken_for_the_next_request() {
    let _clock = clock::manual();
    let (endpoint, reader) = three_reads().await;
    // Holding 0's answer (100) comes 1 s after its request timed out, while the request for
    // holding 100 waits on the same connection.
    endpoint.inject(
        Fault::delay(TIMEOUT + Duration::from_secs(1))
            .function(3)
            .address(0)
            .once(),
    );
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {
        endpoint.wait_for_requests(1).await;
        advance(TIMEOUT).await;
        endpoint.wait_for_requests(2).await;
        advance(Duration::from_secs(1)).await;
    })
    .await;
    assert_eq!(
        failures(&poll),
        failed(&[("a", FailureKind::Timeout, None)])
    );
    // "b" is 200, not the late 100 of the request before it.
    assert_eq!(values(&poll, "meter-1"), only(&["b", "c"]));
    // The late answer came from the same unit: not a unit mismatch.
    assert!(
        poll.requests
            .iter()
            .all(|request| request.unit_mismatches == 0)
    );
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn an_answer_from_another_unit_is_discarded() {
    let _clock = clock::manual();
    let (endpoint, reader) = three_reads().await;
    endpoint.inject(Fault::unit_mismatch(9).address(100).once());
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {
        endpoint.wait_for_requests(2).await;
        advance(TIMEOUT).await;
    })
    .await;
    // The answer labelled unit 9 is not "b"'s value; "b" waits for its own and times out.
    assert_eq!(values(&poll, "meter-1"), only(&["a", "c"]));
    assert_eq!(
        failures(&poll),
        failed(&[("b", FailureKind::Timeout, None)])
    );
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_connection_is_replaced_within_the_request() {
    let _clock = clock::manual();
    for close in [Close::Rst, Close::Fin] {
        let (endpoint, reader) = three_reads().await;
        // The device drops the connection it kept open since "a" instead of answering "b".
        endpoint.inject(Fault::disconnect(close).address(100).once());
        let mut connection = connection(&endpoint);
        let poll = poll_with(&reader, &mut connection, async {}).await;
        assert_eq!(values(&poll, "meter-1"), all_read(), "{close:?}");
        assert!(
            poll.requests
                .iter()
                .all(|request| request.failure.is_none())
        );
        // "b" was sent again on a second connection, which also carried "c".
        assert_eq!(endpoint.connections().len(), 2, "{close:?}");
        assert_eq!(
            requests(&endpoint),
            [(3, 0, 1), (3, 100, 1), (3, 100, 1), (4, 0, 1)],
            "{close:?}"
        );
        assert_read_only(&endpoint);
    }
}

#[tokio::test(start_paused = true)]
async fn a_connection_lost_twice_ends_the_poll_and_the_next_poll_reconnects() {
    let _clock = clock::manual();
    for close in [Close::Rst, Close::Fin] {
        let (endpoint, reader) = three_reads().await;
        // Dropped again on the new connection: the request fails.
        endpoint.inject(Fault::disconnect(close).address(100).times(2));
        let mut connection = connection(&endpoint);
        let poll = poll_with(&reader, &mut connection, async {}).await;
        assert_eq!(values(&poll, "meter-1"), only(&["a"]), "{close:?}");
        // "c" was not sent: the connection was gone.
        assert_eq!(
            failures(&poll),
            failed(&[
                ("b", FailureKind::Connect, None),
                ("c", FailureKind::Connect, None)
            ]),
            "{close:?}"
        );
        assert_eq!(poll.requests.len(), 2, "{close:?}");
        assert!(!connection.is_open());

        let poll = poll_with(&reader, &mut connection, async {}).await;
        assert_eq!(values(&poll, "meter-1"), all_read(), "{close:?}");
        assert_eq!(endpoint.connections().len(), 3, "{close:?}");
        assert_read_only(&endpoint);
    }
}

#[tokio::test(start_paused = true)]
async fn a_half_open_connection_times_out() {
    let _clock = clock::manual();
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[100]);
    endpoint.set_holding_registers(1, 100, &[200]);
    let reader = reader(
        &endpoint,
        "meter-1",
        1,
        vec![
            holding("a", 0, DataType::U16),
            holding("b", 100, DataType::U16),
        ],
    );
    // The device forgets the connection: no FIN, no RST, only silence.
    endpoint.inject(Fault::half_open().address(100).once());
    let mut connection = connection(&endpoint);
    let poll = poll_with(&reader, &mut connection, async {
        endpoint.wait_for_requests(2).await;
        advance(TIMEOUT).await;
    })
    .await;
    endpoint
        .wait_for_connection(ConnectionId(0), ConnectionState::Forgotten)
        .await;
    assert_eq!(
        values(&poll, "meter-1"),
        BTreeMap::from([("a".to_owned(), Value::Int(100))])
    );
    assert_eq!(
        failures(&poll),
        failed(&[("b", FailureKind::Timeout, None)])
    );
    assert_read_only(&endpoint);
}

#[tokio::test(start_paused = true)]
async fn a_broadcast_answer_for_another_unit_is_not_misattributed() {
    let _clock = clock::manual();
    // Two devices behind one Modbus TCP gateway, each polled over its own connection; the
    // gateway copies unit 1's answer to every connection (Laysi/trustiot#183).
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[2301]);
    endpoint.set_holding_registers(2, 0, &[500]);
    let meter = reader(
        &endpoint,
        "meter-1",
        1,
        vec![holding("value", 0, DataType::U16)],
    );
    let drive = reader(
        &endpoint,
        "drive-1",
        2,
        vec![holding("value", 0, DataType::U16)],
    );
    let mut meter_connection = connection(&endpoint);
    let mut drive_connection = connection(&endpoint);
    let value = |poll: &Poll, device: &str| values(poll, device).remove("value");

    let poll = poll_with(&meter, &mut meter_connection, async {}).await;
    assert_eq!(value(&poll, "meter-1"), Some(Value::Int(2301)));
    let poll = poll_with(&drive, &mut drive_connection, async {}).await;
    assert_eq!(value(&poll, "drive-1"), Some(Value::Int(500)));
    endpoint.wait_for_connections(2).await;

    // Unit 1's answer goes to the drive's connection too. It arrives before the drive's next
    // request, which drops it and counts it as a unit mismatch.
    endpoint.inject(Fault::broadcast().unit(1).once());
    let poll = poll_with(&meter, &mut meter_connection, async {}).await;
    assert_eq!(value(&poll, "meter-1"), Some(Value::Int(2301)));
    let poll = poll_with(&drive, &mut drive_connection, async {}).await;
    assert_eq!(value(&poll, "drive-1"), Some(Value::Int(500)));
    assert_eq!(poll.requests[0].unit_mismatches, 1);
    assert_read_only(&endpoint);
}
