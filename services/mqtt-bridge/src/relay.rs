//! One gateway connection: the WebSocket's binary messages to the broker's TCP connection and
//! back, byte for byte (README.md, "轉送").
//!
//! The bytes are the gateway's inner TLS to the broker (mTLS, decision 6). The relay never reads,
//! parses, logs or alters them: each binary message's payload is written to the broker as it is,
//! and what the broker sends is cut into binary messages of at most [`BUFFER_SIZE`] bytes in the
//! order it arrived. Text messages are not part of MQTT over WebSocket and end the connection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
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
        }
    }

    /// The close the bridge sends the gateway, if it sends one.
    fn close_frame(self) -> Option<CloseFrame> {
        let (code, reason) = match self {
            // The gateway's own close is answered by tungstenite; a broken connection gets none.
            Self::GatewayClosed | Self::GatewayError => return None,
            Self::TextMessage => (CloseCode::Unsupported, "binary messages only"),
            Self::BrokerClosed => (CloseCode::Normal, "the broker closed the connection"),
            Self::BrokerError => (CloseCode::Error, "the broker connection failed"),
            // 1001: the server is going away; the gateway reconnects, through the ingress, to a
            // pod that is still ready.
            Self::Shutdown => (CloseCode::Away, "the bridge is restarting"),
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

/// Relays until either side ends or `shutdown` resolves, then closes the WebSocket the way
/// [`End`] says. Never fails: how it ended is the result.
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

    let upstream = async {
        while let Some(message) = from_gateway.next().await {
            match message {
                Ok(Message::Binary(data)) => {
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
                        if to_gateway.send(message).await.is_err() {
                            return End::GatewayError;
                        }
                        totals.add(metrics, Direction::ToGateway, length);
                    }
                    Err(_) => return End::BrokerError,
                },
                _ = ping.tick() => {
                    if to_gateway.send(Message::Ping(Bytes::new())).await.is_err() {
                        return End::GatewayError;
                    }
                }
            }
        }
    };

    let end = tokio::select! {
        end = upstream => end,
        end = downstream => end,
        () = shutdown => End::Shutdown,
    };
    // The broker's connection closes when its halves drop at the end of this function, after the
    // gateway was told; nothing more is forwarded to it.
    match end.close_frame() {
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
