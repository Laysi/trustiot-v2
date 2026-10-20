//! Modbus TCP: requests and responses in MBAP frames over one TCP connection (Modbus Messaging
//! on TCP/IP Implementation Guide V1.0b, section 3.1.3).
//!
//! A response counts only when both its unit identifier and its transaction identifier match
//! the request. Anything else is read and discarded while the request keeps waiting: a response
//! from another unit (a converter that sends every response to every connection,
//! Laysi/trustiot#183) is counted as a unit mismatch, and one with an earlier transaction
//! identifier is the late answer to a request that already timed out. Frames that arrive
//! between requests answer nothing that is still asked, so they are dropped before the next
//! request is sent.

use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout, timeout_at};

use super::pdu::{Data, Read};
use super::{Connection, Exchange, ReadError};

/// Largest MBAP frame: the 7-byte header and a 253-byte PDU.
const MAX_FRAME: usize = 260;
/// Bytes one socket read takes.
const READ_CHUNK: usize = 512;
/// Most socket reads between two requests. A device that keeps sending without being asked is
/// not a Modbus TCP server, and draining it must not hold up the runtime.
const MAX_IDLE_READS: usize = 64;

/// A Modbus TCP device or gateway at one host and port. It connects on the first read, and
/// again on the first read after the connection failed or after the device closed it while it
/// was idle.
///
/// A device or gateway may close a connection it has kept open at any time, for example after
/// an idle timeout or after every answer. When that connection fails during a request, the
/// request is sent once more on a new connection before it counts as
/// [`ReadError::ConnectionLost`]: a read changes nothing on the device. A request on a
/// connection it just opened is not repeated.
///
/// Dropping a [`Connection::read`] future (a caller's own timeout) is safe: the next read
/// replaces a connection whose request was cut off while it was being sent, and bytes already
/// received stay buffered, so the stream stays framed.
#[derive(Debug)]
pub struct TcpConnection {
    host: String,
    port: u16,
    connect_timeout: Duration,
    stream: Option<TcpStream>,
    /// Bytes received and not yet part of a whole frame; never longer than one frame plus one
    /// socket read.
    buffer: Vec<u8>,
    /// Starts at a random value, so that another client of the same device, which usually
    /// counts from 0 or 1, is unlikely to use the same identifier at the same time.
    next_transaction: u16,
    /// A request is being written; still set on the next read when the read before was dropped
    /// halfway through writing, which leaves the device's side of the stream out of step.
    sending: bool,
}

impl TcpConnection {
    /// A connection to `host` (an IP address or a DNS name) and `port`; connecting waits at most
    /// `connect_timeout`, name resolution included. Nothing is opened until the first read.
    pub fn new(host: impl Into<String>, port: u16, connect_timeout: Duration) -> Self {
        Self {
            host: host.into(),
            port,
            connect_timeout,
            stream: None,
            buffer: Vec::with_capacity(MAX_FRAME + READ_CHUNK),
            next_transaction: random_u16(),
            sending: false,
        }
    }

    /// Whether a TCP connection is open.
    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// Closes the TCP connection; the next read opens a new one.
    pub fn close(&mut self) {
        self.stream = None;
        self.buffer.clear();
        self.sending = false;
    }

    /// Opens a connection unless one is open and usable, and says whether it opened one. On an
    /// open connection, drops what arrived since the last request, counting frames from units
    /// other than `unit`; replaces one the device closed while it was idle, or one whose last
    /// request was cut off while it was being sent.
    async fn open(&mut self, unit: u8, unit_mismatches: &mut u32) -> Result<bool, ReadError> {
        if self.sending {
            self.close();
        }
        if let Some(stream) = &self.stream {
            match drop_idle_input(stream, &mut self.buffer, unit) {
                Some(mismatches) => *unit_mismatches += mismatches,
                None => self.close(),
            }
        }
        if self.stream.is_some() {
            return Ok(false);
        }
        let stream = connect(&self.host, self.port, self.connect_timeout).await?;
        self.buffer.clear();
        self.stream = Some(stream);
        Ok(true)
    }

    async fn exchange(
        &mut self,
        unit: u8,
        read: &Read,
        timeout: Duration,
        unit_mismatches: &mut u32,
    ) -> Result<Data, ReadError> {
        let opened = self.open(unit, unit_mismatches).await?;
        match self.attempt(unit, read, timeout, unit_mismatches).await {
            Err(ReadError::ConnectionLost(_)) if !opened => {
                self.open(unit, unit_mismatches).await?;
                self.attempt(unit, read, timeout, unit_mismatches).await
            }
            result => result,
        }
    }

