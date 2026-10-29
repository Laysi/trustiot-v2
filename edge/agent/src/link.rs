//! The agent's link to the cloud: one MQTT 5 connection at a time, reconnected forever.
//!
//! [`link`] returns three things:
//!
//! - [`Link`], a cloneable handle: [`Link::publish`] sends a message on an `up` channel with the
//!   channel's QoS and retain flag ([`protocol::topic::Channel::delivery`]) and returns the
//!   outcome: written (QoS 0), acknowledged, or refused with the broker's PUBACK reason code
//!   (0x80 and above, for example 0x87 not authorized). It never waits for room in the queue:
//!   a full queue is [`PublishError::Busy`]. A caller that wants to wait first takes a
//!   [`PublishPermit`] from [`Link::reserve`], which waits without holding any payload.
//!   Publishes are accepted only while the link is [`LinkState::Connected`]: while it connects,
//!   sets up a connection (CONNACK, `online`, SUBACK), waits to reconnect or stops, they fail
//!   at once. [`Link::state`] watches the connection.
//! - [`Incoming`], the messages the cloud sends on the `down` channels the link subscribes to
//!   on every connection ([`LinkSettings::subscriptions`]: only channels the application
//!   handles, since a message is acknowledged once it is queued here).
//! - [`Supervisor`], the future that connects, runs the connection and reconnects with
//!   exponential backoff ([`crate::backoff`]) until the shutdown token is cancelled.
//!
//! Presence is the link's own: on every connection it creates a new [`SessionId`], registers
//! [`protocol::health::Status::last_will`] for it as the will, publishes the same session's `online` before any
//! other message, and on shutdown publishes `offline`/`shutdown` and disconnects normally, so
//! the broker discards the will. [`Link::publish`] therefore refuses `up/status`.
//!
//! Memory is bounded per connection: at most `inflight_limit` QoS 1 publishes wait for a
//! PUBACK (and never more than the broker's Receive Maximum), keeping no payload; further
//! messages wait in two queues (QoS 0 and QoS 1) of [`REQUEST_QUEUE`] requests each. Nothing
//! else holds a payload for the link: [`Link::publish`] refuses with [`PublishError::Busy`]
//! when its queue is full, and callers waiting in [`Link::reserve`] hold only their place.
//! Incoming messages are limited to the protocol's largest packet
//! ([`protocol::topic::MIN_MAX_PACKET_BYTES`], announced as Maximum Packet Size) and to
//! [`INCOMING_QUEUE`] undelivered messages (announced as Receive Maximum); when the queue is
//! full the connection stops reading until the application takes a message.
//!
//! The link does not buffer or resend: a publish that fails with [`PublishError::NotConnected`]
//! or [`PublishError::ConnectionLost`] is the caller's to retry (the offline buffer of
//! Laysi/trustiot-v2#90). The queues belong to one connection: each connection creates its own
//! when it becomes Connected, and when it ends every request still in them is answered
//! ([`PublishError::NotConnected`], or [`PublishError::Stopped`] when the link stops), so a
//! message is never carried over to a later connection. Every connection starts a clean MQTT session (Clean Start, Session
//! Expiry Interval 0), so nothing queues at the broker while the gateway is offline.

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use protocol::topic::{Channel, Direction};
use protocol::{AgentVersion, GatewayId, SessionId};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::backoff::{Backoff, BackoffSettings, STABLE_AFTER};
use crate::liveness::{Phase, Progress};
use crate::mqtt::connection::{self, Channels, Outcome, Params};
use crate::mqtt::packet::ReasonCode;

/// The MQTT keepalive: Cloudflare cuts a WebSocket that carries nothing for 125 seconds, so the
/// keepalive must stay well below that (decision 6).
pub const KEEP_ALIVE: Duration = Duration::from_secs(60);
/// How long opening a connection may take: TCP, both TLS handshakes (one of them signed by
/// the TPM, about 70 ms) and the WebSocket upgrade. Waiting for CONNACK afterwards has the
/// same limit again.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Publish requests that may wait for the connection, per queue (QoS 0 and QoS 1 each);
/// further callers get [`PublishError::Busy`] from [`Link::publish`] or wait for a place in
/// [`Link::reserve`].
pub const REQUEST_QUEUE: usize = 8;
/// Received messages the application has not taken yet; also the Receive Maximum the agent
/// announces.
pub const INCOMING_QUEUE: usize = 16;
/// The default and the bounds of the in-flight limit (cloud PoC 2026-10-09, MQTT broker item
/// 5: rumqttc's default preallocated about 13 MB per connection, a limit of 32 about 80 KB).
pub const DEFAULT_INFLIGHT_LIMIT: u16 = 32;

/// What the link needs to know.
#[derive(Clone, Debug)]
pub struct LinkSettings {
    /// The gateway: the client certificate's CN, used as the MQTT client ID and in topics.
    pub gateway: GatewayId,
    /// This agent's release, sent in `online` and heartbeats.
    pub agent_version: AgentVersion,
    /// QoS 1 publishes that may wait for a PUBACK at once.
    pub inflight_limit: u16,
    /// Reconnect delays.
    pub reconnect: BackoffSettings,
    /// The MQTT keepalive; [`KEEP_ALIVE`] outside tests.
    pub keep_alive: Duration,
    /// The limit of opening a connection, and then of waiting for CONNACK;
    /// [`CONNECT_TIMEOUT`] outside tests.
    pub connect_timeout: Duration,
    /// The source of the backoff jitter: a uniformly random `u64` from the OS outside tests;
    /// tests fix it to see the delays.
    pub jitter: fn() -> u64,
    /// The `down` channels to subscribe to on every connection: only those the application
    /// takes from [`Incoming`] and handles.
    pub subscriptions: Subscriptions,
    /// The version of the client credentials the connector presents, if they can change while
    /// the link runs (certificate renewal, [`crate::renewal`]): it grows each time new
    /// credentials are in place for the next connection. A connection opened with an older
    /// version finishes what it has accepted and reconnects at once ([`LinkError::Renewed`]);
    /// the version of a refused certificate is what [`Link::certificate_refused`] reports.
    /// `None`: the credentials never change (version 0).
    pub credentials: Option<watch::Receiver<u64>>,
}

