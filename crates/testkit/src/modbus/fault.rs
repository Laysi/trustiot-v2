//! Faults: which requests they apply to and what the endpoint does instead of a normal answer.

use std::time::Duration;

use super::ConnectionId;
use super::frame::read_range;

/// How the endpoint closes a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Close {
    /// Graceful close: the endpoint sends FIN (`shutdown(SHUT_WR)`), so the client reads end of
    /// stream. The endpoint keeps reading and logging what the client still sends, answers
    /// nothing more, and releases the socket when the client closes its side.
    Fin,
    /// Abortive close: `SO_LINGER` 0 and close, so the kernel sends RST and the client's next
    /// read or write fails with "connection reset".
    Rst,
}

/// When the endpoint acts on a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Timing {
    /// As soon as the request is processed.
    Now,
    /// No earlier than this long after the request was received, by the tokio clock.
    Delay(Duration),
    /// When the test lets it through with [`super::FakeModbus::release`].
    Hold,
}

/// What the endpoint does with a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The answer the unit gives without a fault.
    Answer,
    /// Nothing.
    NoResponse,
    /// The normal answer, with this unit identifier in the response.
    UnitMismatch(u8),
    /// An exception response with this code.
    Exception(u8),
    /// Close the connection instead of answering.
    Disconnect(Close),
    /// Forget the connection instead of answering.
    HalfOpen,
}

/// A fault: which requests it applies to (unit, function code, address, connection, position in
/// the request log; every condition set must hold), how often (every match, or only the first
/// few), and what the endpoint does with a matching request instead of answering normally.
///
/// Start from one of the constructors, which picks what happens; narrow it with the matchers
/// ([`unit`](Self::unit), [`function`](Self::function), [`address`](Self::address),
/// [`connection`](Self::connection), [`nth`](Self::nth)) and limit it with
/// [`once`](Self::once) or [`times`](Self::times). [`delayed`](Self::delayed),
/// [`held`](Self::held) and [`to_all_connections`](Self::to_all_connections) combine timing and
/// broadcast with any constructor. A fault without matchers applies to every request.
///
/// The endpoint picks the fault for a request when the request arrives: the first injected fault
/// that matches and has uses left, in the order they were injected. That fault, if any, is in the
/// request's log entry ([`super::Request::fault`]). Frames with a bad CRC are never answered and
/// never match.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a fault does nothing until it is injected with FakeModbus::inject"]
pub struct Fault {
    pub(super) timing: Timing,
    pub(super) outcome: Outcome,
    pub(super) broadcast: bool,
    unit: Option<u8>,
    function: Option<u8>,
    address: Option<u16>,
    connection: Option<ConnectionId>,
    nth: Option<usize>,
    /// Uses left; `None` for every match.
    pub(super) remaining: Option<usize>,
}

impl Fault {
    fn new(outcome: Outcome) -> Self {
        Self {
            timing: Timing::Now,
            outcome,
            broadcast: false,
            unit: None,
            function: None,
            address: None,
            connection: None,
            nth: None,
            remaining: None,
        }
    }

    /// Delay: the endpoint answers normally, but no earlier than `delay` after it received the
    /// request, measured by the tokio clock, so a paused clock controls it. Requests behind it
    /// (on the same connection for Modbus TCP, on the bus for RTU-over-TCP) wait for it.
    pub fn delay(delay: Duration) -> Self {
        Self::new(Outcome::Answer).delayed(delay)
    }

    /// Hold: the endpoint answers normally once the test lets the response through with
    /// [`super::FakeModbus::release`]; no clock involved. Requests behind it wait for it.
    pub fn hold() -> Self {
        Self::new(Outcome::Answer).held()
    }

    /// No response: the endpoint reads and logs the request and never answers it. The
    /// connection stays open and healthy: later requests are answered, and TCP keepalive keeps
    /// succeeding (Laysi/trustiot#159, #164). On RTU-over-TCP the bus is free again at once.
    pub fn no_response() -> Self {
        Self::new(Outcome::NoResponse)
    }

