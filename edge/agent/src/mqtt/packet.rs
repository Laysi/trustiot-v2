//! The MQTT 5.0 packets of a client (OASIS MQTT Version 5.0, 2019-03-07), without I/O.
//!
//! Only what the agent needs: it encodes CONNECT, PUBLISH, PUBACK, SUBSCRIBE, PINGREQ and
//! DISCONNECT, and decodes what a server sends a client: CONNACK, PUBLISH, PUBACK, SUBACK,
//! PINGRESP and DISCONNECT. Every other packet type is decoded far enough to name it and then
//! refused as a protocol error, because the agent never asks for it (it never sends QoS 2 or
//! UNSUBSCRIBE, and a server must not send AUTH to a client whose CONNECT had no Authentication
//! Method, MQTT-4.12.0-6).
//!
//! The MUST rules of MQTT 5 on the contents of these packets are checked as they are decoded:
//! the shortest encoding of a variable byte integer, the fixed header flags, the property set
//! of each packet type with each property at most once (User Property and Subscription
//! Identifier aside) and in range, well-formed UTF-8 strings without U+0000, topics without
//! wildcards, non-zero packet identifiers, and a Remaining Length that the fields fill exactly.
//! Rules about a connection's state (CONNACK first and once, Session Present after a clean
//! start, acknowledgements of what the client sent, the QoS granted) are checked in
//! `connection`, which also drops a PUBLISH outside the gateway's down channels.
//!
//! Decoding treats the input as hostile: a length is never trusted beyond the bytes at hand,
//! the declared size of a packet is checked against the client's Maximum Packet Size before
//! anything of its body is read or buffered, and no input panics. Properties the agent does not
//! use (user properties, correlation data, ...) are checked and skipped without allocating.

use std::fmt;

/// The protocol level of MQTT 5.0 in CONNECT.
const PROTOCOL_LEVEL: u8 = 5;
/// The largest Remaining Length a variable byte integer can hold (four bytes).
pub(crate) const MAX_REMAINING_LENGTH: usize = 268_435_455;

/// A packet type: the high nibble of the first byte of the fixed header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PacketType {
    Connect,
    ConnAck,
    Publish,
    PubAck,
    PubRec,
    PubRel,
    PubComp,
    Subscribe,
    SubAck,
    Unsubscribe,
    UnsubAck,
    PingReq,
    PingResp,
    Disconnect,
    Auth,
}

impl PacketType {
    fn from_nibble(nibble: u8) -> Option<Self> {
        Some(match nibble {
            1 => Self::Connect,
            2 => Self::ConnAck,
            3 => Self::Publish,
            4 => Self::PubAck,
            5 => Self::PubRec,
            6 => Self::PubRel,
            7 => Self::PubComp,
            8 => Self::Subscribe,
            9 => Self::SubAck,
            10 => Self::Unsubscribe,
            11 => Self::UnsubAck,
            12 => Self::PingReq,
            13 => Self::PingResp,
            14 => Self::Disconnect,
            15 => Self::Auth,
            _ => return None,
        })
    }
}

/// Why received bytes are not a packet the agent accepts. Each maps to the reason code MQTT 5
/// has for it ([`DecodeError::reason_code`]), which the agent logs before it disconnects with
/// 0x04.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DecodeError {
    /// The bytes break the packet format (MQTT 5 section 1.2, "Malformed Packet").
    #[error("malformed packet: {0}")]
    Malformed(&'static str),
    /// The bytes are well formed but break the protocol's rules ("Protocol Error").
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    /// The packet is larger than the Maximum Packet Size the client announced.
    #[error("packet of {size} bytes is above the maximum packet size of {limit} bytes")]
    TooLarge { size: usize, limit: usize },
    /// A topic alias, which the client allows none of (Topic Alias Maximum 0).
    #[error("protocol error: topic alias, but the client allows none")]
    TopicAlias,
    /// A packet a server never sends to this client.
    #[error("protocol error: unexpected {0:?} packet")]
    Unexpected(PacketType),
    /// A reason code MQTT 5 does not allow in that packet. Neither a success nor a failure:
    /// the client cannot know what the server meant.
    #[error(
        "protocol error: {packet} with reason code 0x{code:02X}, which MQTT 5 does not allow there"
    )]
    ReasonCode { packet: &'static str, code: u8 },
}

impl DecodeError {
    /// The reason code MQTT 5 has for this error. The agent logs it; the DISCONNECT it sends is
    /// 0x04 either way, so that the broker publishes the will (see `connection::violation`).
    pub(crate) const fn reason_code(&self) -> u8 {
        match self {
            Self::Malformed(_) => reason::MALFORMED_PACKET,
            Self::Protocol(_) | Self::Unexpected(_) | Self::ReasonCode { .. } => {
                reason::PROTOCOL_ERROR
            }
            Self::TooLarge { .. } => reason::PACKET_TOO_LARGE,
            Self::TopicAlias => reason::TOPIC_ALIAS_INVALID,
        }
    }
}

/// Why a packet cannot be encoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EncodeError {
    /// A string or binary field is longer than its two-byte length prefix allows.
    #[error("{0} is longer than 65535 bytes")]
    FieldTooLong(&'static str),
    /// The packet would exceed the largest Remaining Length.
    #[error("packet is larger than MQTT allows")]
    PacketTooLarge,
    /// A string holds U+0000, which MQTT forbids in UTF-8 strings.
    #[error("{0} contains U+0000")]
    NulInString(&'static str),
    /// A SUBSCRIBE without a topic filter, which MQTT 5 forbids (MQTT-3.8.3-2).
    #[error("SUBSCRIBE has no topic filter")]
    NoTopicFilter,
}

/// MQTT 5 reason codes the agent sends or acts on.
pub(crate) mod reason {
    /// Success (CONNACK, PUBACK), normal disconnection (DISCONNECT).
    pub(crate) const SUCCESS: u8 = 0x00;
    /// SUBACK: granted QoS 0, less than the QoS 1 the agent asks for (same value as SUCCESS).
    pub(crate) const GRANTED_QOS_0: u8 = 0x00;
    /// DISCONNECT: the client disconnects but wants the server to publish its will. The agent
    /// ends every failed connection with it: Mosquitto drops the will after any other code.
    pub(crate) const DISCONNECT_WITH_WILL: u8 = 0x04;
    /// PUBACK: accepted, but nobody subscribes to the topic. A success.
    pub(crate) const NO_MATCHING_SUBSCRIBERS: u8 = 0x10;
    /// SUBACK: granted QoS 1 (what the agent asks for).
    pub(crate) const GRANTED_QOS_1: u8 = 0x01;
    /// SUBACK: granted QoS 2, more than the agent asks for.
    pub(crate) const GRANTED_QOS_2: u8 = 0x02;
    /// The codes MQTT 5 has for a broken packet; the agent logs them and sends 0x04.
    pub(crate) const MALFORMED_PACKET: u8 = 0x81;
    pub(crate) const PROTOCOL_ERROR: u8 = 0x82;
    pub(crate) const TOPIC_ALIAS_INVALID: u8 = 0x94;
    pub(crate) const PACKET_TOO_LARGE: u8 = 0x95;

    /// The codes a server may send in CONNACK (MQTT 5 section 3.2.2.2).
    pub(crate) const CONNACK: &[u8] = &[
        0x00, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x8C, 0x90, 0x95,
        0x97, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9F,
    ];
    /// The codes of a PUBACK (section 3.4.2.1).
    pub(crate) const PUBACK: &[u8] = &[0x00, 0x10, 0x80, 0x83, 0x87, 0x90, 0x91, 0x97, 0x99];
    /// The codes of a SUBACK, one per topic filter (section 3.9.3).
    pub(crate) const SUBACK: &[u8] = &[
        0x00, 0x01, 0x02, 0x80, 0x83, 0x87, 0x8F, 0x91, 0x97, 0x9E, 0xA1, 0xA2,
    ];
    /// The codes a server may send in DISCONNECT (section 3.14.2.1; 0x04 is the client's).
    pub(crate) const SERVER_DISCONNECT: &[u8] = &[
        0x00, 0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8B, 0x8D, 0x8E, 0x8F, 0x90, 0x93, 0x94, 0x95,
        0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1, 0xA2,
    ];

    /// `code` if `allowed` lists it; otherwise a protocol error naming `packet`.
    pub(crate) fn check(
        packet: &'static str,
        code: u8,
        allowed: &[u8],
    ) -> Result<u8, super::DecodeError> {
        if allowed.contains(&code) {
            Ok(code)
        } else {
            Err(super::DecodeError::ReasonCode { packet, code })
        }
    }

    /// The name of a reason code, for logs; `None` for a code MQTT 5 does not define.
    pub(crate) const fn name(code: u8) -> Option<&'static str> {
        Some(match code {
            0x00 => "success",
            0x01 => "granted QoS 1",
            0x02 => "granted QoS 2",
            0x04 => "disconnect with will message",
            0x10 => "no matching subscribers",
            0x80 => "unspecified error",
            0x81 => "malformed packet",
            0x82 => "protocol error",
            0x83 => "implementation specific error",
            0x84 => "unsupported protocol version",
            0x85 => "client identifier not valid",
            0x86 => "bad user name or password",
            0x87 => "not authorized",
            0x88 => "server unavailable",
            0x89 => "server busy",
            0x8A => "banned",
            0x8B => "server shutting down",
            0x8C => "bad authentication method",
            0x8D => "keep alive timeout",
            0x8E => "session taken over",
            0x8F => "topic filter invalid",
            0x90 => "topic name invalid",
            0x91 => "packet identifier in use",
            0x93 => "receive maximum exceeded",
            0x94 => "topic alias invalid",
            0x95 => "packet too large",
            0x96 => "message rate too high",
            0x97 => "quota exceeded",
            0x98 => "administrative action",
            0x99 => "payload format invalid",
            0x9A => "retain not supported",
            0x9B => "QoS not supported",
            0x9C => "use another server",
            0x9D => "server moved",
            0x9E => "shared subscriptions not supported",
            0x9F => "connection rate exceeded",
            0xA0 => "maximum connect time",
            0xA1 => "subscription identifiers not supported",
            0xA2 => "wildcard subscriptions not supported",
            _ => return None,
        })
    }
}

