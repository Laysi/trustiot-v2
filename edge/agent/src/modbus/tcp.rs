//! Modbus over one TCP connection, in the framing of the device's transport: MBAP frames for
//! Modbus TCP (`mbap.rs`), RTU frames with a CRC for an RS485 converter (`rtu.rs`).
//!
//! This module manages the connection: when it is opened, replaced and closed, sending a
//! request, and receiving until the framing finds the answer or the request's deadline passes.
//! The framing decides which response counts (unit and transaction for MBAP; unit, function
//! code and byte count for RTU) and what is done with frames that arrive between requests.

use std::time::Duration;

use protocol::config::Transport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout, timeout_at};

use super::pdu::{Data, Read};
use super::{Connection, Exchange, ReadError, mbap, rtu};

/// Bytes one socket read takes.
const READ_CHUNK: usize = 512;
/// Most socket reads between two requests. A device that keeps sending without being asked is
/// not a Modbus server, and draining it must not hold up the runtime.
const MAX_IDLE_READS: usize = 64;

/// A Modbus TCP device or gateway, or an RS485 converter that carries RTU frames over TCP, at
/// one host and port. It connects on the first read, and again on the first read after the
/// connection failed or after the device closed it while it was idle.
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
///
/// Several devices behind one converter or gateway share one `TcpConnection` and use it in
/// turn ([`super::endpoint`]).
#[derive(Debug)]
pub struct TcpConnection {
    host: String,
    port: u16,
    connect_timeout: Duration,
    stream: Option<TcpStream>,
    /// Bytes received and not yet part of a whole frame; never longer than one frame plus one
    /// socket read.
    buffer: Vec<u8>,
    framing: Framing,
    /// A request is being written; still set on the next read when the read before was dropped
    /// halfway through writing, which leaves the device's side of the stream out of step.
    sending: bool,
}

impl TcpConnection {
    /// A connection to `host` (an IP address or a DNS name) and `port` that frames requests and
    /// responses as `transport` says; connecting waits at most `connect_timeout`, name
    /// resolution included. Nothing is opened until the first read.
    pub fn new(
        transport: Transport,
        host: impl Into<String>,
        port: u16,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            connect_timeout,
            stream: None,
            buffer: Vec::with_capacity(mbap::MAX_FRAME + READ_CHUNK),
            framing: Framing::new(transport),
            sending: false,
        }
    }

    /// How frames travel on the connection.
    pub fn transport(&self) -> Transport {
        self.framing.transport()
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
            match drop_idle_input(stream, &mut self.buffer, &self.framing, unit) {
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
        read: Read,
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
        read: Read,
        timeout: Duration,
        unit_mismatches: &mut u32,
    ) -> Result<Data, ReadError> {
        let (frame, mut waiting) = self.framing.request(unit, read);
        self.framing.before_request(&mut self.buffer);
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
                    &mut waiting,
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
            .exchange(unit, read, timeout, &mut unit_mismatches)
            .await;
        Exchange {
            result,
            unit_mismatches,
        }
    }
}

/// How requests and responses are framed on the stream.
#[derive(Debug)]
enum Framing {
    /// Modbus TCP: MBAP frames, each request with the next transaction identifier.
    Mbap(mbap::Transactions),
    /// RTU frames with a CRC, to an RS485 converter.
    Rtu,
}

impl Framing {
    fn new(transport: Transport) -> Self {
        match transport {
            Transport::ModbusTcp => Self::Mbap(mbap::Transactions::new()),
            Transport::RtuOverTcp => Self::Rtu,
        }
    }

    fn transport(&self) -> Transport {
        match self {
            Self::Mbap(_) => Transport::ModbusTcp,
            Self::Rtu => Transport::RtuOverTcp,
        }
    }

    /// The request frame for `read` to `unit`, and what waits for its answer.
    fn request(&mut self, unit: u8, read: Read) -> (Vec<u8>, Waiting) {
        match self {
            Self::Mbap(transactions) => {
                let (frame, waiting) = transactions.request(unit, read);
                (frame.to_vec(), Waiting::Mbap(waiting))
            }
            Self::Rtu => {
                let (frame, waiting) = rtu::request(unit, read);
                (frame.to_vec(), Waiting::Rtu(waiting))
            }
        }
    }

