//! The MQTT 5 client the cloud services use on the broker's service port (1883, inside the
//! mesh): certs for dynamic security's commands, ingest for telemetry and its acknowledgements,
//! gateway-ops for configuration. README.md has the usage and the reasons behind each rule.
//!
//! - [`Client::start`] checks the settings, reads the password file once, and starts a task
//!   that owns the connection. It returns a [`Client`] to publish with and the [`Events`] of the
//!   connection.
//! - Every connection is a clean start as `service:<service>` with the client id
//!   `service:<service>:<random>`, the password read again from its file; the subscriptions
//!   given to [`Client::start`] are made again on every connection, before [`Event::Connected`]
//!   reports their SUBACK reason codes.
//! - [`Client::publish`] sends a QoS 1 PUBLISH and waits for its PUBACK; a reason code of 0x80
//!   or above (0x87 for a topic the account may not publish to) is [`PublishError::Refused`].
//! - A lost or refused connection is [`Event::Disconnected`], followed by a reconnect after a
//!   delay that doubles from [`RECONNECT_DELAY_MIN`] up to [`RECONNECT_DELAY_MAX`], with
//!   jitter.
//! - What the client holds for the caller is bounded: messages by [`MAX_HELD_BYTES`] (the
//!   client stops reading from the broker while they fill it), status events by
//!   [`MAX_QUEUED_EVENTS`] (the client does not reconnect while they fill it).
//!
//! The password never reaches a log or an error at any level: [`Password`] prints as
//! `Password(<redacted>)`, and the CONNECT packet is encoded into a buffer that is wiped once
//! written.

mod backoff;
mod codes;
mod config;
mod driver;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

pub use crate::config::{
    Config, ConfigError, DEFAULT_PORT, MAX_PASSWORD_BYTES, Password, PasswordError, USERNAME_PREFIX,
};
use crate::driver::{Ack, Budget, Command, Delivery, Driver, ShutdownRequest};

/// The keepalive the client asks for; the broker may set another in its CONNACK, which the
/// client then follows. The client sends PINGREQ at this interval and treats the connection as
/// lost when a PINGRESP has not arrived by the next one.
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// How long the TCP connection, CONNECT, CONNACK, SUBSCRIBE and SUBACK may take together.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one write to the broker may block before the connection counts as lost.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Most QoS 1 publishes waiting for their PUBACK at once (fewer when the broker's Receive
/// Maximum is lower; the chart's broker allows 20). Further publishes wait for a free slot.
pub const MAX_INFLIGHT: u16 = 16;

/// How long a sent publish may wait for its PUBACK. When one waits longer, the connection
/// counts as lost: every publish in flight fails with [`PublishError::ConnectionLost`] and its
/// slot is free again. The packet id of an unanswered publish cannot be reused, and the broker
/// counts it against its Receive Maximum, so only a new connection frees the slot for certain.
pub const PUBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// The Receive Maximum the client sends: most QoS 1 messages the broker sends before the
/// client acknowledges them. The client acknowledges a message when [`Events::recv`] hands it
/// over, so the broker never sends more than this many ahead of the caller.
pub const RECEIVE_MAXIMUM: u16 = 32;

/// Most bytes of messages the client holds for the caller: those waiting in [`Events`] (QoS 1
/// ones not acknowledged yet among them) and those that arrived before the SUBACK. Each message
/// counts its payload, its topic and a fixed overhead for its bookkeeping. The client stops
/// reading from the broker before a message could take the total above this, so TCP flow
/// control and the broker's own queue limits hold everything else, QoS 0 messages included,
/// until [`Events::recv`] makes room. It equals the broker's `max_queued_bytes` (16 MiB,
/// deploy/chart): the client holds no more for a slow caller than the broker queues for it.
pub const MAX_HELD_BYTES: usize = 16 * 1024 * 1024;

