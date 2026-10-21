//! The RTU-over-TCP client against a hand-driven converter socket: what reaches the client is
//! written byte for byte, so these tests pin down the request bytes on the wire, which frames
//! count as the answer, that noise and a frame cut short never hide the answer, and when a
//! dropped connection is replaced. Timeouts follow a clock that moves only when a test advances
//! it (`testkit::clock::manual`), so a slow machine never makes a read time out.

use std::time::Duration;

use agent::modbus::pdu::{Data, Read};
use agent::modbus::tcp::TcpConnection;
use agent::modbus::{Connection, DEFAULT_CONNECT_TIMEOUT, Exchange, ReadError};
use protocol::config::{FunctionCode, Transport};
use protocol::health::FailureKind;
use testkit::clock::{self, bounded};
use testkit::modbus::crc16;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
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
            Transport::RtuOverTcp,
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

/// `unit`, `pdu` and their CRC, low byte first (computed by testkit, not by the agent).
fn frame(unit: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = vec![unit];
    frame.extend(pdu);
    frame.extend(crc16(&frame).to_le_bytes());
    frame
}

/// Reads one request frame and checks its CRC: returns the whole frame.
async fn request(stream: &mut TcpStream) -> [u8; 8] {
    let mut frame = [0; 8];
    bounded("a request", stream.read_exact(&mut frame))
        .await
        .expect("a whole request frame");
    assert_eq!(
        frame[6..],
        crc16(&frame[..6]).to_le_bytes(),
        "the CRC of {frame:02X?}"
    );
    frame
}

/// A response frame with holding register values.
fn registers(unit: u8, values: &[u16]) -> Vec<u8> {
    let mut pdu = vec![
        3,
        u8::try_from(values.len() * 2).expect("at most 125 registers"),
    ];
    for value in values {
        pdu.extend(value.to_be_bytes());
    }
    frame(unit, &pdu)
}

fn holding(start: u16) -> Read {
    Read::new(FunctionCode::ReadHoldingRegisters, start, 1).expect("a valid range")
}

/// Reads holding register `start` of `unit` while `device` plays the converter's side.
async fn read_with<T>(
    connection: &mut TcpConnection,
    unit: u8,
    start: u16,
    device: impl Future<Output = T>,
) -> (Exchange, T) {
    bounded("a read and the converter's side of it", async {
        tokio::join!(connection.read(unit, holding(start), TIMEOUT), device)
    })
    .await
}

/// Accepts one connection, answers its first request with `value` in `before` and then the
/// answer, in one write, and returns the stream.
async fn answer_after(listener: &TcpListener, before: &[u8], value: u16) -> TcpStream {
    let mut stream = accept(listener).await;
    let sent = request(&mut stream).await;
    let mut bytes = before.to_vec();
    bytes.extend(registers(sent[0], &[value]));
    stream
        .write_all(&bytes)
        .await
        .expect("the answer is written");
    stream
}

#[tokio::test(start_paused = true)]
async fn requests_are_rtu_frames_on_the_wire() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let reads = [
        // The specification's example: read 10 holding registers of unit 1 from address 0.
        (
            1,
            Read::new(FunctionCode::ReadHoldingRegisters, 0, 10).unwrap(),
        ),
        (
            17,
            Read::new(FunctionCode::ReadCoils, 0x0013, 0x0013).unwrap(),
        ),
        (
            247,
            Read::new(FunctionCode::ReadDiscreteInputs, 0xFFFF, 1).unwrap(),
        ),
        (
            2,
            Read::new(FunctionCode::ReadInputRegisters, 0x0102, 125).unwrap(),
        ),
    ];
    let device = async {
        let mut stream = accept(&listener).await;
        let mut sent = Vec::new();
        for (_, read) in reads {
            let bytes = request(&mut stream).await;
            sent.push(bytes);
            // An exception keeps the answers short: function code with the high bit set.
            let answer = frame(bytes[0], &[read.function().number() | 0x80, 2]);
            stream.write_all(&answer).await.unwrap();
        }
        (stream, sent)
    };
    let client = async {
        let mut results = Vec::new();
        for (unit, read) in reads {
            results.push(connection.read(unit, read, TIMEOUT).await.result);
        }
        results
    };
    let (results, (_stream, sent)) =
        bounded("four reads", async { tokio::join!(client, device) }).await;
    assert!(
        results
            .iter()
            .all(|result| *result == Err(ReadError::Exception(2))),
        "{results:?}"
    );
    assert_eq!(
        sent[0],
        [0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5, 0xCD],
        "the specification's example"
    );
    assert_eq!(sent[1][..6], [17, 1, 0x00, 0x13, 0x00, 0x13]);
    assert_eq!(sent[2][..6], [247, 2, 0xFF, 0xFF, 0x00, 0x01]);
    assert_eq!(sent[3][..6], [2, 4, 0x01, 0x02, 0x00, 0x7D]);
}

