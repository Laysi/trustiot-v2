//! A scripted MQTT 5 broker for the link tests: it parses what the client sends just far enough
//! to check it, and answers with hand-written packets. It is written from the MQTT 5.0
//! standard independently of the agent's codec, so a shared misunderstanding would have to be
//! made twice.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the support module"
)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::time::Duration;

use agent::link::{ConnectError, ConnectStage, Connector};
use protocol::health::Status;
use protocol::topic::Channel;
use protocol::{GatewayId, SessionId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

pub mod wss;

/// The gateway of the tests.
pub const GATEWAY: &str = "gw-test-1";

pub fn gateway() -> GatewayId {
    GatewayId::new(GATEWAY).unwrap()
}

/// Hands every connection's broker side to the test.
pub struct DuplexConnector {
    brokers: mpsc::UnboundedSender<DuplexStream>,
    buffer: usize,
}

impl DuplexConnector {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<DuplexStream>) {
        Self::with_buffer(256 * 1024)
    }

    /// Connections that hold at most `buffer` bytes in each direction: a write blocks while
    /// the other side does not read.
    pub fn with_buffer(buffer: usize) -> (Self, mpsc::UnboundedReceiver<DuplexStream>) {
        let (brokers, receiver) = mpsc::unbounded_channel();
        (Self { brokers, buffer }, receiver)
    }
}

impl Connector for DuplexConnector {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        let (client, broker) = tokio::io::duplex(self.buffer);
        self.brokers
            .send(broker)
            .map_err(|_| ConnectError::new(ConnectStage::Tcp, "the test stopped accepting"))?;
        Ok(client)
    }
}

/// What the client sent in CONNECT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub client_id: String,
    pub keep_alive: u16,
    pub clean_start: bool,
    pub session_expiry: u32,
    pub receive_maximum: Option<u16>,
    pub maximum_packet_size: Option<u32>,
    pub will_topic: Option<String>,
    pub will_payload: Vec<u8>,
    pub will_qos: u8,
    pub will_retain: bool,
    pub username: bool,
    pub password: bool,
}

/// A PUBLISH from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    pub dup: bool,
    pub id: Option<u16>,
    pub message_expiry: Option<u32>,
}

/// A packet from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Connect(Connect),
    Publish(Publish),
    PubAck { id: u16, reason: u8 },
    Subscribe { id: u16, filters: Vec<(String, u8)> },
    PingReq,
    Disconnect { reason: u8 },
}

struct Body<'a>(&'a [u8]);

impl Body<'_> {
    fn u8(&mut self) -> u8 {
        let (first, rest) = self.0.split_first().expect("a byte");
        self.0 = rest;
        *first
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    fn varint(&mut self) -> usize {
        let mut value = 0;
        for shift in [0, 7, 14, 21] {
            let byte = self.u8();
            value |= usize::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
        }
        panic!("variable byte integer longer than four bytes")
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        head.to_vec()
    }

    fn binary(&mut self) -> Vec<u8> {
        let len = usize::from(self.u16());
        self.bytes(len)
    }

    fn string(&mut self) -> String {
        String::from_utf8(self.binary()).expect("UTF-8 string")
    }

    /// The properties the client may send: (identifier, value as u32).
    fn properties(&mut self) -> Vec<(u8, u32)> {
        let len = self.varint();
        let mut list = Body(&self.0[..len]);
        self.0 = &self.0[len..];
        let mut properties = Vec::new();
        while !list.0.is_empty() {
            let id = list.u8();
            let value = match id {
                0x02 | 0x11 | 0x27 => list.u32(),
                0x21 => u32::from(list.u16()),
                other => panic!("the client sent property {other:#x}"),
            };
            properties.push((id, value));
        }
        properties
    }
}

fn property(properties: &[(u8, u32)], id: u8) -> Option<u32> {
    properties
        .iter()
        .find_map(|&(found, value)| (found == id).then_some(value))
}

