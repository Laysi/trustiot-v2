//! The fake Modbus endpoint, byte for byte: what it answers on each transport, the request log,
//! and one test per fault. Clients here are raw sockets, so every frame is checked exactly; the
//! walk-through with a Modbus client is `modbus_example.rs`.
//!
//! No test waits a fixed time: absence of an answer is shown by ordering (a later request's
//! answer arrives first), and every wait is bounded by `clock::WAIT_LIMIT`.

use std::io::ErrorKind;
use std::time::Duration;

use testkit::clock::{self, bounded};
use testkit::modbus::{Close, ConnectionId, ConnectionState, FakeModbus, Fault, Transport, crc16};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BOTH: [Transport; 2] = [Transport::ModbusTcp, Transport::RtuOverTcp];

/// A Modbus TCP frame.
fn mbap(transaction: u16, unit: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = transaction.to_be_bytes().to_vec();
    frame.extend([0, 0]);
    frame.extend(
        u16::try_from(pdu.len() + 1)
            .expect("a PDU fits the MBAP length field")
            .to_be_bytes(),
    );
    frame.push(unit);
    frame.extend(pdu);
    frame
}

/// An RTU frame with its CRC, low byte first.
fn rtu(unit: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = vec![unit];
    frame.extend(pdu);
    let crc = crc16(&frame);
    frame.extend(crc.to_le_bytes());
    frame
}

/// A read request PDU.
fn read(function: u8, address: u16, quantity: u16) -> Vec<u8> {
    let mut pdu = vec![function];
    pdu.extend(address.to_be_bytes());
    pdu.extend(quantity.to_be_bytes());
    pdu
}

/// A frame for `transport`; `transaction` is used by Modbus TCP only.
fn frame(transport: Transport, transaction: u16, unit: u8, pdu: &[u8]) -> Vec<u8> {
    match transport {
        Transport::ModbusTcp => mbap(transaction, unit, pdu),
        Transport::RtuOverTcp => rtu(unit, pdu),
    }
}

/// A register read response PDU.
fn registers(function: u8, values: &[u16]) -> Vec<u8> {
    let mut pdu = vec![
        function,
        u8::try_from(values.len() * 2).expect("a read response fits its byte count"),
    ];
    pdu.extend(values.iter().flat_map(|value| value.to_be_bytes()));
    pdu
}

async fn connect(endpoint: &FakeModbus) -> TcpStream {
    bounded(
        "a connection to the fake endpoint",
        TcpStream::connect(endpoint.address()),
    )
    .await
    .expect("connect to the fake endpoint")
}

async fn send(stream: &mut TcpStream, bytes: &[u8]) {
    stream
        .write_all(bytes)
        .await
        .expect("send to the fake endpoint");
}

/// Reads one response frame: MBAP by its length field, RTU by its function code.
async fn response(transport: Transport, stream: &mut TcpStream) -> Vec<u8> {
    bounded(format!("a {transport:?} response"), async {
        match transport {
            Transport::ModbusTcp => {
                let mut frame = vec![0; 6];
                stream
                    .read_exact(&mut frame)
                    .await
                    .expect("read an MBAP header");
                let length = usize::from(u16::from_be_bytes([frame[4], frame[5]]));
                frame.resize(6 + length, 0);
                stream
                    .read_exact(&mut frame[6..])
                    .await
                    .expect("read an MBAP body");
                frame
            }
            Transport::RtuOverTcp => {
                let mut frame = vec![0; 3];
                stream
                    .read_exact(&mut frame)
                    .await
                    .expect("read the start of an RTU response");
                // Unit, function code, then the exception code or the byte count.
                let rest = if frame[1] & 0x80 == 0 {
                    usize::from(frame[2]) + 2
                } else {
                    2
                };
                frame.resize(3 + rest, 0);
                stream
                    .read_exact(&mut frame[3..])
                    .await
                    .expect("read the rest of an RTU response");
                frame
            }
        }
    })
    .await
}

