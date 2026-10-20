//! A fake Modbus endpoint started inside the test process, with fault injection (rebuild
//! checklist M7). It replaces the dropped simulator: a test starts one, gives its units register
//! values, injects the faults the field has shown, and points the code under test at
//! [`FakeModbus::address`].
//!
//! - Both transports of the gateway ([`Transport`]): Modbus TCP, and RTU frames over TCP to an
//!   RS485 converter.
//! - Unit identifiers: one endpoint hosts several units, each with its own coils, discrete
//!   inputs, holding registers and input registers. Function codes 1–4 are served; everything
//!   else is answered with exception 01, and the request log records it.
//! - Faults ([`Fault`]): delay, hold, no response, unit mismatch, exception, disconnect (FIN or
//!   RST), half-open, and broadcast to every connection (Laysi/trustiot#183).
//! - Observation: the request log ([`FakeModbus::requests`]) and connection states
//!   ([`FakeModbus::connections`]), with waits that fail the test after
//!   [`crate::clock::WAIT_LIMIT`] instead of sleeping.
//! - Time: delays follow the tokio clock, so [`crate::clock::manual`] and
//!   `tokio::time::advance` control them; [`Fault::hold`] needs no clock at all.
//!
//! The crate README, section 「假 Modbus 端點」, shows how a test uses it.

mod fault;
mod frame;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

pub use fault::{Close, Fault};
pub use frame::{MAX_READ_BITS, MAX_READ_REGISTERS, crc16};

use crate::clock;
use fault::{Outcome, Timing};
use frame::{Cut, Frame, Tables};

/// How Modbus frames travel to the endpoint, as in the gateway's configuration
/// (`Transport` in `crates/protocol`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Modbus TCP (MBAP framing): a device that speaks TCP itself. Each connection is served on
    /// its own, in order: a request that waits (a delay, a hold) holds up later requests on the
    /// same connection only. A request for a unit the endpoint does not host is answered with
    /// exception 0B (gateway target device failed to respond).
    ModbusTcp,
    /// RTU frames with a CRC over TCP: an RS485 converter with the units on its bus. Requests
    /// from every connection go onto the one bus in the order they arrive, and a request that
    /// waits holds up all of them. A request for a unit that is not on the bus, or a frame with a
    /// bad CRC, gets no answer, as on a real bus.
    RtuOverTcp,
}

/// A connection the endpoint accepted, numbered from 0 in the order of acceptance, so a test can
/// name a connection (for [`Fault::connection`]) before the client opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub usize);

/// Where a connection stands, from the endpoint's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Requests are read and answered.
    Open,
    /// The endpoint sent FIN ([`Close::Fin`]) and waits for the client to close its side; it
    /// still reads and logs requests but answers none.
    FinSent,
    /// The endpoint forgot the connection ([`Fault::half_open`]): it never reads from or writes
    /// to it again, and the socket stays open until the endpoint is dropped.
    Forgotten,
    /// The socket is closed: by the client, by a reset ([`Close::Rst`]), after FIN once the
    /// client closed too, or because the client sent an MBAP header no Modbus TCP client sends.
    Closed,
}

/// One frame the endpoint received, in the request log ([`FakeModbus::requests`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Request {
    /// Position in the log: 0 for the first frame the endpoint received, on any connection.
    pub index: usize,
    /// The connection it came on.
    pub connection: ConnectionId,
    /// The unit identifier it was addressed to.
    pub unit: u8,
    /// The function code.
    pub function: u8,
    /// Start address and quantity, for a well-formed read (function codes 1–4).
    pub range: Option<(u16, u16)>,
    /// The MBAP transaction identifier (Modbus TCP only).
    pub transaction_id: Option<u16>,
    /// The whole frame as received.
    pub raw: Vec<u8>,
    /// Whether the RTU CRC was right; always true for Modbus TCP, which has none. A frame with a
    /// bad CRC is never answered and never matches a fault.
    pub checksum_ok: bool,
    /// The injected fault the endpoint applied to it, as it was injected.
    pub fault: Option<Fault>,
}

/// The fake Modbus endpoint. It listens on an ephemeral port of 127.0.0.1 from
/// [`start`](Self::start) until it is dropped; dropping it stops every task and closes every
/// socket, the forgotten ones included.
pub struct FakeModbus {
    address: SocketAddr,
    shared: Arc<Shared>,
}

impl fmt::Debug for FakeModbus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeModbus")
            .field("address", &self.address)
            .field("transport", &self.shared.transport)
            .finish_non_exhaustive()
    }
}

