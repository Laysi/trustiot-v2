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
//! - [`heartbeat`]: the basic heartbeat; [`sdnotify`]: readiness and the systemd watchdog, which
//!   restarts the whole service and is fed only while the connection task makes progress
//!   ([`liveness`]).
//! - [`modbus`]: reads the data points of devices over Modbus TCP or over RTU-over-TCP to an
//!   RS485 converter, with one shared connection per endpoint (rebuild checklist F1, F2, F5).
//! - [`polling`]: polls each device on a fixed schedule with bounded waits, reconnect backoff
//!   and a per-device watchdog that closes the connection of a device that stopped answering
//!   (F4); not the systemd watchdog. The service does not run it yet.
//!
//! Scheduling the polls in the service, the offline buffer and configuration apply build on
//! these (`README.md` lists what is where).

pub mod backoff;
pub mod config;
pub mod heartbeat;
pub mod identity;
pub mod link;
pub mod liveness;
pub mod modbus;
mod mqtt;
pub mod polling;
pub mod sdnotify;
pub mod setup;
pub mod tls;
pub mod tpm;
pub mod transport;

pub use mqtt::packet::ReasonCode;
