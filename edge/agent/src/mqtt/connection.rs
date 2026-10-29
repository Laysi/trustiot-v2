//! One MQTT connection, from CONNECT to the end of the stream.
//!
//! In order: CONNECT (client ID = gateway ID, Clean Start, Session Expiry 0, the will
//! `offline`/`connection_lost` for this session, retained at QoS 1), CONNACK, the session's
//! retained `online` as the first PUBLISH, SUBSCRIBE to every `down` channel, and then a loop
//! that writes publish requests, reads packets, answers PUBLISH with PUBACK, sends PINGREQ and
//! detects a missing PINGRESP, and gives up on a PUBACK or SUBACK that takes longer than twice
//! the keepalive. Shutdown publishes `offline`/`shutdown`, waits for its PUBACK and
//! sends DISCONNECT 0x00, which makes the broker discard the will; if the PUBACK does not come
//! in [`STOP_ACK_TIMEOUT`], DISCONNECT 0x04 asks the broker to publish the will instead, so the
//! cloud still learns that the gateway is gone. A shutdown while CONNACK is awaited waits for
//! it within the same limit and then stops the same way. The limit counts from the shutdown and
//! bounds a write in progress as well (a large message on a slow uplink): a write cut short
//! leaves part of a packet on the stream, after which nothing else can be sent, so the
//! connection closes without DISCONNECT and the broker publishes the will.
//!
//! Every other end of a connection must leave the will published, or the retained `online`
//! would outlive the connection. Mosquitto publishes the will only after DISCONNECT 0x04 or a
//! connection closed without DISCONNECT, and drops it after DISCONNECT with any other reason
//! code (0x80 to 0x95 included), although MQTT 5 says otherwise (`tests/broker.rs`:
//! `which_ends_make_mosquitto_publish_the_will`). So the agent ends a failed connection with
//! DISCONNECT 0x04, whatever the failure (a protocol error, a refused subscription, a missing
//! acknowledgement), and logs the reason code MQTT 5 has for it; where it cannot write any more
//! it closes without DISCONNECT.
//!
//! The connection has its own publish queues, which [`crate::link::Link`] can reach only from
//! the moment the connection is Connected (`online` acknowledged, subscriptions granted) until
//! it ends; at the end every request still in them is answered, so none is carried to the next
//! connection. QoS 0 requests (heartbeats) are taken whenever the connection is up; QoS 1
//! requests only while the in-flight window has room, so a full window never holds back a
//! heartbeat.
//!
//! Every wait is declared to [`Progress`] before it starts, with the latest moment it may
//! legitimately end (a write's timeout, CONNACK's, the main loop's next timer, the stop's
//! deadline), so the systemd watchdog notices a wait that never ends.
//!
//! Reads and writes alternate in one task: a write runs to completion (bounded by a timeout, and
//! by the stop's deadline once a stop is requested) before the next read, so there is no second buffer of outgoing data. The broker never waits
//! for the client to read before it reads, so this cannot deadlock.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use protocol::health::{Offline, OfflineReason, Online, Status};
use protocol::topic::{Channel, Direction, MIN_MAX_PACKET_BYTES, parse_topic};
use protocol::{AgentVersion, GatewayId, PROTOCOL_REVISION, SessionId, V1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use super::packet::{self, DecodeError, Inbound, PublishHeader, ReasonCode, reason};
use crate::link::{
    Accepting, Counters, Current, DownMessage, INCOMING_QUEUE, LinkError, LinkState, PublishError,
    Published, Reply, Request, Requests, Subscriptions,
};
use crate::liveness::{DELIVERY_LIMIT, Phase, Progress};

/// The largest packet the agent accepts, announced as Maximum Packet Size: the protocol's
/// largest payload plus room for the topic and properties.
pub(crate) const MAX_INBOUND_PACKET: usize = MIN_MAX_PACKET_BYTES;
/// Bytes read from the stream at a time.
const READ_CHUNK: usize = 16 * 1024;
/// A read buffer that grew above this for a large packet shrinks back once it is processed.
const SHRINK_ABOVE: usize = 64 * 1024;
/// How long a stop may take from the shutdown: the CONNACK it may still wait for, writes in
/// progress and the PUBACK of `offline` all fit in it.
pub(crate) const STOP_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the closing DISCONNECT and the close of the stream may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// What one connection needs from the link.
pub(crate) struct Params<'a> {
    pub(crate) gateway: &'a GatewayId,
    pub(crate) agent_version: &'a AgentVersion,
    /// The presence session of this connection.
    pub(crate) session: SessionId,
    pub(crate) keep_alive: Duration,
    pub(crate) inflight_limit: u16,
    pub(crate) connack_timeout: Duration,
    /// The `down` channels to subscribe to; none means no SUBSCRIBE.
    pub(crate) subscriptions: &'a Subscriptions,
}

/// How a connection ended.
pub(crate) struct Ended {
    /// When the connection became usable (`online` acknowledged, and a SUBACK that granted
    /// every subscription when there are any); `None` if it never did.
    pub(crate) connected_at: Option<Instant>,
    pub(crate) outcome: Outcome,
}

pub(crate) enum Outcome {
    /// Stopped on shutdown, after a clean disconnect when possible.
    Stopped,
    /// The connection failed or was closed; the link reconnects.
    Lost(LinkError),
}

/// What a packet identifier in flight is waiting for.
enum Pending {
    /// A publish of the application, answered on its reply channel.
    User(Reply),
    /// This session's `online`.
    Online,
    /// This session's `offline`/`shutdown`, during a clean stop.
    Offline,
    /// The SUBSCRIBE to the `down` channels.
    Subscribe,
}

/// A packet identifier in flight: what it waits for, and since when. It is registered before
/// its packet is written, so that a failed write still answers the caller; `since` restarts
/// once the packet is written ([`State::written`]), because the broker cannot answer before it
/// has the whole packet.
struct InFlight {
    pending: Pending,
    since: Instant,
}

impl InFlight {
    fn new(pending: Pending) -> Self {
        Self {
            pending,
            since: Instant::now(),
        }
    }
}

/// Which publish queue a request came from.
enum Queue {
    AtMostOnce,
    AtLeastOnce,
}

/// A clean stop in progress.
struct Stop {
    deadline: Instant,
    offline_sent: bool,
}

/// A rotation in progress: newer client credentials are in place, so the connection takes no
/// new publishes, writes what it already accepted, waits for every acknowledgement and then
/// ends ([`LinkError::Renewed`]) for the link to reconnect with them. `deadline` bounds the
/// wait; what is still unacknowledged then ends as when a connection is lost.
struct Rotation {
    deadline: Instant,
}

