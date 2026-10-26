//! Internal APIs (decision 4), one module per serving service: each operation is an
//! [`Endpoint`](crate::client::Endpoint) with its request and response types. The rules for
//! adding one are in this crate's `README.md`, 「內部 API」.

pub mod identity;
pub mod telemetry_query;