impl LinkSettings {
    /// Settings with the default limits and no subscriptions.
    pub fn new(gateway: GatewayId, agent_version: AgentVersion) -> Self {
        Self {
            gateway,
            agent_version,
            inflight_limit: DEFAULT_INFLIGHT_LIMIT,
            reconnect: BackoffSettings::default(),
            keep_alive: KEEP_ALIVE,
            connect_timeout: CONNECT_TIMEOUT,
            jitter: random_u64,
            subscriptions: Subscriptions::none(),
            credentials: None,
        }
    }
}

/// How long connection attempts may keep failing at the inner mTLS or before CONNACK, while
/// TCP, the outer TLS and the WebSocket succeed (so the cloud is reachable), before the link
/// reports the certificate as refused although no attempt said so ([`Link::certificate_refused`]).
/// A broker that refuses a certificate sends a TLS alert, but the alert can be lost: the broker
/// closes the connection with the gateway's unread CONNECT in its buffer, which resets the
/// connection, and the relay may see the reset before the alert. Each run of failures adds a
/// random part of [`REFUSED_JITTER`], so gateways that lose the broker together do not all
/// renew at once.
pub const REFUSED_AFTER: Duration = Duration::from_secs(600);

/// The most that is added at random to [`REFUSED_AFTER`].
pub const REFUSED_JITTER: Duration = Duration::from_secs(300);

/// A refusal of the client certificate, as [`Link::certificate_refused`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The credentials version ([`LinkSettings::credentials`]) the refused attempt presented.
    pub credentials: u64,
    /// `true` when the broker said so: a certificate alert in the inner mTLS handshake or
    /// stream, or CONNACK 0x86 or 0x87. `false` when it is inferred from failures at the inner
    /// mTLS that lasted [`REFUSED_AFTER`] (plus its jitter).
    pub explicit: bool,
}

/// A connection attempt: its presence session and the credentials version it presents (at
/// least this one: renewal puts new credentials in place before it raises the version, and the
/// attempt reads them at its inner handshake), as [`Link::attempts`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The session the attempt announces once it is Connected ([`LinkState::Connected`]).
    pub session: SessionId,
    /// The credentials version ([`LinkSettings::credentials`]).
    pub credentials: u64,
}

/// The `down` channels the link subscribes to, each at most once, in the order given. The
/// connection acknowledges a message (PUBACK) once it is in the incoming queue, before anyone
/// handles it, so a channel belongs here only when the application has a consumer for it: a
/// message on a channel nobody handles would be acknowledged and lost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subscriptions(Vec<Channel>);

/// Why a list of channels is no [`Subscriptions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubscriptionsError {
    /// The gateway receives only on `down` channels.
    #[error("{0:?} is not a down channel")]
    NotDown(Channel),
    /// A channel listed twice.
    #[error("{0:?} is listed twice")]
    Duplicate(Channel),
}

impl Subscriptions {
    /// No subscriptions: the link sends no SUBSCRIBE.
    pub const fn none() -> Self {
        Self(Vec::new())
    }

    /// Subscriptions to `channels`.
    ///
    /// # Errors
    ///
    /// A channel that is not a `down` channel, or one listed twice.
    pub fn new(channels: impl IntoIterator<Item = Channel>) -> Result<Self, SubscriptionsError> {
        let mut list = Vec::new();
        for channel in channels {
            if channel.direction() != Direction::Down {
                return Err(SubscriptionsError::NotDown(channel));
            }
            if list.contains(&channel) {
                return Err(SubscriptionsError::Duplicate(channel));
            }
            list.push(channel);
        }
        Ok(Self(list))
    }

    /// The channels, in order.
    pub fn channels(&self) -> &[Channel] {
        &self.0
    }

    /// Whether `channel` is one of them.
    pub fn contains(&self, channel: Channel) -> bool {
        self.0.contains(&channel)
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Opens the byte stream an MQTT connection runs over. The agent's connector is
/// [`crate::transport::WssConnector`] (TCP, TLS, WebSocket, then mTLS); tests use others.
pub trait Connector: Send + Sync + 'static {
    /// The stream; the MQTT bytes go through it unchanged.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Opens one connection.
    fn connect(&self) -> impl Future<Output = Result<Self::Stream, ConnectError>> + Send;
}

/// Which step of opening a connection failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectStage {
    /// Resolving the gateway host name.
    Resolve,
    /// The TCP connection.
    Tcp,
    /// The outer TLS handshake (to Cloudflare or the on-prem ingress).
    OuterTls,
    /// The WebSocket upgrade.
    WebSocket,
    /// The inner mTLS handshake with the broker.
    InnerTls,
}

impl fmt::Display for ConnectStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Resolve => "resolving the host",
            Self::Tcp => "TCP",
            Self::OuterTls => "outer TLS",
            Self::WebSocket => "WebSocket upgrade",
            Self::InnerTls => "inner mTLS",
        })
    }
}

/// A connection attempt that failed before MQTT.
#[derive(Debug, thiserror::Error)]
#[error("{stage}: {source}")]
pub struct ConnectError {
    /// The step that failed.
    pub stage: ConnectStage,
    /// Why.
    #[source]
    pub source: Box<dyn StdError + Send + Sync>,
}

impl ConnectError {
    /// A failure at `stage`.
    pub fn new(stage: ConnectStage, source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }
}