/// The protocol state of one connection.
struct State<'a> {
    params: &'a Params<'a>,
    counters: &'a Counters,
    shutdown: &'a CancellationToken,
    progress: &'a Progress,
    status_topic: String,
    /// The topic filters to subscribe to, in the order of [`Params::subscriptions`].
    subscriptions: Vec<String>,
    /// The broker acknowledged `online`.
    online_acknowledged: bool,
    /// The broker granted every subscription (true from the start when there are none).
    subscribed: bool,
    in_flight: HashMap<u16, InFlight>,
    /// QoS 1 PUBLISH packets in `in_flight` (the SUBSCRIBE does not count).
    publishes: usize,
    next_id: u16,
    /// The in-flight limit: the configured one, or the broker's Receive Maximum if lower.
    limit: usize,
    server_max_packet: usize,
    keep_alive: Duration,
    connected_at: Option<Instant>,
    last_sent: Instant,
    last_received: Instant,
    ping_sent: Option<Instant>,
    /// When the connection last started reading again after waiting to deliver a message (or
    /// when it started). An answer cannot be read while the connection does not read, so the
    /// PUBACK, SUBACK and PINGRESP deadlines count from here at the earliest.
    reading_since: Instant,
    /// A received message waiting for room in the incoming queue, with its packet identifier
    /// (QoS 1) to acknowledge once it is queued.
    pending_delivery: Option<(DownMessage, Option<u16>)>,
    /// Since when `pending_delivery` waits.
    delivering_since: Instant,
    stopping: Option<Stop>,
    rotating: Option<Rotation>,
}

impl<'a> State<'a> {
    fn new(
        params: &'a Params<'a>,
        counters: &'a Counters,
        shutdown: &'a CancellationToken,
        progress: &'a Progress,
    ) -> Self {
        let now = Instant::now();
        Self {
            params,
            counters,
            shutdown,
            progress,
            status_topic: Channel::Status.topic(params.gateway),
            subscriptions: params
                .subscriptions
                .channels()
                .iter()
                .map(|channel| channel.topic(params.gateway))
                .collect(),
            online_acknowledged: false,
            subscribed: params.subscriptions.is_empty(),
            in_flight: HashMap::new(),
            publishes: 0,
            next_id: 0,
            limit: usize::from(params.inflight_limit.max(1)),
            server_max_packet: usize::MAX,
            keep_alive: params.keep_alive,
            connected_at: None,
            last_sent: now,
            last_received: now,
            ping_sent: None,
            reading_since: now,
            pending_delivery: None,
            delivering_since: now,
            stopping: None,
            rotating: None,
        }
    }

    /// A packet identifier not in use. The loop ends: at most `limit + 1` (257) are in use.
    fn allocate_id(&mut self) -> u16 {
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.in_flight.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
    }

    /// Records a QoS 1 publish waiting for its PUBACK.
    fn track_publish(&mut self, id: u16, pending: Pending) {
        self.in_flight.insert(id, InFlight::new(pending));
        self.publishes += 1;
        self.counters
            .in_flight
            .store(self.publishes, Ordering::Relaxed);
    }

    /// The packet of `id` is written: its acknowledgement's deadline counts from now, so a slow
    /// write does not use up the broker's time to answer.
    fn written(&mut self, id: u16) {
        if let Some(entry) = self.in_flight.get_mut(&id) {
            entry.since = Instant::now();
        }
    }

    /// Marks the connection usable once `online` is acknowledged and every subscription is
    /// granted, whichever comes last.
    fn ready_when_set_up(&mut self) {
        if self.online_acknowledged && self.subscribed && self.connected_at.is_none() {
            self.connected_at = Some(Instant::now());
        }
    }

    fn untrack_publish(&mut self) {
        self.publishes = self.publishes.saturating_sub(1);
        self.counters
            .in_flight
            .store(self.publishes, Ordering::Relaxed);
    }

    fn ping_timeout(&self) -> Duration {
        (self.keep_alive / 2).max(Duration::from_secs(1))
    }

    /// How long a PUBACK or SUBACK may take: twice the keepalive. A broker that answers pings
    /// but not publishes would otherwise hold the in-flight window full for good.
    fn ack_timeout(&self) -> Duration {
        self.keep_alive * 2
    }

    /// When the oldest unanswered PUBLISH or SUBSCRIBE runs out of time: the time the
    /// connection spent not reading (waiting to deliver a message) does not count.
    fn ack_deadline(&self) -> Option<Instant> {
        if self.keep_alive.is_zero() {
            return None;
        }
        self.in_flight
            .values()
            .map(|entry| entry.since)
            .min()
            .map(|since| since.max(self.reading_since) + self.ack_timeout())
    }

    /// A write that takes longer than the broker's own keepalive limit (1.5 times the
    /// keepalive in force: the broker's Server Keep Alive once CONNACK set one) means the
    /// connection is gone.
    fn write_timeout(&self) -> Duration {
        (self.keep_alive * 3 / 2).max(Duration::from_secs(10))
    }

    /// When to send PINGREQ, or to give up on the PINGRESP. Pings start when either direction
    /// has been quiet for the keepalive, so a dead connection is noticed even while the agent
    /// keeps sending QoS 0 messages that nobody acknowledges. While the connection waits to
    /// deliver a message it does not read, so it still sends PINGREQ (the broker expects a
    /// packet every keepalive) but does not wait for the PINGRESP until it reads again.
    fn next_tick(&self) -> Option<Instant> {
        if self.keep_alive.is_zero() {
            return None;
        }
        match self.ping_sent {
            Some(_) if self.pending_delivery.is_some() => None,
            Some(sent) => Some(sent.max(self.reading_since) + self.ping_timeout()),
            None => Some(self.last_sent.min(self.last_received) + self.keep_alive),
        }
    }
}

/// Received bytes not yet decoded. Holds at most one packet of up to [`MAX_INBOUND_PACKET`]
/// plus one read; a buffer that grew for a large packet shrinks back afterwards.
pub(crate) struct ReadBuffer {
    pending: Vec<u8>,
    scratch: Box<[u8]>,
}

