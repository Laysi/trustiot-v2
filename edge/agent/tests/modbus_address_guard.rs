//! The address guard on the path the agent polls through (Laysi/trustiot-v2#653): a device whose
//! host is the gateway itself, or a cloud metadata address, gets a `connect` failure from
//! `Endpoints::new` and `TcpConnection::new`, and nothing reaches the fake endpoint listening on
//! the gateway's loopback. The same endpoint answers once a test exempts 127.0.0.1, which only
//! test code can do. The rules themselves, names that resolve to several addresses, and that a
//! name is resolved once are tested in `src/modbus/address.rs` and `src/modbus/tcp.rs`.

use std::net::Ipv4Addr;
use std::time::Duration;

use agent::modbus::endpoint::Endpoints;
use agent::modbus::pdu::Read;
use agent::modbus::tcp::TcpConnection;
use agent::modbus::{Clock, Connection, DeviceReader, Poll, ReadError, Timestamp};
use protocol::config::{
    ByteOrder, DataPoint, DataType, Device, DeviceModel, FunctionCode, Host, Transport,
};
use protocol::health::FailureKind;
use protocol::{DeviceId, ModelName, PointKey, UnixMillis, Value};
use testkit::clock::bounded;
use testkit::modbus::{self as fake, FakeModbus};

/// Connect and request timeout: long enough never to fire on a loaded machine. A refusal comes
/// before any wait, and the endpoint answers at once.
const TIMEOUT: Duration = Duration::from_secs(60);
/// What every refusal starts with.
const REFUSED: &str =
    "refused: every address the host resolved to points at the gateway itself or is not allowed";

struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(1_760_054_400_000),
            synced: true,
        }
    }
}

/// A meter at `host` and the endpoint's port, with one holding register.
fn meter(endpoint: &FakeModbus, host: &str) -> (Device, DeviceReader) {
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
        host: Host::new(host).expect("a valid host"),
        port: endpoint.address().port(),
        unit_id: 1,
        poll_interval_ms: 1000,
    };
    let reader = DeviceReader::new(&device, &model).expect("a plan for a valid model");
    (device, reader)
}

async fn poll(endpoints: &mut Endpoints, device: &Device, reader: &DeviceReader) -> Poll {
    let mut connection = endpoints.connection(device);
    bounded("a poll", reader.poll(&mut connection, &TestClock, TIMEOUT)).await
}

#[tokio::test]
async fn hosts_on_the_gateway_itself_reach_nothing() {
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[2301]);

    // Hosts a device configuration can hold: a validated `Host` accepts each of them.
    let mut endpoints = Endpoints::new(TIMEOUT);
    for host in [
        "127.0.0.1",
        "127.0.0.2",
        "::1",
        "::ffff:127.0.0.1",
        "localhost",
        "169.254.169.254",
        "fd00:ec2::254",
    ] {
        let (device, reader) = meter(&endpoint, host);
        let poll = poll(&mut endpoints, &device, &reader).await;
        assert!(poll.readings.is_empty(), "{host}: {poll:?}");
        let [failed] = &poll.failed[..] else {
            panic!("{host}: one failed point: {poll:?}");
        };
        assert_eq!(failed.failure.kind, FailureKind::Connect, "{host}");
        let detail = failed.failure.detail.as_deref().unwrap_or_default();
        assert!(detail.starts_with(REFUSED), "{host}: {detail}");
    }

    // Hosts that configuration validation refuses go through the same check when they come by
    // another way.
    let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 1).expect("a valid range");
    for host in ["0.0.0.0", "::", "255.255.255.255", "224.0.0.1", "ff02::1"] {
        let mut connection = TcpConnection::new(
            Transport::ModbusTcp,
            host,
            endpoint.address().port(),
            TIMEOUT,
        );
        let exchange = bounded("a read", connection.read(1, read, TIMEOUT)).await;
        assert!(
            matches!(&exchange.result, Err(ReadError::Connect(detail)) if detail.starts_with(REFUSED)),
            "{host}: {exchange:?}"
        );
        assert!(!connection.is_open(), "{host}");
    }

    // The control: with 127.0.0.1 exempted, the same device reads its value, through the first
    // connection the endpoint accepted. A connection a refused host had opened would have
    // been accepted before it.
    let mut exempted = Endpoints::new(TIMEOUT).exempt_for_tests([Ipv4Addr::LOCALHOST.into()]);
    let (device, reader) = meter(&endpoint, "127.0.0.1");
    let poll = poll(&mut exempted, &device, &reader).await;
    let values: Vec<&Value> = poll
        .readings
        .iter()
        .map(|reading| &reading.sample.value)
        .collect();
    assert_eq!(values, [&Value::Int(2301)], "{poll:?}");
    assert_eq!(
        endpoint.connections().len(),
        1,
        "{:?}",
        endpoint.connections()
    );
    assert_eq!(endpoint.requests().len(), 1);
}