/// A reason code with its name, for logs and errors: `0x87 (not authorized)`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReasonCode(pub u8);

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match reason::name(self.0) {
            Some(name) => write!(f, "{:#04x} ({name})", self.0),
            None => write!(f, "{:#04x}", self.0),
        }
    }
}

impl fmt::Debug for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

// ----- Encoding (client to server) -----

/// The last will registered with CONNECT.
#[derive(Clone, Debug)]
pub(crate) struct Will<'a> {
    pub(crate) topic: &'a str,
    pub(crate) payload: &'a [u8],
    /// QoS 0 or 1.
    pub(crate) qos: u8,
    pub(crate) retain: bool,
}

/// A CONNECT packet. The agent never sends a user name or password: the broker takes the
/// identity from the client certificate.
#[derive(Clone, Debug)]
pub(crate) struct Connect<'a> {
    pub(crate) client_id: &'a str,
    pub(crate) keep_alive_secs: u16,
    pub(crate) clean_start: bool,
    /// Session Expiry Interval in seconds; 0 ends the session with the connection.
    pub(crate) session_expiry_secs: u32,
    /// How many QoS 1 PUBLISH packets the client accepts unacknowledged (at least 1).
    pub(crate) receive_maximum: u16,
    /// The largest packet the client accepts.
    pub(crate) maximum_packet_size: u32,
    pub(crate) will: Option<Will<'a>>,
}

/// The header of an outgoing PUBLISH: everything but the payload, which the caller writes
/// right after it, so a large payload is never copied into a second buffer.
#[derive(Clone, Debug)]
pub(crate) struct PublishHeader<'a> {
    pub(crate) topic: &'a str,
    /// QoS 0 or 1.
    pub(crate) qos: u8,
    pub(crate) retain: bool,
    /// Required for QoS 1.
    pub(crate) packet_id: Option<u16>,
    /// Message Expiry Interval in seconds.
    pub(crate) message_expiry_secs: Option<u32>,
    pub(crate) payload_len: usize,
}

/// Bytes a variable byte integer takes.
const fn varint_len(value: usize) -> usize {
    match value {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    loop {
        // The low seven bits; the mask makes the narrowing exact.
        let mut byte = u8::try_from(value & 0x7F).unwrap_or(0);
        value >>= 7;
        if value > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn checked_len(len: usize, what: &'static str) -> Result<u16, EncodeError> {
    u16::try_from(len).map_err(|_| EncodeError::FieldTooLong(what))
}

fn put_string(out: &mut Vec<u8>, value: &str, what: &'static str) -> Result<(), EncodeError> {
    if value.contains('\0') {
        return Err(EncodeError::NulInString(what));
    }
    put_binary(out, value.as_bytes(), what)
}

fn put_binary(out: &mut Vec<u8>, value: &[u8], what: &'static str) -> Result<(), EncodeError> {
    put_u16(out, checked_len(value.len(), what)?);
    out.extend_from_slice(value);
    Ok(())
}

/// Prepends the fixed header to `body` and returns the whole packet.
fn finish(first_byte: u8, body: &[u8]) -> Result<Vec<u8>, EncodeError> {
    finish_with_trailer(first_byte, body, 0)
}

/// The fixed header and `body`, for a packet whose remaining length also counts `trailer`
/// bytes the caller sends after it (a PUBLISH payload).
fn finish_with_trailer(
    first_byte: u8,
    body: &[u8],
    trailer: usize,
) -> Result<Vec<u8>, EncodeError> {
    let remaining = body
        .len()
        .checked_add(trailer)
        .filter(|&len| len <= MAX_REMAINING_LENGTH)
        .ok_or(EncodeError::PacketTooLarge)?;
    let mut packet = Vec::with_capacity(1 + varint_len(remaining) + body.len());
    packet.push(first_byte);
    put_varint(&mut packet, remaining);
    packet.extend_from_slice(body);
    Ok(packet)
}

/// Encodes a CONNECT packet.
pub(crate) fn connect(connect: &Connect<'_>) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::with_capacity(64 + connect.will.as_ref().map_or(0, |w| w.payload.len()));
    put_string(&mut body, "MQTT", "protocol name")?;
    body.push(PROTOCOL_LEVEL);
    let mut flags = 0_u8;
    if connect.clean_start {
        flags |= 0x02;
    }
    if let Some(will) = &connect.will {
        flags |= 0x04 | ((will.qos & 0x03) << 3);
        if will.retain {
            flags |= 0x20;
        }
    }
    body.push(flags);
    put_u16(&mut body, connect.keep_alive_secs);

    let mut properties = Vec::with_capacity(16);
    if connect.session_expiry_secs != 0 {
        properties.push(0x11);
        put_u32(&mut properties, connect.session_expiry_secs);
    }
    properties.push(0x21);
    put_u16(&mut properties, connect.receive_maximum.max(1));
    properties.push(0x27);
    put_u32(&mut properties, connect.maximum_packet_size.max(1));
    put_varint(&mut body, properties.len());
    body.extend_from_slice(&properties);

    put_string(&mut body, connect.client_id, "client identifier")?;
    if let Some(will) = &connect.will {
        // No will properties: the will is published at once (Will Delay Interval 0).
        put_varint(&mut body, 0);
        put_string(&mut body, will.topic, "will topic")?;
        put_binary(&mut body, will.payload, "will payload")?;
    }
    finish(0x10, &body)
}

/// Encodes the header of a PUBLISH packet; the payload follows it on the wire.
pub(crate) fn publish_header(header: &PublishHeader<'_>) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::with_capacity(header.topic.len() + 16);
    put_string(&mut body, header.topic, "topic")?;
    if header.qos > 0 {
        put_u16(&mut body, header.packet_id.unwrap_or(0));
    }
    match header.message_expiry_secs {
        Some(secs) => {
            put_varint(&mut body, 5);
            body.push(0x02);
            put_u32(&mut body, secs);
        }
        None => put_varint(&mut body, 0),
    }
    let mut first = 0x30 | ((header.qos & 0x03) << 1);
    if header.retain {
        first |= 0x01;
    }
    finish_with_trailer(first, &body, header.payload_len)
}

/// The total size of a PUBLISH packet with this header, to compare with the server's Maximum
/// Packet Size before sending.
pub(crate) fn publish_size(header_bytes: &[u8], payload_len: usize) -> usize {
    header_bytes.len().saturating_add(payload_len)
}

/// Encodes a PUBACK with reason code 0 (success), in its short form.
pub(crate) fn puback(packet_id: u16) -> Vec<u8> {
    let id = packet_id.to_be_bytes();
    vec![0x40, 0x02, id[0], id[1]]
}

/// Encodes a SUBSCRIBE for `filters`, each with maximum QoS `qos`, retained messages sent at
/// subscribe time (Retain Handling 0), and Retain As Published off. `filters` must not be
/// empty.
pub(crate) fn subscribe(
    packet_id: u16,
    filters: &[String],
    qos: u8,
) -> Result<Vec<u8>, EncodeError> {
    if filters.is_empty() {
        return Err(EncodeError::NoTopicFilter);
    }
    let mut body = Vec::with_capacity(8 + filters.iter().map(|f| f.len() + 3).sum::<usize>());
    put_u16(&mut body, packet_id);
    put_varint(&mut body, 0);
    for filter in filters {
        put_string(&mut body, filter, "topic filter")?;
        body.push(qos & 0x03);
    }
    finish(0x82, &body)
}

/// Encodes a PINGREQ.
pub(crate) const fn pingreq() -> [u8; 2] {
    [0xC0, 0x00]
}

/// Encodes a DISCONNECT with `reason`; the normal disconnection takes the short form.
pub(crate) fn disconnect(reason: u8) -> Vec<u8> {
    if reason == reason::SUCCESS {
        vec![0xE0, 0x00]
    } else {
        vec![0xE0, 0x01, reason]
    }
}

// ----- Decoding (server to client) -----

/// What the server told the client in CONNACK.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnAck {
    pub(crate) session_present: bool,
    pub(crate) reason: u8,
    pub(crate) receive_maximum: Option<u16>,
    pub(crate) maximum_qos: Option<u8>,
    pub(crate) retain_available: Option<bool>,
    pub(crate) maximum_packet_size: Option<u32>,
    pub(crate) server_keep_alive: Option<u16>,
    pub(crate) reason_string: Option<String>,
}

/// A PUBLISH from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Publish {
    pub(crate) topic: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) qos: u8,
    pub(crate) retain: bool,
    pub(crate) packet_id: Option<u16>,
}