impl FakeModbus {
    /// Starts an endpoint without units on an ephemeral port of 127.0.0.1.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime, or when no loopback port can be bound.
    pub async fn start(transport: Transport) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the fake Modbus endpoint to a port of 127.0.0.1");
        let address = listener
            .local_addr()
            .expect("the fake Modbus endpoint's address");
        let shared = Arc::new(Shared::new(transport));
        let bus = (transport == Transport::RtuOverTcp).then(|| {
            let (queue, queued) = mpsc::unbounded_channel();
            shared.spawn(process(Arc::clone(&shared), queued));
            queue
        });
        shared.spawn(accept(Arc::clone(&shared), listener, bus));
        Self { address, shared }
    }

    /// The address clients connect to.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The transport the endpoint speaks.
    #[must_use]
    pub fn transport(&self) -> Transport {
        self.shared.transport
    }

    /// Adds `unit` without any values, so it answers: a read of an address that was never set
    /// is exception 02. Setting a value adds the unit too.
    pub fn add_unit(&self, unit: u8) {
        self.shared.state().units.entry(unit).or_default();
    }

    /// Sets coils (function code 1) of `unit` from address `start` on, adding the unit if needed.
    ///
    /// # Panics
    ///
    /// When the values run past address 65535.
    pub fn set_coils(&self, unit: u8, start: u16, values: &[bool]) {
        self.set(unit, start, values, |tables| &mut tables.coils);
    }

    /// Sets discrete inputs (function code 2) of `unit` from address `start` on.
    ///
    /// # Panics
    ///
    /// When the values run past address 65535.
    pub fn set_discrete_inputs(&self, unit: u8, start: u16, values: &[bool]) {
        self.set(unit, start, values, |tables| &mut tables.discrete_inputs);
    }

    /// Sets holding registers (function code 3) of `unit` from address `start` on.
    ///
    /// # Panics
    ///
    /// When the values run past address 65535.
    pub fn set_holding_registers(&self, unit: u8, start: u16, values: &[u16]) {
        self.set(unit, start, values, |tables| &mut tables.holding_registers);
    }

    /// Sets input registers (function code 4) of `unit` from address `start` on.
    ///
    /// # Panics
    ///
    /// When the values run past address 65535.
    pub fn set_input_registers(&self, unit: u8, start: u16, values: &[u16]) {
        self.set(unit, start, values, |tables| &mut tables.input_registers);
    }

    fn set<T: Copy>(
        &self,
        unit: u8,
        start: u16,
        values: &[T],
        table: impl FnOnce(&mut Tables) -> &mut BTreeMap<u16, T>,
    ) {
        let mut state = self.shared.state();
        let table = table(state.units.entry(unit).or_default());
        for (offset, value) in values.iter().enumerate() {
            let address = u16::try_from(offset)
                .ok()
                .and_then(|offset| start.checked_add(offset))
                .expect("Modbus addresses end at 65535");
            table.insert(address, *value);
        }
    }

    /// Injects a fault. It applies to requests that arrive from now on; the first injected
    /// fault that matches a request and has uses left wins.
    pub fn inject(&self, fault: Fault) {
        self.shared.state().faults.push(fault);
    }

    /// Removes every injected fault. Requests that already picked one keep it.
    pub fn clear_faults(&self) {
        self.shared.state().faults.clear();
    }

    /// Lets the held response of the request at `index` in the log go out ([`Fault::hold`]).
    /// Releasing a request before the endpoint gets to hold it is fine: it then goes out at
    /// once.
    pub fn release(&self, index: usize) {
        let mut state = self.shared.state();
        match state.held.remove(&index) {
            Some(held) => {
                // The receiver is gone only when the endpoint is being dropped.
                let _ = held.send(());
            }
            None => {
                state.released.insert(index);
            }
        }
    }

    /// The request log so far.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.shared.state().log.clone()
    }

    /// Waits until the log holds at least `count` requests and returns it.
    ///
    /// # Panics
    ///
    /// When that takes longer than [`clock::WAIT_LIMIT`].
    pub async fn wait_for_requests(&self, count: usize) -> Vec<Request> {
        self.wait_until(
            format!("the fake Modbus endpoint to receive {count} requests"),
            |state| (state.log.len() >= count).then(|| state.log.clone()),
        )
        .await
    }

    /// Every connection the endpoint accepted so far, with its state.
    #[must_use]
    pub fn connections(&self) -> BTreeMap<ConnectionId, ConnectionState> {
        self.shared
            .state()
            .connections
            .iter()
            .map(|(id, connection)| (*id, connection.state))
            .collect()
    }

    /// Waits until the endpoint has accepted at least `count` connections. A client's connect
    /// returns before the endpoint accepts, so a test that needs every client registered (a
    /// broadcast) waits for this first.
    ///
    /// # Panics
    ///
    /// When that takes longer than [`clock::WAIT_LIMIT`].
    pub async fn wait_for_connections(&self, count: usize) {
        self.wait_until(
            format!("the fake Modbus endpoint to accept {count} connections"),
            |state| (state.connections.len() >= count).then_some(()),
        )
        .await;
    }

    /// Waits until connection `id` is in `wanted`.
    ///
    /// # Panics
    ///
    /// When that takes longer than [`clock::WAIT_LIMIT`].
    pub async fn wait_for_connection(&self, id: ConnectionId, wanted: ConnectionState) {
        self.wait_until(format!("connection {} to be {wanted:?}", id.0), |state| {
            state
                .connections
                .get(&id)
                .is_some_and(|connection| connection.state == wanted)
                .then_some(())
        })
        .await;
    }

    async fn wait_until<T>(&self, what: String, mut check: impl FnMut(&State) -> Option<T>) -> T {
        // Subscribed before the first check, so no change between the check and the wait is
        // missed.
        let mut changes = self.shared.changes.subscribe();
        clock::bounded(what, async {
            loop {
                let found = check(&self.shared.state());
                if let Some(found) = found {
                    return found;
                }
                // The sender lives in `shared`, which `self` keeps alive.
                let _ = changes.changed().await;
            }
        })
        .await
    }
}