/// Most status events ([`Event::Connected`], [`Event::Disconnected`]) waiting in [`Events`].
/// The client makes no new connection attempt while it could take them above this, so a caller
/// that stops reading stops the reconnects rather than growing the queue.
pub const MAX_QUEUED_EVENTS: usize = 64;

/// The Maximum Packet Size of both directions: `protocol::topic::MIN_MAX_PACKET_BYTES`
/// (1 MiB + 8 KiB), what the broker and every client of the protocol accept.
pub const MAX_PACKET_BYTES: u32 = max_packet_bytes();

#[allow(
    clippy::cast_possible_truncation,
    reason = "the assertion before the cast proves the value fits"
)]
const fn max_packet_bytes() -> u32 {
    assert!(protocol::topic::MIN_MAX_PACKET_BYTES <= u32::MAX as usize);
    protocol::topic::MIN_MAX_PACKET_BYTES as u32
}

/// The first reconnect waits between half of this and this.
pub const RECONNECT_DELAY_MIN: Duration = Duration::from_millis(500);

/// The longest wait before a reconnect: the delay doubles after each failure up to this.
pub const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(30);

/// A connection that stayed up this long resets the reconnect delay to its minimum.
pub const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// Commands waiting for the connection task: further callers of [`Client::publish`] wait.
const COMMAND_QUEUE: usize = 64;

/// A handle to publish through the connection. Cheap to clone; the connection task ends when
/// the last clone is dropped or [`Client::shutdown`] is called.
#[derive(Clone, Debug)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    shutdown: mpsc::UnboundedSender<ShutdownRequest>,
    client_id: Arc<str>,
}

/// Why [`Client::start`] refused to start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// A setting is unacceptable.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The password file cannot be used.
    #[error(transparent)]
    Password(#[from] PasswordError),
    /// The subscription at this index is not a valid topic filter.
    #[error("subscription {index} is not a valid MQTT topic filter")]
    Subscription {
        /// Its position in the list given to [`Client::start`].
        index: usize,
    },
    /// The operating system gave no randomness for the client id.
    #[error("no randomness for the mqtt client id")]
    Random,
    /// Not called inside a Tokio runtime.
    #[error("the mqtt client must be started inside a Tokio runtime")]
    NoRuntime,
}

/// How a message is published. QoS is always 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishOptions {
    /// The retain flag.
    pub retain: bool,
    /// The MQTT 5 message expiry interval, rounded up to whole seconds;
    /// `protocol::topic::Delivery::message_expiry` of the channel.
    pub message_expiry: Option<Duration>,
}

/// Why a publish did not succeed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// The broker answered with a PUBACK reason code of 0x80 or above, for example 0x87 when
    /// the account may not publish to the topic.
    #[error("the broker refused the publish with reason code {code:#04x}")]
    Refused {
        /// The PUBACK reason code.
        code: u8,
        /// The broker's reason string, if it sent one.
        reason: Option<String>,
    },
    /// There is no connection right now; it is being made or re-established. Nothing was sent.
    #[error("not connected to the mqtt broker")]
    NotConnected,
    /// The connection was lost before the PUBACK arrived: the broker may or may not have the
    /// message.
    #[error("the mqtt connection was lost before the broker acknowledged the publish")]
    ConnectionLost,
    /// The packet is larger than the broker accepts. Nothing was sent.
    #[error("the publish is {size} bytes, more than the broker's maximum of {max}")]
    TooLarge {
        /// The size of the PUBLISH packet.
        size: usize,
        /// The largest packet the broker accepts.
        max: u32,
    },
    /// The topic is empty, longer than 65535 bytes, or has a wildcard, a control character or
    /// a Unicode noncharacter. Nothing was sent.
    #[error("not a valid mqtt topic name")]
    InvalidTopic,
    /// The client was shut down.
    #[error("the mqtt client is shut down")]
    Closed,
}