/// Why a connection ended or never came up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// Opening the stream failed.
    #[error("connecting failed at {0}")]
    Connect(#[from] ConnectError),
    /// A step did not finish in time.
    #[error("timed out after {after:?} waiting for {what}")]
    Timeout {
        /// What was awaited.
        what: &'static str,
        /// How long.
        after: Duration,
    },
    /// Reading or writing failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The stream ended without a DISCONNECT.
    #[error("the connection was closed without a DISCONNECT")]
    Closed,
    /// CONNACK refused the connection.
    #[error("the broker refused the connection: {reason}{}", detail(.reason_string))]
    Refused {
        /// The CONNACK reason code.
        reason: ReasonCode,
        /// The broker's explanation, if any.
        reason_string: Option<String>,
    },
    /// The broker sent DISCONNECT.
    #[error("the broker disconnected: {reason}{}", detail(.reason_string))]
    Disconnected {
        /// The DISCONNECT reason code.
        reason: ReasonCode,
        /// The broker's explanation, if any.
        reason_string: Option<String>,
    },
    /// The broker broke the protocol; the agent logged the reason code MQTT 5 has for it and
    /// disconnected with 0x04, so that the broker publishes the will.
    #[error("{0}")]
    Protocol(String),
    /// No PINGRESP arrived in time.
    #[error("no PINGRESP within {0:?} of PINGREQ")]
    KeepAlive(Duration),
    /// The broker refused the `online` status (for example 0x87 when the gateway's role is
    /// missing `up/status`), so the cloud would never see the gateway online.
    #[error("the broker refused the online status: {0}")]
    OnlineRefused(ReasonCode),
    /// The broker refused the subscription to one of the gateway's `down` channels (for
    /// example 0x87 when the gateway's role lacks it), so messages on it would never arrive.
    #[error("the broker refused the subscription to {topic}: {reason}")]
    SubscriptionRefused {
        /// The topic filter.
        topic: String,
        /// The SUBACK reason code.
        reason: ReasonCode,
    },
    /// The broker granted only QoS 0 for one of the gateway's `down` channels, which need
    /// QoS 1: acknowledged delivery within the agent's Receive Maximum.
    #[error("the broker granted only QoS 0 for {topic}, which needs QoS 1")]
    SubscriptionDowngraded {
        /// The topic filter.
        topic: String,
    },
    /// A packet the connection cannot do without (a status, SUBSCRIBE, PUBACK or PINGREQ) is
    /// larger than the broker's Maximum Packet Size, which a client must not exceed
    /// (MQTT-3.2.2-15). The agent did not send it and ended the connection with the will.
    #[error("a {size}-byte packet is above the broker's Maximum Packet Size of {limit} bytes")]
    PacketTooLarge {
        /// The packet's size in bytes.
        size: usize,
        /// The broker's Maximum Packet Size.
        limit: usize,
    },
    /// A bug in the agent, such as a status payload that does not encode.
    #[error("internal error: {0}")]
    Internal(String),
    /// New client credentials are in place ([`LinkSettings::credentials`]): the connection
    /// finished what it had accepted and ended its session like a clean stop
    /// (`offline`/`shutdown`, DISCONNECT 0x00; with the will when that did not finish in
    /// time), and the link reconnects at once with them.
    #[error("the client certificate was renewed; reconnecting with it")]
    Renewed,
}

/// TLS alerts with which a server refuses a client certificate.
fn refuses_the_certificate(alert: rustls::AlertDescription) -> bool {
    use rustls::AlertDescription as Alert;
    matches!(
        alert,
        Alert::BadCertificate
            | Alert::UnsupportedCertificate
            | Alert::CertificateRevoked
            | Alert::CertificateExpired
            | Alert::CertificateUnknown
            | Alert::UnknownCA
            | Alert::AccessDenied
            | Alert::CertificateRequired
    )
}