/// A PUBACK for one of the client's QoS 1 PUBLISH packets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PubAck {
    pub(crate) packet_id: u16,
    pub(crate) reason: u8,
    pub(crate) reason_string: Option<String>,
}

/// A SUBACK: one reason code per topic filter of the SUBSCRIBE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubAck {
    pub(crate) packet_id: u16,
    pub(crate) reasons: Vec<u8>,
    pub(crate) reason_string: Option<String>,
}

/// A DISCONNECT from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Disconnect {
    pub(crate) reason: u8,
    pub(crate) reason_string: Option<String>,
}

/// A packet a server sends a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inbound {
    ConnAck(ConnAck),
    Publish(Publish),
    PubAck(PubAck),
    SubAck(SubAck),
    PingResp,
    Disconnect(Disconnect),
}

/// Decodes the first packet in `buf`. Returns `Ok(None)` when `buf` holds only part of it, and
/// otherwise the packet with the number of bytes it took. A packet larger than
/// `max_packet_size` is refused as soon as its fixed header is complete, before its body
/// arrives.
pub(crate) fn decode(
    buf: &[u8],
    max_packet_size: usize,
) -> Result<Option<(Inbound, usize)>, DecodeError> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    let Some((remaining, varint_bytes)) = read_varint(buf.get(1..).unwrap_or_default())? else {
        return Ok(None);
    };
    let total = 1 + varint_bytes + remaining;
    if total > max_packet_size {
        return Err(DecodeError::TooLarge {
            size: total,
            limit: max_packet_size,
        });
    }
    let Some(body) = buf.get(1 + varint_bytes..total) else {
        return Ok(None);
    };
    let flags = first & 0x0F;
    let kind = PacketType::from_nibble(first >> 4)
        .ok_or(DecodeError::Malformed("packet type 0 is reserved"))?;
    let packet = match kind {
        PacketType::ConnAck => {
            fixed_flags(flags, 0)?;
            Inbound::ConnAck(connack(body)?)
        }
        PacketType::Publish => Inbound::Publish(publish(flags, body)?),
        PacketType::PubAck => {
            fixed_flags(flags, 0)?;
            Inbound::PubAck(puback_body(body)?)
        }
        PacketType::SubAck => {
            fixed_flags(flags, 0)?;
            Inbound::SubAck(suback(body)?)
        }
        PacketType::PingResp => {
            fixed_flags(flags, 0)?;
            if !body.is_empty() {
                return Err(DecodeError::Malformed("PINGRESP has a body"));
            }
            Inbound::PingResp
        }
        PacketType::Disconnect => {
            fixed_flags(flags, 0)?;
            Inbound::Disconnect(server_disconnect(body)?)
        }
        other => return Err(DecodeError::Unexpected(other)),
    };
    Ok(Some((packet, total)))
}

/// The total size of the packet that starts `buf`, once its fixed header is complete.
pub(crate) fn frame_len(buf: &[u8]) -> Option<usize> {
    let (remaining, varint_bytes) = read_varint(buf.get(1..)?).ok()??;
    Some(1 + varint_bytes + remaining)
}

fn fixed_flags(flags: u8, expected: u8) -> Result<(), DecodeError> {
    if flags == expected {
        Ok(())
    } else {
        Err(DecodeError::Malformed(
            "reserved fixed header flags are set",
        ))
    }
}

/// Reads a variable byte integer; `None` when `bytes` ends before it does. It must use as few
/// bytes as its value needs (MQTT-1.5.5-1): a last byte of 0 after a continuation byte adds
/// nothing, so that encoding is malformed. This covers every variable byte integer the client
/// receives: the Remaining Length, the property length, property identifiers and Subscription
/// Identifiers.
fn read_varint(bytes: &[u8]) -> Result<Option<(usize, usize)>, DecodeError> {
    let mut value = 0_usize;
    for (index, &byte) in bytes.iter().enumerate().take(4) {
        value |= usize::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            if index > 0 && byte == 0 {
                return Err(DecodeError::Malformed(
                    "variable byte integer is not in its shortest encoding",
                ));
            }
            return Ok(Some((value, index + 1)));
        }
    }
    if bytes.len() >= 4 {
        Err(DecodeError::Malformed(
            "variable byte integer longer than four bytes",
        ))
    } else {
        Ok(None)
    }
}