    /// Sends the request on the open connection and waits for its answer.
    async fn attempt(
        &mut self,
        unit: u8,
        read: &Read,
        timeout: Duration,
        unit_mismatches: &mut u32,
    ) -> Result<Data, ReadError> {
        let transaction = self.next_transaction;
        self.next_transaction = transaction.wrapping_add(1);
        let frame = request_frame(transaction, unit, read);
        let Some(stream) = self.stream.as_mut() else {
            return Err(ReadError::Connect("no connection".to_owned()));
        };
        let deadline = deadline_after(timeout);
        self.sending = true;
        let sent = timeout_at(deadline, stream.write_all(&frame)).await;
        let ended = match sent {
            Ok(Ok(())) => {
                self.sending = false;
                await_response(
                    stream,
                    &mut self.buffer,
                    transaction,
                    unit,
                    read,
                    deadline,
                    unit_mismatches,
                )
                .await
            }
            Ok(Err(error)) => Ended::Broken(ReadError::ConnectionLost(format!(
                "sending failed: {error}"
            ))),
            // Part of the request may have been sent, so the stream is no longer framed.
            Err(_) => Ended::Broken(ReadError::Timeout),
        };
        match ended {
            Ended::Done(result) => result,
            Ended::Broken(error) => {
                self.close();
                Err(error)
            }
        }
    }
}

/// The instant `timeout` from now. A timeout too long for the clock waits a year instead of
/// overflowing.
fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 60 * 60))
}

impl Connection for TcpConnection {
    async fn read(&mut self, unit: u8, read: Read, timeout: Duration) -> Exchange {
        let mut unit_mismatches = 0;
        let result = self
            .exchange(unit, &read, timeout, &mut unit_mismatches)
            .await;
        Exchange {
            result,
            unit_mismatches,
        }
    }
}

/// How waiting for a response ended.
enum Ended {
    /// With an answer or an error; the connection stays usable.
    Done(Result<Data, ReadError>),
    /// The connection cannot be used any more.
    Broken(ReadError),
}

async fn connect(host: &str, port: u16, limit: Duration) -> Result<TcpStream, ReadError> {
    let stream = match timeout(limit, TcpStream::connect((host, port))).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => return Err(ReadError::Connect(error.to_string())),
        Err(_) => {
            return Err(ReadError::Connect(format!(
                "no connection within {} ms",
                limit.as_millis()
            )));
        }
    };
    // Requests are small and each waits for its response: send them at once.
    stream
        .set_nodelay(true)
        .map_err(|error| ReadError::Connect(format!("setting TCP_NODELAY failed: {error}")))?;
    Ok(stream)
}

/// Reads, without waiting, what arrived on an idle connection and drops every whole frame: late
/// answers to requests that timed out, or answers a converter copied from other connections.
/// None of them can answer a request not sent yet. A frame cut short stays buffered, so the
/// stream stays framed. Returns how many frames came from units other than `unit`, or `None`
/// when the connection cannot carry a request any more: the device closed or reset it, sent a
/// header no Modbus TCP device sends, or kept sending for [`MAX_IDLE_READS`] reads.
///
/// `try_read` reads only once the runtime has seen the socket become readable, which it does
/// while the agent waits between polls; a close that arrives just before a request is found by
/// that request instead ([`ReadError::ConnectionLost`]).
fn drop_idle_input(stream: &TcpStream, buffer: &mut Vec<u8>, unit: u8) -> Option<u32> {
    let mut mismatches = 0;
    let mut chunk = [0; READ_CHUNK];
    for _ in 0..MAX_IDLE_READS {
        match stream.try_read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => buffer.extend_from_slice(chunk.get(..read).unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Some(mismatches);
            }
            Err(_) => return None,
        }
        loop {
            match take_frame(buffer) {
                Ok(Some(frame)) if frame.unit != unit => mismatches += 1,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => return None,
            }
        }
    }
    None
}

/// 16 random bits from the standard library's randomly keyed hasher.
fn random_u16() -> u16 {
    let [first, second, ..] = RandomState::new().hash_one(0_u8).to_le_bytes();
    u16::from_le_bytes([first, second])
}