/// Whether `error` or one it wraps is a TLS alert from the peer refusing the client
/// certificate. tokio-rustls hands rustls' errors over inside `io::Error`, whose `source` skips
/// the wrapped error itself, so those are unwrapped with `get_ref`.
fn certificate_alert(error: &(dyn StdError + 'static)) -> bool {
    let mut next = Some(error);
    // A chain of wrapped errors is short; the bound only guards against a cycle.
    for _ in 0..16 {
        let Some(error) = next else {
            return false;
        };
        if let Some(rustls::Error::AlertReceived(alert)) = error.downcast_ref::<rustls::Error>() {
            return refuses_the_certificate(*alert);
        }
        next = match error.downcast_ref::<std::io::Error>() {
            Some(io) => match io.get_ref() {
                Some(inner) => Some(inner as &(dyn StdError + 'static)),
                None => error.source(),
            },
            None => error.source(),
        };
    }
    false
}

/// What a failed connection attempt says about the client certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blame {
    /// The broker refused it: a TLS alert about the certificate from the inner mTLS (in its
    /// handshake, or in the stream it protects), or CONNACK 0x86 or 0x87 (the
    /// broker's dynamic security no longer knows the gateway, which happens once it has no
    /// valid certificate left; services/certs/README.md, 「broker 權限」).
    Certificate,
    /// It failed at the inner mTLS or before CONNACK, without saying why: the cloud was
    /// reachable, so it may be the certificate.
    Inner,
    /// Anything else: the network, the outer TLS (alerts included: before the outer handshake
    /// is done they are not authenticated) or the WebSocket, or a connection that came up.
    Elsewhere,
}

impl Blame {
    /// The blame for `error`, which ended an attempt that never became Connected.
    fn of(error: &LinkError) -> Self {
        match error {
            LinkError::Connect(connect) if connect.stage == ConnectStage::InnerTls => {
                if certificate_alert(&*connect.source) {
                    Self::Certificate
                } else {
                    Self::Inner
                }
            }
            LinkError::Connect(_) => Self::Elsewhere,
            LinkError::Io(io) if certificate_alert(io) => Self::Certificate,
            LinkError::Refused { reason, .. } if matches!(reason.0, 0x86 | 0x87) => {
                Self::Certificate
            }
            LinkError::Io(_) | LinkError::Closed => Self::Inner,
            LinkError::Timeout { what, .. } if *what == "CONNACK" => Self::Inner,
            _ => Self::Elsewhere,
        }
    }
}

fn detail(reason_string: &Option<String>) -> String {
    match reason_string {
        // A reason string comes from the broker; keep the log line short.
        Some(text) => format!(": {}", printable(text, 200)),
        None => String::new(),
    }
}

/// The first `limit` characters of `text` with every control character (C0, DEL, C1: line
/// breaks, tabs, escape sequences) replaced by a space, so text from the broker cannot forge or
/// split log lines (the log formatter does not escape them).
pub(crate) fn printable(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The state of the link.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkState {
    /// A connection attempt is running.
    Connecting,
    /// Connected: the broker acknowledged `online` for `session` (the first message) and
    /// granted every requested subscription at QoS 1 (SUBACK; with no subscriptions there is no
    /// SUBSCRIBE). Reported only then.
    Connected {
        /// The presence session of this connection.
        session: SessionId,
    },
    /// The last attempt or connection failed; the next attempt starts after `retry_in`.
    Disconnected {
        /// What went wrong.
        error: String,
        /// The backoff delay.
        retry_in: Duration,
    },
    /// The supervisor has stopped (shutdown).
    Stopped,
}

/// The outcome of a successful [`Link::publish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Published {
    /// QoS 0: written to the connection; there is no acknowledgement.
    Sent,
    /// QoS 1: acknowledged with a success code (0x00, or 0x10 no matching subscribers).
    Acknowledged(ReasonCode),
}

/// Why [`Link::publish`] did not deliver a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// Only `up` channels can be published by the gateway.
    #[error("{0:?} is not an up channel")]
    NotUp(Channel),
    /// `up/status` belongs to the link's presence handling.
    #[error("up/status is published by the link itself")]
    StatusIsReserved,
    /// Larger than the channel's limit or the broker's Maximum Packet Size.
    #[error("{size} bytes is above the limit of {limit} bytes")]
    TooLarge {
        /// The payload or packet size.
        size: usize,
        /// The limit it broke.
        limit: usize,
    },
    /// No connection takes messages right now (the link is connecting, setting a connection
    /// up, or waiting to reconnect), or the connection the message was accepted for ended
    /// before writing it; nothing was sent.
    #[error("not connected")]
    NotConnected,
    /// The connection ended after the message was handed to it: it may or may not have
    /// reached the broker.
    #[error("the connection ended before a PUBACK")]
    ConnectionLost,
    /// The broker answered with a failure reason code (0x80 and above).
    #[error("the broker refused the message: {reason}{}", detail(.reason_string))]
    Refused {
        /// The PUBACK reason code.
        reason: ReasonCode,
        /// The broker's explanation, if any.
        reason_string: Option<String>,
    },
    /// The queue for the channel's QoS is full; nothing was queued. Retry later, or wait for
    /// room with [`Link::reserve`].
    #[error("the publish queue is full")]
    Busy,
    /// The link is stopping or has stopped, and the message was not written; nothing was sent.
    /// A message written but not acknowledged when the link stops is
    /// [`PublishError::ConnectionLost`].
    #[error("the link has stopped")]
    Stopped,
}

/// A message the cloud sent on one of the gateway's `down` channels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownMessage {
    /// The channel, from the topic (the topic's gateway is this gateway).
    pub channel: Channel,
    /// The payload, at most the channel's limit.
    pub payload: Vec<u8>,
    /// Whether the broker sent it from its retained store (for example `down/config` at
    /// subscribe time).
    pub retained: bool,
}

type PublishOutcome = Result<Published, PublishError>;

/// The answer to one publish request. Once the connection has taken the request to write it
/// ([`Reply::taken`]), dropping the answer unsent reports [`PublishError::ConnectionLost`]:
/// the message may have been written, also when the connection's state is dropped without
/// running to its end (the supervisor future dropped).
pub(crate) struct Reply {
    sender: Option<oneshot::Sender<PublishOutcome>>,
    lost_on_drop: bool,
}

impl Reply {
    fn new(sender: oneshot::Sender<PublishOutcome>) -> Self {
        Self {
            sender: Some(sender),
            lost_on_drop: false,
        }
    }

    /// Marks the request as taken for writing.
    pub(crate) fn taken(mut self) -> Self {
        self.lost_on_drop = true;
        self
    }

    /// Answers the request; `Err` when the caller has stopped waiting.
    pub(crate) fn send(mut self, outcome: PublishOutcome) -> Result<(), PublishOutcome> {
        match self.sender.take() {
            Some(sender) => sender.send(outcome),
            None => Err(outcome),
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if self.lost_on_drop
            && let Some(sender) = self.sender.take()
        {
            let _ = sender.send(Err(PublishError::ConnectionLost));
        }
    }
}

/// One publish request on its way to the connection.
pub(crate) struct Request {
    pub(crate) channel: Channel,
    pub(crate) payload: Vec<u8>,
    pub(crate) reply: Reply,
}

/// One connection's two publish queues. QoS 0 messages (heartbeats, live data) never wait
/// behind QoS 1 messages that wait for room in the in-flight window.
pub(crate) struct Requests {
    /// QoS 0 requests; taken whenever the connection is up.
    pub(crate) at_most_once: mpsc::Receiver<Request>,
    /// QoS 1 requests; taken only while the in-flight window has room.
    pub(crate) at_least_once: mpsc::Receiver<Request>,
}

impl Requests {
    /// New queues for the connection of `session`, and the senders that [`Link`] uses once
    /// the connection is Connected.
    pub(crate) fn new(session: SessionId) -> (Accepting, Self) {
        let (at_most_once, at_most_once_requests) = mpsc::channel(REQUEST_QUEUE);
        let (at_least_once, at_least_once_requests) = mpsc::channel(REQUEST_QUEUE);
        (
            Accepting {
                session,
                at_most_once,
                at_least_once,
            },
            Self {
                at_most_once: at_most_once_requests,
                at_least_once: at_least_once_requests,
            },
        )
    }