impl ReadBuffer {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            scratch: vec![0; READ_CHUNK].into_boxed_slice(),
        }
    }

    /// Appends the first `len` bytes of the scratch buffer.
    pub(crate) fn append(&mut self, len: usize) {
        let incoming = &self.scratch[..len.min(self.scratch.len())];
        let after = self.pending.len() + incoming.len();
        // Room for the whole packet once its size is known (and allowed), so a large packet is
        // not copied again with every read.
        let needed = match packet::frame_len(&self.pending) {
            Some(total) if total <= MAX_INBOUND_PACKET => total.max(after),
            _ => after,
        };
        if needed > self.pending.capacity() {
            self.pending.reserve_exact(needed - self.pending.len());
        }
        self.pending.extend_from_slice(incoming);
    }

    fn consume(&mut self, len: usize) {
        self.pending.drain(..len.min(self.pending.len()));
    }

    fn shrink(&mut self) {
        if self.pending.capacity() > SHRINK_ABOVE && self.pending.len() <= READ_CHUNK {
            self.pending.shrink_to(READ_CHUNK);
        }
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.pending.capacity()
    }

    pub(crate) fn scratch_mut(&mut self) -> &mut [u8] {
        &mut self.scratch
    }

    pub(crate) fn pending(&self) -> &[u8] {
        &self.pending
    }
}

/// The links between one connection and its link.
pub(crate) struct Channels<'a> {
    /// Where the connection declares each wait, for the watchdog.
    pub(crate) progress: &'a Progress,
    /// Where the connection makes its publish queues reachable once it is Connected.
    pub(crate) current: &'a Current,
    pub(crate) incoming: &'a mpsc::Sender<DownMessage>,
    pub(crate) state: &'a watch::Sender<LinkState>,
    pub(crate) counters: &'a Counters,
    pub(crate) shutdown: &'a CancellationToken,
    /// The client credentials' version, already marked seen for the version this connection
    /// opened with: a change means newer credentials, and the connection rotates.
    pub(crate) credentials: Option<watch::Receiver<u64>>,
}

/// Runs one connection over `stream` until it ends.
pub(crate) async fn run<S>(mut stream: S, params: Params<'_>, channels: Channels<'_>) -> Ended
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Channels {
        progress,
        current,
        incoming,
        state,
        counters,
        shutdown,
        mut credentials,
    } = channels;
    let mut st = State::new(&params, counters, shutdown, progress);
    let mut buffer = ReadBuffer::new();
    let (accepting, mut requests) = Requests::new(params.session.clone());
    let mut accepting = Some(accepting);
    let result = drive(
        &mut stream,
        &mut buffer,
        &mut st,
        Queues {
            requests: &mut requests,
            accepting: &mut accepting,
            current,
            credentials: &mut credentials,
        },
        incoming,
        state,
        shutdown,
    )
    .await;
    // A rotation ends like a clean stop, but the link goes on with the renewed credentials.
    let result = match result {
        Ok(()) if st.rotating.is_some() && !shutdown.is_cancelled() => Err(LinkError::Renewed),
        other => other,
    };
    let outcome = match result {
        Ok(()) => Outcome::Stopped,
        Err(error) => Outcome::Lost(error),
    };
    // A publish without its PUBACK was written and may have reached the broker, however the
    // connection ended (a clean stop too): `ConnectionLost`. `Stopped` is for requests still
    // in the queues, which were never written.
    for (_, entry) in st.in_flight.drain() {
        if let Pending::User(reply) = entry.pending {
            let _ = reply.send(Err(PublishError::ConnectionLost));
        }
    }
    // Requests still queued were never written. The queues end with this connection: no later
    // connection can take them.
    current.withdraw(&params.session);
    let unsent = if matches!(outcome, Outcome::Stopped) || shutdown.is_cancelled() {
        PublishError::Stopped
    } else {
        PublishError::NotConnected
    };
    requests.close(&unsent);
    if let Some((message, _)) = st.pending_delivery.take() {
        tracing::warn!(channel = ?message.channel, "a received message was not delivered before the connection ended");
    }
    counters.in_flight.store(0, Ordering::Relaxed);
    Ended {
        connected_at: st.connected_at,
        outcome,
    }
}

/// What woke the connection loop.
enum Event<'p> {
    Shutdown,
    StopDeadline,
    /// Newer client credentials are in place (`true`), or nothing can announce them any more
    /// (`false`).
    Renewed(bool),
    RotateDeadline,
    Deliver(Option<mpsc::Permit<'p, DownMessage>>),
    Read(io::Result<usize>),
    Request(Option<Request>, Queue),
    AckTimeout,
    Tick,
}

/// This connection's publish queues: the receivers, and the senders that [`Current`] gets
/// once the connection is Connected.
struct Queues<'q> {
    requests: &'q mut Requests,
    accepting: &'q mut Option<Accepting>,
    current: &'q Current,
    /// Announces newer client credentials, which withdraw `current` and rotate the connection.
    credentials: &'q mut Option<watch::Receiver<u64>>,
}

/// Why reading a packet failed.
enum ReadFailure {
    Io(io::Error),
    Closed,
    Decode(DecodeError),
}

