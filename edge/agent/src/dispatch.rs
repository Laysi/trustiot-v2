//! Hands the messages the cloud sends on the `down` channels to their handlers.
//!
//! The link acknowledges a QoS 1 message (PUBACK) as soon as it is queued for the application,
//! and stops reading while that queue is full; a message that waits longer than
//! [`crate::liveness::DELIVERY_LIMIT`] counts as a stuck agent. So [`run`] takes every message
//! at once and hands it over without waiting: a handler that is busy loses old messages, never
//! the link's progress. A subscribed channel nobody takes messages from would acknowledge them
//! and lose them (a retained configuration, for one), so the link subscribes to exactly the
//! channels [`subscriptions`] lists, each with its handler in [`Handlers`]: `config` to the
//! configuration side ([`crate::desired`]), `telemetry-ack` to the uploader ([`crate::upload`]).
//! Each later issue adds its channel there together with its handler: log-request
//! (Laysi/trustiot-v2#185), live-control (Laysi/trustiot-v2#121).

use protocol::telemetry::TelemetryAck;
use protocol::topic::Channel;
use tokio::time::Instant;

use crate::desired::Inbox;
use crate::link::{Incoming, Subscriptions};
use crate::upload::Acks;

/// How often an undecodable message is logged at most.
const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The channels that have a handler, for [`crate::link::LinkSettings::subscriptions`].
pub fn subscriptions() -> Subscriptions {
    Subscriptions::new([Channel::Config, Channel::TelemetryAck])
        .unwrap_or_else(|error| unreachable!("config and telemetry-ack are down channels: {error}"))
}

/// Where each channel's messages go.
#[derive(Debug)]
pub struct Handlers {
    /// `down/config`, as received, to the configuration side, which decodes and answers it.
    pub config: Inbox,
    /// `down/telemetry-ack`, decoded, to the uploader.
    pub telemetry_ack: Acks,
}

/// Takes every message from `incoming` until the link has stopped, and hands it to its handler
/// without waiting.
pub async fn run(mut incoming: Incoming, handlers: Handlers) {
    let mut undecodable = 0_u64;
    let mut logged: Option<Instant> = None;
    while let Some(message) = incoming.recv().await {
        match message.channel {
            Channel::Config => handlers.config.deliver(message.payload),
            Channel::TelemetryAck => match protocol::decode::<TelemetryAck>(&message.payload) {
                Ok(ack) => handlers.telemetry_ack.offer(ack),
                Err(error) => {
                    undecodable += 1;
                    if logged.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
                        logged = Some(Instant::now());
                        tracing::warn!(%error, undecodable, "ignored a telemetry acknowledgement that does not decode");
                    }
                }
            },
            // The link delivers only the channels it subscribed to, which all have a handler.
            channel => tracing::warn!(?channel, "no handler for a message; dropped"),
        }
    }
}