impl Drop for FakeModbus {
    fn drop(&mut self) {
        let mut state = self.shared.state();
        // On a multi-thread runtime the accept task may be starting a connection's tasks right
        // now; under the same lock, `Shared::spawn` aborts anything started from here on.
        state.closed = true;
        for task in state.tasks.drain(..) {
            task.abort();
        }
        state.forgotten.clear();
        state.held.clear();
        for connection in state.connections.values_mut() {
            connection.commands = None;
        }
    }
}

/// What a connection's task does next, sent by the processor.
enum Command {
    Write(Vec<u8>),
    Close(Close),
    Forget,
}

/// A request on its way to the processor, with the fault it picked on arrival.
struct Queued {
    index: usize,
    connection: ConnectionId,
    received: Instant,
    frame: Frame,
    fault: Option<Fault>,
}

struct Shared {
    transport: Transport,
    state: Mutex<State>,
    /// Bumped on every change a test can wait for: a logged request, a connection state.
    changes: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    units: BTreeMap<u8, Tables>,
    faults: Vec<Fault>,
    log: Vec<Request>,
    connections: BTreeMap<ConnectionId, Connection>,
    /// Held responses, by request index, waiting for [`FakeModbus::release`].
    held: BTreeMap<usize, oneshot::Sender<()>>,
    /// Requests released before the processor got to hold them.
    released: BTreeSet<usize>,
    /// Sockets of half-open connections, kept open until the endpoint is dropped.
    forgotten: Vec<TcpStream>,
    tasks: Vec<AbortHandle>,
    /// Set when the endpoint is dropped: no task, connection or socket is kept after that.
    closed: bool,
}

struct Connection {
    state: ConnectionState,
    /// `None` once the connection takes no more commands (it is no longer open).
    commands: Option<mpsc::UnboundedSender<Command>>,
}