/// Asserts that nothing is waiting on `stream`: no bytes and no end of stream. Peeks with a
/// system call on the std socket, because tokio's `try_read` trusts the readiness its driver
/// last saw and could miss a FIN that just arrived.
fn assert_silent(stream: TcpStream, why: &str) -> TcpStream {
    let stream = stream.into_std().expect("take the std socket");
    let mut byte = [0];
    match stream.peek(&mut byte) {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("{why}: expected nothing to read, got {other:?}"),
    }
    TcpStream::from_std(stream).expect("register the socket again")
}

#[tokio::test]
async fn function_codes_1_to_4_are_served_on_both_transports() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_coils(
            1,
            10,
            &[true, false, true, true, false, false, true, true, true],
        );
        endpoint.set_discrete_inputs(1, 0, &[false, true]);
        endpoint.set_holding_registers(1, 100, &[0x1234, 0xABCD]);
        endpoint.set_input_registers(1, 0, &[7]);
        let mut client = connect(&endpoint).await;

        let cases: [(Vec<u8>, Vec<u8>); 4] = [
            (read(1, 10, 9), vec![1, 2, 0b1100_1101, 0b0000_0001]),
            (read(2, 0, 2), vec![2, 1, 0b10]),
            (read(3, 100, 2), registers(3, &[0x1234, 0xABCD])),
            (read(4, 0, 1), registers(4, &[7])),
        ];
        for (transaction, (request, answer)) in (1..).zip(cases) {
            send(&mut client, &frame(transport, transaction, 1, &request)).await;
            assert_eq!(
                response(transport, &mut client).await,
                frame(transport, transaction, 1, &answer),
                "{transport:?}, request {request:?}"
            );
        }
    }
}

#[tokio::test]
async fn each_unit_has_its_own_tables() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[11]);
        endpoint.set_holding_registers(2, 0, &[22]);
        let mut client = connect(&endpoint).await;
        for (transaction, unit, value) in [(1, 2, 22), (2, 1, 11)] {
            send(
                &mut client,
                &frame(transport, transaction, unit, &read(3, 0, 1)),
            )
            .await;
            assert_eq!(
                response(transport, &mut client).await,
                frame(transport, transaction, unit, &registers(3, &[value])),
                "{transport:?}, unit {unit}"
            );
        }
    }
}

#[tokio::test]
async fn requests_outside_the_served_range_get_standard_exceptions() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[0; 125]);
        let mut client = connect(&endpoint).await;
        let cases: [(Vec<u8>, [u8; 2]); 4] = [
            // Write single register: the endpoint only serves reads.
            (vec![6, 0, 0, 0, 1], [0x86, 0x01]),
            // Address 125 was never set.
            (read(3, 1, 125), [0x83, 0x02]),
            // More than 125 registers.
            (read(3, 0, 126), [0x83, 0x03]),
            // More than 2000 bits.
            (read(1, 0, 2001), [0x81, 0x03]),
        ];
        for (transaction, (request, exception)) in (1..).zip(cases) {
            send(&mut client, &frame(transport, transaction, 1, &request)).await;
            assert_eq!(
                response(transport, &mut client).await,
                frame(transport, transaction, 1, &exception),
                "{transport:?}, request {request:?}"
            );
        }
        // The log shows what a client sent, so a test can prove it never wrote.
        let functions: Vec<u8> = endpoint
            .requests()
            .iter()
            .map(|request| request.function)
            .collect();
        assert_eq!(functions, [6, 3, 3, 1]);
    }
}

#[tokio::test]
async fn an_unknown_unit_is_silent_on_rtu_and_exception_0b_on_tcp() {
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[5]);
    let mut client = connect(&endpoint).await;
    send(&mut client, &rtu(9, &read(3, 0, 1))).await;
    send(&mut client, &rtu(1, &read(3, 0, 1))).await;
    assert_eq!(
        response(Transport::RtuOverTcp, &mut client).await,
        rtu(1, &registers(3, &[5])),
        "unit 9 is not on the bus, so the first frame back answers unit 1"
    );

    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    let mut client = connect(&endpoint).await;
    send(&mut client, &mbap(1, 9, &read(3, 0, 1))).await;
    assert_eq!(
        response(Transport::ModbusTcp, &mut client).await,
        mbap(1, 9, &[0x83, 0x0B])
    );
}