    /// Drops the whole frames at the front of `buffer`, which arrived while no request waited,
    /// and returns how many came from units other than `unit`, the unit of the next request.
    /// Fails when the bytes show the stream is not framed any more (MBAP only).
    fn drop_frames(&self, buffer: &mut Vec<u8>, unit: u8) -> Result<u32, String> {
        match self {
            Self::Mbap(_) => mbap::drop_frames(buffer, unit),
            Self::Rtu => Ok(rtu::drop_frames(buffer, unit)),
        }
    }

    /// Prepares `buffer` for the next request, after [`Self::drop_frames`].
    fn before_request(&self, buffer: &mut Vec<u8>) {
        match self {
            // A frame cut short stays: its length says where the next frame starts.
            Self::Mbap(_) => {}
            // Without a transaction identifier, a frame cut short cannot be told from the start
            // of the answer, and its rest joined to it could read as a whole late answer.
            // Dropped, its rest arrives as bytes that form no frame and is skipped.
            Self::Rtu => buffer.clear(),
        }
    }
}

/// What waits for the answer to one request.
#[derive(Debug)]
enum Waiting {
    Mbap(mbap::Waiting),
    Rtu(rtu::Waiting),
}

impl Waiting {
    fn scan(&mut self, buffer: &mut Vec<u8>, unit_mismatches: &mut u32) -> Scan {
        match self {
            Self::Mbap(waiting) => waiting.scan(buffer, unit_mismatches),
            Self::Rtu(waiting) => waiting.scan(buffer, unit_mismatches),
        }
    }

    /// The failure of a request whose deadline passed without an answer.
    fn timed_out(&self) -> ReadError {
        match self {
            Self::Mbap(_) => ReadError::Timeout,
            Self::Rtu(waiting) => waiting.timed_out(),
        }
    }
}

/// What the bytes received so far say about the request that waits.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Scan {
    /// Its answer: the values, the device's exception, or a response that does not fit.
    Answer(Result<Data, ReadError>),
    /// Not there yet: read more.
    More,
    /// The bytes are not Modbus TCP, so the stream is not framed any more (MBAP only; RTU
    /// resynchronises instead).
    Garbled(String),
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
/// None of them can answer a request not sent yet. A frame cut short stays buffered until the
/// framing prepares the next request ([`Framing::before_request`]). Returns how many frames came
/// from units other than `unit`, or `None` when the connection cannot carry a request any more:
/// the device closed or reset it, sent a header no Modbus TCP device sends, or kept sending for
/// [`MAX_IDLE_READS`] reads.
///
/// `try_read` reads only once the runtime has seen the socket become readable, which it does
/// while the agent waits between polls; a close that arrives just before a request is found by
/// that request instead ([`ReadError::ConnectionLost`]).
fn drop_idle_input(
    stream: &TcpStream,
    buffer: &mut Vec<u8>,
    framing: &Framing,
    unit: u8,
) -> Option<u32> {
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
        mismatches += framing.drop_frames(buffer, unit).ok()?;
    }
    None
}

