//! The README's example: a Modbus client written by others (`tokio-modbus`) reads the fake
//! endpoint behind an RS485 converter and meets each fault in turn, the way an acquisition test
//! of the gateway will. Timeouts follow a clock that moves only when the test advances it.

use std::io::ErrorKind;
use std::time::Duration;

use testkit::clock::{self, bounded};
use testkit::modbus::{Close, ConnectionId, ConnectionState, FakeModbus, Fault, Transport};
use tokio::net::TcpStream;
use tokio::time::{advance, timeout};
use tokio_modbus::client::{Context, Reader, rtu};
use tokio_modbus::{Error, ExceptionCode, ProtocolError, Slave};

/// The client's response timeout. Under `clock::manual` it fires only when the test advances
/// the clock past it, never because the machine is slow.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Opens an RTU-over-TCP client for `unit`.
async fn client(endpoint: &FakeModbus, unit: u8) -> Context {
    let stream = bounded("a connection", TcpStream::connect(endpoint.address()))
        .await
        .expect("connect to the fake endpoint");
    rtu::attach_slave(stream, Slave(unit))
}

/// Reads holding registers 0 and 1 with the response timeout, while `meanwhile` moves the
/// clock or releases held responses.
async fn read_with(
    client: &mut Context,
    meanwhile: impl Future<Output = ()>,
) -> Result<tokio_modbus::Result<Vec<u16>>, tokio::time::error::Elapsed> {
    let read = timeout(RESPONSE_TIMEOUT, client.read_holding_registers(0, 2));
    let (result, ()) = bounded("a read", async { tokio::join!(read, meanwhile) }).await;
    result
}

/// Waits until the endpoint has `count` requests, then lets the response timeout pass.
async fn time_out_after(endpoint: &FakeModbus, count: usize) {
    endpoint.wait_for_requests(count).await;
    advance(RESPONSE_TIMEOUT).await;
}

#[tokio::test(start_paused = true)]
async fn read_the_fake_endpoint_and_trigger_each_fault() {
    let _clock = clock::manual();
    // An RS485 converter with two devices: unit 1 and unit 2.
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[2301, 2299]);
    endpoint.set_holding_registers(2, 0, &[500, 501]);
    let mut meter = client(&endpoint, 1).await;
    let mut drive = client(&endpoint, 2).await;

    // A normal read, and the second unit on the same converter.
    let read = read_with(&mut meter, async {}).await;
    assert_eq!(read.unwrap().unwrap(), Ok(vec![2301, 2299]));
    let read = read_with(&mut drive, async {}).await;
    assert_eq!(read.unwrap().unwrap(), Ok(vec![500, 501]));

    // Exception: the device answers with exception 04.
    endpoint.inject(Fault::exception(0x04).once());
    let read = read_with(&mut meter, async {}).await;
    assert_eq!(
        read.unwrap().unwrap(),
        Err(ExceptionCode::ServerDeviceFailure)
    );

    // Unit mismatch: the answer comes back labelled unit 2.
    endpoint.inject(Fault::unit_mismatch(2).once());
    let read = read_with(&mut meter, async {}).await;
    assert!(matches!(
        read.unwrap(),
        Err(Error::Protocol(ProtocolError::HeaderMismatch { .. }))
    ));

    // Delay shorter than the timeout: the answer arrives in time.
    endpoint.inject(Fault::delay(Duration::from_secs(2)).once());
    let count = endpoint.requests().len() + 1;
    let read = read_with(&mut meter, async {
        endpoint.wait_for_requests(count).await;
        advance(Duration::from_secs(2)).await;
    })
    .await;
    assert_eq!(read.unwrap().unwrap(), Ok(vec![2301, 2299]));

    // No response: the client times out, and the connection stays usable (Laysi/trustiot#159).
    endpoint.inject(Fault::no_response().once());
    let count = endpoint.requests().len() + 1;
    let read = read_with(&mut meter, time_out_after(&endpoint, count)).await;
    assert!(read.is_err(), "no answer, so the timeout fires");
    let read = read_with(&mut meter, async {}).await;
    assert_eq!(read.unwrap().unwrap(), Ok(vec![2301, 2299]));

    // Broadcast (Laysi/trustiot#183): the converter copies unit 1's answer to every connection,
    // so the drive's client, waiting for unit 2 on the same bus, reads unit 1's frame.
    endpoint.inject(Fault::broadcast().held().unit(1).once());
    let held = endpoint.requests().len();
    let release = async {
        endpoint.wait_for_requests(held + 2).await;
        endpoint.release(held);
    };
    let (meter_read, drive_read) = tokio::join!(
        read_with(&mut meter, release),
        read_with(&mut drive, async {}),
    );
    assert_eq!(meter_read.unwrap().unwrap(), Ok(vec![2301, 2299]));
    assert!(matches!(
        drive_read.unwrap(),
        Err(Error::Protocol(ProtocolError::HeaderMismatch { .. }))
    ));

    // Disconnect with RST, then with FIN: the client sees a reset, then the end of the stream.
    let mut meter = client(&endpoint, 1).await;
    endpoint.inject(Fault::disconnect(Close::Rst).once());
    let read = read_with(&mut meter, async {}).await;
    assert!(matches!(
        read.unwrap(),
        Err(Error::Transport(error)) if error.kind() == ErrorKind::ConnectionReset
    ));
    let mut meter = client(&endpoint, 1).await;
    endpoint.inject(Fault::disconnect(Close::Fin).once());
    let read = read_with(&mut meter, async {}).await;
    assert!(matches!(
        read.unwrap(),
        Err(Error::Transport(error)) if error.kind() == ErrorKind::BrokenPipe
    ));

    // Half-open: the converter forgets the connection; only the client's timeout notices.
    let mut meter = client(&endpoint, 1).await;
    endpoint.inject(Fault::half_open().once());
    let count = endpoint.requests().len() + 1;
    let read = read_with(&mut meter, time_out_after(&endpoint, count)).await;
    assert!(read.is_err(), "nothing ever comes back");
    // The fifth connection: meter, drive, and the two that were disconnected came before it.
    endpoint
        .wait_for_connection(ConnectionId(4), ConnectionState::Forgotten)
        .await;
    let mut meter = client(&endpoint, 1).await;
    let read = read_with(&mut meter, async {}).await;
    assert_eq!(read.unwrap().unwrap(), Ok(vec![2301, 2299]));

    // The client only ever read: no function code above 4 reached the converter.
    assert!(
        endpoint
            .requests()
            .iter()
            .all(|request| (1..=4).contains(&request.function))
    );
}
