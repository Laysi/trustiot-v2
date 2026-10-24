//! Contracts between TrustIoT v2 cloud services (decision 4 in `docs/architecture/decisions.md`).
//!
//! Services talk to each other only through the types in this crate, so changing a contract
//! breaks the build of every service that depends on it instead of failing at run time:
//!
//! - [`error`]: the M1 error format, one JSON shape and one closed set of codes for every error
//!   response; bodies carry translation keys, never user-facing text.
//! - [`id`]: typed UUIDv7 identifiers (portability rule 3 in `docs/architecture/deployment.md`).
//! - [`event`]: the versioned envelope for events that cross service boundaries through the
//!   outbox.
//! - [`events`]: the events themselves, one module per producing service.
//! - [`identity`]: the claims of the internal identity token (decision 27) and the header that
//!   carries it.
//! - [`client`]: the typed HTTP/JSON client for synchronous internal API calls: a timeout is
//!   mandatory and every failure maps to an M1 code.
//! - [`api`]: the internal APIs themselves, one module per serving service.
//! - [`certs`]: the certs service's client API, workstation certificates (Laysi/trustiot-v2#534),
//!   and the MQTT broker's server certificate (Laysi/trustiot-v2#557).
//! - [`service`]: the names of the services that take part in these contracts.
//! - [`openapi`]: the schemas shared by every service spec merged into `api/openapi.json`.
//!
//! `services/ai-gateway` must not depend on this crate (decision 28), so nothing it needs lives
//! here. The rules for adding API types, events and error codes are in this crate's
//! `README.md`.

pub mod api;
pub mod certs;
pub mod client;
pub mod error;
pub mod event;
pub mod events;
pub mod id;
pub mod identity;
pub mod json;
pub mod openapi;
pub mod service;

mod naming;

pub use client::{CallError, CallErrorKind, ClientConfigError, Endpoint, InternalClient, Method};
pub use error::{ErrorBody, ErrorCode, FieldError, MessageKey};
// `contract::Actor` is the internal token's `act` claim (identity); the event envelope's
// actor, who made a change, stays `contract::event::Actor`.
pub use event::{DecodeError, EncodeError, Event, EventEnvelope};
pub use id::{
    DeviceId, DeviceModelId, EventId, GatewayBindingId, RunId, SessionId, TenantId, UserId,
};
pub use identity::{
    Actor, INTERNAL_IDENTITY_HEADER, InternalClaims, InternalToken, TenantGrant, TenantRole,
};
pub use service::{Service, ServiceName};
