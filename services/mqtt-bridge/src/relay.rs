//! One gateway connection: the WebSocket's binary messages to the broker's TCP connection and
//! back, byte for byte (README.md, "轉送").
//!
//! The bytes are the gateway's inner TLS to the broker (mTLS, decision 6). The relay never reads,
//! parses, logs or alters them: each binary message's payload is written to the broker as it is,
//! and what the broker sends is cut into binary messages of at most [`BUFFER_SIZE`] bytes in the
//! order it arrived. Text messages are not part of MQTT over WebSocket and end the connection.
//!
//! Two deadlines free the slot of a connection that holds it without being a gateway
//! (README.md, "連線上限"): the inner connection must make progress within
//! `broker.handshake_timeout` ([`Progress`]), and after each ping the gateway must send something
//! within `websocket.pong_timeout` ([`PongWatch`]) and take every message sent to it within that
//! time ([`send_within`]).

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{Sink, SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Bytes, Message, Utf8Bytes};

use crate::metrics::{Direction, Metrics};

/// The read buffer of each direction, and the largest message the bridge sends: 8 KiB, as the
/// 2026-10-10 feasibility test settled (tungstenite's default 128 KiB cost 159 MiB more for
/// 1,000 connections, docs/feasibility/2026-10-10-edge-poc.md).
pub const BUFFER_SIZE: usize = 8 * 1024;
/// The largest message or frame accepted from the gateway: exactly what the gateway sends at
/// most per message (edge/agent, `MAX_OUTGOING_MESSAGE`, Laysi/trustiot-v2#86). tungstenite
/// reserves a frame's declared length as soon as it reads the frame's header, so this bounds
/// what a client that sends only a header makes the bridge hold; a larger one ends the connection.
pub const MAX_INCOMING_MESSAGE: usize = 32 * 1024;
/// What one connection can make the bridge hold at most, rounded up: a fragmented message being
/// assembled and the next frame's reservation (`MAX_INCOMING_MESSAGE` each), the read buffer from
/// the broker (`BUFFER_SIZE`), the WebSocket write buffer (`2 * BUFFER_SIZE` at most), and the
/// task, the two sockets' state and the kernel's socket buffers. The connection cap is checked
/// against the memory limit with it (`crate::memory`).
pub const WORST_CASE_PER_CONNECTION: usize = 128 * 1024;

/// The bridge's WebSocket settings: small buffers, every message written at once (MQTT is
/// interactive), and a bound on what a gateway can make the bridge hold.
pub fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(BUFFER_SIZE)
        .write_buffer_size(0)
        .max_write_buffer_size(BUFFER_SIZE * 2)
        .max_message_size(Some(MAX_INCOMING_MESSAGE))
        .max_frame_size(Some(MAX_INCOMING_MESSAGE))
}

/// How a relayed connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The gateway sent a close or ended its connection.
    GatewayClosed,
    /// Reading from or writing to the gateway failed (including a message over the limit).
    GatewayError,
    /// The gateway sent a text message.
    TextMessage,
    /// The broker ended the TCP connection.
    BrokerClosed,
    /// Reading from or writing to the broker failed.
    BrokerError,
    /// The bridge is shutting down (SIGTERM): the gateway was told to reconnect elsewhere.
    Shutdown,
    /// The inner connection made no progress within `broker.handshake_timeout`.
    HandshakeTimeout,
    /// The gateway sent nothing within `websocket.pong_timeout` of a ping, or took no message
    /// the bridge sent it within that time.
    PongTimeout,
}

impl End {
    /// The `reason` of the log line and the metric.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::GatewayClosed => "gateway_closed",
            Self::GatewayError => "gateway_error",
            Self::TextMessage => "text_message",
            Self::BrokerClosed => "broker_closed",
            Self::BrokerError => "broker_error",
            Self::Shutdown => "shutdown",
            Self::HandshakeTimeout => "handshake_timeout",
            Self::PongTimeout => "pong_timeout",
        }
    }

    /// The close the bridge sends the gateway, if it sends one.
    fn close_frame(self) -> Option<CloseFrame> {
        let (code, reason) = match self {
            // The gateway's own close is answered by tungstenite; a broken connection, or one
            // whose gateway stopped answering, gets none.
            Self::GatewayClosed | Self::GatewayError | Self::PongTimeout => return None,
            Self::TextMessage => (CloseCode::Unsupported, "binary messages only"),
            Self::BrokerClosed => (CloseCode::Normal, "the broker closed the connection"),
            Self::BrokerError => (CloseCode::Error, "the broker connection failed"),
            // 1001: the server is going away; the gateway reconnects, through the ingress, to a
            // pod that is still ready.
            Self::Shutdown => (CloseCode::Away, "the bridge is restarting"),
            // 1008: a gateway reaches the broker's MQTT within the deadline; this client did not.
            Self::HandshakeTimeout => (CloseCode::Policy, "no progress to the broker in time"),
        };
        Some(CloseFrame {
            code,
            reason: Utf8Bytes::from_static(reason),
        })
    }
}