    /// Closes both queues and answers every request still in them with `error`.
    pub(crate) fn close(&mut self, error: &PublishError) {
        for queue in [&mut self.at_most_once, &mut self.at_least_once] {
            queue.close();
            while let Ok(request) = queue.try_recv() {
                let _ = request.reply.send(Err(error.clone()));
            }
        }
    }
}

/// The queues of the connection that takes publishes, and its session.
pub(crate) struct Accepting {
    session: SessionId,
    at_most_once: mpsc::Sender<Request>,
    at_least_once: mpsc::Sender<Request>,
}

/// The connection that takes publishes: `Some` only from the moment the connection is
/// Connected until it ends.
#[derive(Default)]
pub(crate) struct Current(Mutex<Option<Accepting>>);

impl Current {
    fn lock(&self) -> MutexGuard<'_, Option<Accepting>> {
        // The lock is never held across an await or a panic point that leaves the slot broken.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `accepting` the connection that takes publishes.
    pub(crate) fn install(&self, accepting: Accepting) {
        *self.lock() = Some(accepting);
    }

    /// Takes publishes away from the connection of `session`, if it still has them.
    pub(crate) fn withdraw(&self, session: &SessionId) {
        let mut current = self.lock();
        if current
            .as_ref()
            .is_some_and(|accepting| accepting.session == *session)
        {
            *current = None;
        }
    }
}

/// Counters shared between the connection and [`Link`].
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// QoS 1 publishes (including the link's own status) waiting for a PUBACK.
    pub(crate) in_flight: AtomicUsize,
}

/// A handle for publishing; cheap to clone.
#[derive(Clone)]
pub struct Link {
    current: Arc<Current>,
    state: watch::Receiver<LinkState>,
    refused: watch::Receiver<Option<Refusal>>,
    attempts: watch::Receiver<Option<Attempt>>,
    counters: Arc<Counters>,
    shutdown: CancellationToken,
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("state", &*self.state.borrow())
            .field("in_flight", &self.in_flight())
            .finish_non_exhaustive()
    }
}

impl Link {
    /// Publishes `payload` on `channel` with the channel's QoS and retain flag, and waits for
    /// the outcome. Never waits for room: when the queue for the channel's QoS is full it fails
    /// at once with [`PublishError::Busy`], so a waiting caller never holds a payload the link
    /// does not count. Accepted only while the link is [`LinkState::Connected`]: otherwise it
    /// fails at once with [`PublishError::NotConnected`] (connecting, setting up, waiting to
    /// reconnect) or [`PublishError::Stopped`] (stopping, stopped). A message is written only
    /// on the connection that accepted it.
    ///
    /// # Errors
    ///
    /// See [`PublishError`]. A refusal (PUBACK reason code 0x80 and above) is
    /// [`PublishError::Refused`], never a success.
    pub async fn publish(
        &self,
        channel: Channel,
        payload: Vec<u8>,
    ) -> Result<Published, PublishError> {
        self.publish_on(None, channel, payload).await
    }

    /// Like [`Link::publish`], but only on the connection of `session`: once that connection
    /// has ended it fails with [`PublishError::NotConnected`], even while a later connection
    /// is up. For messages that name their session, such as heartbeats.
    ///
    /// # Errors
    ///
    /// As [`Link::publish`].
    pub async fn publish_in_session(
        &self,
        session: &SessionId,
        channel: Channel,
        payload: Vec<u8>,
    ) -> Result<Published, PublishError> {
        self.publish_on(Some(session), channel, payload).await
    }