async fn drive<S>(
    stream: &mut S,
    buffer: &mut ReadBuffer,
    st: &mut State<'_>,
    queues: Queues<'_>,
    incoming: &mpsc::Sender<DownMessage>,
    state: &watch::Sender<LinkState>,
    shutdown: &CancellationToken,
) -> Result<(), LinkError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Queues {
        requests,
        accepting,
        current,
        credentials,
    } = queues;
    let params = st.params;
    let will = Status::last_will(params.session.clone())
        .encode()
        .map_err(|error| LinkError::Internal(format!("encoding the will: {error}")))?;
    let keep_alive_secs = u16::try_from(params.keep_alive.as_secs()).unwrap_or(u16::MAX);
    let connect = packet::connect(&packet::Connect {
        client_id: params.gateway.as_str(),
        keep_alive_secs,
        clean_start: true,
        session_expiry_secs: 0,
        receive_maximum: u16::try_from(INCOMING_QUEUE).unwrap_or(u16::MAX),
        maximum_packet_size: u32::try_from(MAX_INBOUND_PACKET).unwrap_or(u32::MAX),
        will: Some(packet::Will {
            topic: &st.status_topic,
            payload: &will,
            qos: 1,
            retain: true,
        }),
    })
    .map_err(|error| LinkError::Internal(format!("encoding CONNECT: {error}")))?;
    write(stream, st, &[&connect]).await?;

    // A stop requested while CONNACK is awaited keeps waiting for it, within the same
    // STOP_ACK_TIMEOUT that also bounds the PUBACK of offline/shutdown, and then stops like
    // any connection: dropping the stream here would make the broker publish the will for a
    // clean stop.
    let mut stop_deadline: Option<Instant> = st.stopping.as_ref().map(|stop| stop.deadline);
    let connack_deadline = Instant::now() + params.connack_timeout;
    let awaited = {
        let read = next_packet(stream, buffer);
        tokio::pin!(read);
        loop {
            let deadline =
                stop_deadline.map_or(connack_deadline, |stop| stop.min(connack_deadline));
            st.progress.enter(Phase::AwaitConnack, Some(deadline));
            tokio::select! {
                biased;
                () = shutdown.cancelled(), if stop_deadline.is_none() => {
                    tracing::info!(session = %params.session, "stopping while waiting for CONNACK");
                    stop_deadline = Some(Instant::now() + STOP_ACK_TIMEOUT);
                }
                read = timeout_at(deadline, &mut read) => break read,
            }
        }
    };
    let connack = match awaited {
        Err(_) if stop_deadline.is_some() => {
            tracing::warn!("no CONNACK before the stop deadline; closing without it");
            return Ok(());
        }
        Err(_) => {
            return Err(LinkError::Timeout {
                what: "CONNACK",
                after: params.connack_timeout,
            });
        }
        Ok(Ok(packet)) => packet,
        Ok(Err(failure)) => return Err(read_failure(stream, st, failure).await),
    };
    let Inbound::ConnAck(ack) = connack else {
        return Err(violation(
            stream,
            st,
            DecodeError::Protocol("the first packet is not CONNACK"),
        )
        .await);
    };
    if ack.reason != reason::SUCCESS {
        return Err(LinkError::Refused {
            reason: ReasonCode(ack.reason),
            reason_string: ack.reason_string,
        });
    }
    if ack.session_present {
        return Err(violation(
            stream,
            st,
            DecodeError::Protocol("session present after a clean start"),
        )
        .await);
    }
    if let Some(receive_maximum) = ack.receive_maximum {
        st.limit = st.limit.min(usize::from(receive_maximum));
    }
    // The CONNECT carried a QoS 1 will, which MQTT 5 requires a broker of Maximum QoS 0 to
    // refuse (MQTT-3.2.2-12). Accepting it anyway is a protocol error, and on such a connection
    // the gateway could publish neither its status nor its telemetry, nor subscribe, at QoS 1.
    if ack.maximum_qos == Some(0) {
        return Err(violation(
            stream,
            st,
            DecodeError::Protocol("CONNACK accepted a QoS 1 will with Maximum QoS 0"),
        )
        .await);
    }
    // The will is retained as well, which a broker without retained messages must refuse
    // (MQTT-3.2.2-13); presence relies on the retained status.
    if ack.retain_available == Some(false) {
        return Err(violation(
            stream,
            st,
            DecodeError::Protocol("CONNACK accepted a retained will with Retain Available 0"),
        )
        .await);
    }
    if let Some(size) = ack.maximum_packet_size {
        st.server_max_packet = usize::try_from(size).unwrap_or(usize::MAX);
    }
    // A Server Keep Alive of 0 means the broker never checks the connection; the agent keeps
    // its own keepalive, since pings are its only way to notice a connection that died
    // silently. A non-zero value replaces the agent's, as MQTT 5 requires.
    match ack.server_keep_alive {
        Some(0) => tracing::warn!(
            keep_alive_s = st.keep_alive.as_secs(),
            "the broker turned keepalive off; keeping the agent's own"
        ),
        Some(secs) => st.keep_alive = Duration::from_secs(u64::from(secs)),
        None => {}
    }
    if let Some(deadline) = stop_deadline {
        st.stopping = Some(Stop {
            deadline,
            offline_sent: false,
        });
    }
    tracing::info!(
        gateway = %params.gateway,
        session = %params.session,
        keep_alive_s = st.keep_alive.as_secs(),
        inflight_limit = st.limit,
        "connected to the broker"
    );

    // The first PUBLISH of the session is its `online`.
    let online = Status::Online(Online {
        v: V1,
        session: params.session.clone(),
        agent_version: params.agent_version.clone(),
        protocol_rev: PROTOCOL_REVISION,
    })
    .encode()
    .map_err(|error| LinkError::Internal(format!("encoding online: {error}")))?;
    send_status(stream, st, &online, Pending::Online).await?;
    // A connection that is already stopping only says goodbye: no subscriptions, and callers
    // never see it as connected. Otherwise the link reports Connected once `online` is
    // acknowledged and the SUBACK grants every subscription (`handle` sets `connected_at`).
    // Without subscriptions there is no SUBSCRIBE at all: MQTT 5 forbids one without a topic
    // filter (MQTT-3.8.3-2).
    if st.stopping.is_none() && !st.subscriptions.is_empty() {
        let subscribe_id = st.allocate_id();
        st.in_flight
            .insert(subscribe_id, InFlight::new(Pending::Subscribe));
        let subscribe = packet::subscribe(subscribe_id, &st.subscriptions, 1)
            .map_err(|error| LinkError::Internal(format!("encoding SUBSCRIBE: {error}")))?;
        write(stream, st, &[&subscribe]).await?;
        st.written(subscribe_id);
    }

    let mut announced = false;
    let mut at_most_once_closed = false;
    let mut at_least_once_closed = false;
    loop {
        if !announced && st.connected_at.is_some() && st.stopping.is_none() && st.rotating.is_none()
        {
            // Publishes are accepted from here on, into this connection's queues; the queues
            // are reachable before the state says Connected, so a caller that sees the state
            // finds them.
            if let Some(accepting) = accepting.take() {
                current.install(accepting);
            }
            state.send_replace(LinkState::Connected {
                session: params.session.clone(),
            });
            announced = true;
        }
        // A rotation has drained once both queues have closed (no caller can add to them any
        // more and everything in them was written), nothing waits for an acknowledgement and no
        // received message waits for the application. It then stops like a clean stop:
        // `offline`/`shutdown`, its PUBACK, DISCONNECT 0x00.
        if st.rotating.is_some()
            && st.stopping.is_none()
            && at_most_once_closed
            && at_least_once_closed
            && st.in_flight.is_empty()
            && st.pending_delivery.is_none()
        {
            tracing::info!(session = %params.session, "every accepted message is acknowledged; ending the session to reconnect with the renewed certificate");
            st.stopping = Some(Stop {
                deadline: Instant::now() + STOP_ACK_TIMEOUT,
                offline_sent: false,
            });
        }
        if let Some(stop) = &st.stopping
            && !stop.offline_sent
            && st.publishes < st.limit
        {
            let offline = Status::Offline(Offline {
                v: V1,
                session: params.session.clone(),
                reason: OfflineReason::Shutdown,
            })
            .encode()
            .map_err(|error| LinkError::Internal(format!("encoding offline: {error}")))?;
            send_status(stream, st, &offline, Pending::Offline).await?;
            if let Some(stop) = &mut st.stopping {
                stop.offline_sent = true;
            }
        }
        let up = st.stopping.is_none();
        // QoS 0 (heartbeats) never waits for the in-flight window; QoS 1 does.
        let take_at_most_once = up && !at_most_once_closed;
        let take_at_least_once = up && st.publishes < st.limit && !at_least_once_closed;
        let delivering = st.pending_delivery.is_some();
        let tick = st.next_tick();
        // An acknowledgement stuck behind a full incoming queue is not the broker's fault (and
        // its deadline restarts once the connection reads again), and the stop has a deadline
        // of its own.
        let ack_deadline = if up && !delivering {
            st.ack_deadline()
        } else {
            None
        };
        let far = Instant::now() + Duration::from_secs(86_400);
        let stop_deadline = st.stopping.as_ref().map(|stop| stop.deadline);
        let rotate_deadline = st
            .rotating
            .as_ref()
            .filter(|_| st.stopping.is_none())
            .map(|rotation| rotation.deadline);
        let take_renewal = credentials.is_some() && st.rotating.is_none() && up;
        // The latest moment this wait may end, for the watchdog: the stop's or the rotation's
        // deadline, the application taking the waiting message, or the connection's next timer
        // (none only with the keepalive turned off, which only tests do).
        match (stop_deadline, rotate_deadline, delivering) {
            (Some(deadline), _, _) => st.progress.enter(Phase::Stopping, Some(deadline)),
            (None, Some(deadline), _) => st.progress.enter(Phase::Rotating, Some(deadline)),
            (None, None, true) => st.progress.enter_since(
                Phase::Delivering,
                st.delivering_since,
                Some(st.delivering_since + DELIVERY_LIMIT),
            ),
            (None, None, false) => st.progress.enter(
                if announced {
                    Phase::Connected
                } else {
                    Phase::SettingUp
                },
                [ack_deadline, tick].into_iter().flatten().min(),
            ),
        }
        let event = tokio::select! {
            biased;
            () = shutdown.cancelled(), if st.stopping.is_none() => Event::Shutdown,
            () = sleep_until(stop_deadline.unwrap_or(far)), if stop_deadline.is_some() => Event::StopDeadline,
            () = sleep_until(rotate_deadline.unwrap_or(far)), if rotate_deadline.is_some() => Event::RotateDeadline,
            renewed = renewed(credentials), if take_renewal => Event::Renewed(renewed),
            permit = incoming.reserve(), if delivering => Event::Deliver(permit.ok()),
            read = stream.read(buffer.scratch_mut()), if !delivering => Event::Read(read),
            request = requests.at_most_once.recv(), if take_at_most_once => Event::Request(request, Queue::AtMostOnce),
            request = requests.at_least_once.recv(), if take_at_least_once => Event::Request(request, Queue::AtLeastOnce),
            () = sleep_until(ack_deadline.unwrap_or(far)), if ack_deadline.is_some() => Event::AckTimeout,
            () = sleep_until(tick.unwrap_or(far)), if tick.is_some() => Event::Tick,
        };
        match event {
            Event::Shutdown => {
                tracing::info!(session = %params.session, "stopping: publishing offline/shutdown");
                // A shutdown during a rotation is a plain stop.
                st.rotating = None;
                st.stopping = Some(Stop {
                    deadline: Instant::now() + STOP_ACK_TIMEOUT,
                    offline_sent: false,
                });
            }
            Event::Renewed(false) => {
                // Nothing can announce newer credentials any more: keep this connection.
                *credentials = None;
            }
            Event::Renewed(true) => {
                let limit = st.ack_timeout().max(STOP_ACK_TIMEOUT);
                tracing::info!(
                    session = %params.session,
                    limit_s = limit.as_secs(),
                    "the client certificate was renewed; finishing this connection before reconnecting with it"
                );
                // Callers get `NotConnected` from now on (the link's publish contract while it
                // is not usable); requests already queued are still written and acknowledged.
                // The queues close once the last sender, a caller's permit included, is gone.
                current.withdraw(&params.session);
                accepting.take();
                st.rotating = Some(Rotation {
                    deadline: Instant::now() + limit,
                });
            }
            Event::RotateDeadline => {
                tracing::warn!(
                    "the rotation did not finish in time; disconnecting with the will and reconnecting with the renewed certificate"
                );
                close(stream, st, reason::DISCONNECT_WITH_WILL).await;
                return Err(LinkError::Renewed);
            }
            Event::StopDeadline => {
                tracing::warn!(
                    "no PUBACK for offline/shutdown in time; disconnecting with the will instead"
                );
                close(stream, st, reason::DISCONNECT_WITH_WILL).await;
                return Ok(());
            }
            Event::Deliver(permit) => {
                if let Some((message, packet_id)) = st.pending_delivery.take() {
                    st.reading_since = Instant::now();
                    match permit {
                        Some(permit) => permit.send(message),
                        None => tracing::debug!("nobody receives down messages; dropped one"),
                    }
                    if let Some(id) = packet_id {
                        write(stream, st, &[&packet::puback(id)]).await?;
                    }
                }
                if process(stream, buffer, st, incoming).await? {
                    return Ok(());
                }
            }
            Event::Read(Ok(0)) => return Err(LinkError::Closed),
            Event::Read(Ok(len)) => {
                buffer.append(len);
                if process(stream, buffer, st, incoming).await? {
                    return Ok(());
                }
            }
            Event::Read(Err(error)) => return Err(LinkError::Io(error)),
            Event::Request(None, Queue::AtMostOnce) => at_most_once_closed = true,
            Event::Request(None, Queue::AtLeastOnce) => at_least_once_closed = true,
            Event::Request(Some(request), _) => send_request(stream, st, request).await?,
            Event::AckTimeout => {
                let after = st.ack_timeout();
                tracing::warn!(
                    after_s = after.as_secs(),
                    "no PUBACK or SUBACK in time; disconnecting with the will"
                );
                close(stream, st, reason::DISCONNECT_WITH_WILL).await;
                return Err(LinkError::Timeout {
                    what: "a PUBACK or SUBACK",
                    after,
                });
            }
            Event::Tick => {
                if st.ping_sent.is_some() {
                    return Err(LinkError::KeepAlive(st.ping_timeout()));
                }
                write(stream, st, &[&packet::pingreq()]).await?;
                st.ping_sent = Some(Instant::now());
            }
        }
    }
}