#[tokio::test]
async fn a_frame_with_a_bad_crc_is_logged_and_never_answered() {
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[1, 2]);
    endpoint.inject(Fault::exception(4));
    let mut client = connect(&endpoint).await;
    let mut corrupted = rtu(1, &read(3, 0, 1));
    *corrupted.last_mut().unwrap() ^= 0xFF;
    send(&mut client, &corrupted).await;
    endpoint.wait_for_requests(1).await;
    endpoint.clear_faults();
    send(&mut client, &rtu(1, &read(3, 1, 1))).await;
    assert_eq!(
        response(Transport::RtuOverTcp, &mut client).await,
        rtu(1, &registers(3, &[2]))
    );
    let log = endpoint.requests();
    assert!(!log[0].checksum_ok);
    assert_eq!(log[0].fault, None, "a bad frame never matches a fault");
    assert!(log[1].checksum_ok);
}

#[tokio::test]
async fn rtu_requests_are_answered_however_tcp_splits_or_joins_them() {
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[7]);
    let mut client = connect(&endpoint).await;
    // Diagnostics in two writes, which the endpoint may read separately; the frame unit tests
    // cover every split point.
    let diagnostics = rtu(1, &[8, 0, 2, 0, 0]);
    let (head, tail) = diagnostics.split_at(4);
    send(&mut client, head).await;
    send(&mut client, tail).await;
    assert_eq!(
        response(Transport::RtuOverTcp, &mut client).await,
        rtu(1, &[0x88, 0x01])
    );
    // A user-defined function code, whose end only its CRC marks, and a read in one write.
    let mut joined = rtu(1, &[65, 1, 2, 3]);
    joined.extend(rtu(1, &read(3, 0, 1)));
    send(&mut client, &joined).await;
    assert_eq!(
        response(Transport::RtuOverTcp, &mut client).await,
        rtu(1, &[0xC1, 0x01])
    );
    assert_eq!(
        response(Transport::RtuOverTcp, &mut client).await,
        rtu(1, &registers(3, &[7]))
    );
    let log: Vec<(u8, bool)> = endpoint
        .requests()
        .iter()
        .map(|request| (request.function, request.checksum_ok))
        .collect();
    assert_eq!(log, [(8, true), (65, true), (3, true)]);
}

#[tokio::test]
async fn the_request_log_records_every_frame() {
    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    endpoint.set_input_registers(3, 40, &[1, 2, 3]);
    let fault = Fault::exception(6).nth(1);
    endpoint.inject(fault.clone());
    let mut first = connect(&endpoint).await;
    let mut second = connect(&endpoint).await;
    let request = mbap(0x0102, 3, &read(4, 40, 3));
    send(&mut first, &request).await;
    response(Transport::ModbusTcp, &mut first).await;
    send(&mut second, &mbap(9, 3, &read(4, 41, 1))).await;
    response(Transport::ModbusTcp, &mut second).await;

    let log = endpoint.wait_for_requests(2).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].index, 0);
    assert_eq!(log[0].connection, ConnectionId(0));
    assert_eq!(log[0].unit, 3);
    assert_eq!(log[0].function, 4);
    assert_eq!(log[0].range, Some((40, 3)));
    assert_eq!(log[0].transaction_id, Some(0x0102));
    assert_eq!(log[0].raw, request);
    assert_eq!(log[0].fault, None);
    assert_eq!(log[1].connection, ConnectionId(1));
    assert_eq!(log[1].fault, Some(fault));
}