/// A cursor over one packet's body; every read checks the bytes left.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        if len > self.bytes.len() {
            return Err(DecodeError::Malformed(what));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, DecodeError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn varint(&mut self, what: &'static str) -> Result<usize, DecodeError> {
        match read_varint(self.bytes)? {
            Some((value, len)) => {
                self.bytes = &self.bytes[len..];
                Ok(value)
            }
            None => Err(DecodeError::Malformed(what)),
        }
    }

    fn binary(&mut self, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let len = self.u16(what)?;
        self.take(usize::from(len), what)
    }

    /// A UTF-8 string (MQTT 5 section 1.5.4): well-formed UTF-8, which `from_utf8` checks
    /// including surrogates and overlong forms (MQTT-1.5.4-1), and no U+0000 (MQTT-1.5.4-2). A
    /// leading U+FEFF stays in the string (MQTT-1.5.4-3). The other control characters and
    /// non-characters, which a sender should avoid but a receiver may accept, are accepted;
    /// text that ends up in logs is made printable where it is used.
    fn string(&mut self, what: &'static str) -> Result<&'a str, DecodeError> {
        let bytes = self.binary(what)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| DecodeError::Malformed("string is not UTF-8"))?;
        if text.contains('\0') {
            return Err(DecodeError::Malformed("string contains U+0000"));
        }
        Ok(text)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.bytes)
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Property identifiers (MQTT 5 section 2.2.2.2).
mod property {
    pub(super) const PAYLOAD_FORMAT_INDICATOR: usize = 0x01;
    pub(super) const MESSAGE_EXPIRY_INTERVAL: usize = 0x02;
    pub(super) const CONTENT_TYPE: usize = 0x03;
    pub(super) const RESPONSE_TOPIC: usize = 0x08;
    pub(super) const CORRELATION_DATA: usize = 0x09;
    pub(super) const SUBSCRIPTION_IDENTIFIER: usize = 0x0B;
    pub(super) const SESSION_EXPIRY_INTERVAL: usize = 0x11;
    pub(super) const ASSIGNED_CLIENT_IDENTIFIER: usize = 0x12;
    pub(super) const SERVER_KEEP_ALIVE: usize = 0x13;
    pub(super) const AUTHENTICATION_METHOD: usize = 0x15;
    pub(super) const AUTHENTICATION_DATA: usize = 0x16;
    pub(super) const RESPONSE_INFORMATION: usize = 0x1A;
    pub(super) const SERVER_REFERENCE: usize = 0x1C;
    pub(super) const REASON_STRING: usize = 0x1F;
    pub(super) const RECEIVE_MAXIMUM: usize = 0x21;
    pub(super) const TOPIC_ALIAS_MAXIMUM: usize = 0x22;
    pub(super) const TOPIC_ALIAS: usize = 0x23;
    pub(super) const MAXIMUM_QOS: usize = 0x24;
    pub(super) const RETAIN_AVAILABLE: usize = 0x25;
    pub(super) const USER_PROPERTY: usize = 0x26;
    pub(super) const MAXIMUM_PACKET_SIZE: usize = 0x27;
    pub(super) const WILDCARD_SUBSCRIPTION_AVAILABLE: usize = 0x28;
    pub(super) const SUBSCRIPTION_IDENTIFIER_AVAILABLE: usize = 0x29;
    pub(super) const SHARED_SUBSCRIPTION_AVAILABLE: usize = 0x2A;
}

/// The properties of a received packet that the agent uses; the others are checked and
/// skipped.
#[derive(Default)]
struct Properties {
    receive_maximum: Option<u16>,
    maximum_qos: Option<u8>,
    retain_available: Option<bool>,
    maximum_packet_size: Option<u32>,
    server_keep_alive: Option<u16>,
    topic_alias: Option<u16>,
    reason_string: Option<String>,
}

/// Reads a property list, accepting only the identifiers in `allowed`. A property other than
/// User Property and Subscription Identifier may appear once. The parser knows the wire type of
/// every property a server can send; each packet's `allowed` list says which it accepts.
fn properties(reader: &mut Reader<'_>, allowed: &[usize]) -> Result<Properties, DecodeError> {
    use property::*;

    let len = reader.varint("property length")?;
    let mut list = Reader {
        bytes: reader.take(len, "property length exceeds the packet")?,
    };
    let mut found = Properties::default();
    // Identifiers seen so far; all defined ones are below 64.
    let mut seen = 0_u64;
    while !list.is_empty() {
        let id = list.varint("property identifier")?;
        if !allowed.contains(&id) {
            return Err(DecodeError::Protocol("property not allowed in this packet"));
        }
        if id != USER_PROPERTY && id != SUBSCRIPTION_IDENTIFIER {
            let bit = 1_u64 << id;
            if seen & bit != 0 {
                return Err(DecodeError::Protocol("property appears more than once"));
            }
            seen |= bit;
        }
        match id {
            PAYLOAD_FORMAT_INDICATOR => {
                if list.u8("payload format indicator")? > 1 {
                    return Err(DecodeError::Protocol(
                        "payload format indicator is not 0 or 1",
                    ));
                }
            }
            MESSAGE_EXPIRY_INTERVAL | SESSION_EXPIRY_INTERVAL => {
                list.u32("four-byte property")?;
            }
            CONTENT_TYPE
            | ASSIGNED_CLIENT_IDENTIFIER
            | AUTHENTICATION_METHOD
            | RESPONSE_INFORMATION
            | SERVER_REFERENCE => {
                list.string("string property")?;
            }
            // A Response Topic is a topic name for the answer, so no wildcards (MQTT-3.3.2-14).
            RESPONSE_TOPIC => {
                if list.string("response topic")?.contains(['+', '#']) {
                    return Err(DecodeError::Malformed("wildcard in a response topic"));
                }
            }
            CORRELATION_DATA | AUTHENTICATION_DATA => {
                list.binary("binary property")?;
            }
            SUBSCRIPTION_IDENTIFIER => {
                if list.varint("subscription identifier")? == 0 {
                    return Err(DecodeError::Protocol("subscription identifier 0"));
                }
            }
            SERVER_KEEP_ALIVE => found.server_keep_alive = Some(list.u16("server keep alive")?),
            // Text from the broker that ends up in logs and errors: no control characters.
            REASON_STRING => {
                found.reason_string = Some(crate::link::printable(
                    list.string("reason string")?,
                    usize::MAX,
                ));
            }
            RECEIVE_MAXIMUM => {
                let value = list.u16("receive maximum")?;
                if value == 0 {
                    return Err(DecodeError::Protocol("receive maximum 0"));
                }
                found.receive_maximum = Some(value);
            }
            TOPIC_ALIAS_MAXIMUM => {
                list.u16("topic alias maximum")?;
            }
            TOPIC_ALIAS => found.topic_alias = Some(list.u16("topic alias")?),
            MAXIMUM_QOS => {
                let value = list.u8("maximum QoS")?;
                if value > 1 {
                    return Err(DecodeError::Protocol("maximum QoS is not 0 or 1"));
                }
                found.maximum_qos = Some(value);
            }
            RETAIN_AVAILABLE => found.retain_available = Some(flag(list.u8("retain available")?)?),
            USER_PROPERTY => {
                list.string("user property name")?;
                list.string("user property value")?;
            }
            MAXIMUM_PACKET_SIZE => {
                let value = list.u32("maximum packet size")?;
                if value == 0 {
                    return Err(DecodeError::Protocol("maximum packet size 0"));
                }
                found.maximum_packet_size = Some(value);
            }
            WILDCARD_SUBSCRIPTION_AVAILABLE
            | SUBSCRIPTION_IDENTIFIER_AVAILABLE
            | SHARED_SUBSCRIPTION_AVAILABLE => {
                flag(list.u8("availability flag")?)?;
            }
            _ => return Err(DecodeError::Protocol("unknown property identifier")),
        }
    }
    Ok(found)
}

fn flag(value: u8) -> Result<bool, DecodeError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::Protocol("flag property is not 0 or 1")),
    }
}

/// A packet identifier, which is never 0 (MQTT 5 section 2.2.1).
fn nonzero_packet_id(reader: &mut Reader<'_>) -> Result<u16, DecodeError> {
    match reader.u16("packet identifier")? {
        0 => Err(DecodeError::Malformed("packet identifier 0")),
        id => Ok(id),
    }
}