/// Waits for newer client credentials: `true` once they are in place, `false` once nothing can
/// announce them any more. Never ends without a receiver.
async fn renewed(credentials: &mut Option<watch::Receiver<u64>>) -> bool {
    match credentials {
        Some(credentials) => credentials.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Reads until one whole packet is buffered; used before the main loop (CONNACK).
async fn next_packet<S>(stream: &mut S, buffer: &mut ReadBuffer) -> Result<Inbound, ReadFailure>
where
    S: AsyncRead + Unpin,
{
    loop {
        match packet::decode(buffer.pending(), MAX_INBOUND_PACKET) {
            Ok(Some((packet, len))) => {
                buffer.consume(len);
                return Ok(packet);
            }
            Ok(None) => {}
            Err(error) => return Err(ReadFailure::Decode(error)),
        }
        match stream.read(buffer.scratch_mut()).await {
            Ok(0) => return Err(ReadFailure::Closed),
            Ok(len) => buffer.append(len),
            Err(error) => return Err(ReadFailure::Io(error)),
        }
    }
}

async fn read_failure<S>(stream: &mut S, st: &mut State<'_>, failure: ReadFailure) -> LinkError
where
    S: AsyncWrite + Unpin,
{
    match failure {
        ReadFailure::Io(error) => LinkError::Io(error),
        ReadFailure::Closed => LinkError::Closed,
        ReadFailure::Decode(error) => violation(stream, st, error).await,
    }
}

/// Decodes and handles every whole packet in the buffer. `Ok(true)` when the connection has
/// stopped cleanly.
async fn process<S>(
    stream: &mut S,
    buffer: &mut ReadBuffer,
    st: &mut State<'_>,
    incoming: &mpsc::Sender<DownMessage>,
) -> Result<bool, LinkError>
where
    S: AsyncWrite + Unpin,
{
    while st.pending_delivery.is_none() {
        match packet::decode(buffer.pending(), MAX_INBOUND_PACKET) {
            Ok(None) => break,
            Err(error) => return Err(violation(stream, st, error).await),
            Ok(Some((packet, len))) => {
                buffer.consume(len);
                st.last_received = Instant::now();
                if handle(stream, st, incoming, packet).await? {
                    return Ok(true);
                }
            }
        }
    }
    buffer.shrink();
    Ok(false)
}

async fn handle<S>(
    stream: &mut S,
    st: &mut State<'_>,
    incoming: &mpsc::Sender<DownMessage>,
    packet: Inbound,
) -> Result<bool, LinkError>
where
    S: AsyncWrite + Unpin,
{
    match packet {
        Inbound::ConnAck(_) => {
            Err(violation(stream, st, DecodeError::Protocol("a second CONNACK")).await)
        }
        Inbound::PingResp => {
            // At most one PINGREQ is outstanding, so a PINGRESP without one answers nothing.
            if st.ping_sent.take().is_none() {
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("PINGRESP without a PINGREQ"),
                )
                .await);
            }
            Ok(false)
        }
        Inbound::Disconnect(disconnect) => Err(LinkError::Disconnected {
            reason: ReasonCode(disconnect.reason),
            reason_string: disconnect.reason_string,
        }),
        Inbound::PubAck(ack) => {
            if matches!(
                st.in_flight.get(&ack.packet_id).map(|entry| &entry.pending),
                Some(Pending::Subscribe)
            ) {
                return Err(
                    violation(stream, st, DecodeError::Protocol("PUBACK for a SUBSCRIBE")).await,
                );
            }
            let Some(pending) = st
                .in_flight
                .remove(&ack.packet_id)
                .map(|entry| entry.pending)
            else {
                // Identifiers start at 1 and leave `in_flight` only with their PUBACK or with
                // the connection, so this acknowledges nothing the agent sent (or repeats a
                // PUBACK; identifier 0 is refused as it is decoded).
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("PUBACK for a packet identifier not in use"),
                )
                .await);
            };
            st.untrack_publish();
            let code = ReasonCode(ack.reason);
            // Only the two success codes count as delivered; decoding refused every code MQTT 5
            // does not allow, so the rest are failures.
            let refused = !matches!(
                ack.reason,
                reason::SUCCESS | reason::NO_MATCHING_SUBSCRIBERS
            );
            match pending {
                Pending::User(reply) => {
                    let outcome = if refused {
                        Err(PublishError::Refused {
                            reason: code,
                            reason_string: ack.reason_string,
                        })
                    } else {
                        Ok(Published::Acknowledged(code))
                    };
                    let _ = reply.send(outcome);
                    Ok(false)
                }
                Pending::Online if refused => Err(LinkError::OnlineRefused(code)),
                Pending::Online => {
                    st.online_acknowledged = true;
                    st.ready_when_set_up();
                    Ok(false)
                }
                Pending::Offline => {
                    if refused {
                        tracing::warn!(reason = %code, "the broker refused offline/shutdown; disconnecting with the will");
                        close(stream, st, reason::DISCONNECT_WITH_WILL).await;
                    } else {
                        close(stream, st, reason::SUCCESS).await;
                    }
                    Ok(true)
                }
                Pending::Subscribe => Ok(false),
            }
        }
        Inbound::SubAck(ack) => {
            if !matches!(
                st.in_flight.get(&ack.packet_id).map(|entry| &entry.pending),
                Some(Pending::Subscribe)
            ) {
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("SUBACK for no SUBSCRIBE"),
                )
                .await);
            }
            st.in_flight.remove(&ack.packet_id);
            if ack.reasons.len() != st.subscriptions.len() {
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("SUBACK with the wrong number of reason codes"),
                )
                .await);
            }
            if ack.reasons.contains(&reason::GRANTED_QOS_2) {
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("SUBACK granted QoS 2, more than the QoS 1 asked for"),
                )
                .await);
            }
            // Only granted QoS 1 is a success. The down channels are QoS 1
            // (`Channel::delivery`): a QoS 0 grant takes the broker's deliveries out of the
            // Receive Maximum flow control and lets it drop messages while the agent's queue is
            // full. Decoding refused every code MQTT 5 does not allow, so any other code is a
            // refusal.
            let mut failed = None;
            for (filter, &code) in st.subscriptions.iter().zip(&ack.reasons) {
                match code {
                    reason::GRANTED_QOS_1 => {}
                    reason::GRANTED_QOS_0 => {
                        tracing::error!(topic = %filter, "the broker granted only QoS 0 for a subscription that needs QoS 1");
                        failed.get_or_insert_with(|| LinkError::SubscriptionDowngraded {
                            topic: filter.clone(),
                        });
                    }
                    _ => {
                        tracing::error!(topic = %filter, reason = %ReasonCode(code), "the broker refused a subscription; messages on it would not arrive");
                        failed.get_or_insert_with(|| LinkError::SubscriptionRefused {
                            topic: filter.clone(),
                            reason: ReasonCode(code),
                        });
                    }
                }
            }
            // Every down channel is required, at QoS 1: a gateway that cannot reliably receive
            // its configuration or requests is not connected. DISCONNECT 0x04 makes the broker
            // publish the will (after the online, so presence stays consistent); the supervisor
            // reconnects after the backoff, which does not reset because the connection never
            // became usable. A stopping connection finishes its clean stop instead.
            if let Some(error) = failed {
                if st.stopping.is_none() {
                    close(stream, st, reason::DISCONNECT_WITH_WILL).await;
                    return Err(error);
                }
                return Ok(false);
            }
            st.subscribed = true;
            st.ready_when_set_up();
            Ok(false)
        }
        Inbound::Publish(publish) => {
            if publish.qos > 1 {
                return Err(violation(
                    stream,
                    st,
                    DecodeError::Protocol("QoS 2 PUBLISH on a QoS 1 subscription"),
                )
                .await);
            }
            if let Some(message) = down_message(
                st.params.gateway,
                st.params.subscriptions,
                publish.topic,
                publish.payload,
                publish.retain,
            ) {
                match incoming.try_send(message) {
                    Ok(()) => {}
                    Err(TrySendError::Full(message)) => {
                        // Stop reading until the application takes a message; acknowledge after.
                        st.pending_delivery = Some((message, publish.packet_id));
                        st.delivering_since = Instant::now();
                        return Ok(false);
                    }
                    Err(TrySendError::Closed(_)) => {
                        tracing::debug!("nobody receives down messages; dropped one");
                    }
                }
            }
            if let Some(id) = publish.packet_id {
                write(stream, st, &[&packet::puback(id)]).await?;
            }
            Ok(false)
        }
    }
}