/// What a relay needs besides its two connections.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// A WebSocket ping this often (`websocket.ping_interval`).
    pub ping_interval: Duration,
    /// How long after a ping the gateway has to send anything (`websocket.pong_timeout`).
    pub pong_timeout: Duration,
    /// How long the inner connection has to make progress (`broker.handshake_timeout`).
    pub handshake_timeout: Duration,
    /// How long to wait for the gateway's close after sending one (`websocket.close_timeout`).
    pub close_timeout: Duration,
}

/// Bytes relayed in each direction, for the connection's last log line.
#[derive(Debug, Default)]
pub struct Totals {
    to_broker: AtomicU64,
    to_gateway: AtomicU64,
}

impl Totals {
    /// Payload bytes the gateway sent to the broker.
    pub fn to_broker(&self) -> u64 {
        self.to_broker.load(Ordering::Relaxed)
    }

    /// Payload bytes the broker sent to the gateway.
    pub fn to_gateway(&self) -> u64 {
        self.to_gateway.load(Ordering::Relaxed)
    }

    fn add(&self, metrics: &Metrics, direction: Direction, bytes: usize) {
        let counter = match direction {
            Direction::ToBroker => &self.to_broker,
            Direction::ToGateway => &self.to_gateway,
        };
        counter.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        metrics.relayed(direction, bytes);
    }
}

/// Whether the inner connection made progress: the broker sent data, then the gateway sent data,
/// then the broker sent data again. The bridge cannot read the inner TLS, so this is judged from
/// the order of the bytes alone. With the broker's mTLS (TLS 1.3, a client certificate required)
/// the broker's second turn comes only after the gateway's second flight: its session tickets and
/// the CONNACK after a completed handshake, or an alert followed by the broker closing the
/// connection. A client that connects and sends nothing, or a ClientHello and then nothing, never
/// gets there.
#[derive(Debug, Default)]
pub struct Progress(AtomicU8);

impl Progress {
    const WAITING_FOR_BROKER: u8 = 0;
    const BROKER_SPOKE: u8 = 1;
    const GATEWAY_ANSWERED: u8 = 2;
    const ESTABLISHED: u8 = 3;

    /// The broker sent data to the gateway.
    pub fn broker_sent(&self) {
        let _ = self.advance(Self::WAITING_FOR_BROKER, Self::BROKER_SPOKE);
        let _ = self.advance(Self::GATEWAY_ANSWERED, Self::ESTABLISHED);
    }

    /// The gateway sent data to the broker.
    pub fn gateway_sent(&self) {
        let _ = self.advance(Self::BROKER_SPOKE, Self::GATEWAY_ANSWERED);
    }

    /// Whether the broker sent data after the gateway answered it.
    pub fn established(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::ESTABLISHED
    }

    fn advance(&self, from: u8, to: u8) -> bool {
        self.0
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Relays until either side ends, a deadline passes, or `shutdown` resolves, then closes the
/// WebSocket the way [`End`] says. Never fails: how it ended is the result.
pub async fn relay<S>(
    websocket: WebSocketStream<S>,
    broker: TcpStream,
    settings: Settings,
    shutdown: impl Future<Output = ()>,
    metrics: &Metrics,
    totals: &Totals,
) -> End
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut to_gateway, mut from_gateway) = websocket.split();
    let (mut from_broker, mut to_broker) = broker.into_split();
    let progress = Progress::default();
    // Every message read from the gateway, for the pong deadline.
    let heard = AtomicU64::new(0);
    let pong = PongWatch::new();

    let upstream = async {
        while let Some(message) = from_gateway.next().await {
            if message.is_ok() {
                heard.fetch_add(1, Ordering::AcqRel);
            }
            match message {
                Ok(Message::Binary(data)) => {
                    // Recorded before the write can yield: the broker's answer may be read (by
                    // the other branch) before the write returns, and must find this turn.
                    if !data.is_empty() {
                        progress.gateway_sent();
                    }
                    if to_broker.write_all(&data).await.is_err() {
                        return End::BrokerError;
                    }
                    totals.add(metrics, Direction::ToBroker, data.len());
                }
                // tungstenite answers pings itself; pongs only show the gateway is there.
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Ok(Message::Text(_)) => return End::TextMessage,
                Ok(Message::Close(_)) => return End::GatewayClosed,
                Err(_) => return End::GatewayError,
            }
        }
        End::GatewayClosed
    };

