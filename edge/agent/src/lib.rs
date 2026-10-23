//! The TrustIoT gateway agent (decision 10): the program on the gateway that reads the field
//! devices and sends their data to the cloud. It runs as the systemd service `trustiot-agent`
//! (`src/main.rs`, `systemd/`).
//!
//! - [`config`] and [`setup`]: the configuration file, the client certificate and its key
//!   ([`identity`], the TPM in production: [`tpm`]), and both TLS layers ([`tls`]), all checked
//!   before the agent reports ready.
//! - [`link`]: the MQTT 5 connection to the cloud over a 443 WebSocket with mTLS inside
//!   ([`transport`]), with presence (`online`, the last will, `offline` on shutdown),
//!   reconnects with backoff ([`backoff`]) and bounded memory. The MQTT client is the agent's
//!   own (`mqtt`); its module documentation says why.
//! - [`heartbeat`]: the basic heartbeat; [`sdnotify`]: readiness and the systemd watchdog, fed
//!   while the connection task makes progress ([`liveness`]).
//! - [`modbus`]: reads the data points of one device over Modbus TCP (rebuild checklist F1,
//!   F5).
//!
//! Scheduling, the offline buffer and configuration apply build on these (`README.md` lists
//! what is where).

pub mod backoff;
pub mod config;
pub mod heartbeat;
pub mod identity;
pub mod link;
pub mod liveness;
pub mod modbus;
mod mqtt;
pub mod sdnotify;
pub mod setup;
pub mod tls;
pub mod tpm;
pub mod transport;

pub use mqtt::packet::ReasonCode;
