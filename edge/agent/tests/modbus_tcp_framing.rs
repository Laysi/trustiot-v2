//! The Modbus TCP client against a hand-driven server socket: what reaches the client is
//! written byte for byte, so these tests pin down which frames count as the answer, that the
//! stream stays framed when a request times out in the middle of a frame, and when a dropped
//! connection is replaced. Timeouts follow a clock that moves only when a test advances it
//! (`testkit::clock::manual`), so a slow machine never makes a read time out.

use std::time::Duration;

use agent::modbus::pdu::{Data, Read};
use agent::modbus::tcp::TcpConnection;
use agent::modbus::{Connection, DEFAULT_CONNECT_TIMEOUT, Exchange, ReadError};
use protocol::config::{FunctionCode, Transport};
use testkit::clock::{self, bounded};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::advance;

const TIMEOUT: Duration = Duration::from_secs(5);

async fn listen() -> (TcpListener, TcpConnection) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    (
        listener,
        TcpConnection::new(
            Transport::ModbusTcp,
            "127.0.0.1",
            port,
            DEFAULT_CONNECT_TIMEOUT,
        ),
    )
}

async fn accept(listener: &TcpListener) -> TcpStream {
    bounded("a connection", listener.accept())
        .await
        .expect("an accepted connection")
        .0
}

/// Reads one request frame: its transaction identifier, unit and PDU.
async fn request(stream: &mut TcpStream) -> (u16, u8, [u8; 5]) {
    let mut frame = [0; 12];
    bounded("a request", stream.read_exact(&mut frame))
        .await
        .expect("a whole request frame");
    assert_eq!(&frame[2..6], [0, 0, 0, 6], "protocol 0, length 6");
    let pdu = [frame[7], frame[8], frame[9], frame[10], frame[11]];
    (u16::from_be_bytes([frame[0], frame[1]]), frame[6], pdu)
}

/// A response frame with holding register values.
fn registers(transaction: u16, unit: u8, values: &[u16]) -> Vec<u8> {
    let mut pdu = vec![
        3,
        u8::try_from(values.len() * 2).expect("at most 125 registers"),
    ];
    for value in values {
        pdu.extend(value.to_be_bytes());
    }
    let mut frame = transaction.to_be_bytes().to_vec();
    frame.extend([0, 0]);
    frame.extend(
        u16::try_from(pdu.len() + 1)
            .expect("a short PDU")
            .to_be_bytes(),
    );
    frame.push(unit);
    frame.extend(pdu);
    frame
}

fn holding(start: u16) -> Read {
    Read::new(FunctionCode::ReadHoldingRegisters, start, 1).expect("a valid range")
}

/// Reads holding register `start` of `unit` while `device` plays the device's side.
async fn read_with<T>(
    connection: &mut TcpConnection,
    unit: u8,
    start: u16,
    device: impl Future<Output = T>,
) -> (Exchange, T) {
    bounded("a read and the device's side of it", async {
        tokio::join!(connection.read(unit, holding(start), TIMEOUT), device)
    })
    .await
}

/// Accepts one connection, answers its first request with `value`, and returns the stream.
async fn answer_once(listener: &TcpListener, value: u16) -> TcpStream {
    let mut stream = accept(listener).await;
    let (transaction, unit, _) = request(&mut stream).await;
    stream
        .write_all(&registers(transaction, unit, &[value]))
        .await
        .expect("the answer is written");
    stream
}

#[tokio::test(start_paused = true)]
async fn only_the_matching_unit_and_transaction_is_the_answer() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        let (transaction, unit, pdu) = request(&mut stream).await;
        assert_eq!((unit, pdu), (1, [3, 0, 0, 0, 1]));
        let mut frames = registers(transaction.wrapping_sub(1), 1, &[111]); // a late answer
        frames.extend(registers(transaction, 9, &[999])); // another unit's answer
        frames.extend(registers(transaction, 1, &[222])); // the answer
        stream.write_all(&frames).await.unwrap();
        stream
    };
    let (exchange, _stream) = read_with(&mut connection, 1, 0, device).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![222])));
    assert_eq!(
        exchange.unit_mismatches, 1,
        "only the other unit's frame counts"
    );
}

#[tokio::test(start_paused = true)]
async fn a_response_that_arrives_byte_by_byte_is_reassembled() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        let (transaction, unit, _) = request(&mut stream).await;
        for byte in registers(transaction, unit, &[0xBEEF]) {
            stream.write_all(&[byte]).await.unwrap();
            stream.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        stream
    };
    let (exchange, _stream) = read_with(&mut connection, 7, 0, device).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![0xBEEF])));
}