impl Shared {
    fn new(transport: Transport) -> Self {
        Self {
            transport,
            state: Mutex::default(),
            changes: watch::Sender::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panicking test thread must not hide the endpoint's state from the others.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.changes.send_modify(|version| *version += 1);
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let task = tokio::spawn(task).abort_handle();
        let mut state = self.state();
        if state.closed {
            // Started while the endpoint was being dropped.
            task.abort();
        } else {
            state.tasks.push(task);
        }
    }

    fn set_connection_state(&self, id: ConnectionId, new: ConnectionState) {
        if let Some(connection) = self.state().connections.get_mut(&id) {
            connection.state = new;
            if new != ConnectionState::Open {
                connection.commands = None;
            }
        }
        self.changed();
    }

    /// Logs a frame that just arrived and picks its fault.
    fn arrive(&self, connection: ConnectionId, frame: Frame, answering: bool) -> Queued {
        let received = Instant::now();
        let mut state = self.state();
        let index = state.log.len();
        let fault = if answering && frame.checksum_ok {
            state
                .faults
                .iter_mut()
                .find(|fault| fault.matches(index, connection, frame.unit, &frame.pdu))
                .map(|fault| {
                    let picked = fault.clone();
                    if let Some(remaining) = fault.remaining.as_mut() {
                        *remaining = remaining.saturating_sub(1);
                    }
                    picked
                })
        } else {
            None
        };
        state.log.push(Request {
            index,
            connection,
            unit: frame.unit,
            function: frame.pdu.first().copied().unwrap_or_default(),
            range: frame::read_range(&frame.pdu),
            transaction_id: frame.transaction_id,
            raw: frame.raw.clone(),
            checksum_ok: frame.checksum_ok,
            fault: fault.clone(),
        });
        drop(state);
        self.changed();
        Queued {
            index,
            connection,
            received,
            frame,
            fault,
        }
    }

    /// Answers one request, or does what its fault says instead.
    async fn handle(&self, queued: Queued) {
        let Queued {
            index,
            connection,
            received,
            frame,
            fault,
        } = queued;
        if !frame.checksum_ok {
            // A device ignores a frame with a bad CRC.
            return;
        }
        let (timing, outcome, broadcast) = fault
            .as_ref()
            .map_or((Timing::Now, Outcome::Answer, false), |fault| {
                (fault.timing, fault.outcome, fault.broadcast)
            });
        match timing {
            Timing::Now => {}
            Timing::Delay(delay) => match received.checked_add(delay) {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            },
            Timing::Hold => self.hold(index).await,
        }
        let function = frame.pdu.first().copied().unwrap_or_default();
        let normal = || self.normal_answer(frame.unit, &frame.pdu);
        let response = match outcome {
            Outcome::Answer => normal().map(|pdu| (frame.unit, pdu)),
            Outcome::NoResponse => None,
            Outcome::UnitMismatch(unit) => normal().map(|pdu| (unit, pdu)),
            Outcome::Exception(code) => Some((frame.unit, frame::exception(function, code))),
            Outcome::Disconnect(close) => {
                self.send(connection, Command::Close(close));
                return;
            }
            Outcome::HalfOpen => {
                self.send(connection, Command::Forget);
                return;
            }
        };
        let Some((unit, pdu)) = response else {
            return;
        };
        let bytes = match self.transport {
            Transport::ModbusTcp => {
                frame::mbap_response(frame.transaction_id.unwrap_or_default(), unit, &pdu)
            }
            Transport::RtuOverTcp => frame::rtu_response(unit, &pdu),
        };
        if broadcast {
            let state = self.state();
            for commands in state
                .connections
                .values()
                .filter_map(|connection| connection.commands.as_ref())
            {
                let _ = commands.send(Command::Write(bytes.clone()));
            }
        } else {
            self.send(connection, Command::Write(bytes));
        }
    }

    /// The response PDU of the addressed unit, or `None` when nobody answers.
    fn normal_answer(&self, unit: u8, pdu: &[u8]) -> Option<Vec<u8>> {
        let state = self.state();
        match (state.units.get(&unit), self.transport) {
            (Some(tables), _) => Some(tables.answer(pdu)),
            (None, Transport::ModbusTcp) => Some(frame::exception(
                pdu.first().copied().unwrap_or_default(),
                frame::GATEWAY_TARGET_FAILED,
            )),
            (None, Transport::RtuOverTcp) => None,
        }
    }

    /// Sends a command to a connection that is still open; a closed one is skipped.
    fn send(&self, id: ConnectionId, command: Command) {
        if let Some(commands) = self
            .state()
            .connections
            .get(&id)
            .and_then(|connection| connection.commands.as_ref())
        {
            // The task is gone only when the connection just closed.
            let _ = commands.send(command);
        }
    }

    /// Waits until the test releases request `index`.
    async fn hold(&self, index: usize) {
        let released = {
            let mut state = self.state();
            if state.released.remove(&index) {
                return;
            }
            let (release, released) = oneshot::channel();
            state.held.insert(index, release);
            released
        };
        // An error means the endpoint is being dropped.
        let _ = released.await;
    }
}

async fn accept(
    shared: Arc<Shared>,
    listener: TcpListener,
    bus: Option<mpsc::UnboundedSender<Queued>>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // The client gave up before the accept: nothing to serve.
            Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => continue,
            // Out of file descriptors or the like. Stop accepting instead of spinning (a paused
            // clock rules out a backoff); the test's waits then fail and name what they awaited.
            Err(_) => return,
        };
        let (commands, received) = mpsc::unbounded_channel();
        let id = {
            let mut state = shared.state();
            if state.closed {
                // The endpoint was dropped while this connection was accepted; returning
                // closes the socket.
                return;
            }
            let id = ConnectionId(state.connections.len());
            state.connections.insert(
                id,
                Connection {
                    state: ConnectionState::Open,
                    commands: Some(commands),
                },
            );
            id
        };
        let queue = bus.clone().unwrap_or_else(|| {
            // Modbus TCP: a processor per connection.
            let (queue, queued) = mpsc::unbounded_channel();
            shared.spawn(process(Arc::clone(&shared), queued));
            queue
        });
        shared.spawn(serve(Arc::clone(&shared), id, stream, received, queue));
        shared.changed();
    }
}

