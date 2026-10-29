//! Test support for TrustIoT v2 services. Use it only as a `[dev-dependencies]` entry
//! (`tools/repo-check` rejects anything else).
//!
//! - [`isolation`]: the cross-tenant negative test suite every service runs (decision 29):
//!   two tenants with their own data, every declared table and endpoint attacked as one tenant
//!   against the other, and a failure for every table or endpoint the suite does not declare.
//! - [`identity`]: internal identity tokens signed with a key made for the test process.
//! - [`postgres`]: a throwaway Postgres with TLS, which service-kit's `verify-full` connections
//!   need.
//! - [`product`]: that Postgres with the whole product's schema, applied by the migration Job
//!   (`tools/migrate`) itself.
//! - [`container`]: podman or docker containers removed on drop, and the rule that a missing
//!   container runtime skips a test locally but fails it in CI.
//! - [`modbus`]: a fake Modbus TCP and RTU-over-TCP endpoint with unit identifiers and fault
//!   injection (rebuild checklist M7), for the gateway's acquisition tests.
//! - [`mqtt`]: the gateway's way to the broker: a test PKI, a WebSocket front standing in for
//!   the ingress and the MQTT relay, and Mosquitto with mTLS and the gateway role.
//! - [`clock`]: time control for tests on real sockets: a paused tokio clock that moves only
//!   when the test advances it, and waits bounded by wall-clock time.
//! - [`chart`]: what the deployment chart assumes about a service, such as its Postgres
//!   connection budget, so the service's tests hold its code to the same numbers.
//! - [`swtpm`]: a software TPM 2.0 in a throwaway container, with sessions that behave like the
//!   kernel's resource manager and tpm2-tools to set up and inspect it, for the gateway's TPM
//!   tests.
//!
//! How a service hooks in is in the crate's README.

pub mod chart;
pub mod clock;
pub mod container;
pub mod identity;
pub mod isolation;
pub mod modbus;
pub mod mqtt;
pub mod postgres;
pub mod product;
pub mod swtpm;