#[tokio::test(start_paused = true)]
async fn a_timeout_in_the_middle_of_a_frame_keeps_the_stream_framed() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let (partial_sent, partial) = oneshot::channel();
    let device = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        // The first answer starts but stops after 5 of its 11 bytes.
        let (first, unit, _) = request(&mut stream).await;
        let late = registers(first, unit, &[111]);
        stream.write_all(&late[..5]).await.unwrap();
        partial_sent.send(()).unwrap();
        // The rest of it arrives after the second request, followed by the second answer.
        let (second, unit, pdu) = request(&mut stream).await;
        assert_eq!(pdu, [3, 0, 10, 0, 1]);
        let mut frames = late[5..].to_vec();
        frames.extend(registers(second, unit, &[222]));
        stream.write_all(&frames).await.unwrap();
        stream
    });

    let (exchange, ()) = read_with(&mut connection, 1, 0, async {
        bounded("the partial answer", partial).await.unwrap();
        advance(TIMEOUT).await;
    })
    .await;
    assert_eq!(exchange.result, Err(ReadError::Timeout));
    assert!(connection.is_open(), "a timeout keeps the connection");

    let (exchange, ()) = read_with(&mut connection, 1, 10, async {}).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![222])));
    assert_eq!(exchange.unit_mismatches, 0);
    drop(bounded("the device", device).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn a_device_that_closes_after_every_answer_is_read_every_time() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    // Each read finds the connection closed, either before sending (the close has arrived) or
    // after (the request meets the closed connection and is sent again on a new one); either
    // way the device sees one connection per read and every read gets its answer.
    for value in [11, 22, 33] {
        let (exchange, stream) =
            read_with(&mut connection, 1, 0, answer_once(&listener, value)).await;
        drop(stream);
        assert_eq!(exchange.result, Ok(Data::Registers(vec![value])));
    }
}

#[tokio::test(start_paused = true)]
async fn a_new_connection_that_fails_is_not_tried_again() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        request(&mut stream).await;
        // Closed without an answer.
    };
    let (exchange, ()) = read_with(&mut connection, 1, 0, device).await;
    assert!(
        matches!(&exchange.result, Err(ReadError::ConnectionLost(detail)) if detail.contains("closed")),
        "{exchange:?}"
    );
    assert!(!connection.is_open());
    // The read after it connects again.
    let (exchange, _stream) = read_with(&mut connection, 1, 0, answer_once(&listener, 7)).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![7])));
}

#[tokio::test(start_paused = true)]
async fn a_header_no_device_sends_is_a_protocol_failure() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        let (transaction, _, _) = request(&mut stream).await;
        let mut frame = registers(transaction, 1, &[1]);
        frame[3] = 1; // protocol identifier 1
        stream.write_all(&frame).await.unwrap();
        stream
    };
    let (exchange, _stream) = read_with(&mut connection, 1, 0, device).await;
    assert!(
        matches!(&exchange.result, Err(ReadError::Protocol(detail)) if detail.contains("protocol identifier 1")),
        "{exchange:?}"
    );
    assert!(!connection.is_open());

    // The next read opens a new connection.
    let (exchange, _stream) = read_with(&mut connection, 1, 0, answer_once(&listener, 42)).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![42])));
}

#[tokio::test(start_paused = true)]
async fn a_refused_connection_is_a_connect_failure() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    drop(listener);
    let exchange = bounded("a read", connection.read(1, holding(0), TIMEOUT)).await;
    assert!(
        matches!(exchange.result, Err(ReadError::Connect(_))),
        "{exchange:?}"
    );
    assert!(!connection.is_open());
}

#[tokio::test(start_paused = true)]
async fn an_exception_and_a_malformed_answer_keep_the_connection() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        let (transaction, unit, _) = request(&mut stream).await;
        let mut exception = transaction.to_be_bytes().to_vec();
        exception.extend([0, 0, 0, 3, unit, 0x83, 0x02]);
        stream.write_all(&exception).await.unwrap();
        let (transaction, unit, _) = request(&mut stream).await;
        // Two registers in the answer to a read of one.
        stream
            .write_all(&registers(transaction, unit, &[1, 2]))
            .await
            .unwrap();
        let (transaction, unit, _) = request(&mut stream).await;
        stream
            .write_all(&registers(transaction, unit, &[3]))
            .await
            .unwrap();
        stream
    };
    let client = async {
        let exception = connection.read(1, holding(0), TIMEOUT).await;
        let malformed = connection.read(1, holding(0), TIMEOUT).await;
        let normal = connection.read(1, holding(0), TIMEOUT).await;
        (exception, malformed, normal)
    };
    let ((exception, malformed, normal), _stream) =
        bounded("three reads", async { tokio::join!(client, device) }).await;
    assert_eq!(exception.result, Err(ReadError::Exception(2)));
    assert!(
        matches!(malformed.result, Err(ReadError::Malformed(_))),
        "{malformed:?}"
    );
    assert_eq!(normal.result, Ok(Data::Registers(vec![3])));
}