/// Processes queued requests one at a time: per connection for Modbus TCP, for the whole bus
/// for RTU-over-TCP.
async fn process(shared: Arc<Shared>, mut queued: mpsc::UnboundedReceiver<Queued>) {
    while let Some(request) = queued.recv().await {
        shared.handle(request).await;
    }
}

/// Reads frames from one connection and carries out the processor's commands on it.
async fn serve(
    shared: Arc<Shared>,
    id: ConnectionId,
    mut stream: TcpStream,
    mut commands: mpsc::UnboundedReceiver<Command>,
    queue: mpsc::UnboundedSender<Queued>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    // False once the endpoint sent FIN: requests are still read and logged, not answered.
    let mut answering = true;
    'serve: loop {
        tokio::select! {
            command = commands.recv(), if answering => match command {
                Some(Command::Write(bytes)) => {
                    if stream.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Command::Close(Close::Fin)) => {
                    answering = false;
                    shared.set_connection_state(id, ConnectionState::FinSent);
                    if stream.shutdown().await.is_err() {
                        break;
                    }
                }
                Some(Command::Close(Close::Rst)) => {
                    // With SO_LINGER 0, closing the socket sends RST instead of FIN.
                    let _ = stream.set_zero_linger();
                    break;
                }
                Some(Command::Forget) => {
                    let mut state = shared.state();
                    if !state.closed {
                        state.forgotten.push(stream);
                    }
                    drop(state);
                    shared.set_connection_state(id, ConnectionState::Forgotten);
                    return;
                }
                None => break,
            },
            read = stream.read(&mut chunk) => {
                let Ok(count @ 1..) = read else {
                    break;
                };
                buffer.extend_from_slice(&chunk[..count]);
                loop {
                    let cut = match shared.transport {
                        Transport::ModbusTcp => frame::cut_mbap(&mut buffer),
                        Transport::RtuOverTcp => frame::cut_rtu(&mut buffer),
                    };
                    match cut {
                        Cut::Frame(frame) => {
                            let request = shared.arrive(id, frame, answering);
                            if answering {
                                // The processor is gone only when the endpoint is dropped.
                                let _ = queue.send(request);
                            }
                        }
                        Cut::Incomplete => break,
                        Cut::Invalid => break 'serve,
                    }
                }
            }
        }
    }
    drop(stream);
    shared.set_connection_state(id, ConnectionState::Closed);
}

#[cfg(test)]
mod tests {
    use super::*;

    // On a multi-thread runtime, `FakeModbus::drop` can run while the accept task, on another
    // worker, is between accepting a socket and starting its tasks. These tests put the endpoint
    // in the state the drop leaves and run those steps after it, which no timing can promise.

    #[tokio::test]
    async fn a_task_started_after_the_drop_is_aborted() {
        let shared = Shared::new(Transport::ModbusTcp);
        shared.state().closed = true;
        let (alive, ended) = oneshot::channel::<()>();
        shared.spawn(async move {
            let _alive = alive;
            std::future::pending::<()>().await;
        });
        // The sender goes away only with the task's future, which an abort drops.
        let ended = clock::bounded("the task started after the drop to be aborted", ended).await;
        assert!(ended.is_err());
        assert!(shared.state().tasks.is_empty());
    }

    #[tokio::test]
    async fn a_connection_accepted_after_the_drop_is_closed() {
        let shared = Arc::new(Shared::new(Transport::ModbusTcp));
        shared.state().closed = true;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        clock::bounded(
            "the accept loop to stop after the drop",
            accept(Arc::clone(&shared), listener, None),
        )
        .await;
        let mut byte = [0];
        let read = clock::bounded("the accepted socket to close", client.read(&mut byte)).await;
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        let state = shared.state();
        assert!(state.connections.is_empty());
        assert!(state.tasks.is_empty());
    }
}