    async fn publish_on(
        &self,
        session: Option<&SessionId>,
        channel: Channel,
        payload: Vec<u8>,
    ) -> Result<Published, PublishError> {
        check_channel(channel)?;
        check_size(channel, &payload)?;
        let permit =
            self.queue(channel, session)?
                .try_reserve_owned()
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => PublishError::Busy,
                    mpsc::error::TrySendError::Closed(_) => self.no_connection(),
                })?;
        self.send(permit, channel, payload).await
    }

    /// Waits for room in the queue of `channel`'s QoS and returns a permit that publishes one
    /// message on `channel` without waiting again. Waiting holds no payload: the caller builds
    /// the message once the permit is there. Waiters get their permits in the order they asked.
    /// Dropping a permit unused gives its place back. The place is in the queue of the
    /// connection that was Connected when `reserve` was called; when that connection ends
    /// first, the wait ends with [`PublishError::NotConnected`] ([`PublishError::Stopped`]
    /// when the link stops).
    ///
    /// # Errors
    ///
    /// [`PublishError::NotUp`] or [`PublishError::StatusIsReserved`] for a channel the gateway
    /// cannot publish; then, as [`Link::publish`], [`PublishError::NotConnected`] or
    /// [`PublishError::Stopped`] at once while the link is not Connected.
    pub async fn reserve(&self, channel: Channel) -> Result<PublishPermit, PublishError> {
        check_channel(channel)?;
        let permit = self
            .queue(channel, None)?
            .reserve_owned()
            .await
            .map_err(|_| self.no_connection())?;
        Ok(PublishPermit {
            permit,
            channel,
            link: self.clone(),
        })
    }

    /// The queue for `channel`'s QoS of the connection that takes publishes now (and is the
    /// connection of `session`, if given).
    fn queue(
        &self,
        channel: Channel,
        session: Option<&SessionId>,
    ) -> Result<mpsc::Sender<Request>, PublishError> {
        if self.shutdown.is_cancelled() {
            return Err(PublishError::Stopped);
        }
        let current = self.current.lock();
        match current.as_ref() {
            Some(accepting) if session.is_none_or(|session| *session == accepting.session) => {
                Ok(if channel.delivery().qos.level() == 0 {
                    accepting.at_most_once.clone()
                } else {
                    accepting.at_least_once.clone()
                })
            }
            _ => Err(self.no_connection()),
        }
    }

    /// Why no connection takes a message: the link is stopping or has stopped (the supervisor
    /// gone included), or there is no connection right now.
    fn no_connection(&self) -> PublishError {
        let stopped = self.shutdown.is_cancelled()
            || self.state.has_changed().is_err()
            || matches!(*self.state.borrow(), LinkState::Stopped);
        if stopped {
            PublishError::Stopped
        } else {
            PublishError::NotConnected
        }
    }

    /// Puts the request in the reserved place (no waiting) and waits for its outcome. A request
    /// that its connection's queue never answers (the queue closed, or dropped with the
    /// supervisor) was not written: [`Link::no_connection`] says why.
    async fn send(
        &self,
        permit: mpsc::OwnedPermit<Request>,
        channel: Channel,
        payload: Vec<u8>,
    ) -> Result<Published, PublishError> {
        let (reply, mut outcome) = oneshot::channel();
        let queue = permit.send(Request {
            channel,
            payload,
            reply: Reply::new(reply),
        });
        tokio::select! {
            biased;
            outcome = &mut outcome => match outcome {
                Ok(result) => result,
                Err(_) => Err(self.no_connection()),
            },
            () = queue.closed() => Err(self.no_connection()),
        }
    }

    /// The connection state, to wait for changes.
    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
    }

    /// The last connection attempt the broker refused for its client certificate: a TLS alert
    /// about the certificate in the inner mTLS (never one in the outer TLS, which anyone on the
    /// path can send before the outer handshake is done), CONNACK 0x86 or 0x87, or attempts that
    /// kept failing at the inner mTLS or before CONNACK for [`REFUSED_AFTER`] (plus jitter) while
    /// the cloud was reachable. `None` until then. Every such attempt sends the value again, so a
    /// watcher wakes for each one. Certificate renewal watches it to renew a certificate the
    /// broker no longer takes (revoked, or expired by the broker's clock).
    pub fn certificate_refused(&self) -> watch::Receiver<Option<Refusal>> {
        self.refused.clone()
    }

    /// The latest connection attempt that got as far as the MQTT session: set before the
    /// attempt can report [`LinkState::Connected`] for its session, so a watcher of
    /// [`Link::state`] can tell which credentials a connected session presents. Certificate
    /// renewal reads it to count only connections made with the certificate in use.
    pub fn attempts(&self) -> watch::Receiver<Option<Attempt>> {
        self.attempts.clone()
    }

    /// QoS 1 publishes waiting for a PUBACK right now (never above the in-flight limit).
    pub fn in_flight(&self) -> usize {
        self.counters.in_flight.load(Ordering::Relaxed)
    }

    /// Publish requests waiting in the current connection's queues right now, places reserved
    /// with [`Link::reserve`] included (never above [`REQUEST_QUEUE`] per queue; 0 while no
    /// connection takes publishes).
    pub fn queued(&self) -> usize {
        self.current.lock().as_ref().map_or(0, |accepting| {
            [&accepting.at_most_once, &accepting.at_least_once]
                .iter()
                .map(|queue| queue.max_capacity() - queue.capacity())
                .sum()
        })
    }
}

/// A place in one connection's publish queue for one message on one channel, from
/// [`Link::reserve`].
#[derive(Debug)]
pub struct PublishPermit {
    permit: mpsc::OwnedPermit<Request>,
    channel: Channel,
    link: Link,
}

impl PublishPermit {
    /// The channel this permit publishes on.
    #[must_use]
    pub const fn channel(&self) -> Channel {
        self.channel
    }

    /// Publishes `payload` in the reserved place, like [`Link::publish`] but without the
    /// chance of [`PublishError::Busy`]. The message goes only to the connection whose queue
    /// holds the place.
    ///
    /// # Errors
    ///
    /// See [`Link::publish`]; a payload above the channel's limit is refused and the place is
    /// given back. A permit used after its connection ended fails with
    /// [`PublishError::NotConnected`], after the link began to stop with
    /// [`PublishError::Stopped`]; nothing is sent.
    pub async fn publish(self, payload: Vec<u8>) -> Result<Published, PublishError> {
        check_size(self.channel, &payload)?;
        if self.link.shutdown.is_cancelled() {
            return Err(PublishError::Stopped);
        }
        self.link.send(self.permit, self.channel, payload).await
    }
}

/// Only `up` channels other than `up/status` (the link's own) can be published.
fn check_channel(channel: Channel) -> Result<(), PublishError> {
    if channel.direction() != Direction::Up {
        return Err(PublishError::NotUp(channel));
    }
    if channel == Channel::Status {
        return Err(PublishError::StatusIsReserved);
    }
    Ok(())
}

fn check_size(channel: Channel, payload: &[u8]) -> Result<(), PublishError> {
    let limit = channel.max_payload_bytes();
    if payload.len() > limit {
        return Err(PublishError::TooLarge {
            size: payload.len(),
            limit,
        });
    }
    Ok(())
}

/// The messages the cloud sends this gateway on the channels of
/// [`LinkSettings::subscriptions`]. Take them promptly: while a message waits for room the
/// connection does not read, and one that waits longer than
/// [`crate::liveness::DELIVERY_LIMIT`] counts as a stuck agent, which the systemd watchdog
/// restarts.
#[derive(Debug)]
pub struct Incoming {
    receiver: mpsc::Receiver<DownMessage>,
}

