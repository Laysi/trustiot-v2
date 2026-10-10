//! Shared skeleton for TrustIoT v2 cloud services (docs/architecture/deployment.md, "讓 k8s
//! 只是打包工作的服務設計規則"):
//!
//! - [`config`]: settings only from environment variables or a TOML file, checked at startup.
//! - [`health`]: liveness and readiness endpoints; readiness fails as soon as SIGTERM arrives.
//! - [`shutdown`]: SIGTERM handling for PID 1 in a container, with draining.
//! - [`telemetry`]: JSON logs with trace ids, optional OTLP export, no sensitive field values.
//! - [`job_lock`]: Postgres advisory locks so a scheduled job runs on one replica at a time.
//! - `identity` (with the default `contract` feature): verification of the internal identity
//!   token and the extractor for the user a request acts for (decision 27).
//! - [`run`]: ties them together around an axum router.
//!
//! See `README.md` in this crate for how a service uses it.

pub mod config;
pub mod health;
pub mod http;
#[cfg(feature = "contract")]
pub mod identity;
pub mod job_lock;
mod service;
pub mod shutdown;
pub mod telemetry;

pub use service::{BoxError, Context, ServiceInfo, run};