    /// Unit mismatch: the endpoint answers as the addressed unit would, but the response carries
    /// `unit` as its unit identifier (with a matching CRC on RTU-over-TCP). A request the
    /// endpoint would not answer (an unknown unit on RTU-over-TCP) stays unanswered.
    pub fn unit_mismatch(unit: u8) -> Self {
        Self::new(Outcome::UnitMismatch(unit))
    }

    /// Exception: the endpoint answers with exception `code` (function code with the high bit
    /// set, then the code), whatever the unit's tables hold.
    pub fn exception(code: u8) -> Self {
        Self::new(Outcome::Exception(code))
    }

    /// Disconnect: instead of answering, the endpoint closes the connection, gracefully or
    /// abortively ([`Close`]). Other connections and new ones are not affected.
    pub fn disconnect(close: Close) -> Self {
        Self::new(Outcome::Disconnect(close))
    }

    /// Half-open: instead of answering, the endpoint forgets the connection. It never reads from
    /// or writes to it again and sends neither FIN nor RST; the socket stays open until the
    /// endpoint is dropped. The kernel still acknowledges what the client sends, so the client's
    /// writes and TCP keepalive keep succeeding: only a response timeout notices. New
    /// connections work normally.
    pub fn half_open() -> Self {
        Self::new(Outcome::HalfOpen)
    }

    /// Broadcast: the endpoint answers normally and sends the response to every open
    /// connection, not only to the one that asked, like an RS485 converter that copies every
    /// frame from the bus to all its TCP clients (Laysi/trustiot#183). Another client waiting
    /// for its own answer then reads a frame of another unit (RTU-over-TCP) or with another
    /// client's transaction identifier (Modbus TCP).
    pub fn broadcast() -> Self {
        Self::new(Outcome::Answer).to_all_connections()
    }

    /// Acts no earlier than `delay` after the request was received; see [`Fault::delay`].
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.timing = Timing::Delay(delay);
        self
    }

    /// Acts only once the test calls [`super::FakeModbus::release`]; see [`Fault::hold`].
    pub fn held(mut self) -> Self {
        self.timing = Timing::Hold;
        self
    }

    /// Sends the response frame to every open connection; see [`Fault::broadcast`]. No effect
    /// on faults that send no response.
    pub fn to_all_connections(mut self) -> Self {
        self.broadcast = true;
        self
    }

    /// Only requests addressed to `unit`.
    pub fn unit(mut self, unit: u8) -> Self {
        self.unit = Some(unit);
        self
    }

    /// Only requests with function code `function`.
    pub fn function(mut self, function: u8) -> Self {
        self.function = Some(function);
        self
    }

    /// Only reads (function codes 1–4) whose range includes `address`, so a merged read that
    /// covers it matches too.
    pub fn address(mut self, address: u16) -> Self {
        self.address = Some(address);
        self
    }

    /// Only requests on connection `connection` (per-connection faults).
    pub fn connection(mut self, connection: ConnectionId) -> Self {
        self.connection = Some(connection);
        self
    }

    /// Only the request at position `index` of the request log (0 for the first request the
    /// endpoint receives); see [`super::Request::index`].
    pub fn nth(mut self, index: usize) -> Self {
        self.nth = Some(index);
        self
    }

    /// Only the first matching request.
    pub fn once(self) -> Self {
        self.times(1)
    }

    /// Only the first `times` matching requests.
    pub fn times(mut self, times: usize) -> Self {
        self.remaining = Some(times);
        self
    }

    /// Whether the fault applies to a request.
    pub(super) fn matches(
        &self,
        index: usize,
        connection: ConnectionId,
        unit: u8,
        pdu: &[u8],
    ) -> bool {
        let function = pdu.first().copied();
        self.remaining != Some(0)
            && self.unit.is_none_or(|wanted| wanted == unit)
            && self.function.is_none_or(|wanted| Some(wanted) == function)
            && self.connection.is_none_or(|wanted| wanted == connection)
            && self.nth.is_none_or(|wanted| wanted == index)
            && self.address.is_none_or(|wanted| {
                read_range(pdu).is_some_and(|(start, quantity)| {
                    u32::from(wanted) >= u32::from(start)
                        && u32::from(wanted) < u32::from(start) + u32::from(quantity)
                })
            })
    }
}