/// The broker side of one connection.
pub struct FakeBroker<S> {
    stream: S,
    buffer: Vec<u8>,
    subscriptions: Vec<Channel>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> FakeBroker<S> {
    /// A broker that expects the agent to subscribe to nothing.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            subscriptions: Vec::new(),
        }
    }

    /// A broker that expects the agent to subscribe to `channels`, in this order.
    pub fn subscribing(mut self, channels: &[Channel]) -> Self {
        self.subscriptions = channels.to_vec();
        self
    }

    /// The connection, for what its TLS negotiated.
    pub fn stream(&self) -> &S {
        &self.stream
    }

    /// Every byte not read yet, up to the client's close, for a connection that may end in the
    /// middle of a packet.
    pub async fn read_raw_to_end(&mut self) -> Vec<u8> {
        let mut bytes = std::mem::take(&mut self.buffer);
        self.stream.read_to_end(&mut bytes).await.unwrap();
        bytes
    }

    /// The next packet; `None` when the client closed the connection.
    pub async fn read(&mut self) -> Option<Packet> {
        loop {
            if let Some(packet) = self.take_packet() {
                return Some(packet);
            }
            let mut chunk = [0_u8; 4096];
            let len = self.stream.read(&mut chunk).await.unwrap_or(0);
            if len == 0 {
                assert!(
                    self.buffer.is_empty(),
                    "the client closed in the middle of a packet"
                );
                return None;
            }
            self.buffer.extend_from_slice(&chunk[..len]);
        }
    }

    /// The next packet other than PINGREQ, answering every PINGREQ.
    pub async fn read_answering_pings(&mut self) -> Option<Packet> {
        loop {
            match self.read().await? {
                Packet::PingReq => self.send(&pingresp()).await,
                packet => return Some(packet),
            }
        }
    }

    fn take_packet(&mut self) -> Option<Packet> {
        let first = *self.buffer.first()?;
        let mut remaining = 0_usize;
        let mut header = 1;
        loop {
            let byte = *self.buffer.get(header)?;
            remaining |= usize::from(byte & 0x7F) << (7 * (header - 1));
            header += 1;
            if byte & 0x80 == 0 {
                break;
            }
            assert!(header <= 4, "variable byte integer too long");
        }
        let total = header + remaining;
        if self.buffer.len() < total {
            return None;
        }
        let packet: Vec<u8> = self.buffer.drain(..total).collect();
        let mut body = Body(&packet[header..]);
        let flags = first & 0x0F;
        Some(match first >> 4 {
            1 => {
                assert_eq!(body.string(), "MQTT");
                assert_eq!(body.u8(), 5, "protocol level");
                let connect_flags = body.u8();
                let keep_alive = body.u16();
                let properties = body.properties();
                let client_id = body.string();
                let will = connect_flags & 0x04 != 0;
                let (will_topic, will_payload) = if will {
                    assert!(body.properties().is_empty(), "will properties");
                    (Some(body.string()), body.binary())
                } else {
                    (None, Vec::new())
                };
                Packet::Connect(Connect {
                    client_id,
                    keep_alive,
                    clean_start: connect_flags & 0x02 != 0,
                    session_expiry: property(&properties, 0x11).unwrap_or(0),
                    receive_maximum: property(&properties, 0x21)
                        .map(|value| u16::try_from(value).unwrap()),
                    maximum_packet_size: property(&properties, 0x27),
                    will_topic,
                    will_payload,
                    will_qos: (connect_flags >> 3) & 0x03,
                    will_retain: connect_flags & 0x20 != 0,
                    username: connect_flags & 0x80 != 0,
                    password: connect_flags & 0x40 != 0,
                })
            }
            3 => {
                let qos = (flags >> 1) & 0x03;
                let topic = body.string();
                let id = (qos > 0).then(|| body.u16());
                let properties = body.properties();
                Packet::Publish(Publish {
                    topic,
                    payload: body.0.to_vec(),
                    qos,
                    retain: flags & 0x01 != 0,
                    dup: flags & 0x08 != 0,
                    id,
                    message_expiry: property(&properties, 0x02),
                })
            }
            4 => {
                let id = body.u16();
                let reason = if body.0.is_empty() { 0 } else { body.u8() };
                Packet::PubAck { id, reason }
            }
            8 => {
                assert_eq!(flags, 0x02, "SUBSCRIBE flags");
                let id = body.u16();
                assert!(body.properties().is_empty());
                let mut filters = Vec::new();
                while !body.0.is_empty() {
                    filters.push((body.string(), body.u8()));
                }
                Packet::Subscribe { id, filters }
            }
            12 => Packet::PingReq,
            14 => Packet::Disconnect {
                reason: if body.0.is_empty() { 0 } else { body.u8() },
            },
            other => panic!("the client sent packet type {other}"),
        })
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        // The client may already be gone; the test notices through what it reads next.
        let _ = self.stream.write_all(bytes).await;
        let _ = self.stream.flush().await;
    }

    /// Accepts a connection: reads CONNECT, answers `connack`, reads the `online` PUBLISH and
    /// the SUBSCRIBE (when the agent subscribes to anything) and acknowledges both. Returns the
    /// CONNECT and the session.
    pub async fn accept_with(&mut self, connack: &[u8]) -> (Connect, SessionId) {
        let Some(Packet::Connect(connect)) = self.read().await else {
            panic!("the first packet is not CONNECT")
        };
        self.send(connack).await;
        let Some(Packet::Publish(online)) = self.read_answering_pings().await else {
            panic!("the first packet after CONNACK is not a PUBLISH")
        };
        assert_eq!(online.topic, Channel::Status.topic(&gateway()));
        assert!(online.retain && online.qos == 1, "{online:?}");
        let Status::Online(status) = Status::decode(&online.payload).unwrap() else {
            panic!("the first PUBLISH is not online")
        };
        self.send(&puback(online.id.unwrap(), 0)).await;
        if !self.subscriptions.is_empty() {
            let Some(Packet::Subscribe { id, filters }) = self.read_answering_pings().await else {
                panic!("no SUBSCRIBE after online")
            };
            let expected: Vec<(String, u8)> = self
                .subscriptions
                .iter()
                .map(|channel| (channel.topic(&gateway()), 1))
                .collect();
            assert_eq!(filters, expected);
            self.send(&suback(id, &vec![1; filters.len()])).await;
        }
        (connect, status.session)
    }

    /// [`Self::accept_with`] a plain successful CONNACK.
    pub async fn accept(&mut self) -> (Connect, SessionId) {
        self.accept_with(&connack(0, &[])).await
    }
}