    let downstream = async {
        let mut buffer = vec![0; BUFFER_SIZE];
        let mut ping = tokio::time::interval_at(
            Instant::now() + settings.ping_interval,
            settings.ping_interval,
        );
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                read = from_broker.read(&mut buffer) => match read {
                    Ok(0) => return End::BrokerClosed,
                    Ok(length) => {
                        let message = Message::Binary(Bytes::copy_from_slice(&buffer[..length]));
                        // Before the send, for the same reason as `gateway_sent` above.
                        progress.broker_sent();
                        if let Err(end) =
                            send_within(&mut to_gateway, message, settings.pong_timeout).await
                        {
                            return end;
                        }
                        totals.add(metrics, Direction::ToGateway, length);
                    }
                    Err(_) => return End::BrokerError,
                },
                _ = ping.tick() => {
                    // Armed before the write: a gateway that stops reading makes the write wait,
                    // and that must not hold the deadline back.
                    pong.arm(heard.load(Ordering::Acquire), settings.pong_timeout);
                    let ping = Message::Ping(Bytes::new());
                    if let Err(end) = send_within(&mut to_gateway, ping, settings.pong_timeout).await
                    {
                        return end;
                    }
                }
            }
        }
    };

    let pong_deadline = async {
        pong.expired(&heard).await;
        End::PongTimeout
    };

    let handshake = async {
        tokio::time::sleep(settings.handshake_timeout).await;
        if progress.established() {
            // Progress was made in time; from here on only the other branches end the relay.
            std::future::pending::<()>().await;
        }
        End::HandshakeTimeout
    };

    let end = tokio::select! {
        end = upstream => end,
        end = downstream => end,
        end = handshake => end,
        end = pong_deadline => end,
        () = shutdown => End::Shutdown,
    };
    // The broker's connection closes when its halves drop at the end of this function, after the
    // gateway was told; nothing more is forwarded to it.
    match end.close_frame() {
        Some(frame) if end == End::HandshakeTimeout => {
            // A client that made no progress gets its close, but neither the full close timeout
            // to take it nor a wait for its answer: its slot is free within
            // `broker.handshake_timeout` + [`HANDSHAKE_CLOSE_SEND`].
            let _ = tokio::time::timeout(
                HANDSHAKE_CLOSE_SEND.min(settings.close_timeout),
                to_gateway.send(Message::Close(Some(frame))),
            )
            .await;
        }
        Some(frame) => {
            let closing = async {
                if to_gateway.send(Message::Close(Some(frame))).await.is_ok() {
                    // Wait for the gateway's close; whatever it still sends is not forwarded.
                    while let Some(Ok(message)) = from_gateway.next().await {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                    }
                }
            };
            let _ = tokio::time::timeout(settings.close_timeout, closing).await;
        }
        None if end == End::GatewayClosed => {
            // Sends the close tungstenite queued in answer to the gateway's.
            let _ = tokio::time::timeout(settings.close_timeout, to_gateway.close()).await;
        }
        None => {}
    }
    end
}

/// The longest the bridge tries to send the close of a connection ended for
/// [`End::HandshakeTimeout`].
pub const HANDSHAKE_CLOSE_SEND: Duration = Duration::from_secs(1);

/// Sends `message` to the gateway. A gateway that stops reading makes the write wait (TCP
/// backpressure) and, with it, everything else the broker sends; one that has not taken a
/// message within the pong deadline is treated like one that answers no ping. Without this bound
/// a gateway that sent something just before it stopped reading would hold its slots forever:
/// the loop that would send (and arm) the next ping is the one waiting here.
async fn send_within<G>(to_gateway: &mut G, message: Message, within: Duration) -> Result<(), End>
where
    G: Sink<Message> + Unpin,
{
    match tokio::time::timeout(within, to_gateway.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(End::GatewayError),
        Err(_) => Err(End::PongTimeout),
    }
}

/// The pong deadline of the oldest unanswered ping. It runs apart from the loop that writes to the
/// gateway: a client that stops reading makes those writes wait (TCP backpressure), and the
/// deadline must pass all the same. Both sides live in the relay's one task.
struct PongWatch {
    started: Instant,
    /// When the oldest unanswered ping times out, in milliseconds after `started`;
    /// [`PongWatch::NOT_ARMED`] while no ping waits for an answer.
    deadline_ms: AtomicU64,
    /// How many gateway messages had been read when that ping was sent.
    heard_then: AtomicU64,
    armed: Notify,
}

