//! The agent's own MQTT 5 client: a packet codec without I/O ([`packet`]) and the state machine
//! of one connection ([`connection`]), driven over any `AsyncRead + AsyncWrite` stream.
//!
//! Why not a client library (2026-10-21, Laysi/trustiot-v2#86): the connection runs MQTT
//! inside TLS inside a WebSocket, and rumqttc 0.25 (the client of the cloud PoC) connects only
//! over its own TCP, TLS, Unix or WebSocket transports, never over a stream it is given. The
//! sans-I/O MQTT 5 crates on crates.io were not mature enough for the gateway:
//! `mqtt5-protocol` had 33 releases in its first eleven months (0.16.1 on 2026-10-09) and
//! depends on its author's own byte-parsing crate; `mqtt-protocol-core` (0.7.9) is about
//! 40,000 lines with six proc-macro dependencies for a client that needs nine packet types;
//! `mqttbytes` has not been released since 2021. The subset here is small, the MQTT 5.0
//! standard does not change, and owning it keeps every buffer bounded where the gateway's
//! memory budget needs it.

pub(crate) mod connection;
pub(crate) mod packet;
