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
//!   (F4); not the systemd watchdog.
//! - [`desired`]: receives the desired configuration on `down/config`, applies it atomically
//!   (one polling loop per device, unchanged devices untouched), saves it on the data partition
//!   so the gateway polls with it after a restart without the cloud, and reports the outcome
//!   (rebuild checklist E1).
//! - [`renewal`]: renews the client certificate over HTTPS with a TPM-signed challenge, on
//!   schedule and when the broker refuses the certificate, and swaps it in without losing what
//!   the connection already accepted (rebuild checklist D2).
//!
//! The offline buffer builds on these (`README.md` lists what is where).

pub mod backoff;
pub mod config;
pub mod desired;
pub mod heartbeat;
pub mod identity;
pub mod link;
pub mod liveness;
pub mod modbus;
mod mqtt;
pub mod polling;
pub mod renewal;
pub mod sdnotify;
pub mod setup;
pub mod tls;
pub mod tpm;
pub mod transport;

pub use mqtt::packet::ReasonCode;