/// Receives until the framing finds the answer in what arrived, or until `deadline`.
async fn await_response(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    waiting: &mut Waiting,
    deadline: Instant,
    unit_mismatches: &mut u32,
) -> Ended {
    loop {
        match waiting.scan(buffer, unit_mismatches) {
            Scan::Answer(result) => return Ended::Done(result),
            Scan::Garbled(detail) => return Ended::Broken(ReadError::Protocol(detail)),
            Scan::More => {}
        }
        let mut chunk = [0; READ_CHUNK];
        // A socket read is cancel safe: it either completes and its bytes are in `buffer`, or
        // it reads nothing. So a late response stays buffered at the deadline, and the next
        // request discards it instead of breaking the framing.
        match timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => {
                return Ended::Broken(ReadError::ConnectionLost(
                    "the device closed the connection".to_owned(),
                ));
            }
            Ok(Ok(read)) => buffer.extend_from_slice(chunk.get(..read).unwrap_or_default()),
            Ok(Err(error)) => {
                return Ended::Broken(ReadError::ConnectionLost(format!(
                    "receiving failed: {error}"
                )));
            }
            Err(_) => return Ended::Done(Err(waiting.timed_out())),
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::config::FunctionCode;
    use testkit::clock::{self, bounded};

    use super::*;

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

    /// Accepts one connection, answers one Modbus TCP read of a holding register with `value`,
    /// and returns the device's side of the connection.
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
        let mut connection = TcpConnection::new(
            Transport::ModbusTcp,
            "127.0.0.1",
            port,
            Duration::from_secs(10),
        );
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
        let framing = Framing::new(Transport::ModbusTcp);
        let mut buffer = Vec::new();
        assert_eq!(
            drop_idle_input(&client, &mut buffer, &framing, 1),
            Some(0),
            "nothing arrived"
        );

        // A late answer from unit 1, an answer from unit 9, and the start of a third frame.
        let mut bytes = vec![0, 5, 0, 0, 0, 5, 1, 3, 2, 0, 7];
        bytes.extend([0, 6, 0, 0, 0, 5, 9, 3, 2, 0, 8]);
        bytes.extend([0, 7, 0, 0]);
        device.write_all(&bytes).await.unwrap();
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, &framing, 1), Some(1));
        assert_eq!(buffer, [0, 7, 0, 0], "the cut frame waits for its rest");
        framing.before_request(&mut buffer);
        assert_eq!(
            buffer,
            [0, 7, 0, 0],
            "its length says where the next starts"
        );

        // Its rest says length 256: not Modbus TCP.
        device.write_all(&[1, 0, 9]).await.unwrap();
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, &framing, 1), None);
    }

    #[tokio::test]
    async fn rtu_frames_between_requests_are_dropped_and_a_cut_one_too() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, mut device) = pair(&listener).await;
        let framing = Framing::new(Transport::RtuOverTcp);
        let frame = |unit: u8, value: u8| {
            let mut frame = vec![unit, 3, 2, 0, value];
            frame.extend(testkit::modbus::crc16(&frame).to_le_bytes());
            frame
        };
        // A late answer from unit 1, an answer from unit 9, and the start of a third frame.
        let mut bytes = frame(1, 7);
        bytes.extend(frame(9, 8));
        bytes.extend(&frame(2, 9)[..4]);
        device.write_all(&bytes).await.unwrap();
        readable(&client).await;
        let mut buffer = Vec::new();
        assert_eq!(drop_idle_input(&client, &mut buffer, &framing, 1), Some(1));
        assert_eq!(buffer, frame(2, 9)[..4], "the cut frame waits for its rest");
        framing.before_request(&mut buffer);
        assert!(buffer.is_empty(), "nothing is kept for the next request");

        // Bytes that form no frame do not close the connection, but the device closing it does.
        device.write_all(&[0x00, 0xFF, 0x13]).await.unwrap();
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, &framing, 1), Some(0));
        drop(device);
        readable(&client).await;
        assert_eq!(drop_idle_input(&client, &mut buffer, &framing, 1), None);
    }

    #[tokio::test]
    async fn an_idle_connection_the_device_closed_is_not_reused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        for transport in [Transport::ModbusTcp, Transport::RtuOverTcp] {
            let (client, device) = pair(&listener).await;
            let mut buffer = Vec::new();
            drop(device);
            readable(&client).await;
            assert_eq!(
                drop_idle_input(&client, &mut buffer, &Framing::new(transport), 1),
                None,
                "{transport:?}"
            );
        }
    }

    #[test]
    fn the_framing_follows_the_transport() {
        for transport in [Transport::ModbusTcp, Transport::RtuOverTcp] {
            let connection =
                TcpConnection::new(transport, "127.0.0.1", 502, Duration::from_secs(1));
            assert_eq!(connection.transport(), transport);
        }
        let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 10).unwrap();
        let (frame, _) = Framing::new(Transport::RtuOverTcp).request(1, read);
        assert_eq!(frame, [0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5, 0xCD]);
        let (frame, _) = Framing::new(Transport::ModbusTcp).request(1, read);
        assert_eq!(frame.len(), 12);
        assert_eq!(frame[2..], [0, 0, 0, 6, 1, 3, 0, 0, 0, 10]);
    }
}