#[tokio::test]
async fn an_mbap_header_no_client_sends_closes_the_connection() {
    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    let mut client = connect(&endpoint).await;
    // Protocol identifier 1.
    send(&mut client, &[0, 1, 0, 1, 0, 6, 1, 3, 0, 0, 0, 1]).await;
    let mut byte = [0];
    let read = bounded("end of stream", client.read(&mut byte)).await;
    assert_eq!(read.unwrap(), 0);
    endpoint
        .wait_for_connection(ConnectionId(0), ConnectionState::Closed)
        .await;
}

#[tokio::test(start_paused = true)]
async fn fault_delay_answers_once_the_clock_reaches_the_delay() {
    // The clock moves only through `advance`, so the client's timeout and the endpoint's delay
    // fire in the order of their deadlines, however loaded the machine is.
    let _clock = clock::manual();
    let client_timeout = Duration::from_secs(3);
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[42]);
        endpoint.inject(Fault::delay(Duration::from_secs(2)).nth(0));
        endpoint.inject(Fault::delay(Duration::from_secs(5)).nth(1));

        // 2 s delay, 3 s timeout: the answer comes in time.
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        let answer = tokio::time::timeout(client_timeout, response(transport, &mut client));
        let (answer, ()) = tokio::join!(answer, async {
            endpoint.wait_for_requests(1).await;
            tokio::time::advance(Duration::from_secs(2)).await;
        });
        assert_eq!(
            answer.expect("the 2 s delay ends before the 3 s timeout"),
            frame(transport, 1, 1, &registers(3, &[42]))
        );

        // 5 s delay, 3 s timeout: the client gives up first, and the answer arrives late.
        send(&mut client, &frame(transport, 2, 1, &read(3, 0, 1))).await;
        let answer = tokio::time::timeout(client_timeout, response(transport, &mut client));
        let (answer, ()) = tokio::join!(answer, async {
            endpoint.wait_for_requests(2).await;
            tokio::time::advance(client_timeout).await;
        });
        assert!(
            answer.is_err(),
            "{transport:?}: the 3 s timeout fires first"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            response(transport, &mut client).await,
            frame(transport, 2, 1, &registers(3, &[42])),
            "{transport:?}: the late answer"
        );
    }
}

#[tokio::test]
async fn fault_hold_answers_when_the_test_releases_it() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[1, 2]);
        endpoint.inject(Fault::hold().nth(0));
        let mut held = connect(&endpoint).await;
        send(&mut held, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        endpoint.wait_for_requests(1).await;
        let mut other = connect(&endpoint).await;
        send(&mut other, &frame(transport, 2, 1, &read(3, 1, 1))).await;
        endpoint.wait_for_requests(2).await;
        if transport == Transport::ModbusTcp {
            // Connections are independent: the other one is answered while the first waits.
            assert_eq!(
                response(transport, &mut other).await,
                frame(transport, 2, 1, &registers(3, &[2]))
            );
            held = assert_silent(held, "the held request");
        }
        endpoint.release(0);
        assert_eq!(
            response(transport, &mut held).await,
            frame(transport, 1, 1, &registers(3, &[1])),
            "{transport:?}"
        );
        if transport == Transport::RtuOverTcp {
            // One bus: the second request waited behind the held one.
            assert_eq!(
                response(transport, &mut other).await,
                frame(transport, 2, 1, &registers(3, &[2]))
            );
        }
    }
}

#[tokio::test]
async fn release_before_the_hold_lets_the_answer_through() {
    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[1]);
    endpoint.inject(Fault::hold());
    endpoint.release(0);
    let mut client = connect(&endpoint).await;
    send(&mut client, &mbap(1, 1, &read(3, 0, 1))).await;
    assert_eq!(
        response(Transport::ModbusTcp, &mut client).await,
        mbap(1, 1, &registers(3, &[1]))
    );
}