/// The message for the application, if `topic` is one of the `down` channels this gateway
/// subscribed to and the payload fits the channel. Anything else is logged and dropped (but
/// still acknowledged): the broker's ACL and its subscription matching should never let it
/// through, so it is a broker bug.
fn down_message(
    gateway: &GatewayId,
    subscriptions: &Subscriptions,
    topic: String,
    payload: Vec<u8>,
    retained: bool,
) -> Option<DownMessage> {
    let excerpt = || topic.chars().take(120).collect::<String>();
    let parsed = match parse_topic(&topic) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(topic = ?excerpt(), %error, "dropped a message on a topic outside the protocol");
            return None;
        }
    };
    if parsed.gateway != *gateway || parsed.channel.direction() != Direction::Down {
        tracing::warn!(topic = ?excerpt(), "dropped a message that is not on this gateway's down channels");
        return None;
    }
    // Nothing would handle it, and the agent never asked for it: a broker bug.
    if !subscriptions.contains(parsed.channel) {
        tracing::warn!(channel = ?parsed.channel, "dropped a message on a channel the agent did not subscribe to");
        return None;
    }
    let limit = parsed.channel.max_payload_bytes();
    if payload.len() > limit {
        tracing::warn!(channel = ?parsed.channel, size = payload.len(), limit, "dropped a message above its channel's limit");
        return None;
    }
    Some(DownMessage {
        channel: parsed.channel,
        payload,
        retained,
    })
}

