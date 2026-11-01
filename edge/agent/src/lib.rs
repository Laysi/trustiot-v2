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
//!   own (`mqtt`); its module documentation says why. [`dispatch`]: the `down` channels the
//!   link subscribes to, and the hand-over of each message to its handler.
//! - [`upload`]: readings into batches and batches to the cloud, one at a time until the cloud
//!   acknowledges them, through the offline buffer ([`buffer`]: memory, then the data
//!   partition), on a stream of sequence numbers (rebuild checklist G1); [`clock`]: the
//!   gateway's clock, whether it is synchronized, and the clock that measures a batch's age.
//! - [`heartbeat`]: the heartbeat; [`sdnotify`]: readiness and the systemd watchdog, which
//!   restarts the whole service and is fed only while the connection task makes progress
//!   ([`liveness`]).
//! - [`modbus`]: reads the data points of devices over Modbus TCP or over RTU-over-TCP to an
//!   RS485 converter, with one shared connection per endpoint (rebuild checklist F1, F2, F5).
//! - [`polling`]: polls each device on a fixed schedule with bounded waits, reconnect backoff
//!   and a per-device watchdog that closes the connection of a device that stopped answering
//!   (F4); not the systemd watchdog.
//! - [`provision`]: the `trustiot-agent provision` subcommand for the production workstation:
//!   the TPM key, a CSR and the endorsement key in a JSON report (decision 12).
//! - [`desired`]: receives the desired configuration on `down/config`, applies it atomically
//!   (one polling loop per device, unchanged devices untouched), saves it on the data partition
//!   so the gateway polls with it after a restart without the cloud, and reports the outcome
//!   (rebuild checklist E1).
//! - [`live`]: live values for open device charts (rebuild checklist I3): the leases the cloud
//!   opens on `down/live-control`, faster polling of the leased devices, and each of their
//!   samples published on `up/live` at once, outside the buffer and without acknowledgement.
//! - [`renewal`]: renews the client certificate over HTTPS with a TPM-signed challenge, on
//!   schedule and when the broker refuses the certificate, and swaps it in without losing what
//!   the connection already accepted (rebuild checklist D2).
//! - [`revocation`]: fetches the root CRL from the gateway host while the agent runs, keeps only
//!   newer ones across restarts and alongside `provision install`, and puts each in use with a
//!   full handshake (Laysi/trustiot-v2#563).
//! - [`allowlist`]: the format of the fieldbus allowlist, the TCP destinations the agent's
//!   account may reach, shared with the separate executable `trustiot-agent-allowlist` that
//!   keeps them in nftables (rebuild checklist D5, Laysi/trustiot-v2#864).
//! - [`nameservers`]: the nameservers of `/etc/resolv.conf` as glibc reads them, which the
//!   separate executable `trustiot-agent-nameservers` puts in the default deny's DNS allowance
//!   (Laysi/trustiot-v2#879).
//!
//! The service wires them together: the polls of the devices [`desired`] applies stamp their
//! readings with the [`clock::GatewayClock`] that [`upload`] compares with the cloud's time
//! and hand them to [`live::LiveSink`], which publishes those of leased devices on `up/live` and
//! passes every poll on to [`upload::Readings`] (`README.md` lists what is where).

pub mod allowlist;
pub mod backoff;
pub mod buffer;
pub mod clock;
pub mod config;
pub mod desired;
mod directory;
pub mod dispatch;
pub mod heartbeat;
pub mod identity;
pub mod link;
pub mod live;
pub mod liveness;
pub mod modbus;
mod mqtt;
pub mod nameservers;
pub mod polling;
pub mod provision;
pub mod renewal;
pub mod revocation;
pub mod sdnotify;
pub mod setup;
pub mod tls;
pub mod tpm;
pub mod transport;
pub mod upload;

pub use mqtt::packet::ReasonCode;