fn request_frame(transaction: u16, unit: u8, read: &Read) -> [u8; 12] {
    let [transaction_high, transaction_low] = transaction.to_be_bytes();
    let [function, start_high, start_low, count_high, count_low] = read.request();
    // Protocol identifier 0; the length counts the unit identifier and the 5-byte PDU.
    [
        transaction_high,
        transaction_low,
        0,
        0,
        0,
        6,
        unit,
        function,
        start_high,
        start_low,
        count_high,
        count_low,
    ]
}

async fn await_response(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    transaction: u16,
    unit: u8,
    read: &Read,
    deadline: Instant,
    unit_mismatches: &mut u32,
) -> Ended {
    loop {
        let frame = match timeout_at(deadline, next_frame(stream, buffer)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(Stop::Lost(detail))) => return Ended::Broken(ReadError::ConnectionLost(detail)),
            Ok(Err(Stop::Garbled(detail))) => return Ended::Broken(ReadError::Protocol(detail)),
            // `next_frame` is cancel safe: whatever arrived stays in the buffer, so a late
            // response is discarded by the next request instead of breaking the framing.
            Err(_) => return Ended::Done(Err(ReadError::Timeout)),
        };
        if frame.unit != unit {
            *unit_mismatches += 1;
            continue;
        }
        if frame.transaction != transaction {
            continue;
        }
        return Ended::Done(read.response(&frame.pdu).map_err(ReadError::from));
    }
}

/// One MBAP frame.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    transaction: u16,
    unit: u8,
    pdu: Vec<u8>,
}

/// Why no further frame can be read.
enum Stop {
    /// The device closed or reset the connection.
    Lost(String),
    /// The bytes are not Modbus TCP.
    Garbled(String),
}