/// Publishes this session's status (retained, QoS 1).
async fn send_status<S>(
    stream: &mut S,
    st: &mut State<'_>,
    payload: &[u8],
    pending: Pending,
) -> Result<(), LinkError>
where
    S: AsyncWrite + Unpin,
{
    let id = st.allocate_id();
    let header = packet::publish_header(&PublishHeader {
        topic: &st.status_topic,
        qos: 1,
        retain: true,
        packet_id: Some(id),
        message_expiry_secs: None,
        payload_len: payload.len(),
    })
    .map_err(|error| LinkError::Internal(format!("encoding a status: {error}")))?;
    st.track_publish(id, pending);
    write(stream, st, &[&header, payload]).await?;
    st.written(id);
    Ok(())
}

/// Writes one publish request of the application.
async fn send_request<S>(
    stream: &mut S,
    st: &mut State<'_>,
    request: Request,
) -> Result<(), LinkError>
where
    S: AsyncWrite + Unpin,
{
    let Request {
        channel,
        payload,
        reply,
    } = request;
    // From here the message may be written: a reply dropped unanswered is `ConnectionLost`.
    let reply = reply.taken();
    let delivery = channel.delivery();
    let qos = delivery.qos.level();
    let topic = channel.topic(st.params.gateway);
    let packet_id = (qos > 0).then(|| st.allocate_id());
    let header = packet::publish_header(&PublishHeader {
        topic: &topic,
        qos,
        retain: delivery.retain,
        packet_id,
        message_expiry_secs: delivery
            .message_expiry
            .map(|expiry| u32::try_from(expiry.as_secs()).unwrap_or(u32::MAX)),
        payload_len: payload.len(),
    });
    let Ok(header) = header else {
        let _ = reply.send(Err(PublishError::TooLarge {
            size: payload.len(),
            limit: packet::MAX_REMAINING_LENGTH,
        }));
        return Ok(());
    };
    let size = packet::publish_size(&header, payload.len());
    if size > st.server_max_packet {
        let _ = reply.send(Err(PublishError::TooLarge {
            size,
            limit: st.server_max_packet,
        }));
        return Ok(());
    }
    match packet_id {
        Some(id) => {
            st.track_publish(id, Pending::User(reply));
            write(stream, st, &[&header, &payload]).await?;
            st.written(id);
            Ok(())
        }
        None => {
            // A failed write may have sent part of the message, or all of it: answer
            // `ConnectionLost` here rather than drop the reply, which a stop racing with this
            // failure would turn into `Stopped` (never sent).
            let written = write(stream, st, &[&header, &payload]).await;
            let _ = reply.send(match written {
                Ok(()) => Ok(Published::Sent),
                Err(_) => Err(PublishError::ConnectionLost),
            });
            written
        }
    }
}