fn connack(body: &[u8]) -> Result<ConnAck, DecodeError> {
    use property::*;

    let mut reader = Reader { bytes: body };
    let ack_flags = reader.u8("CONNACK flags")?;
    if ack_flags & 0xFE != 0 {
        return Err(DecodeError::Malformed("reserved CONNACK flags are set"));
    }
    let code = reader.u8("CONNACK reason code")?;
    // An MQTT 3.1.1 server answers a version-5 CONNECT with a two-byte CONNACK and a 3.1.1
    // return code (0x01 unacceptable protocol version, up to 0x05); name that, since the code
    // means nothing in MQTT 5.
    if reader.is_empty() && (0x01..=0x05).contains(&code) {
        return Err(DecodeError::Protocol(
            "an MQTT 3.1.1 CONNACK refusal: the broker does not speak MQTT 5",
        ));
    }
    // Unlike PUBACK and DISCONNECT, an MQTT 5 CONNACK has no short form: no properties are a
    // Property Length of 0 (MQTT-2.2.2-1).
    if reader.is_empty() {
        return Err(DecodeError::Malformed("CONNACK without a property length"));
    }
    let reason = reason::check("CONNACK", code, reason::CONNACK)?;
    let session_present = ack_flags & 0x01 != 0;
    // A refusal never has a session (MQTT-3.2.2-6). After a clean start an accepting CONNACK
    // must not have one either; `connection` refuses that.
    if session_present && reason != reason::SUCCESS {
        return Err(DecodeError::Protocol(
            "Session Present on a refused CONNACK",
        ));
    }
    // Left out: Authentication Method, which a server must not send to a client whose CONNECT
    // had none (MQTT-4.12.0-6), with the Authentication Data that only such an exchange carries,
    // and Response Information, which it must not send unless the CONNECT asked for it
    // (MQTT-3.1.2-28). The agent's CONNECT has neither.
    let props = properties(
        &mut reader,
        &[
            SESSION_EXPIRY_INTERVAL,
            RECEIVE_MAXIMUM,
            MAXIMUM_QOS,
            RETAIN_AVAILABLE,
            MAXIMUM_PACKET_SIZE,
            ASSIGNED_CLIENT_IDENTIFIER,
            TOPIC_ALIAS_MAXIMUM,
            REASON_STRING,
            USER_PROPERTY,
            WILDCARD_SUBSCRIPTION_AVAILABLE,
            SUBSCRIPTION_IDENTIFIER_AVAILABLE,
            SHARED_SUBSCRIPTION_AVAILABLE,
            SERVER_KEEP_ALIVE,
            SERVER_REFERENCE,
        ],
    )?;
    if !reader.is_empty() {
        return Err(DecodeError::Malformed("bytes after the CONNACK properties"));
    }
    Ok(ConnAck {
        session_present,
        reason,
        receive_maximum: props.receive_maximum,
        maximum_qos: props.maximum_qos,
        retain_available: props.retain_available,
        maximum_packet_size: props.maximum_packet_size,
        server_keep_alive: props.server_keep_alive,
        reason_string: props.reason_string,
    })
}

fn publish(flags: u8, body: &[u8]) -> Result<Publish, DecodeError> {
    use property::*;

    let qos = (flags >> 1) & 0x03;
    if qos == 3 {
        return Err(DecodeError::Malformed("PUBLISH QoS 3"));
    }
    let dup = flags & 0x08 != 0;
    if dup && qos == 0 {
        return Err(DecodeError::Malformed("DUP flag on a QoS 0 PUBLISH"));
    }
    let mut reader = Reader { bytes: body };
    let topic = reader.string("topic name")?;
    let packet_id = if qos > 0 {
        Some(nonzero_packet_id(&mut reader)?)
    } else {
        None
    };
    let props = properties(
        &mut reader,
        &[
            PAYLOAD_FORMAT_INDICATOR,
            MESSAGE_EXPIRY_INTERVAL,
            TOPIC_ALIAS,
            RESPONSE_TOPIC,
            CORRELATION_DATA,
            USER_PROPERTY,
            SUBSCRIPTION_IDENTIFIER,
            CONTENT_TYPE,
        ],
    )?;
    if props.topic_alias.is_some() {
        return Err(DecodeError::TopicAlias);
    }
    if topic.is_empty() {
        return Err(DecodeError::Protocol(
            "empty topic name without a topic alias",
        ));
    }
    if topic.contains(['+', '#']) {
        return Err(DecodeError::Malformed("wildcard in a topic name"));
    }
    // A Payload Format Indicator of 1 is not checked against the payload: MQTT 5 leaves that to
    // the receiver (it MAY validate, section 3.3.2.3.2), and every down channel carries CBOR
    // that `protocol` decodes whatever the indicator says.
    Ok(Publish {
        topic: topic.to_owned(),
        payload: reader.rest().to_vec(),
        qos,
        retain: flags & 0x01 != 0,
        packet_id,
    })
}

/// PUBACK: the packet identifier, then optionally the reason code and properties.
fn puback_body(body: &[u8]) -> Result<PubAck, DecodeError> {
    use property::*;

    let mut reader = Reader { bytes: body };
    let packet_id = nonzero_packet_id(&mut reader)?;
    let (reason, reason_string) = if reader.is_empty() {
        (reason::SUCCESS, None)
    } else {
        let code = reason::check("PUBACK", reader.u8("reason code")?, reason::PUBACK)?;
        let props = if reader.is_empty() {
            Properties::default()
        } else {
            properties(&mut reader, &[REASON_STRING, USER_PROPERTY])?
        };
        (code, props.reason_string)
    };
    if !reader.is_empty() {
        return Err(DecodeError::Malformed("bytes after the PUBACK properties"));
    }
    Ok(PubAck {
        packet_id,
        reason,
        reason_string,
    })
}

fn suback(body: &[u8]) -> Result<SubAck, DecodeError> {
    use property::*;

    let mut reader = Reader { bytes: body };
    let packet_id = nonzero_packet_id(&mut reader)?;
    let props = properties(&mut reader, &[REASON_STRING, USER_PROPERTY])?;
    let reasons = reader.rest().to_vec();
    if reasons.is_empty() {
        return Err(DecodeError::Protocol("SUBACK without reason codes"));
    }
    for &code in &reasons {
        reason::check("SUBACK", code, reason::SUBACK)?;
    }
    Ok(SubAck {
        packet_id,
        reasons,
        reason_string: props.reason_string,
    })
}