/// Why a connection could not be made or was lost. Never contains the password.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    /// The password file cannot be used.
    #[error(transparent)]
    Password(PasswordError),
    /// A network operation failed.
    #[error("{during}: {source}")]
    Io {
        /// What the client was doing.
        during: &'static str,
        /// The error.
        source: std::io::Error,
    },
    /// Something did not happen in time.
    #[error("timed out waiting for {waiting_for}")]
    Timeout {
        /// What the client was waiting for.
        waiting_for: &'static str,
    },
    /// The broker refused the connection: CONNACK reason code 0x80 or above. Mosquitto 2.1
    /// with dynamic security answers a wrong username or password with 0x87 (not authorized).
    #[error("the broker refused the connection with reason code {code:#04x}")]
    Refused {
        /// The CONNACK reason code.
        code: u8,
        /// The broker's reason string, if it sent one.
        reason: Option<String>,
    },
    /// The broker ended the connection with a DISCONNECT.
    #[error("the broker disconnected with reason code {code:#04x}")]
    ServerDisconnect {
        /// The DISCONNECT reason code.
        code: u8,
        /// The broker's reason string, if it sent one.
        reason: Option<String>,
    },
    /// The broker closed the connection without a DISCONNECT.
    #[error("the broker closed the connection")]
    Closed,
    /// No PINGRESP arrived within the keepalive.
    #[error("the broker did not answer PINGREQ within the keepalive")]
    KeepAlive,
    /// The broker sent something this client does not accept.
    #[error("mqtt protocol error: {0}")]
    Protocol(String),
}

/// What happened on the connection, in order. Its `Debug` form never shows a payload.
#[derive(Debug)]
pub enum Event {
    /// A connection is up: the first one or a reconnect. Each connection is a new session, so
    /// anything the caller keeps in sync through the broker should be synchronised again.
    Connected {
        /// The subscriptions given to [`Client::start`], in the same order, with the broker's
        /// SUBACK reason codes. [`Subscriptions::all_granted`] says whether the broker granted
        /// every one with QoS 1, as asked; the client logs a warning for each one it did not.
        subscriptions: Subscriptions,
    },
    /// The connection could not be made or was lost. The client tries again after
    /// `retry_in`.
    Disconnected {
        /// Why.
        error: ConnectionError,
        /// The delay before the next attempt.
        retry_in: Duration,
    },
    /// A message arrived on a subscription.
    Message(Message),
}

/// The broker's answers to the subscriptions of one connection, in the order given to
/// [`Client::start`]. Derefs to a slice of [`SubscriptionOutcome`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subscriptions(Vec<SubscriptionOutcome>);

impl Subscriptions {
    pub(crate) fn new(outcomes: Vec<SubscriptionOutcome>) -> Self {
        Self(outcomes)
    }

    /// Whether the broker granted every subscription with QoS 1, as asked: `false` when it
    /// refused one or granted only QoS 0. True when there are no subscriptions.
    #[must_use]
    pub fn all_granted(&self) -> bool {
        self.0.iter().all(SubscriptionOutcome::is_granted)
    }
}

impl std::ops::Deref for Subscriptions {
    type Target = [SubscriptionOutcome];

    fn deref(&self) -> &[SubscriptionOutcome] {
        &self.0
    }
}

impl IntoIterator for Subscriptions {
    type Item = SubscriptionOutcome;
    type IntoIter = std::vec::IntoIter<SubscriptionOutcome>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// One subscription and the broker's answer to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionOutcome {
    /// The topic filter, as given to [`Client::start`].
    pub filter: String,
    /// The SUBACK reason code: the granted QoS (0 or 1), or 0x80 and above when refused (0x87:
    /// the account may not subscribe to the filter).
    pub code: u8,
}

impl SubscriptionOutcome {
    /// The QoS the broker granted, or `None` when it refused the subscription. Every
    /// subscription asks for QoS 1, so 0 means the broker granted less than asked.
    #[must_use]
    pub fn granted_qos(&self) -> Option<u8> {
        (self.code < 0x80).then_some(self.code)
    }