#[tokio::test]
async fn fault_no_response_leaves_the_connection_healthy() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[10, 20]);
        endpoint.inject(Fault::no_response().once());
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        send(&mut client, &frame(transport, 2, 1, &read(3, 1, 1))).await;
        assert_eq!(
            response(transport, &mut client).await,
            frame(transport, 2, 1, &registers(3, &[20])),
            "{transport:?}: the first frame back answers the second request"
        );
        assert_eq!(
            endpoint.connections()[&ConnectionId(0)],
            ConnectionState::Open
        );
        assert!(endpoint.requests()[0].fault.is_some());
    }
}

#[tokio::test]
async fn fault_unit_mismatch_answers_with_another_unit() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[99]);
        endpoint.inject(Fault::unit_mismatch(7).once());
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut client).await,
            frame(transport, 1, 7, &registers(3, &[99])),
            "{transport:?}: unit 1's answer, labelled unit 7"
        );
    }
}

#[tokio::test]
async fn fault_exception_answers_with_the_chosen_code() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[1]);
        endpoint.inject(Fault::exception(0x04).function(3).once());
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut client).await,
            frame(transport, 1, 1, &[0x83, 0x04]),
            "{transport:?}"
        );
        send(&mut client, &frame(transport, 2, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut client).await,
            frame(transport, 2, 1, &registers(3, &[1])),
            "{transport:?}: once"
        );
    }
}

#[tokio::test]
async fn fault_disconnect_fin_ends_the_stream() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[1]);
        endpoint.inject(Fault::disconnect(Close::Fin).once());
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        let mut byte = [0];
        let read_result = bounded("end of stream", client.read(&mut byte)).await;
        assert_eq!(read_result.unwrap(), 0, "{transport:?}: FIN, no answer");
        endpoint
            .wait_for_connection(ConnectionId(0), ConnectionState::FinSent)
            .await;

        // What the client still sends is logged and not answered.
        send(&mut client, &frame(transport, 2, 1, &read(3, 0, 1))).await;
        let log = endpoint.wait_for_requests(2).await;
        assert_eq!(log[1].connection, ConnectionId(0));
        drop(client);
        endpoint
            .wait_for_connection(ConnectionId(0), ConnectionState::Closed)
            .await;

        let mut next = connect(&endpoint).await;
        send(&mut next, &frame(transport, 3, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut next).await,
            frame(transport, 3, 1, &registers(3, &[1])),
            "{transport:?}: a new connection works"
        );
    }
}

#[tokio::test]
async fn fault_disconnect_rst_resets_the_connection() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[1]);
        endpoint.inject(Fault::disconnect(Close::Rst).connection(ConnectionId(0)));
        let mut client = connect(&endpoint).await;
        let mut bystander = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        let mut byte = [0];
        let read_result = bounded("a reset", client.read(&mut byte)).await;
        assert_eq!(
            read_result.map_err(|error| error.kind()),
            Err(ErrorKind::ConnectionReset),
            "{transport:?}"
        );
        endpoint
            .wait_for_connection(ConnectionId(0), ConnectionState::Closed)
            .await;

        send(&mut bystander, &frame(transport, 2, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut bystander).await,
            frame(transport, 2, 1, &registers(3, &[1])),
            "{transport:?}: other connections are not affected"
        );
    }
}

#[tokio::test]
async fn fault_half_open_keeps_the_socket_silent() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[1]);
        endpoint.inject(Fault::half_open().once());
        let mut client = connect(&endpoint).await;
        send(&mut client, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        endpoint
            .wait_for_connection(ConnectionId(0), ConnectionState::Forgotten)
            .await;

        // The kernel still takes the client's bytes: writes succeed, nobody reads them.
        send(&mut client, &frame(transport, 2, 1, &read(3, 0, 1))).await;

        // A new connection works; by the time it has its answer, a FIN or RST on the old one
        // would have arrived, and none did.
        let mut next = connect(&endpoint).await;
        send(&mut next, &frame(transport, 3, 1, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut next).await,
            frame(transport, 3, 1, &registers(3, &[1]))
        );
        assert_silent(client, "the half-open connection");
        assert_eq!(
            endpoint.requests().len(),
            2,
            "{transport:?}: the forgotten connection's second request was never read"
        );
    }
}