fn server_disconnect(body: &[u8]) -> Result<Disconnect, DecodeError> {
    use property::*;

    let mut reader = Reader { bytes: body };
    if reader.is_empty() {
        return Ok(Disconnect {
            reason: reason::SUCCESS,
            reason_string: None,
        });
    }
    let code = reason::check(
        "DISCONNECT",
        reader.u8("reason code")?,
        reason::SERVER_DISCONNECT,
    )?;
    // A server must not send Session Expiry Interval in DISCONNECT (section 3.14.2.2.2).
    let props = if reader.is_empty() {
        Properties::default()
    } else {
        properties(
            &mut reader,
            &[REASON_STRING, USER_PROPERTY, SERVER_REFERENCE],
        )?
    };
    if !reader.is_empty() {
        return Err(DecodeError::Malformed(
            "bytes after the DISCONNECT properties",
        ));
    }
    Ok(Disconnect {
        reason: code,
        reason_string: props.reason_string,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1024 * 1024;

    /// Every reason code a server may send in each acknowledgement (MQTT 5 sections 3.2.2.2,
    /// 3.4.2.1, 3.9.3 and 3.14.2.1); every other value must be refused.
    #[test]
    fn only_the_reason_codes_mqtt_5_allows_are_accepted() {
        let connack_allowed = [
            0x00, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x8C, 0x90,
            0x95, 0x97, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9F,
        ];
        let puback_allowed = [0x00, 0x10, 0x80, 0x83, 0x87, 0x90, 0x91, 0x97, 0x99];
        let suback_allowed = [
            0x00, 0x01, 0x02, 0x80, 0x83, 0x87, 0x8F, 0x91, 0x97, 0x9E, 0xA1, 0xA2,
        ];
        let disconnect_allowed = [
            0x00, 0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8B, 0x8D, 0x8E, 0x8F, 0x90, 0x93, 0x94,
            0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1, 0xA2,
        ];
        type Packet = fn(u8) -> Vec<u8>;
        let packets: [(&str, Packet, &[u8]); 4] = [
            (
                "CONNACK",
                |code| vec![0x20, 3, 0x00, code, 0],
                &connack_allowed,
            ),
            ("PUBACK", |code| vec![0x40, 3, 0, 1, code], &puback_allowed),
            (
                "SUBACK",
                |code| vec![0x90, 4, 0, 1, 0, code],
                &suback_allowed,
            ),
            (
                "DISCONNECT",
                |code| vec![0xE0, 1, code],
                &disconnect_allowed,
            ),
        ];
        for (name, packet, allowed) in packets {
            for code in 0..=u8::MAX {
                let result = decode(&packet(code), MAX);
                if allowed.contains(&code) {
                    assert!(
                        matches!(result, Ok(Some(_))),
                        "{name} 0x{code:02X}: {result:?}"
                    );
                } else {
                    let error = result.expect_err(&format!("{name} 0x{code:02X} was accepted"));
                    assert_eq!(
                        error.reason_code(),
                        reason::PROTOCOL_ERROR,
                        "{name} 0x{code:02X}: {error}"
                    );
                }
            }
        }
    }

    fn decoded(bytes: &[u8]) -> Inbound {
        let (packet, len) = decode(bytes, MAX).unwrap().expect("a complete packet");
        assert_eq!(len, bytes.len());
        packet
    }

    #[test]
    fn varints_round_trip_at_every_width_boundary() {
        for value in [
            0,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            MAX_REMAINING_LENGTH,
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(out.len(), varint_len(value), "{value}");
            assert_eq!(
                read_varint(&out).unwrap(),
                Some((value, out.len())),
                "{value}"
            );
        }
        // The examples of MQTT 5 section 1.5.5.
        let mut out = Vec::new();
        put_varint(&mut out, 321);
        assert_eq!(out, [0xC1, 0x02]);
        assert_eq!(
            read_varint(&[0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(DecodeError::Malformed(
                "variable byte integer longer than four bytes"
            ))
        );
        assert_eq!(read_varint(&[0x80, 0x80]), Ok(None));
        // Only the shortest encoding (MQTT-1.5.5-1): 0 in two to four bytes, 127 or 2^21 - 1
        // with a byte too many.
        for overlong in [
            &[0x80, 0x00][..],
            &[0x80, 0x80, 0x00],
            &[0x80, 0x80, 0x80, 0x00],
            &[0xFF, 0x00],
            &[0xFF, 0xFF, 0xFF, 0x00],
        ] {
            assert_eq!(
                read_varint(overlong),
                Err(DecodeError::Malformed(
                    "variable byte integer is not in its shortest encoding"
                )),
                "{overlong:02x?}"
            );
        }
    }

    #[test]
    fn connect_encodes_the_will_and_limits() {
        let packet = connect(&Connect {
            client_id: "gw-1",
            keep_alive_secs: 60,
            clean_start: true,
            session_expiry_secs: 0,
            receive_maximum: 16,
            maximum_packet_size: 1_056_768,
            will: Some(Will {
                topic: "t/w",
                payload: b"bye",
                qos: 1,
                retain: true,
            }),
        })
        .unwrap();
        let expected: Vec<u8> = [
            &[0x10, 36][..],
            &[0, 4, b'M', b'Q', b'T', b'T', 5],
            // Clean Start, Will, Will QoS 1, Will Retain.
            &[0x02 | 0x04 | 0x08 | 0x20],
            &[0, 60],
            // Properties: Receive Maximum 16, Maximum Packet Size 1 MiB + 8 KiB.
            &[8, 0x21, 0, 16, 0x27, 0x00, 0x10, 0x20, 0x00],
            &[0, 4, b'g', b'w', b'-', b'1'],
            &[0, 0, 3, b't', b'/', b'w', 0, 3, b'b', b'y', b'e'],
        ]
        .concat();
        assert_eq!(packet, expected);
    }

    #[test]
    fn publish_header_counts_the_payload_that_follows() {
        let header = publish_header(&PublishHeader {
            topic: "a/b",
            qos: 1,
            retain: true,
            packet_id: Some(7),
            message_expiry_secs: None,
            payload_len: 200,
        })
        .unwrap();
        // Remaining length 5 (topic) + 2 (id) + 1 (no properties) + 200 = 208.
        assert_eq!(header, [0x33, 0xD0, 0x01, 0, 3, b'a', b'/', b'b', 0, 7, 0]);
        assert_eq!(publish_size(&header, 200), header.len() + 200);
        let expiring = publish_header(&PublishHeader {
            topic: "a",
            qos: 0,
            retain: false,
            packet_id: None,
            message_expiry_secs: Some(30),
            payload_len: 0,
        })
        .unwrap();
        assert_eq!(expiring, [0x30, 9, 0, 1, b'a', 5, 0x02, 0, 0, 0, 30]);
    }

    #[test]
    fn encoders_refuse_what_mqtt_cannot_carry() {
        let long = "x".repeat(65_536);
        assert_eq!(
            subscribe(1, &[long], 1),
            Err(EncodeError::FieldTooLong("topic filter"))
        );
        assert_eq!(
            subscribe(1, &["a\0b".to_owned()], 1),
            Err(EncodeError::NulInString("topic filter"))
        );
        assert_eq!(subscribe(1, &[], 1), Err(EncodeError::NoTopicFilter));
        assert_eq!(
            publish_header(&PublishHeader {
                topic: "a",
                qos: 0,
                retain: false,
                packet_id: None,
                message_expiry_secs: None,
                payload_len: MAX_REMAINING_LENGTH,
            }),
            Err(EncodeError::PacketTooLarge)
        );
    }

    #[test]
    fn small_packets_have_their_fixed_forms() {
        assert_eq!(puback(0x1234), [0x40, 2, 0x12, 0x34]);
        assert_eq!(pingreq(), [0xC0, 0]);
        assert_eq!(disconnect(0), [0xE0, 0]);
        assert_eq!(disconnect(0x04), [0xE0, 1, 0x04]);
        assert_eq!(
            subscribe(9, &["a".to_owned(), "bc".to_owned()], 1).unwrap(),
            [0x82, 12, 0, 9, 0, 0, 1, b'a', 1, 0, 2, b'b', b'c', 1]
        );
    }

    #[test]
    fn connack_reads_the_server_limits() {
        let bytes = [
            0x20, 18, 0x00, 0x00, 15, 0x21, 0, 20, 0x24, 1, 0x25, 0, 0x27, 0, 1, 0, 0, 0x13, 0, 30,
        ];
        assert_eq!(
            decoded(&bytes),
            Inbound::ConnAck(ConnAck {
                session_present: false,
                reason: 0,
                receive_maximum: Some(20),
                maximum_qos: Some(1),
                retain_available: Some(false),
                maximum_packet_size: Some(65_536),
                server_keep_alive: Some(30),
                reason_string: None,
            })
        );
        // Session Present decodes on an accepting CONNACK (`connection` refuses it after the
        // clean start); User Property may repeat.
        let Inbound::ConnAck(ack) = decoded(&[
            0x20, 14, 0x01, 0x00, 11, 0x26, 0, 1, b'k', 0, 0, 0x26, 0, 0, 0, 0,
        ]) else {
            panic!("CONNACK")
        };
        assert!(ack.session_present);
        // A refusal with a reason string.
        let refused = [0x20, 9, 0x00, 0x87, 6, 0x1F, 0, 3, b'n', b'o', b'!'];
        let Inbound::ConnAck(ack) = decoded(&refused) else {
            panic!("CONNACK")
        };
        assert_eq!(
            (ack.reason, ack.reason_string.as_deref()),
            (0x87, Some("no!"))
        );
        // A reason string loses its control characters as it is decoded.
        let forged = [0x20, 9, 0x00, 0x87, 6, 0x1F, 0, 3, b'a', b'\n', b'b'];
        let Inbound::ConnAck(ack) = decoded(&forged) else {
            panic!("CONNACK")
        };
        assert_eq!(ack.reason_string.as_deref(), Some("a b"));
        // A 3.1.1 server's refusal of the version-5 CONNECT says so.
        let error = decode(&[0x20, 2, 0, 1], MAX).unwrap_err();
        assert!(error.to_string().contains("MQTT 3.1.1"), "{error}");
        assert_eq!(error.reason_code(), reason::PROTOCOL_ERROR);
    }

    #[test]
    fn publish_from_the_server_decodes_with_its_flags() {
        // QoS 1, retained; a Payload Format Indicator property.
        let bytes = [
            0x33, 12, 0, 3, b'a', b'/', b'b', 0, 5, 2, 0x01, 0, b'x', b'y',
        ];
        assert_eq!(
            decoded(&bytes),
            Inbound::Publish(Publish {
                topic: "a/b".to_owned(),
                payload: b"xy".to_vec(),
                qos: 1,
                retain: true,
                packet_id: Some(5),
            })
        );
        let qos0 = [0x30, 6, 0, 1, b't', 0, b'h', b'i'];
        let Inbound::Publish(publish) = decoded(&qos0) else {
            panic!("PUBLISH")
        };
        assert_eq!(
            (publish.qos, publish.packet_id, publish.retain),
            (0, None, false)
        );
        // What MQTT 5 allows: a Response Topic without wildcards, Subscription Identifier
        // repeated and at its largest value, a leading U+FEFF that stays in the topic, and a
        // control character, which a sender should avoid but a receiver may accept.
        let allowed: [(&[u8], &str); 5] = [
            (&[0x30, 9, 0, 1, b'a', 5, 0x08, 0, 2, b'r', b's'], "a"),
            (&[0x30, 8, 0, 1, b'a', 4, 0x0B, 1, 0x0B, 2], "a"),
            (&[0x30, 9, 0, 1, b'a', 5, 0x0B, 0xFF, 0xFF, 0xFF, 0x7F], "a"),
            (&[0x30, 7, 0, 4, 0xEF, 0xBB, 0xBF, b'a', 0], "\u{FEFF}a"),
            (&[0x30, 4, 0, 1, 0x01, 0], "\u{1}"),
        ];
        for (bytes, topic) in allowed {
            let Inbound::Publish(publish) = decoded(bytes) else {
                panic!("PUBLISH {bytes:02x?}")
            };
            assert_eq!(publish.topic, topic, "{bytes:02x?}");
        }
    }

    #[test]
    fn puback_suback_pingresp_and_disconnect_decode_in_every_form() {
        assert_eq!(
            decoded(&[0x40, 2, 0, 7]),
            Inbound::PubAck(PubAck {
                packet_id: 7,
                reason: 0,
                reason_string: None
            })
        );
        assert_eq!(
            decoded(&[0x40, 3, 0, 7, 0x87]),
            Inbound::PubAck(PubAck {
                packet_id: 7,
                reason: 0x87,
                reason_string: None
            })
        );
        assert_eq!(
            decoded(&[0x40, 8, 0, 7, 0x10, 4, 0x1F, 0, 1, b'z']),
            Inbound::PubAck(PubAck {
                packet_id: 7,
                reason: 0x10,
                reason_string: Some("z".to_owned())
            })
        );
        assert_eq!(
            decoded(&[0x90, 5, 0, 3, 0, 1, 0x87]),
            Inbound::SubAck(SubAck {
                packet_id: 3,
                reasons: vec![1, 0x87],
                reason_string: None
            })
        );
        assert_eq!(decoded(&[0xD0, 0]), Inbound::PingResp);
        assert_eq!(
            decoded(&[0xE0, 0]),
            Inbound::Disconnect(Disconnect {
                reason: 0,
                reason_string: None
            })
        );
        assert_eq!(
            decoded(&[0xE0, 1, 0x98]),
            Inbound::Disconnect(Disconnect {
                reason: 0x98,
                reason_string: None
            })
        );
    }

    #[test]
    fn a_partial_packet_waits_for_more_bytes() {
        let bytes = [0x40, 2, 0, 7];
        for len in 0..bytes.len() {
            assert_eq!(decode(&bytes[..len], MAX), Ok(None), "{len} bytes");
        }
        // A second packet behind the first is left for the next call.
        let two = [0x40, 2, 0, 7, 0xD0, 0];
        assert_eq!(decode(&two, MAX).unwrap().map(|(_, len)| len), Some(4));
    }

    #[test]
    fn a_packet_above_the_limit_is_refused_from_its_header_alone() {
        // Declares a 2 MiB PUBLISH; only the fixed header is present.
        let header = [0x30, 0x80, 0x80, 0x80, 0x01];
        assert_eq!(
            decode(&header, MAX),
            Err(DecodeError::TooLarge {
                size: 1 + 4 + 2_097_152,
                limit: MAX
            })
        );
        assert_eq!(decode(&header, MAX).unwrap_err().reason_code(), 0x95);
    }

    #[test]
    fn rule_breaking_packets_are_refused_with_the_right_reason() {
        let cases: &[(&[u8], u8)] = &[
            // Reserved packet type 0, unexpected types, wrong fixed flags.
            (&[0x00, 0], 0x81),
            (&[0x10, 0], 0x82),
            (&[0x50, 2, 0, 1], 0x82),
            (&[0xF0, 0], 0x82),
            (&[0x41, 2, 0, 1], 0x81),
            (&[0xD0, 1, 0], 0x81),
            // QoS 3, DUP on QoS 0, packet identifier 0, wildcard topic, empty topic.
            (&[0x36, 5, 0, 1, b'a', 0, 1], 0x81),
            (&[0x38, 4, 0, 1, b'a', 0], 0x81),
            (&[0x32, 6, 0, 1, b'a', 0, 0, 0], 0x81),
            (&[0x30, 4, 0, 1, b'+', 0], 0x81),
            (&[0x30, 3, 0, 0, 0], 0x82),
            // Topic alias, which the client allows none of.
            (&[0x30, 7, 0, 1, b'a', 3, 0x23, 0, 1], 0x94),
            // Property not allowed in PUBLISH, repeated property, length beyond the packet.
            (&[0x30, 7, 0, 1, b'a', 3, 0x21, 0, 1], 0x82),
            (&[0x40, 10, 0, 1, 0x80, 6, 0x1F, 0, 0, 0x1F, 0, 0], 0x82),
            (&[0x40, 5, 0, 1, 0, 9, 0x1F], 0x81),
            // String that is not UTF-8, or holds U+0000.
            (&[0x30, 4, 0, 1, 0xFF, 0], 0x81),
            (&[0x30, 4, 0, 1, 0x00, 0], 0x81),
            // Receive Maximum 0, Maximum QoS 2, a session expiry in a server DISCONNECT.
            (&[0x20, 6, 0, 0, 3, 0x21, 0, 0], 0x82),
            (&[0x20, 5, 0, 0, 2, 0x24, 2], 0x82),
            (&[0xE0, 7, 0, 5, 0x11, 0, 0, 0, 1], 0x82),
            // Reserved CONNACK flags, SUBACK without reason codes, trailing bytes.
            (&[0x20, 3, 0x02, 0, 0], 0x81),
            (&[0x90, 3, 0, 1, 0], 0x82),
            (&[0x40, 5, 0, 1, 0, 0, 9], 0x81),
        ];
        for (bytes, code) in cases {
            let error = decode(bytes, MAX).expect_err(&format!("{bytes:02x?} must be refused"));
            assert_eq!(error.reason_code(), *code, "{bytes:02x?}: {error}");
        }
    }

    /// The MUST rules of MQTT 5 on the packets a client receives, rule by rule; each case is
    /// one rule broken in an otherwise valid packet.
    #[test]
    fn every_mqtt_5_must_rule_on_received_packets_is_enforced() {
        let cases: &[(&str, &[u8], u8)] = &[
            // Variable byte integers in their shortest encoding (MQTT-1.5.5-1).
            ("Remaining Length 0 in two bytes", &[0xD0, 0x80, 0x00], 0x81),
            (
                "Remaining Length 2 in two bytes",
                &[0x40, 0x82, 0x00, 0, 1],
                0x81,
            ),
            (
                "property length 0 in two bytes",
                &[0x40, 5, 0, 1, 0x10, 0x80, 0x00],
                0x81,
            ),
            (
                "property identifier 0x1F in two bytes",
                &[0x40, 8, 0, 1, 0x10, 4, 0x9F, 0x00, 0, 0],
                0x81,
            ),
            (
                "Subscription Identifier 1 in two bytes",
                &[0x30, 7, 0, 1, b'a', 3, 0x0B, 0x81, 0x00],
                0x81,
            ),
            // Fixed header flags (MQTT-2.1.3-1, MQTT-3.3.1-4).
            ("CONNACK flags", &[0x21, 3, 0, 0, 0], 0x81),
            ("SUBACK flags", &[0x92, 4, 0, 1, 0, 1], 0x81),
            ("PINGRESP flags", &[0xD1, 0], 0x81),
            ("DISCONNECT flags", &[0xE1, 0], 0x81),
            // CONNACK: a Property Length is always present (MQTT-2.2.2-1), a refusal has no
            // session (MQTT-3.2.2-6), and no property the agent did not ask for.
            ("CONNACK without properties", &[0x20, 2, 0, 0], 0x81),
            (
                "refusing CONNACK without properties",
                &[0x20, 2, 0, 0x87],
                0x81,
            ),
            (
                "Session Present on a refusal",
                &[0x20, 3, 0x01, 0x87, 0],
                0x82,
            ),
            (
                "Authentication Method in CONNACK",
                &[0x20, 8, 0, 0, 5, 0x15, 0, 2, b'a', b'b'],
                0x82,
            ),
            (
                "Authentication Data in CONNACK",
                &[0x20, 6, 0, 0, 3, 0x16, 0, 0],
                0x82,
            ),
            (
                "Response Information in CONNACK",
                &[0x20, 6, 0, 0, 3, 0x1A, 0, 0],
                0x82,
            ),
            // Each property only where MQTT 5 allows it, and unknown identifiers refused.
            (
                "Topic Alias in CONNACK",
                &[0x20, 6, 0, 0, 3, 0x23, 0, 1],
                0x82,
            ),
            (
                "Reason String in PUBLISH",
                &[0x30, 7, 0, 1, b'a', 3, 0x1F, 0, 0],
                0x82,
            ),
            (
                "Server Reference in PUBACK",
                &[0x40, 7, 0, 1, 0x80, 3, 0x1C, 0, 0],
                0x82,
            ),
            (
                "unknown property 0x7F",
                &[0x40, 6, 0, 1, 0, 2, 0x7F, 0],
                0x82,
            ),
            // At most once, except User Property and Subscription Identifier.
            (
                "Maximum QoS twice",
                &[0x20, 7, 0, 0, 4, 0x24, 1, 0x24, 1],
                0x82,
            ),
            (
                "Payload Format Indicator twice",
                &[0x30, 8, 0, 1, b'a', 4, 0x01, 0, 0x01, 0],
                0x82,
            ),
            // Values in range.
            (
                "Maximum Packet Size 0",
                &[0x20, 8, 0, 0, 5, 0x27, 0, 0, 0, 0],
                0x82,
            ),
            ("Retain Available 2", &[0x20, 5, 0, 0, 2, 0x25, 2], 0x82),
            (
                "Wildcard Subscription Available 2",
                &[0x20, 5, 0, 0, 2, 0x28, 2],
                0x82,
            ),
            (
                "Subscription Identifiers Available 2",
                &[0x20, 5, 0, 0, 2, 0x29, 2],
                0x82,
            ),
            (
                "Shared Subscription Available 2",
                &[0x20, 5, 0, 0, 2, 0x2A, 2],
                0x82,
            ),
            (
                "Payload Format Indicator 2",
                &[0x30, 6, 0, 1, b'a', 2, 0x01, 2],
                0x82,
            ),
            (
                "Subscription Identifier 0",
                &[0x30, 6, 0, 1, b'a', 2, 0x0B, 0],
                0x82,
            ),
            // UTF-8 strings: well formed, no surrogates or overlong forms (MQTT-1.5.4-1), no
            // U+0000 (MQTT-1.5.4-2), in every string field.
            (
                "surrogate U+D800 in a topic",
                &[0x30, 6, 0, 3, 0xED, 0xA0, 0x80, 0],
                0x81,
            ),
            (
                "overlong U+0000 in a topic",
                &[0x30, 5, 0, 2, 0xC0, 0x80, 0],
                0x81,
            ),
            (
                "U+0000 in a reason string",
                &[0x40, 8, 0, 1, 0x10, 4, 0x1F, 0, 1, 0x00],
                0x81,
            ),
            (
                "U+0000 in a user property",
                &[0xE0, 8, 0x80, 6, 0x26, 0, 0, 0, 1, 0x00],
                0x81,
            ),
            (
                "surrogate U+DFFF in a server reference",
                &[0xE0, 8, 0x9C, 6, 0x1C, 0, 3, 0xED, 0xBF, 0xBF],
                0x81,
            ),
            // Topic names (MQTT-3.3.2-2) and Response Topics (MQTT-3.3.2-14) without wildcards.
            ("'#' in a topic", &[0x30, 4, 0, 1, b'#', 0], 0x81),
            (
                "'#' in a response topic",
                &[0x30, 9, 0, 1, b'a', 5, 0x08, 0, 2, b'r', b'#'],
                0x81,
            ),
            (
                "'+' in a response topic",
                &[0x30, 9, 0, 1, b'a', 5, 0x08, 0, 2, b'+', b'r'],
                0x81,
            ),
            // Packet identifiers are never 0 (section 2.2.1).
            ("PUBACK for packet identifier 0", &[0x40, 2, 0, 0], 0x81),
            (
                "SUBACK for packet identifier 0",
                &[0x90, 4, 0, 0, 0, 1],
                0x81,
            ),
            // The fields fill the Remaining Length exactly.
            (
                "QoS 1 PUBLISH without its packet identifier",
                &[0x32, 3, 0, 1, b'a'],
                0x81,
            ),
            ("topic longer than the packet", &[0x30, 3, 0, 5, b'a'], 0x81),
            (
                "CONNACK properties beyond the packet",
                &[0x20, 3, 0, 0, 5],
                0x81,
            ),
            (
                "SUBACK properties beyond the packet",
                &[0x90, 3, 0, 1, 5],
                0x81,
            ),
            (
                "a byte after the CONNACK properties",
                &[0x20, 4, 0, 0, 0, 9],
                0x81,
            ),
            (
                "a byte after the DISCONNECT properties",
                &[0xE0, 3, 0, 0, 9],
                0x81,
            ),
            // AUTH, which a server must not send without an Authentication Method in CONNECT
            // (MQTT-4.12.0-6).
            ("AUTH", &[0xF0, 2, 0x18, 0], 0x82),
        ];
        for (rule, bytes, code) in cases {
            let error = decode(bytes, MAX).expect_err(&format!("{rule}: {bytes:02x?} accepted"));
            assert_eq!(error.reason_code(), *code, "{rule}: {error}");
        }
        // Every packet type a server never sends to this client: CONNECT, PUBREC, PUBREL,
        // PUBCOMP, SUBSCRIBE, UNSUBSCRIBE, UNSUBACK, PINGREQ and AUTH.
        for kind in [1_u8, 5, 6, 7, 8, 10, 11, 12, 15] {
            let error = decode(&[kind << 4, 0], MAX).expect_err(&format!("type {kind} accepted"));
            assert!(
                matches!(error, DecodeError::Unexpected(_)),
                "type {kind}: {error}"
            );
        }
    }

    #[test]
    fn no_input_panics_or_reads_past_its_end() {
        // Every prefix and every single-byte corruption of a set of valid packets.
        let valid: &[&[u8]] = &[
            &[
                0x20, 18, 0, 0, 15, 0x21, 0, 20, 0x24, 1, 0x25, 0, 0x27, 0, 1, 0, 0, 0x13, 0, 30,
            ],
            &[
                0x33, 12, 0, 3, b'a', b'/', b'b', 0, 5, 2, 0x01, 0, b'x', b'y',
            ],
            &[0x40, 8, 0, 7, 0x10, 4, 0x1F, 0, 1, b'z'],
            &[0x90, 5, 0, 3, 0, 1, 0x87],
            &[0xE0, 8, 0x98, 6, 0x26, 0, 1, b'k', 0, 0],
        ];
        for packet in valid {
            for len in 0..=packet.len() {
                let _ = decode(&packet[..len], MAX);
            }
            for index in 0..packet.len() {
                for value in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
                    let mut corrupted = packet.to_vec();
                    corrupted[index] = value;
                    if let Ok(Some((_, len))) = decode(&corrupted, MAX) {
                        assert!(len <= corrupted.len());
                    }
                }
            }
        }
    }

    #[test]
    fn reason_codes_print_with_their_names() {
        assert_eq!(ReasonCode(0x87).to_string(), "0x87 (not authorized)");
        assert_eq!(ReasonCode(0x7E).to_string(), "0x7e");
    }
}