    /// Whether the broker granted the subscription with QoS 1, as asked.
    #[must_use]
    pub fn is_granted(&self) -> bool {
        self.granted_qos() == Some(1)
    }
}

/// A message from a subscription. Its `Debug` form shows the topic and the payload's length,
/// never the payload.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    /// The topic it was published to.
    pub topic: String,
    /// The payload.
    pub payload: Bytes,
    /// The QoS the broker delivered it with: 1, or 0 when the publisher sent it with QoS 0 or
    /// the broker granted the subscription only QoS 0. A caller that needs every message
    /// acknowledged end to end refuses QoS 0 messages.
    pub qos: u8,
    /// Whether it is a retained message the broker sent because of a new subscription.
    pub retain: bool,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("topic", &self.topic)
            .field("payload_bytes", &self.payload.len())
            .field("qos", &self.qos)
            .field("retain", &self.retain)
            .finish()
    }
}

/// The events of the connection. Keep reading them: QoS 1 messages are acknowledged as
/// [`Events::recv`] returns them, and the client stops reading from the broker while the
/// messages waiting here fill [`MAX_HELD_BYTES`], so a caller that stops reading stops the
/// flow of messages (README.md, "要一直讀 `Events`").
///
/// Dropping it while subscribed discards messages: from then on the client acknowledges every
/// QoS 1 message at once, those that were waiting here included, and drops it.
pub struct Events {
    deliveries: mpsc::UnboundedReceiver<Delivery>,
    acks: mpsc::UnboundedSender<Ack>,
    budget: Arc<Budget>,
}

impl fmt::Debug for Events {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Events")
            .field("held_bytes", &self.budget.held_bytes())
            .field("queued_events", &self.budget.queued_events())
            .finish_non_exhaustive()
    }
}

impl Events {
    /// The next event. A QoS 1 message is acknowledged to the broker when it is returned here.
    ///
    /// `None` once the connection task has ended: after [`Client::shutdown`] or once every
    /// [`Client`] is dropped, after the remaining events. `None` without either means the task
    /// failed (it logs an error): the client is unusable and the service is not ready.
    pub async fn recv(&mut self) -> Option<Event> {
        let event = match self.deliveries.recv().await? {
            Delivery::Event(event) => {
                self.budget.event_taken();
                event
            }
            Delivery::Message { message, ack, cost } => {
                self.budget.message_taken(cost);
                if let Some(ack) = ack {
                    // The task is gone once the client shut down; nothing to acknowledge then.
                    let _ = self.acks.send(ack);
                }
                Event::Message(message)
            }
        };
        Some(event)
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.budget.consumer_dropped();
    }
}

impl Client {
    /// Checks `config` and the password file, and starts the connection task. The first
    /// connection attempt starts at once; [`Event::Connected`] reports it. `subscriptions` are
    /// topic filters, shared ones (`$share/<group>/<filter>`) included, subscribed with QoS 1
    /// on every connection.
    ///
    /// # Errors
    ///
    /// When a setting, the password file or a subscription is unacceptable, or when called
    /// outside a Tokio runtime.
    pub fn start(
        config: &Config,
        subscriptions: Vec<String>,
    ) -> Result<(Self, Events), StartError> {
        config.validate()?;
        // Fail at startup, not at the first connection, when the password file is unusable.
        drop(Password::read(&config.password_file)?);
        if let Some(index) = subscriptions
            .iter()
            .position(|filter| !valid_filter(filter))
        {
            return Err(StartError::Subscription { index });
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| StartError::NoRuntime)?;
        let client_id: Arc<str> = client_id(&config.username)?.into();
        let (commands, command_receiver) = mpsc::channel(COMMAND_QUEUE);
        // One entry per Client::shutdown call; the task ends at the first.
        let (shutdown, shutdown_receiver) = mpsc::unbounded_channel();
        let (acks, ack_receiver) = mpsc::unbounded_channel();
        // Unbounded as a channel, bounded by the budget: the task counts what it sends.
        let (delivery_sender, deliveries) = mpsc::unbounded_channel();
        let budget = Arc::new(Budget::default());
        let driver = Driver::new(
            config,
            Arc::clone(&client_id),
            subscriptions,
            command_receiver,
            shutdown_receiver,
            ack_receiver,
            delivery_sender,
            Arc::clone(&budget),
        );
        let task = runtime.spawn(driver.run());
        let task_client_id = Arc::clone(&client_id);
        runtime.spawn(async move {
            // A panic drops the task's channels: publishes fail with Closed and Events::recv
            // returns None. Say why, without the panic's message.
            if let Err(error) = task.await
                && error.is_panic()
            {
                tracing::error!(
                    client_id = %task_client_id,
                    "the mqtt connection task panicked; the client is closed"
                );
            }
        });
        Ok((
            Self {
                commands,
                shutdown,
                client_id,
            },
            Events {
                deliveries,
                acks,
                budget,
            },
        ))
    }