fn with_header(first: u8, body: &[u8]) -> Vec<u8> {
    let mut packet = vec![first];
    let mut remaining = body.len();
    loop {
        let mut byte = u8::try_from(remaining & 0x7F).unwrap();
        remaining >>= 7;
        if remaining > 0 {
            byte |= 0x80;
        }
        packet.push(byte);
        if remaining == 0 {
            break;
        }
    }
    packet.extend_from_slice(body);
    packet
}

/// A client's CONNECT (MQTT 5, Clean Start, keepalive 60 s, no user name) with a retained QoS 1
/// will `will` on `will_topic`, as the agent registers its `offline` status.
pub fn connect_with_will(client_id: &str, will_topic: &str, will: &[u8]) -> Vec<u8> {
    fn field(body: &mut Vec<u8>, bytes: &[u8]) {
        body.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_be_bytes());
        body.extend_from_slice(bytes);
    }
    let mut body = Vec::new();
    field(&mut body, b"MQTT");
    // Version 5; flags: Will Retain 0x20, Will QoS 1 0x08, Will Flag 0x04, Clean Start 0x02.
    body.extend_from_slice(&[5, 0x2E, 0, 60, 0]);
    field(&mut body, client_id.as_bytes());
    body.push(0); // no will properties
    field(&mut body, will_topic.as_bytes());
    field(&mut body, will);
    with_header(0x10, &body)
}

/// CONNACK with `reason` and raw `properties`.
pub fn connack(reason: u8, properties: &[u8]) -> Vec<u8> {
    let mut body = vec![0, reason, u8::try_from(properties.len()).unwrap()];
    body.extend_from_slice(properties);
    with_header(0x20, &body)
}

pub fn puback(id: u16, reason: u8) -> Vec<u8> {
    let id = id.to_be_bytes();
    if reason == 0 {
        vec![0x40, 2, id[0], id[1]]
    } else {
        vec![0x40, 3, id[0], id[1], reason]
    }
}

pub fn suback(id: u16, reasons: &[u8]) -> Vec<u8> {
    let mut body = id.to_be_bytes().to_vec();
    body.push(0);
    body.extend_from_slice(reasons);
    with_header(0x90, &body)
}

pub fn pingresp() -> [u8; 2] {
    [0xD0, 0]
}

/// A PUBLISH from the broker; QoS 1 needs `id`.
pub fn publish(topic: &str, payload: &[u8], qos: u8, retain: bool, id: Option<u16>) -> Vec<u8> {
    let mut body = u16::try_from(topic.len()).unwrap().to_be_bytes().to_vec();
    body.extend_from_slice(topic.as_bytes());
    if let Some(id) = id {
        body.extend_from_slice(&id.to_be_bytes());
    }
    body.push(0);
    body.extend_from_slice(payload);
    with_header(0x30 | (qos << 1) | u8::from(retain), &body)
}

pub fn disconnect(reason: u8) -> Vec<u8> {
    vec![0xE0, 1, reason]
}

/// DISCONNECT with `reason` and a Reason String property.
pub fn disconnect_with_reason_string(reason: u8, text: &str) -> Vec<u8> {
    let mut body = vec![reason];
    body.extend_from_slice(&reason_string_properties(text));
    with_header(0xE0, &body)
}

/// PUBACK for `id` with `reason` and a Reason String property.
pub fn puback_with_reason_string(id: u16, reason: u8, text: &str) -> Vec<u8> {
    let mut body = id.to_be_bytes().to_vec();
    body.push(reason);
    body.extend_from_slice(&reason_string_properties(text));
    with_header(0x40, &body)
}

/// A property list (its length first) with one Reason String.
fn reason_string_properties(text: &str) -> Vec<u8> {
    let mut property = vec![0x1F];
    property.extend_from_slice(&u16::try_from(text.len()).unwrap().to_be_bytes());
    property.extend_from_slice(text.as_bytes());
    let mut properties = varint(property.len());
    properties.extend_from_slice(&property);
    properties
}

/// A variable byte integer.
fn varint(mut value: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = u8::try_from(value & 0x7F).unwrap();
        value >>= 7;
        if value > 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            return bytes;
        }
    }
}

/// A generous bound for awaiting something in a test (CLAUDE.md: test time limits).
pub const WAIT: Duration = Duration::from_secs(60);
