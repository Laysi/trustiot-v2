//! Gateway ↔ cloud MQTT message formats for TrustIoT v2 (rebuild checklist M4).
//!
//! This crate is the single definition of everything a gateway and the cloud exchange over
//! MQTT 5: topic names ([`topic`]), the broker ACL that confines each gateway to its own topics
//! ([`acl`]), payload types and their CBOR encoding ([`codec`]), size limits and batch
//! splitting ([`telemetry::BatchBuilder`]); the HTTPS certificate renewal's requests,
//! answers and signing input ([`renewal`]); where gateways fetch the root CRL over HTTPS
//! ([`root_crl`]); and the factory provisioning report that the agent's `provision` subcommand
//! hands the production workstation ([`provisioning`]). The edge agent and the cloud services
//! both depend on it, so a format change breaks the build or the golden tests on both sides at once.
//! The crate's `README.md` is the specification for humans: topics, messages, delivery rules,
//! size limits, versioning and the reasoning behind the encoding.
//!
//! The crate builds for the gateway (`aarch64-unknown-linux-gnu`) and depends only on pure-Rust
//! crates; `tests/dependencies.rs` keeps it that way.
//!
//! ```
//! use protocol::telemetry::{BatchBuilder, Reading, Sample, TelemetryBatch};
//! use protocol::{DeviceId, PointKey, Seq, StreamId, UnixMillis, Value};
//!
//! let mut builder = BatchBuilder::new(StreamId::new("stream-1")?, Seq::FIRST);
//! let reading = Reading {
//!     device: DeviceId::new("meter-1")?,
//!     point: PointKey::new("voltage_a")?,
//!     sample: Sample { at: UnixMillis::new(1_760_054_400_000), value: Value::Float(230.5) },
//!     clock_synced: true,
//! };
//! assert!(builder.push(reading)?.is_none());
//! let batch = builder.finish().expect("one reading is buffered");
//! let payload = batch.encode()?;
//! assert_eq!(TelemetryBatch::decode(&payload)?, batch);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod acl;
pub mod codec;
pub mod config;
mod float;
pub mod health;
mod ids;
pub mod live;
pub mod logs;
mod open_enum;
pub mod provisioning;
pub mod renewal;
pub mod root_crl;
pub mod telemetry;
pub mod topic;
mod value;

pub use codec::{DecodeError, EncodeError, Invalid, Issue, IssueCode, Message, V1, decode, encode};
pub use ids::{
    AgentVersion, ConfigVersion, DeviceId, GatewayId, IdError, LeaseId, MAX_AGENT_VERSION_LEN,
    MAX_ID_LEN, MAX_MODEL_NAME_LEN, ModelName, PointKey, RequestId, Seq, SessionId, StreamId,
    UnixMillis,
};
pub use value::{MAX_TEXT_BYTES, Value, ValueError};

/// The wire format's major version: the `v1` topic level and the `v` field of every payload.
/// An incompatible change starts a new major version with its own topic tree.
pub const PROTOCOL_VERSION: u16 = 1;

/// The protocol revision this crate implements within [`PROTOCOL_VERSION`]. A revision adds
/// fields that a receiver must not ignore; a payload that needs a newer revision says so in
/// `rev` and is refused by older receivers ([`DecodeError::UnsupportedRevision`]).
pub const PROTOCOL_REVISION: u16 = 1;