    /// The client id of this instance: `service:<service>:<16 hex digits>`. It always has a
    /// `:`, which a gateway id never has, so a service's connection can never take over a
    /// gateway's session; the random part differs between instances, so replicas do not take
    /// over each other's connections.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Publishes `payload` to `topic` with QoS 1 and waits for the broker's PUBACK. Waits for a
    /// free slot when [`MAX_INFLIGHT`] publishes are waiting already. Fails at once with
    /// [`PublishError::NotConnected`] while there is no connection, also while one is being
    /// made. Cancelling the returned future before the publish is sent drops it; a publish
    /// already sent is not taken back.
    ///
    /// # Errors
    ///
    /// [`PublishError::Refused`] when the PUBACK reason code is 0x80 or above, and the other
    /// [`PublishError`] variants as documented there.
    pub async fn publish(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        options: PublishOptions,
    ) -> Result<(), PublishError> {
        if !valid_topic(topic) {
            return Err(PublishError::InvalidTopic);
        }
        let (reply, answer) = oneshot::channel();
        let command = Command {
            topic: topic.to_owned(),
            payload: payload.into(),
            options,
            reply,
        };
        self.commands
            .send(command)
            .await
            .map_err(|_| PublishError::Closed)?;
        answer.await.unwrap_or(Err(PublishError::Closed))
    }

    /// Disconnects from the broker (with a DISCONNECT when connected) and ends the connection
    /// task; publishes still waiting fail with [`PublishError::Closed`], and [`Events::recv`]
    /// returns `None` after the remaining events. Returns once the task has ended.
    pub async fn shutdown(&self) {
        let (done, finished) = oneshot::channel();
        if self.shutdown.send(done).is_ok() {
            let _ = finished.await;
        }
    }
}

/// `<username>:<16 random hex digits>`.
fn client_id(username: &str) -> Result<String, StartError> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| StartError::Random)?;
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{username}:{hex}"))
}

/// An MQTT string the broker accepts: at most 65535 bytes, no U+0000, and none of the code
/// points MQTT 5 section 1.5.4 lets a receiver treat as malformed (control characters
/// U+0001–U+001F and U+007F–U+009F, and Unicode noncharacters), which Mosquitto refuses by
/// closing the connection.
fn valid_string(text: &str) -> bool {
    text.len() <= usize::from(u16::MAX)
        && !text.chars().any(|c| c.is_control() || is_noncharacter(c))
}

/// U+FDD0–U+FDEF and the last two code points of every plane.
fn is_noncharacter(c: char) -> bool {
    let code = u32::from(c);
    (0xFDD0..=0xFDEF).contains(&code) || code & 0xFFFE == 0xFFFE
}

/// A topic name to publish to: not empty, no wildcard (MQTT 5, section 4.7).
fn valid_topic(topic: &str) -> bool {
    !topic.is_empty() && valid_string(topic) && !topic.contains(['+', '#'])
}