/// Writes `parts` and flushes, within the write timeout and, once a stop is requested (before
/// or during the write), within the stop's deadline: a write that the uplink cannot finish must
/// not hold the stop past [`STOP_ACK_TIMEOUT`]. A shutdown noticed here starts the stop as the
/// main loop would. A write cut short leaves part of a packet on the stream, so the error ends
/// the connection without DISCONNECT, and the broker publishes the will.
async fn write<S>(stream: &mut S, st: &mut State<'_>, parts: &[&[u8]]) -> Result<(), LinkError>
where
    S: AsyncWrite + Unpin,
{
    // The client must not send a packet above the broker's Maximum Packet Size (MQTT-3.2.2-15).
    // Application messages are checked before they get here and refused on their own; any
    // other packet is one the connection needs, so it ends, at a packet boundary.
    let size = parts.iter().map(|part| part.len()).sum::<usize>();
    if size > st.server_max_packet {
        let limit = st.server_max_packet;
        tracing::error!(
            size,
            limit,
            "a packet is above the broker's Maximum Packet Size; disconnecting with the will"
        );
        close(stream, st, reason::DISCONNECT_WITH_WILL).await;
        return Err(LinkError::PacketTooLarge { size, limit });
    }
    let started = Instant::now();
    let limit = st.write_timeout();
    st.progress.enter(Phase::Writing, Some(started + limit));
    let shutdown = st.shutdown;
    let io = async {
        for part in parts {
            stream.write_all(part).await?;
        }
        stream.flush().await
    };
    tokio::pin!(io);
    let written = loop {
        let stop_deadline = st.stopping.as_ref().map(|stop| stop.deadline);
        let deadline = stop_deadline.map_or(started + limit, |stop| stop.min(started + limit));
        tokio::select! {
            biased;
            written = &mut io => break Ok(written),
            () = shutdown.cancelled(), if st.stopping.is_none() => {
                tracing::info!(session = %st.params.session, "stopping during a write; it has until the stop deadline");
                st.stopping = Some(Stop {
                    deadline: Instant::now() + STOP_ACK_TIMEOUT,
                    offline_sent: false,
                });
            }
            () = sleep_until(deadline) => break Err(stop_deadline.is_some_and(|stop| stop < started + limit)),
        }
    };
    match written {
        Err(cut_by_stop) => Err(LinkError::Timeout {
            what: if cut_by_stop {
                "a write to the broker before the stop deadline"
            } else {
                "a write to the broker"
            },
            after: started.elapsed(),
        }),
        Ok(Err(error)) => Err(LinkError::Io(error)),
        Ok(Ok(())) => {
            st.last_sent = Instant::now();
            Ok(())
        }
    }
}

/// Ends the connection over a broken packet: logs the reason code MQTT 5 has for it (0x81,
/// 0x82, 0x94 or 0x95) and sends DISCONNECT 0x04, after which Mosquitto still publishes the will
/// (with the matching code it would drop it; see the module documentation).
async fn violation<S>(stream: &mut S, st: &mut State<'_>, error: DecodeError) -> LinkError
where
    S: AsyncWrite + Unpin,
{
    tracing::warn!(%error, reason = %ReasonCode(error.reason_code()), "the broker broke the MQTT protocol; disconnecting with the will");
    close(stream, st, reason::DISCONNECT_WITH_WILL).await;
    LinkError::Protocol(error.to_string())
}

/// Sends DISCONNECT with `code` and closes the stream (TLS close_notify, WebSocket close), both
/// best effort and bounded: the connection is over either way. A broker whose Maximum Packet
/// Size cannot hold the DISCONNECT gets none, which publishes the will as 0x04 would.
async fn close<S>(stream: &mut S, st: &mut State<'_>, code: u8)
where
    S: AsyncWrite + Unpin,
{
    let disconnect = packet::disconnect(code);
    let fits = disconnect.len() <= st.server_max_packet;
    st.progress
        .enter(Phase::Closing, Some(Instant::now() + CLOSE_TIMEOUT));
    let closed = timeout(CLOSE_TIMEOUT, async {
        if fits {
            stream.write_all(&disconnect).await?;
            stream.flush().await?;
        }
        stream.shutdown().await
    })
    .await;
    match closed {
        Ok(Ok(())) if fits => {
            st.last_sent = Instant::now();
            tracing::info!(reason = %ReasonCode(code), "disconnected from the broker");
        }
        Ok(Ok(())) => tracing::warn!(
            limit = st.server_max_packet,
            "closed without DISCONNECT: the broker's Maximum Packet Size cannot hold one"
        ),
        Ok(Err(error)) => tracing::debug!(%error, "closing the connection failed"),
        Err(_) => tracing::debug!("closing the connection timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PUBLISH packet of `total` bytes with a one-byte topic.
    fn publish_of(total: usize) -> Vec<u8> {
        // Fixed header (1 + 3 bytes for these sizes), topic (3), property length (1), payload.
        let payload = total - 1 - 3 - 3 - 1;
        let remaining = total - 4;
        let mut bytes = vec![0x30];
        let mut value = remaining;
        for _ in 0..3 {
            let mut byte = u8::try_from(value & 0x7F).unwrap();
            value >>= 7;
            if value > 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                break;
            }
        }
        assert_eq!(bytes.len(), 4, "use a total that needs a three-byte length");
        bytes.extend_from_slice(&[0, 1, b't', 0]);
        bytes.resize(total, 0xAB);
        assert_eq!(payload, total - 8);
        bytes
    }

    fn feed(buffer: &mut ReadBuffer, bytes: &[u8]) -> usize {
        let mut peak = 0;
        for chunk in bytes.chunks(READ_CHUNK) {
            buffer.scratch_mut()[..chunk.len()].copy_from_slice(chunk);
            buffer.append(chunk.len());
            peak = peak.max(buffer.capacity());
        }
        peak
    }

    #[test]
    fn the_read_buffer_holds_one_largest_packet_and_shrinks_back() {
        let mut buffer = ReadBuffer::new();
        let largest = publish_of(MAX_INBOUND_PACKET);
        let peak = feed(&mut buffer, &largest);
        assert!(
            peak <= MAX_INBOUND_PACKET + READ_CHUNK,
            "peak capacity {peak} for a packet of {MAX_INBOUND_PACKET}"
        );
        let (packet, len) = packet::decode(buffer.pending(), MAX_INBOUND_PACKET)
            .unwrap()
            .unwrap();
        assert!(matches!(packet, Inbound::Publish(_)));
        buffer.consume(len);
        buffer.shrink();
        assert!(buffer.capacity() <= READ_CHUNK, "{}", buffer.capacity());
        // Many small packets afterwards do not grow it again.
        let small = [0xD0, 0x00].repeat(1000);
        let peak = feed(&mut buffer, &small);
        assert!(peak <= 2 * READ_CHUNK, "{peak}");
    }

    #[test]
    fn a_declared_size_above_the_limit_reserves_nothing_for_it() {
        let mut buffer = ReadBuffer::new();
        // The fixed header of a 200 MB PUBLISH.
        let header = [0x30, 0xFF, 0xFF, 0xFF, 0x5F];
        feed(&mut buffer, &header);
        assert!(buffer.capacity() < 64, "{}", buffer.capacity());
        assert!(matches!(
            packet::decode(buffer.pending(), MAX_INBOUND_PACKET),
            Err(DecodeError::TooLarge { .. })
        ));
        // A Remaining Length that is not in its shortest encoding sizes nothing either.
        let mut buffer = ReadBuffer::new();
        feed(&mut buffer, &[0x30, 0x80, 0x80, 0x80, 0x00]);
        assert!(buffer.capacity() < 64, "{}", buffer.capacity());
        assert!(matches!(
            packet::decode(buffer.pending(), MAX_INBOUND_PACKET),
            Err(DecodeError::Malformed(_))
        ));
    }
}