#[tokio::test(start_paused = true)]
async fn an_echo_another_units_answer_and_the_answer_in_one_write() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let device = async {
        let mut stream = accept(&listener).await;
        let sent = request(&mut stream).await;
        // A converter that echoes the request, unit 9's answer copied from the bus, the answer.
        let mut bytes = sent.to_vec();
        bytes.extend(registers(9, &[999]));
        bytes.extend(registers(1, &[222]));
        stream.write_all(&bytes).await.unwrap();
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
async fn a_frame_cut_before_the_next_request_does_not_hide_its_answer() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let (partial_sent, partial) = oneshot::channel();
    let device = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        // The first answer starts but stops after 3 of its 7 bytes.
        let sent = request(&mut stream).await;
        let late = registers(sent[0], &[111]);
        stream.write_all(&late[..3]).await.unwrap();
        partial_sent.send(()).unwrap();
        // The rest of it arrives after the second request, followed by the second answer. Kept
        // with its first part, the late answer would be a whole frame of the same shape as the
        // second request's answer.
        let sent = request(&mut stream).await;
        assert_eq!(sent[1..6], [3, 0, 10, 0, 1]);
        let mut bytes = late[3..].to_vec();
        bytes.extend(registers(sent[0], &[222]));
        stream.write_all(&bytes).await.unwrap();
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
    drop(bounded("the converter", device).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn a_kept_open_connection_closed_during_a_request_is_replaced() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let (exchange, first) = read_with(&mut connection, 1, 0, answer_after(&listener, &[], 7)).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![7])));

    // The converter closes the connection it kept open after reading the next request, without
    // answering; the request goes out again on a new connection.
    let device = async {
        let mut first = first;
        let dropped = request(&mut first).await;
        drop(first);
        let mut second = accept(&listener).await;
        let again = request(&mut second).await;
        second.write_all(&registers(1, &[8])).await.unwrap();
        (dropped, again, second)
    };
    let (exchange, (dropped, again, _second)) = read_with(&mut connection, 1, 5, device).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![8])));
    assert_eq!(dropped, again, "the same request, sent again");
    assert_eq!(dropped[..6], [1, 3, 0, 5, 0, 1]);
}

#[tokio::test(start_paused = true)]
async fn noise_and_frames_with_a_bad_crc_before_the_answer_are_skipped() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let mut corrupted = registers(1, &[111]);
    if let Some(last) = corrupted.last_mut() {
        *last ^= 0x5A;
    }
    // Line noise, a frame whose CRC is wrong, and bytes that read like the start of a 245-byte
    // frame of unit 1, all before the answer.
    let mut before = vec![0x00, 0xFF, 0x13];
    before.extend(&corrupted);
    before.extend([0x01, 0x03, 0xF0]);
    let (exchange, _stream) =
        read_with(&mut connection, 1, 0, answer_after(&listener, &before, 222)).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![222])));
    assert_eq!(exchange.unit_mismatches, 0);
}

#[tokio::test(start_paused = true)]
async fn what_arrives_instead_of_an_answer_decides_the_failure() {
    let _clock = clock::manual();
    let (listener, mut connection) = listen().await;
    let (written, mut sent) = mpsc::unbounded_channel();
    let device = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        for bytes in [
            // Bytes that never form a frame, as from a converter at another baud rate.
            vec![0x55, 0x00, 0xAA, 0x7F, 0x13, 0x00],
            // A frame of the unit with two registers for a read of one.
            registers(1, &[1, 2]),
            // Only another unit's answer.
            registers(2, &[3]),
        ] {
            request(&mut stream).await;
            stream.write_all(&bytes).await.unwrap();
            written.send(()).unwrap();
        }
        // Then a normal answer.
        let sent = request(&mut stream).await;
        stream.write_all(&registers(sent[0], &[4])).await.unwrap();
        stream
    });
    let mut results = Vec::new();
    for _ in 0..3 {
        let (exchange, ()) = read_with(&mut connection, 1, 0, async {
            // The bytes are on their way before the deadline passes: the clock moves only here.
            bounded("the converter's bytes", sent.recv()).await.unwrap();
            advance(TIMEOUT).await;
        })
        .await;
        results.push(exchange);
    }

    let noise = &results[0].result;
    assert!(
        matches!(noise, Err(ReadError::Malformed(detail))
            if detail.contains("form no RTU frame") && detail.contains("serial settings")),
        "{noise:?}"
    );
    let unfit = &results[1].result;
    assert!(
        matches!(unfit, Err(ReadError::Malformed(detail))
            if detail.contains("did not fit") && detail.contains("4 data bytes for 1 registers")),
        "{unfit:?}"
    );
    for result in [noise, unfit] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.kind(), FailureKind::InvalidValue);
        assert!(!error.ends_poll(), "the poll goes on");
    }
    assert_eq!(results[2].result, Err(ReadError::Timeout));
    assert_eq!(results[2].unit_mismatches, 1);
    // The connection stayed open through all three, and carries the next read.
    let (exchange, ()) = read_with(&mut connection, 1, 0, async {}).await;
    assert_eq!(exchange.result, Ok(Data::Registers(vec![4])));
    drop(bounded("the converter", device).await.unwrap());
}