/// A topic filter: wildcards only as whole levels, `#` only last (MQTT 5, section 4.7).
fn valid_filter(filter: &str) -> bool {
    !filter.is_empty()
        && valid_string(filter)
        && rumqttc::v5::mqttbytes::valid_filter(filter)
        && filter
            .split('/')
            .all(|level| !(level.contains(['+', '#']) && level.len() > 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_have_a_colon_and_differ_between_instances() {
        let first = client_id("service:ingest").unwrap();
        let second = client_id("service:ingest").unwrap();
        assert!(first.starts_with("service:ingest:"), "{first}");
        assert_eq!(first.len(), "service:ingest:".len() + 16);
        assert_ne!(first, second);
        // A gateway id never has a colon (crates/protocol, ids), so no service client id can
        // be one.
        assert!(protocol::GatewayId::new(&first).is_err());
    }

    #[test]
    fn topics_and_filters_are_checked() {
        assert!(valid_topic("trustiot/v1/gw/gw-a/down/config"));
        assert!(valid_topic("$CONTROL/dynamic-security/v1"));
        for topic in ["", "a/+", "a/#", "a\0b"] {
            assert!(!valid_topic(topic), "{topic:?}");
        }
        assert!(!valid_topic(&"a".repeat(65_536)));
        for filter in [
            "trustiot/v1/gw/+/up/telemetry",
            "$share/ingest/trustiot/v1/gw/+/up/telemetry",
            "$CONTROL/dynamic-security/v1/response",
            "#",
            "a/#",
        ] {
            assert!(valid_filter(filter), "{filter:?}");
        }
        for filter in ["", "a/#/b", "a/b#", "a+/b", "a\0"] {
            assert!(!valid_filter(filter), "{filter:?}");
        }
        // Code points the broker treats as malformed, in topics and filters alike.
        for bad in [
            "\u{1}",
            "\n",
            "\u{7f}",
            "\u{9f}",
            "\u{fdd0}",
            "\u{fffe}",
            "\u{1ffff}",
        ] {
            let text = format!("a/{bad}/b");
            assert!(!valid_topic(&text), "{text:?}");
            assert!(!valid_filter(&text), "{text:?}");
        }
        // Other non-ASCII text is fine.
        assert!(valid_topic("a/\u{e9}\u{4e2d}\u{fffd}/b"));
    }

    #[test]
    fn a_subscription_granted_qos_0_is_not_granted() {
        let outcome = |code| SubscriptionOutcome {
            filter: "a/b".to_owned(),
            code,
        };
        assert!(outcome(0x01).is_granted());
        assert_eq!(outcome(0x00).granted_qos(), Some(0));
        assert!(!outcome(0x00).is_granted());
        assert_eq!(outcome(0x87).granted_qos(), None);
        assert!(Subscriptions::new(vec![outcome(0x01), outcome(0x01)]).all_granted());
        assert!(!Subscriptions::new(vec![outcome(0x01), outcome(0x00)]).all_granted());
        assert!(!Subscriptions::new(vec![outcome(0x87)]).all_granted());
        assert!(Subscriptions::default().all_granted());
    }

    #[test]
    fn a_message_debug_never_shows_its_payload() {
        let message = Message {
            topic: "a/b".to_owned(),
            payload: Bytes::from_static(b"payload-text"),
            qos: 1,
            retain: false,
        };
        let debug = format!("{:?}", Event::Message(message));
        assert!(!debug.contains("payload-text"), "{debug}");
        assert!(debug.contains("payload_bytes: 12"), "{debug}");
    }

    #[test]
    fn the_packet_limit_is_the_protocol_minimum() {
        assert_eq!(
            usize::try_from(MAX_PACKET_BYTES).unwrap(),
            protocol::topic::MIN_MAX_PACKET_BYTES
        );
    }
}
