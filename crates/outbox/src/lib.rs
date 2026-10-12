//! Transactional outbox for cross-service writes (decision 4).
//!
//! - A producer writes events with [`publish`] in the same transaction as the business change,
//!   so both commit or neither does.
//! - A consumer service runs a [`Consumer`] per subscription. It polls the producer's log as
//!   the service's platform role and processes each event as its service role, in a
//!   transaction in the event's tenant (decision 29) that also records the event ID and the new
//!   position: delivery is at least once, processing is once per event ID, and events of one
//!   aggregate arrive in the order they were written.
//! - [`replay_from`] moves a subscription back to re-read the log.
//! - [`notify`] supports consumers that treat events as notifications and read the producer's
//!   current state.
//! - Lag is exported as OpenTelemetry gauges (see [`lag`]).
//!
//! The design, the delivery decision and its trade-offs, and how the tables fit row-level
//! security (decision 29) are in this crate's `README.md`.

pub mod aggregate;
mod consumer;
pub mod lag;
pub mod notify;
mod position;
mod producer;
mod redact;
pub mod schema;

pub use aggregate::{Aggregate, InvalidAggregate};
pub use consumer::{
    Consumer, ConsumerConfig, ConsumerError, Delivery, FailureKind, Handler, HandlerError,
    InvalidConfig, MAX_HANDLER_PANICS, Outcome, Progress, Replay, StartFrom, replay_from,
};
pub use lag::Lag;
pub use position::{InvalidPosition, Position};
pub use producer::{PublishError, Published, aggregate_version, publish, write};
