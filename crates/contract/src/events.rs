//! The events services exchange through the outbox (decision 4), one module per producing
//! service. The envelope and the evolution rules are in [`crate::event`] and this crate's
//! `README.md`; every event here has fixtures under `tests/fixtures/events/<TYPE>/` that
//! `tests/events.rs` decodes.

pub mod assets;
pub mod certs;
pub mod identity;