#[tokio::test]
async fn fault_broadcast_sends_the_answer_to_every_connection() {
    for transport in BOTH {
        let endpoint = FakeModbus::start(transport).await;
        endpoint.set_holding_registers(1, 0, &[0x0101]);
        endpoint.set_holding_registers(2, 0, &[0x0202]);
        endpoint.inject(Fault::broadcast().unit(1).once());
        let mut asking = connect(&endpoint).await;
        let mut other = connect(&endpoint).await;
        endpoint.wait_for_connections(2).await;

        send(&mut asking, &frame(transport, 1, 1, &read(3, 0, 1))).await;
        let answer = frame(transport, 1, 1, &registers(3, &[0x0101]));
        assert_eq!(response(transport, &mut asking).await, answer);
        // The other client asks unit 2 and reads unit 1's answer first (Laysi/trustiot#183); over
        // Modbus TCP that frame carries the asking client's transaction identifier.
        send(&mut other, &frame(transport, 7, 2, &read(3, 0, 1))).await;
        assert_eq!(
            response(transport, &mut other).await,
            answer,
            "{transport:?}"
        );
        assert_eq!(
            response(transport, &mut other).await,
            frame(transport, 7, 2, &registers(3, &[0x0202])),
            "{transport:?}: then its own answer"
        );
    }
}

#[tokio::test]
async fn fault_matchers_select_the_requests() {
    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[0; 20]);
    endpoint.set_holding_registers(2, 0, &[0; 20]);
    endpoint.inject(Fault::exception(0x0A).unit(2).address(15).times(2));
    let mut client = connect(&endpoint).await;
    let cases: [(u8, Vec<u8>, bool); 5] = [
        (1, read(3, 10, 10), false), // wrong unit
        (2, read(3, 0, 10), false),  // range 0..10 misses 15
        (2, read(3, 10, 10), true),  // a merged read that covers 15
        (2, read(3, 15, 1), true),   // second and last use
        (2, read(3, 15, 1), false),  // used up
    ];
    for (transaction, (unit, request, faulted)) in (1..).zip(cases) {
        send(&mut client, &mbap(transaction, unit, &request)).await;
        let answer = response(Transport::ModbusTcp, &mut client).await;
        assert_eq!(
            answer[7] & 0x80 != 0,
            faulted,
            "request {transaction}: {answer:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_endpoint_closes_every_socket_on_a_multi_thread_runtime() {
    // Clients connect while the endpoint is dropped on a multi-thread runtime. After the drop
    // each client sends a request: a connection the endpoint closed reads end of stream or a
    // reset, one it never accepted is reset by the kernel (a listener that closes mid-handshake
    // can leave the client established until it sends), and one that a task outlived the drop on
    // would be read and kept open. The narrow race between the accept task and the drop is
    // covered deterministically by the unit tests in `src/modbus.rs`.
    for round in 0..200 {
        for transport in BOTH {
            let endpoint = FakeModbus::start(transport).await;
            endpoint.set_holding_registers(1, 0, &[1]);
            let address = endpoint.address();
            let clients: Vec<_> = (0..8)
                .map(|_| tokio::spawn(TcpStream::connect(address)))
                .collect();
            tokio::task::yield_now().await;
            drop(endpoint);
            for client in clients {
                let Ok(mut stream) = client.await.expect("the connect task") else {
                    // Refused: the listener was already closed.
                    continue;
                };
                let what = format!("round {round}, {transport:?}: the connection to end");
                let ended = bounded(what, async {
                    if stream
                        .write_all(&frame(transport, 1, 1, &read(3, 0, 1)))
                        .await
                        .is_err()
                    {
                        return None;
                    }
                    let mut byte = [0];
                    Some(stream.read(&mut byte).await)
                })
                .await;
                assert!(
                    matches!(ended, None | Some(Ok(0) | Err(_))),
                    "round {round}, {transport:?}: answered after the drop: {ended:?}"
                );
            }
        }
    }
}