/// The next whole frame from the stream. Cancel safe: a socket read either completes and its
/// bytes are in `buffer`, or it reads nothing.
async fn next_frame(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Frame, Stop> {
    loop {
        if let Some(frame) = take_frame(buffer).map_err(Stop::Garbled)? {
            return Ok(frame);
        }
        let mut chunk = [0; READ_CHUNK];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| Stop::Lost(format!("receiving failed: {error}")))?;
        if read == 0 {
            return Err(Stop::Lost("the device closed the connection".to_owned()));
        }
        buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
}

/// Takes the first frame off `buffer` once it is whole. A header no Modbus TCP device sends
/// means the stream is not framed any more.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, String> {
    let Some(&[t0, t1, p0, p1, l0, l1, unit]) = buffer.get(..7) else {
        return Ok(None);
    };
    let protocol = u16::from_be_bytes([p0, p1]);
    let length = u16::from_be_bytes([l0, l1]);
    if protocol != 0 {
        return Err(format!("MBAP protocol identifier {protocol} instead of 0"));
    }
    // The length counts the unit identifier and a PDU of 1 to 253 bytes.
    if !(2..=254).contains(&length) {
        return Err(format!("MBAP length {length} outside 2..=254"));
    }
    let end = 6 + usize::from(length);
    let Some(pdu) = buffer.get(7..end) else {
        return Ok(None);
    };
    let frame = Frame {
        transaction: u16::from_be_bytes([t0, t1]),
        unit,
        pdu: pdu.to_vec(),
    };
    buffer.drain(..end);
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use protocol::config::FunctionCode;
    use testkit::clock::{self, bounded};

    use super::*;

    #[test]
    fn requests_are_mbap_frames() {
        let read = Read::new(FunctionCode::ReadInputRegisters, 0x0102, 0x0003).unwrap();
        assert_eq!(
            request_frame(0xABCD, 17, &read),
            [0xAB, 0xCD, 0, 0, 0, 6, 17, 4, 0x01, 0x02, 0x00, 0x03]
        );
    }

    #[test]
    fn frames_are_taken_once_whole() {
        let mut buffer = vec![0x00, 0x07, 0, 0, 0, 5, 1, 3, 2];
        assert_eq!(take_frame(&mut buffer), Ok(None));
        buffer.extend([0x12, 0x34, 0x00, 0x08]); // the rest, and the start of the next frame
        assert_eq!(
            take_frame(&mut buffer),
            Ok(Some(Frame {
                transaction: 7,
                unit: 1,
                pdu: vec![3, 2, 0x12, 0x34],
            }))
        );
        assert_eq!(buffer, [0x00, 0x08]);
        assert_eq!(take_frame(&mut buffer), Ok(None));
    }

    #[test]
    fn headers_no_device_sends_end_the_framing() {
        let mut other_protocol = vec![0, 1, 0, 1, 0, 3, 1, 0x83, 2];
        assert!(take_frame(&mut other_protocol).is_err());
        for length in [0_u16, 1, 255, 0xFFFF] {
            let [high, low] = length.to_be_bytes();
            let mut buffer = vec![0, 1, 0, 0, high, low, 1];
            assert!(take_frame(&mut buffer).is_err(), "length {length}");
        }
        let mut longest = vec![0, 1, 0, 0, 0, 254, 1];
        longest.extend([3; 253]);
        assert!(matches!(take_frame(&mut longest), Ok(Some(frame)) if frame.pdu.len() == 253));
    }

    /// A connected client socket and the device side of it.
    async fn pair(listener: &tokio::net::TcpListener) -> (TcpStream, TcpStream) {
        let address = listener.local_addr().unwrap();
        let client = bounded("a connection", TcpStream::connect(address))
            .await
            .unwrap();
        let device = bounded("an accepted connection", listener.accept())
            .await
            .unwrap();
        (client, device.0)
    }

    async fn readable(stream: &TcpStream) {
        bounded("the socket to become readable", stream.readable())
            .await
            .unwrap();
    }

    /// Accepts one connection, answers one read of a holding register with `value`, and
    /// returns the device's side of the connection.
    async fn answer_once(listener: &tokio::net::TcpListener, value: u16) -> TcpStream {
        let (mut stream, _) = bounded("an accepted connection", listener.accept())
            .await
            .unwrap();
        let mut request = [0; 12];
        bounded("a request", stream.read_exact(&mut request))
            .await
            .unwrap();
        let [high, low] = value.to_be_bytes();
        let answer = [
            request[0], request[1], 0, 0, 0, 5, request[6], 3, 2, high, low,
        ];
        stream.write_all(&answer).await.unwrap();
        stream
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_cut_off_while_sending_does_not_reuse_the_connection() {
        let _clock = clock::manual();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut connection = TcpConnection::new("127.0.0.1", port, Duration::from_secs(10));
        let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 1).unwrap();
        let timeout = Duration::from_secs(5);

        let (exchange, mut first) = bounded("the first read", async {
            tokio::join!(connection.read(1, read, timeout), answer_once(&listener, 1))
        })
        .await;
        assert_eq!(exchange.result, Ok(Data::Registers(vec![1])));

        // What a read dropped halfway through writing its request leaves behind.
        connection.sending = true;
        let (exchange, _second) = bounded("the second read", async {
            tokio::join!(connection.read(1, read, timeout), answer_once(&listener, 2))
        })
        .await;
        assert_eq!(exchange.result, Ok(Data::Registers(vec![2])));
        // The first connection was closed, not written to again.
        let mut rest = Vec::new();
        bounded(
            "the first connection to close",
            first.read_to_end(&mut rest),
        )
        .await
        .unwrap();
        assert!(rest.is_empty(), "nothing more was sent on it: {rest:?}");
    }

    #[tokio::test]
    async fn what_arrives_between_requests_is_dropped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, mut device) = pair(&listener).await;
        let mut buffer = Vec::new();
        assert_eq!(
            drop_idle_input(&client, &mut buffer, 1),
            Some(0),
            "nothing arrived"
        );

        // A late answer from unit 1, an answer from unit 9, and the start of a third frame.
        let mut bytes = vec![0, 5, 0, 0, 0, 5, 1, 3, 2, 0, 7];
        bytes.extend([0, 6, 0, 0, 0, 5, 9, 3, 2, 0, 8]);
        bytes.extend([0, 7, 0, 0]);
        device.write_all(&bytes).await.unwrap();
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, 1), Some(1));
        assert_eq!(buffer, [0, 7, 0, 0], "the cut frame waits for its rest");

        // Its rest says length 256: not Modbus TCP.
        device.write_all(&[1, 0, 9]).await.unwrap();
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, 1), None);
    }

    #[tokio::test]
    async fn an_idle_connection_the_device_closed_is_not_reused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, device) = pair(&listener).await;
        let mut buffer = Vec::new();
        drop(device);
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, 1), None);
    }

    #[test]
    fn transactions_start_at_random() {
        // 8 connections all starting at the same identifier: probability 2^-112.
        let firsts: std::collections::HashSet<u16> = (0..8)
            .map(|_| TcpConnection::new("127.0.0.1", 502, Duration::from_secs(1)).next_transaction)
            .collect();
        assert!(firsts.len() > 1, "{firsts:?}");
    }
}