impl PongWatch {
    const NOT_ARMED: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            started: Instant::now(),
            deadline_ms: AtomicU64::new(Self::NOT_ARMED),
            heard_then: AtomicU64::new(0),
            armed: Notify::new(),
        }
    }

    /// A ping is about to be sent, with `heard` gateway messages read so far. An unanswered ping
    /// (nothing heard since it) keeps its deadline: a later ping does not move it. One the gateway
    /// answered no longer counts, so this ping starts its own deadline.
    fn arm(&self, heard: u64, timeout: Duration) {
        let waiting = self.deadline_ms.load(Ordering::Acquire) != Self::NOT_ARMED;
        if waiting && self.heard_then.load(Ordering::Acquire) == heard {
            return;
        }
        let deadline = self.started.elapsed().saturating_add(timeout);
        let deadline = u64::try_from(deadline.as_millis()).unwrap_or(Self::NOT_ARMED - 1);
        self.heard_then.store(heard, Ordering::Release);
        self.deadline_ms.store(deadline, Ordering::Release);
        self.armed.notify_one();
    }

    /// Resolves when a ping's deadline passed and the gateway sent nothing since the ping
    /// (`heard` unchanged). When it did send something, the next ping starts a new deadline.
    async fn expired(&self, heard: &AtomicU64) {
        loop {
            let deadline = self.deadline_ms.load(Ordering::Acquire);
            if deadline == Self::NOT_ARMED {
                self.armed.notified().await;
                continue;
            }
            tokio::time::sleep_until(self.started + Duration::from_millis(deadline)).await;
            if self.deadline_ms.load(Ordering::Acquire) != deadline {
                // The ping slept on was answered and a later one armed its own, later deadline.
                continue;
            }
            if heard.load(Ordering::Acquire) == self.heard_then.load(Ordering::Acquire) {
                return;
            }
            self.deadline_ms.store(Self::NOT_ARMED, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_needs_the_broker_to_speak_after_the_gateway_answered() {
        // A gateway: ClientHello, the broker's first flight (perhaps in two reads), the
        // gateway's second flight, then session tickets and CONNACK.
        let progress = Progress::default();
        progress.gateway_sent();
        assert!(!progress.established(), "a ClientHello alone");
        progress.broker_sent();
        progress.broker_sent();
        assert!(!progress.established(), "the broker's first flight alone");
        progress.gateway_sent();
        assert!(!progress.established(), "the gateway's answer alone");
        progress.broker_sent();
        assert!(progress.established());
        // Nothing moves it back.
        progress.gateway_sent();
        progress.broker_sent();
        assert!(progress.established());
    }

    const PONG: Duration = Duration::from_secs(10);
    /// Far beyond any deadline these tests arm: reaching it means the watch never fired.
    const NEVER: Duration = Duration::from_secs(3600);

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_ping_keeps_its_deadline() {
        let watch = PongWatch::new();
        let heard = AtomicU64::new(0);
        watch.arm(0, PONG);
        tokio::time::advance(Duration::from_secs(5)).await;
        // A later ping, still nothing heard: the deadline stays at 10 s.
        watch.arm(0, PONG);
        tokio::time::timeout(NEVER, watch.expired(&heard))
            .await
            .expect("the first ping's deadline");
        assert_eq!(watch.started.elapsed(), PONG);
    }

    #[tokio::test(start_paused = true)]
    async fn after_an_answer_the_next_ping_starts_its_own_deadline() {
        let watch = PongWatch::new();
        let heard = AtomicU64::new(0);
        let expired = watch.expired(&heard);
        tokio::pin!(expired);
        watch.arm(0, PONG);
        // The watch is asleep on the first ping's deadline (10 s) when the gateway answers at
        // 1 s and the next ping goes out at 2 s; then the gateway goes silent.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut expired)
                .await
                .is_err()
        );
        heard.store(1, Ordering::Release);
        tokio::time::advance(Duration::from_secs(1)).await;
        watch.arm(1, PONG);
        tokio::time::timeout(NEVER, &mut expired)
            .await
            .expect("the second ping's deadline");
        assert_eq!(watch.started.elapsed(), Duration::from_secs(12));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answered_ping_alone_never_expires() {
        let watch = PongWatch::new();
        let heard = AtomicU64::new(0);
        watch.arm(0, PONG);
        heard.store(1, Ordering::Release);
        assert!(
            tokio::time::timeout(NEVER, watch.expired(&heard))
                .await
                .is_err(),
            "expired although the gateway answered"
        );
    }

    #[test]
    fn gateway_data_before_the_broker_spoke_is_no_answer() {
        let progress = Progress::default();
        progress.gateway_sent();
        progress.gateway_sent();
        progress.broker_sent();
        assert!(!progress.established());
    }
}