impl Incoming {
    /// The next message; `None` once the supervisor has stopped and every message was taken.
    pub async fn recv(&mut self) -> Option<DownMessage> {
        self.receiver.recv().await
    }
}

/// Runs the link: connects, reconnects with backoff, and stops when the shutdown token is
/// cancelled (after a clean disconnect when connected).
pub struct Supervisor<C> {
    settings: LinkSettings,
    connector: C,
    progress: Arc<Progress>,
    current: Arc<Current>,
    incoming: mpsc::Sender<DownMessage>,
    state: watch::Sender<LinkState>,
    refused: watch::Sender<Option<Refusal>>,
    attempts: watch::Sender<Option<Attempt>>,
    counters: Arc<Counters>,
    shutdown: CancellationToken,
}

impl<C> fmt::Debug for Supervisor<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("gateway", &self.settings.gateway)
            .finish_non_exhaustive()
    }
}

/// Creates a link; nothing happens until [`Supervisor::run`] is awaited.
pub fn link<C: Connector>(
    settings: LinkSettings,
    connector: C,
    shutdown: CancellationToken,
) -> (Link, Incoming, Supervisor<C>) {
    let (incoming_sender, incoming) = mpsc::channel(INCOMING_QUEUE);
    let (state_sender, state) = watch::channel(LinkState::Connecting);
    let (refused_sender, refused) = watch::channel(None);
    let (attempts_sender, attempts) = watch::channel(None);
    let counters = Arc::new(Counters::default());
    let current = Arc::new(Current::default());
    let link = Link {
        current: Arc::clone(&current),
        state,
        refused,
        attempts,
        counters: Arc::clone(&counters),
        shutdown: shutdown.clone(),
    };
    let supervisor = Supervisor {
        settings,
        connector,
        progress: Arc::new(Progress::default()),
        current,
        incoming: incoming_sender,
        state: state_sender,
        refused: refused_sender,
        attempts: attempts_sender,
        counters,
        shutdown,
    };
    (link, Incoming { receiver: incoming }, supervisor)
}

impl<C: Connector> Supervisor<C> {
    /// What the connection task waits for, and until when, for the systemd watchdog: every
    /// wait of [`Supervisor::run`] is declared there before it starts.
    pub fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }

    /// Connects and keeps the gateway connected until shutdown. Never gives up: every failure,
    /// including a refused CONNACK or a failed TLS handshake, waits for the next backoff delay.
    pub async fn run(self) {
        let mut backoff = Backoff::new(self.settings.reconnect);
        // Since when attempts have failed in a row without blaming anything but the inner mTLS
        // or the broker ([`Blame`]), and how long such failures may last before they count as a
        // refusal; `None` after any other end, and with new credentials.
        let mut inner_failures_since: Option<(tokio::time::Instant, Duration)> = None;
        let mut last_version = 0;
        loop {
            self.state.send_replace(LinkState::Connecting);
            // The credentials this attempt presents: at least this version, since renewal puts
            // new credentials in place before it raises the version.
            let mut credentials = self.settings.credentials.clone();
            let version = credentials
                .as_mut()
                .map_or(0, |credentials| *credentials.borrow_and_update());
            if version != last_version {
                // New credentials get their own REFUSED_AFTER before the failures blame them.
                inner_failures_since = None;
                last_version = version;
            }
            // A TPM signature in the inner handshake blocks this thread, so the timeout below
            // may fire up to one TPM response timeout late.
            self.progress.enter(
                Phase::Connecting,
                tokio::time::Instant::now()
                    .checked_add(self.settings.connect_timeout)
                    .and_then(|at| at.checked_add(crate::tpm::RESPONSE_TIMEOUT)),
            );
            let attempt =
                tokio::time::timeout(self.settings.connect_timeout, self.connector.connect());
            let Some(result) = unless_stopped(&self.shutdown, attempt).await else {
                break;
            };
            // The error, and whether the attempt ever became Connected.
            let (error, connected) = match result {
                Err(_) => (
                    LinkError::Timeout {
                        what: "the connection to the cloud",
                        after: self.settings.connect_timeout,
                    },
                    false,
                ),
                Ok(Err(error)) => (LinkError::Connect(error), false),
                Ok(Ok(_)) if self.shutdown.is_cancelled() => break,
                Ok(Ok(stream)) => match new_session_id() {
                    Err(error) => (
                        LinkError::Io(std::io::Error::other(format!("session ID: {error}"))),
                        false,
                    ),
                    Ok(session) => {
                        self.attempts.send_replace(Some(Attempt {
                            session: session.clone(),
                            credentials: version,
                        }));
                        let ended = connection::run(
                            stream,
                            Params {
                                gateway: &self.settings.gateway,
                                agent_version: &self.settings.agent_version,
                                session,
                                keep_alive: self.settings.keep_alive,
                                inflight_limit: self.settings.inflight_limit,
                                connack_timeout: self.settings.connect_timeout,
                                subscriptions: &self.settings.subscriptions,
                            },
                            Channels {
                                progress: &self.progress,
                                current: &self.current,
                                incoming: &self.incoming,
                                state: &self.state,
                                counters: &self.counters,
                                shutdown: &self.shutdown,
                                credentials,
                            },
                        )
                        .await;
                        match ended.outcome {
                            Outcome::Stopped => break,
                            Outcome::Lost(error) => {
                                if ended
                                    .connected_at
                                    .is_some_and(|at| at.elapsed() >= STABLE_AFTER)
                                {
                                    backoff.reset();
                                }
                                (error, ended.connected_at.is_some())
                            }
                        }
                    }
                },
            };
            if self.shutdown.is_cancelled() {
                break;
            }
            if matches!(error, LinkError::Renewed) {
                // A planned end: reconnect at once with the new credentials, no backoff.
                inner_failures_since = None;
                tracing::info!("reconnecting with the renewed client certificate");
                continue;
            }
            let blame = if connected {
                Blame::Elsewhere
            } else {
                Blame::of(&error)
            };
            self.report_refusal(blame, version, &mut inner_failures_since);
            let delay = backoff.next_delay((self.settings.jitter)());
            tracing::warn!(
                error = %error,
                retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                failures = backoff.failures(),
                "the connection to the cloud failed"
            );
            self.state.send_replace(LinkState::Disconnected {
                error: error.to_string(),
                retry_in: delay,
            });
            self.progress.enter(
                Phase::Backoff,
                tokio::time::Instant::now().checked_add(delay),
            );
            if unless_stopped(&self.shutdown, tokio::time::sleep(delay))
                .await
                .is_none()
            {
                break;
            }
        }
        self.progress.enter(Phase::Stopped, None);
        self.state.send_replace(LinkState::Stopped);
        tracing::info!("the link to the cloud has stopped");
    }

    /// Tells [`Link::certificate_refused`] about an attempt with the credentials of `version`
    /// that ended with `blame`: at once when the broker refused the certificate, and for
    /// failures at the inner mTLS or before CONNACK once they have lasted [`REFUSED_AFTER`] and a
    /// random part of [`REFUSED_JITTER`].
    fn report_refusal(
        &self,
        blame: Blame,
        version: u64,
        inner_failures_since: &mut Option<(tokio::time::Instant, Duration)>,
    ) {
        let now = tokio::time::Instant::now();
        let jitter = self.settings.jitter;
        let mut streak = || {
            *inner_failures_since.get_or_insert_with(|| {
                // `jitter / 2^64` of REFUSED_JITTER: uniform, never above it.
                let extra = (REFUSED_JITTER.as_millis() * u128::from(jitter())) >> 64;
                let extra = Duration::from_millis(u64::try_from(extra).unwrap_or(0));
                (now, REFUSED_AFTER + extra)
            })
        };
        let explicit = match blame {
            Blame::Elsewhere => {
                *inner_failures_since = None;
                return;
            }
            Blame::Certificate => {
                streak();
                true
            }
            Blame::Inner => {
                let (since, after) = streak();
                if now.duration_since(since) < after {
                    return;
                }
                false
            }
        };
        if explicit {
            tracing::warn!(
                credentials = version,
                "the broker refuses the client certificate"
            );
        } else {
            tracing::warn!(
                credentials = version,
                "the inner mTLS keeps failing while the cloud is reachable; the certificate may be refused"
            );
        }
        self.refused.send_replace(Some(Refusal {
            credentials: version,
            explicit,
        }));
    }
}

