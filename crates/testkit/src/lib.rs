//! Test support for TrustIoT v2 services. Use it only as a `[dev-dependencies]` entry
//! (`tools/repo-check` rejects anything else).
//!
//! - [`isolation`]: the cross-tenant negative test suite every service runs (decision 29):
//!   two tenants with their own data, every declared table and endpoint attacked as one tenant
//!   against the other, and a failure for every table or endpoint the suite does not declare.
//! - [`identity`]: internal identity tokens signed with a key made for the test process.
//! - [`postgres`]: a throwaway Postgres with TLS, which service-kit's `verify-full` connections
//!   need.
//! - [`container`]: podman or docker containers removed on drop, and the rule that a missing
//!   container runtime skips a test locally but fails it in CI.
//!
//! How a service hooks in is in the crate's README.

pub mod container;
pub mod identity;
pub mod isolation;
pub mod postgres;