/// Drives `future` (a connection attempt or a backoff delay) unless the shutdown token is
/// cancelled first (`None`). No connection takes publishes meanwhile, so [`Link::publish`]
/// fails at once.
async fn unless_stopped<F: Future>(shutdown: &CancellationToken, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => None,
        output = future => Some(output),
    }
}

/// A new presence session: 32 random hex digits (or, without randomness from the OS, the
/// clock in nanoseconds, which is still unique per connection).
fn new_session_id() -> Result<SessionId, protocol::IdError> {
    let mut bytes = [0_u8; 16];
    let id = if fill_random(&mut bytes) {
        bytes
            .iter()
            .fold(String::with_capacity(32), |mut text, byte| {
                text.push_str(&format!("{byte:02x}"));
                text
            })
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!("t{nanos:x}")
    };
    SessionId::new(id)
}

fn fill_random(bytes: &mut [u8]) -> bool {
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(bytes)
        .is_ok()
}

/// A uniformly random `u64` for backoff jitter; 0 (the shortest delay) if the OS has no
/// randomness, which still never loops faster than the backoff floor.
pub(crate) fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    if fill_random(&mut bytes) {
        u64::from_le_bytes(bytes)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_down_channels_each_once_in_order() {
        let subscriptions = Subscriptions::new([Channel::LogRequest, Channel::Config]).unwrap();
        assert_eq!(
            subscriptions.channels(),
            [Channel::LogRequest, Channel::Config]
        );
        assert!(subscriptions.contains(Channel::Config));
        assert!(!subscriptions.contains(Channel::LiveControl));
        assert_eq!(
            Subscriptions::new([Channel::Config, Channel::Telemetry]),
            Err(SubscriptionsError::NotDown(Channel::Telemetry))
        );
        assert_eq!(
            Subscriptions::new([Channel::Config, Channel::LogRequest, Channel::Config]),
            Err(SubscriptionsError::Duplicate(Channel::Config))
        );
        assert!(Subscriptions::none().is_empty());
        assert_eq!(Subscriptions::new([]), Ok(Subscriptions::none()));
        assert!(
            LinkSettings::new(
                GatewayId::new("gw-1").unwrap(),
                AgentVersion::new("0.1.0").unwrap()
            )
            .subscriptions
            .is_empty(),
            "nothing is subscribed unless asked for"
        );
    }

    #[test]
    fn session_ids_are_valid_and_differ() {
        let first = new_session_id().unwrap();
        let second = new_session_id().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.as_str().len(), 32);
        let nanos = format!("t{:x}", u128::MAX);
        assert!(
            SessionId::new(nanos).is_ok(),
            "the clock fallback is a valid ID"
        );
    }

    #[test]
    fn errors_name_the_reason_code() {
        let refused = PublishError::Refused {
            reason: ReasonCode(0x87),
            reason_string: None,
        };
        assert_eq!(
            refused.to_string(),
            "the broker refused the message: 0x87 (not authorized)"
        );
        let error = LinkError::Refused {
            reason: ReasonCode(0x86),
            reason_string: Some("bad".to_owned()),
        };
        assert_eq!(
            error.to_string(),
            "the broker refused the connection: 0x86 (bad user name or password): bad"
        );
    }
}
