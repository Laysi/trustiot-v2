//! The connection task: connects, subscribes, serves publishes and acknowledgements, and
//! reconnects with a bounded delay. It owns the socket; callers reach it through channels.
//!
//! Only rumqttc's MQTT 5 packet types and codec are used. The packet ids, the keepalive and the
//! matching of each PUBACK to its publish are kept here, so a reason code always reaches the
//! caller of the publish it belongs to (README.md, "為什麼這樣用 rumqttc").

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt as _, StreamExt as _};
use rumqttc::v5::mqttbytes::v5::{
    Codec, ConnAck, Connect, ConnectProperties, ConnectReturnCode, Disconnect,
    DisconnectReasonCode, Filter, Login, Packet, PingReq, PubAck, PubAckReason, Publish,
    PublishProperties, Subscribe,
};
use rumqttc::v5::mqttbytes::{Error as CodecError, QoS};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until, timeout};
use tokio_util::codec::Framed;
use zeroize::Zeroize as _;

use crate::config::{Config, Password};
use crate::{
    CONNECT_TIMEOUT, ConnectionError, Event, KEEP_ALIVE, MAX_HELD_BYTES, MAX_INFLIGHT,
    MAX_PACKET_BYTES, MAX_QUEUED_EVENTS, Message, PUBACK_TIMEOUT, PublishError, PublishOptions,
    RECEIVE_MAXIMUM, STABLE_CONNECTION, SubscriptionOutcome, Subscriptions, WRITE_TIMEOUT, backoff,
    codes,
};

/// The packet id of the SUBSCRIBE sent on every connection; publishes take the others.
const SUBSCRIBE_PACKET_ID: u16 = 1;

/// What a held message costs besides its topic and payload: the [`Delivery`], the topic's and
/// the payload's allocations and the channel's slot, rounded up.
const MESSAGE_OVERHEAD: usize = 256;

/// The most one message can cost: a packet of [`MAX_PACKET_BYTES`] and the overhead.
const MAX_MESSAGE_COST: usize = protocol::topic::MIN_MAX_PACKET_BYTES + MESSAGE_OVERHEAD;

/// The client reads the next packet only while the held messages leave room for one more of
/// the largest kind, so they never take more than [`MAX_HELD_BYTES`].
const READ_LIMIT: usize = MAX_HELD_BYTES - MAX_MESSAGE_COST;

const _: () = assert!(MAX_HELD_BYTES >= 4 * MAX_MESSAGE_COST);

/// Most status events one connection attempt delivers: [`Event::Connected`] and
/// [`Event::Disconnected`].
const EVENTS_PER_ATTEMPT: usize = 2;

/// How often at most the client warns that it stopped reading because the caller is behind.
const PAUSE_WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// What the client holds for [`crate::Events`], shared between the two.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    /// The cost ([`message_cost`]) of the messages waiting in the channel.
    held_bytes: AtomicUsize,
    /// Status events waiting in the channel.
    queued_events: AtomicUsize,
    /// [`crate::Events`] was dropped: nobody takes messages any more.
    consumer_gone: AtomicBool,
    /// Woken when [`crate::Events`] takes something or is dropped. The task is its only
    /// waiter; `notify_one` keeps a wakeup that comes while the task is not waiting.
    space: Notify,
}

impl Budget {
    pub(crate) fn held_bytes(&self) -> usize {
        self.held_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn queued_events(&self) -> usize {
        self.queued_events.load(Ordering::Acquire)
    }

    pub(crate) fn message_taken(&self, cost: usize) {
        self.held_bytes.fetch_sub(cost, Ordering::AcqRel);
        self.space.notify_one();
    }

    pub(crate) fn event_taken(&self) {
        self.queued_events.fetch_sub(1, Ordering::AcqRel);
        self.space.notify_one();
    }

    pub(crate) fn consumer_dropped(&self) {
        self.consumer_gone.store(true, Ordering::Release);
        self.space.notify_one();
    }

    fn is_consumer_gone(&self) -> bool {
        self.consumer_gone.load(Ordering::Acquire)
    }

    /// Whether the held messages and `extra` bytes leave room for one more message. Once the
    /// caller is gone nothing is held for it: the channel is dropped with what it held.
    fn fits(&self, extra: usize) -> bool {
        let held = if self.is_consumer_gone() {
            0
        } else {
            self.held_bytes()
        };
        held.saturating_add(extra) <= READ_LIMIT
    }

    /// Whether a connection attempt may start: room to read a message and for the status
    /// events of one attempt.
    fn fits_an_attempt(&self) -> bool {
        self.is_consumer_gone()
            || (self.fits(0)
                && self.queued_events().saturating_add(EVENTS_PER_ATTEMPT) <= MAX_QUEUED_EVENTS)
    }
}

/// What a message held for the caller counts against [`MAX_HELD_BYTES`].
fn message_cost(topic_bytes: usize, payload_bytes: usize) -> usize {
    topic_bytes
        .saturating_add(payload_bytes)
        .saturating_add(MESSAGE_OVERHEAD)
}

/// A publish waiting for its PUBACK.
struct InFlight {
    reply: oneshot::Sender<Result<(), PublishError>>,
    /// When the connection counts as lost if no PUBACK came.
    deadline: Instant,
}

type Window = HashMap<u16, InFlight>;

/// A publish from a [`crate::Client`].
pub(crate) struct Command {
    pub(crate) topic: String,
    pub(crate) payload: Bytes,
    pub(crate) options: PublishOptions,
    pub(crate) reply: oneshot::Sender<Result<(), PublishError>>,
}

/// A [`crate::Client::shutdown`]: answered once the task has ended. It has its own channel,
/// which the task always reads, so a shutdown never waits behind publishes that wait for a
/// free slot in the in-flight window.
pub(crate) type ShutdownRequest = oneshot::Sender<()>;

/// The caller took a QoS 1 message: acknowledge it on the connection it came from.
pub(crate) struct Ack {
    connection: u64,
    packet_id: u16,
}

/// What the task hands to [`crate::Events`].
pub(crate) enum Delivery {
    Event(Event),
    Message {
        message: Message,
        ack: Option<Ack>,
        /// Its [`message_cost`], given back to the budget when the caller takes it.
        cost: usize,
    },
}

type Connection = Framed<TcpStream, Codec>;

/// A connection that has its CONNACK and SUBACK.
struct Session {
    framed: Connection,
    keep_alive: Duration,
    inflight_limit: usize,
    /// The broker's Maximum Packet Size, or ours when it sent none.
    max_outgoing: u32,
    /// Messages that arrived before the SUBACK, delivered after [`Event::Connected`], each in
    /// its own allocation ([`detach`]).
    early: Vec<Publish>,
    /// Their [`message_cost`] together.
    early_bytes: usize,
}

/// The encoded CONNECT, which holds the password: wiped when dropped, also when the future
/// writing it is cancelled (by [`CONNECT_TIMEOUT`] or a shutdown).
struct WipedOnDrop(BytesMut);

impl Drop for WipedOnDrop {
    fn drop(&mut self) {
        self.0.as_mut().zeroize();
    }
}

/// Why serving a connection ended.
enum Stop {
    Shutdown(Option<oneshot::Sender<()>>),
    Lost(ConnectionError),
}

/// Where and as whom to connect, and what to subscribe to.
struct Target {
    host: String,
    port: u16,
    username: String,
    password_file: PathBuf,
    client_id: Arc<str>,
    subscriptions: Vec<String>,
}

pub(crate) struct Driver {
    target: Target,
    commands: mpsc::Receiver<Command>,
    /// Closed (`recv` returns `None`) once every [`crate::Client`] is dropped.
    shutdown: mpsc::UnboundedReceiver<ShutdownRequest>,
    acks: mpsc::UnboundedReceiver<Ack>,
    /// Unbounded as a channel; [`Budget`] bounds what it holds.
    deliveries: mpsc::UnboundedSender<Delivery>,
    budget: Arc<Budget>,
    /// Numbers the connections, so an acknowledgement never goes to a later connection.
    connection: u64,
    /// The packet ids of this connection's QoS 1 messages not acknowledged yet, in the order
    /// they arrived, which is the order their PUBACKs must go in (MQTT 5, section 4.6).
    pending: VecDeque<u16>,
    /// The warning that the caller dropped [`crate::Events`] was logged.
    warned_consumer_gone: bool,
    /// When the client last warned that it stopped reading because the caller is behind.
    last_pause_warning: Option<Instant>,
}

impl Driver {
    #[allow(
        clippy::too_many_arguments,
        reason = "the channels of one client, wired once in Client::start"
    )]
    pub(crate) fn new(
        config: &Config,
        client_id: Arc<str>,
        subscriptions: Vec<String>,
        commands: mpsc::Receiver<Command>,
        shutdown: mpsc::UnboundedReceiver<ShutdownRequest>,
        acks: mpsc::UnboundedReceiver<Ack>,
        deliveries: mpsc::UnboundedSender<Delivery>,
        budget: Arc<Budget>,
    ) -> Self {
        Self {
            target: Target {
                host: config.host.clone(),
                port: config.port,
                username: config.username.clone(),
                password_file: config.password_file.clone(),
                client_id,
                subscriptions,
            },
            commands,
            shutdown,
            acks,
            deliveries,
            budget,
            connection: 0,
            pending: VecDeque::new(),
            warned_consumer_gone: false,
            last_pause_warning: None,
        }
    }

    /// Connects, serves and reconnects until shut down.
    pub(crate) async fn run(mut self) {
        let mut failures: u32 = 0;
        loop {
            self.connection = self.connection.wrapping_add(1);
            self.pending.clear();
            let error = match self.connect().await {
                Ok(session) => {
                    let since = Instant::now();
                    match self.serve(session).await {
                        Stop::Shutdown(done) => return finish(done),
                        Stop::Lost(error) => {
                            if since.elapsed() >= STABLE_CONNECTION {
                                failures = 0;
                            }
                            error
                        }
                    }
                }
                Err(Stop::Shutdown(done)) => return finish(done),
                Err(Stop::Lost(error)) => error,
            };
            let retry_in = backoff::delay(failures, backoff::jitter());
            failures = failures.saturating_add(1);
            tracing::warn!(
                client_id = %self.target.client_id,
                error = %error,
                retry_in_ms = retry_in.as_millis(),
                "mqtt connection failed or lost; reconnecting"
            );
            self.deliver_event(Event::Disconnected { error, retry_in });
            if let Some(done) = self.wait(retry_in).await {
                return finish(done);
            }
        }
    }

    /// Waits `delay` before the next attempt, and then until the caller has read enough of
    /// [`crate::Events`] for one ([`Budget::fits_an_attempt`]). Publishes fail at once
    /// meanwhile. Returns `Some` when the client shut down.
    async fn wait(&mut self, delay: Duration) -> Option<Option<oneshot::Sender<()>>> {
        let deadline = sleep(delay);
        tokio::pin!(deadline);
        let mut delay_over = false;
        let mut warned = false;
        loop {
            if delay_over {
                if self.budget.fits_an_attempt() {
                    return None;
                }
                if !warned {
                    warned = true;
                    tracing::warn!(
                        client_id = %self.target.client_id,
                        held_bytes = self.budget.held_bytes(),
                        queued_events = self.budget.queued_events(),
                        "the caller is not reading mqtt events; the next connection attempt waits for it"
                    );
                }
            }
            tokio::select! {
                () = &mut deadline, if !delay_over => delay_over = true,
                () = self.budget.space.notified(), if delay_over => {}
                done = self.shutdown.recv() => return Some(done),
                command = self.commands.recv() => match command {
                    // Every Client is gone.
                    None => return Some(None),
                    Some(Command { reply, .. }) => {
                        let _ = reply.send(Err(PublishError::NotConnected));
                    }
                },
                // Acknowledgements of an earlier connection: nothing to send them on.
                Some(_) = self.acks.recv() => {}
            }
        }
    }

    /// The TCP connection, CONNECT and CONNACK, then SUBSCRIBE and SUBACK, within
    /// [`CONNECT_TIMEOUT`]. Meanwhile publishes fail at once with
    /// [`PublishError::NotConnected`], as between attempts, and a shutdown ends the attempt.
    async fn connect(&mut self) -> Result<Session, Stop> {
        let password = Password::read(&self.target.password_file)
            .map_err(|error| Stop::Lost(ConnectionError::Password(error)))?;
        let target = &self.target;
        tracing::debug!(
            client_id = %target.client_id,
            host = %target.host,
            port = target.port,
            username = %target.username,
            "connecting to the mqtt broker"
        );
        let handshake = timeout(CONNECT_TIMEOUT, target.handshake(password, &self.budget));
        tokio::pin!(handshake);
        let (session, subscriptions) = loop {
            tokio::select! {
                result = &mut handshake => match result {
                    Ok(Ok(connected)) => break connected,
                    Ok(Err(error)) => return Err(Stop::Lost(error)),
                    Err(_) => {
                        return Err(Stop::Lost(ConnectionError::Timeout {
                            waiting_for: "the connection to the broker, its CONNACK and SUBACK",
                        }));
                    }
                },
                done = self.shutdown.recv() => return Err(Stop::Shutdown(done)),
                command = self.commands.recv() => match command {
                    None => return Err(Stop::Shutdown(None)),
                    Some(Command { reply, .. }) => {
                        let _ = reply.send(Err(PublishError::NotConnected));
                    }
                },
            }
        };
        tracing::info!(
            client_id = %self.target.client_id,
            host = %self.target.host,
            port = self.target.port,
            keep_alive_s = session.keep_alive.as_secs(),
            inflight = session.inflight_limit,
            max_packet_bytes = session.max_outgoing,
            "connected to the mqtt broker"
        );
        for outcome in &subscriptions {
            match outcome.granted_qos() {
                Some(1) => {}
                Some(qos) => tracing::warn!(
                    client_id = %self.target.client_id,
                    filter = %outcome.filter,
                    granted_qos = qos,
                    "the broker granted a subscription a lower QoS than asked; its messages are not acknowledged"
                ),
                None => tracing::warn!(
                    client_id = %self.target.client_id,
                    filter = %outcome.filter,
                    code = outcome.code,
                    "the broker refused a subscription"
                ),
            }
        }
        self.deliver_event(Event::Connected {
            subscriptions: Subscriptions::new(subscriptions),
        });
        Ok(session)
    }
}

impl Target {
    async fn handshake(
        &self,
        password: Password,
        budget: &Budget,
    ) -> Result<(Session, Vec<SubscriptionOutcome>), ConnectionError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|source| ConnectionError::Io {
                during: "connecting to the broker",
                source,
            })?;
        stream
            .set_nodelay(true)
            .map_err(|source| ConnectionError::Io {
                during: "setting TCP_NODELAY",
                source,
            })?;
        self.send_connect(&mut stream, password).await?;
        let mut framed = Framed::new(
            stream,
            Codec {
                max_incoming_size: Some(MAX_PACKET_BYTES),
                max_outgoing_size: None,
            },
        );
        let connack = match read(&mut framed).await? {
            Packet::ConnAck(connack) => connack,
            other => {
                return Err(ConnectionError::Protocol(format!(
                    "expected CONNACK, received {}",
                    packet_name(&other)
                )));
            }
        };
        let mut session = session(framed, &connack)?;
        let subscriptions = self.subscribe(&mut session, budget).await?;
        Ok((session, subscriptions))
    }

    /// Writes CONNECT straight to the socket from a buffer that is wiped afterwards, so the
    /// password stays in no buffer of the codec.
    async fn send_connect(
        &self,
        stream: &mut TcpStream,
        password: Password,
    ) -> Result<(), ConnectionError> {
        let connect = Connect {
            keep_alive: u16::try_from(KEEP_ALIVE.as_secs()).unwrap_or(u16::MAX),
            client_id: self.client_id.to_string(),
            clean_start: true,
            properties: Some(ConnectProperties {
                receive_maximum: Some(RECEIVE_MAXIMUM),
                max_packet_size: Some(MAX_PACKET_BYTES),
                ..ConnectProperties::new()
            }),
        };
        let mut secret = String::with_capacity(password.expose().len());
        secret.push_str(password.expose());
        drop(password);
        let login = Some(Login {
            username: self.username.clone(),
            password: secret,
        });
        // Allocated once at the packet's size: a growing buffer would leave copies behind.
        let mut buffer = WipedOnDrop(BytesMut::with_capacity(connect.size(&None, &login)));
        let encoded = connect.write(&None, &login, &mut buffer.0);
        if let Some(mut login) = login {
            login.password.zeroize();
        }
        match encoded {
            Ok(_) => match timeout(WRITE_TIMEOUT, stream.write_all(&buffer.0)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(source)) => Err(ConnectionError::Io {
                    during: "sending CONNECT",
                    source,
                }),
                Err(_) => Err(ConnectionError::Timeout {
                    waiting_for: "the broker to accept CONNECT",
                }),
            },
            Err(error) => Err(ConnectionError::Protocol(format!(
                "cannot encode CONNECT: {error}"
            ))),
        }
    }

    /// Subscribes to every filter in one SUBSCRIBE and waits for its SUBACK. Messages that
    /// arrive first are kept for after [`Event::Connected`], within the budget.
    async fn subscribe(
        &self,
        session: &mut Session,
        budget: &Budget,
    ) -> Result<Vec<SubscriptionOutcome>, ConnectionError> {
        if self.subscriptions.is_empty() {
            return Ok(Vec::new());
        }
        let filters = self
            .subscriptions
            .iter()
            .map(|filter| Filter::new(filter.clone(), QoS::AtLeastOnce));
        let mut subscribe = Subscribe::new_many(filters, None);
        subscribe.pkid = SUBSCRIBE_PACKET_ID;
        send(&mut session.framed, Packet::Subscribe(subscribe)).await?;
        loop {
            match read(&mut session.framed).await? {
                Packet::SubAck(suback) if suback.pkid == SUBSCRIBE_PACKET_ID => {
                    if suback.return_codes.len() != self.subscriptions.len() {
                        return Err(ConnectionError::Protocol(format!(
                            "SUBACK has {} reason codes for {} topic filters",
                            suback.return_codes.len(),
                            self.subscriptions.len()
                        )));
                    }
                    return Ok(self
                        .subscriptions
                        .iter()
                        .zip(&suback.return_codes)
                        .map(|(filter, code)| SubscriptionOutcome {
                            filter: filter.clone(),
                            code: codes::suback(code),
                        })
                        .collect());
                }
                Packet::Publish(mut publish) => {
                    // Room for this one was checked before it was read.
                    detach(&mut publish);
                    session.early_bytes = session
                        .early_bytes
                        .saturating_add(message_cost(publish.topic.len(), publish.payload.len()));
                    session.early.push(publish);
                    if !budget.fits(session.early_bytes) {
                        // Every message before the SUBACK is unusual: with a clean start the
                        // broker has nothing queued for a new session.
                        return Err(ConnectionError::Protocol(
                            "more messages before the SUBACK than the client holds".to_owned(),
                        ));
                    }
                }
                Packet::Disconnect(disconnect) => return Err(server_disconnect(&disconnect)),
                other => {
                    return Err(ConnectionError::Protocol(format!(
                        "expected SUBACK, received {}",
                        packet_name(&other)
                    )));
                }
            }
        }
    }
}

impl Driver {
    /// Serves a connection until it is lost or the client shuts down.
    async fn serve(&mut self, mut session: Session) -> Stop {
        let mut inflight = Window::new();
        let stop = self.serve_loop(&mut session, &mut inflight).await;
        let failure = match &stop {
            Stop::Shutdown(_) => PublishError::Closed,
            Stop::Lost(_) => PublishError::ConnectionLost,
        };
        for (_, entry) in inflight.drain() {
            let _ = entry.reply.send(Err(failure.clone()));
        }
        if let Stop::Shutdown(_) = &stop {
            let disconnect = Disconnect::new(DisconnectReasonCode::NormalDisconnection);
            // Best effort: the socket is closed either way.
            let _ = send(&mut session.framed, Packet::Disconnect(disconnect)).await;
            tracing::info!(client_id = %self.target.client_id, "disconnected from the mqtt broker");
        }
        stop
    }

    /// Reads from the broker only while the held messages leave room for one more
    /// ([`Budget::fits`]). While it does not read, it still writes: publishes, acknowledgements
    /// of the messages the caller takes, and a PINGREQ at every keepalive tick, so the broker
    /// keeps the connection. A missing PINGRESP counts only after a whole keepalive of reading,
    /// so a dead broker is noticed late while the caller is behind; a publish still fails after
    /// [`PUBACK_TIMEOUT`] without its PUBACK, which also ends a stall that lasts that long.
    async fn serve_loop(&mut self, session: &mut Session, inflight: &mut Window) -> Stop {
        session.early_bytes = 0;
        for publish in std::mem::take(&mut session.early) {
            if let Err(error) = self.receive(publish) {
                return Stop::Lost(error);
            }
        }
        let mut next_packet_id = SUBSCRIBE_PACKET_ID + 1;
        let mut ping = interval_at(Instant::now() + session.keep_alive, session.keep_alive);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut awaiting_pong = false;
        // Since when the client reads from the broker; `None` while it does not.
        let mut reading_since = Some(Instant::now());
        loop {
            if self.budget.is_consumer_gone()
                && !self.pending.is_empty()
                && let Err(error) = self.acknowledge_pending(session).await
            {
                return Stop::Lost(error);
            }
            let can_read = self.budget.fits(0);
            match (can_read, reading_since) {
                (true, None) => {
                    reading_since = Some(Instant::now());
                    tracing::debug!(client_id = %self.target.client_id, "reading from the mqtt broker again");
                }
                (false, Some(_)) => {
                    reading_since = None;
                    self.warn_paused();
                }
                _ => {}
            }
            // Woken when the caller takes something or drops the events: to read again, or to
            // acknowledge what it left.
            let watch_caller = !can_read || !self.pending.is_empty();
            let can_publish = inflight.len() < session.inflight_limit;
            let puback_deadline = inflight.values().map(|entry| entry.deadline).min();
            let result = tokio::select! {
                packet = session.framed.next(), if can_read => match packet {
                    Some(Ok(packet)) => self.incoming(packet, inflight, &mut awaiting_pong),
                    Some(Err(error)) => Err(codec_error(error, "reading from the broker")),
                    None => Err(ConnectionError::Closed),
                },
                () = self.budget.space.notified(), if watch_caller => Ok(()),
                Some(ack) = self.acks.recv() => self.acknowledge(session, &ack).await,
                // Read even while the in-flight window is full.
                done = self.shutdown.recv() => return Stop::Shutdown(done),
                command = self.commands.recv(), if can_publish => match command {
                    None => return Stop::Shutdown(None),
                    Some(command) => {
                        self.publish(session, inflight, &mut next_packet_id, command).await
                    }
                },
                () = sleep_until(puback_deadline.unwrap_or_else(Instant::now)),
                    if puback_deadline.is_some() =>
                {
                    Err(ConnectionError::Timeout {
                        waiting_for: "the broker's PUBACK",
                    })
                }
                _ = ping.tick() => {
                    let read_a_whole_keepalive = reading_since
                        .is_some_and(|since| since.elapsed() >= session.keep_alive);
                    if awaiting_pong && read_a_whole_keepalive {
                        Err(ConnectionError::KeepAlive)
                    } else {
                        awaiting_pong = true;
                        send(&mut session.framed, Packet::PingReq(PingReq)).await
                    }
                }
            };
            if let Err(error) = result {
                return Stop::Lost(error);
            }
        }
    }

    /// The caller took a QoS 1 message: acknowledge it, when it came on this connection and is
    /// still waiting for its acknowledgement.
    async fn acknowledge(
        &mut self,
        session: &mut Session,
        ack: &Ack,
    ) -> Result<(), ConnectionError> {
        if ack.connection != self.connection {
            return Ok(());
        }
        let Some(position) = self.pending.iter().position(|&id| id == ack.packet_id) else {
            return Ok(());
        };
        // The caller takes messages in order, so this is the oldest one.
        self.pending.remove(position);
        let puback = PubAck::new(ack.packet_id, None);
        send(&mut session.framed, Packet::PubAck(puback)).await
    }

    /// The caller dropped [`crate::Events`]: acknowledge every QoS 1 message it will never
    /// take, in the order they arrived, so the broker goes on sending.
    async fn acknowledge_pending(&mut self, session: &mut Session) -> Result<(), ConnectionError> {
        self.warn_consumer_gone();
        while let Some(packet_id) = self.pending.pop_front() {
            let puback = PubAck::new(packet_id, None);
            send(&mut session.framed, Packet::PubAck(puback)).await?;
        }
        Ok(())
    }

    fn warn_paused(&mut self) {
        if self
            .last_pause_warning
            .is_some_and(|last| last.elapsed() < PAUSE_WARNING_INTERVAL)
        {
            return;
        }
        self.last_pause_warning = Some(Instant::now());
        tracing::warn!(
            client_id = %self.target.client_id,
            held_bytes = self.budget.held_bytes(),
            "the caller is behind on mqtt events; reading from the broker stops until it catches up"
        );
    }

    /// Sends one publish and records it as in flight, unless its caller stopped waiting before
    /// it was taken from the queue or it is larger than the broker accepts.
    async fn publish(
        &self,
        session: &mut Session,
        inflight: &mut Window,
        next_packet_id: &mut u16,
        command: Command,
    ) -> Result<(), ConnectionError> {
        let Command {
            topic,
            payload,
            options,
            reply,
        } = command;
        if reply.is_closed() {
            // The caller's future was dropped: nobody wants this publish any more.
            return Ok(());
        }
        let packet_id = allocate(next_packet_id, inflight);
        let publish = publish_packet(&topic, payload, options, packet_id);
        let size = publish.size();
        if size > usize::try_from(session.max_outgoing).unwrap_or(usize::MAX) {
            let _ = reply.send(Err(PublishError::TooLarge {
                size,
                max: session.max_outgoing,
            }));
            return Ok(());
        }
        tracing::trace!(
            client_id = %self.target.client_id,
            packet_id,
            topic = %topic,
            bytes = size,
            "publish"
        );
        inflight.insert(
            packet_id,
            InFlight {
                reply,
                deadline: Instant::now() + PUBACK_TIMEOUT,
            },
        );
        send(&mut session.framed, Packet::Publish(publish)).await
    }

    /// One packet from the broker on an established connection.
    fn incoming(
        &mut self,
        packet: Packet,
        inflight: &mut Window,
        awaiting_pong: &mut bool,
    ) -> Result<(), ConnectionError> {
        match packet {
            Packet::PubAck(puback) => {
                let Some(entry) = inflight.remove(&puback.pkid) else {
                    return Err(ConnectionError::Protocol(format!(
                        "PUBACK for packet id {} that is not in flight",
                        puback.pkid
                    )));
                };
                let code = codes::puback(puback.reason);
                tracing::trace!(
                    client_id = %self.target.client_id,
                    packet_id = puback.pkid,
                    code,
                    "puback"
                );
                let result = match puback.reason {
                    PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Ok(()),
                    _ => Err(PublishError::Refused {
                        code,
                        reason: puback.properties.and_then(|p| p.reason_string),
                    }),
                };
                // The caller may have stopped waiting.
                let _ = entry.reply.send(result);
                Ok(())
            }
            Packet::Publish(publish) => self.receive(publish),
            Packet::PingResp(_) => {
                *awaiting_pong = false;
                Ok(())
            }
            Packet::Disconnect(disconnect) => Err(server_disconnect(&disconnect)),
            other => Err(ConnectionError::Protocol(format!(
                "unexpected {} from the broker",
                packet_name(&other)
            ))),
        }
    }

    /// A message from a subscription: hands it to [`crate::Events`]. A QoS 1 message is
    /// acknowledged when the caller takes it, or at once when the caller dropped the events.
    fn receive(&mut self, publish: Publish) -> Result<(), ConnectionError> {
        let topic = std::str::from_utf8(&publish.topic)
            .map_err(|_| ConnectionError::Protocol("a topic name is not UTF-8".to_owned()))?;
        if topic.is_empty() {
            // The client allows no topic aliases (Topic Alias Maximum 0), so every PUBLISH
            // names its topic.
            return Err(ConnectionError::Protocol(
                "a PUBLISH without a topic name".to_owned(),
            ));
        }
        let qos = match publish.qos {
            QoS::AtMostOnce => 0,
            QoS::AtLeastOnce => {
                if self.pending.len() >= usize::from(RECEIVE_MAXIMUM) {
                    return Err(ConnectionError::Protocol(
                        "the broker sent more QoS 1 messages than the Receive Maximum".to_owned(),
                    ));
                }
                if self.pending.contains(&publish.pkid) {
                    return Err(ConnectionError::Protocol(format!(
                        "packet id {} reused before its PUBACK",
                        publish.pkid
                    )));
                }
                1
            }
            QoS::ExactlyOnce => {
                return Err(ConnectionError::Protocol(
                    "a QoS 2 message, but every subscription is QoS 1".to_owned(),
                ));
            }
        };
        if self.budget.is_consumer_gone() {
            self.discard(qos, publish.pkid);
            return Ok(());
        }
        tracing::trace!(
            client_id = %self.target.client_id,
            topic = %topic,
            bytes = publish.payload.len(),
            qos,
            "message"
        );
        let cost = message_cost(topic.len(), publish.payload.len());
        let message = Message {
            topic: topic.to_owned(),
            // Its own allocation: a slice would keep the codec's whole read buffer alive, which
            // the budget does not count.
            payload: Bytes::copy_from_slice(&publish.payload),
            qos,
            retain: publish.retain,
        };
        let ack = (qos == 1).then_some(Ack {
            connection: self.connection,
            packet_id: publish.pkid,
        });
        // Counted before it is sent, so the caller never gives back more than was counted.
        self.budget.held_bytes.fetch_add(cost, Ordering::AcqRel);
        if self
            .deliveries
            .send(Delivery::Message { message, ack, cost })
            .is_err()
        {
            self.budget.held_bytes.fetch_sub(cost, Ordering::AcqRel);
            self.discard(qos, publish.pkid);
            return Ok(());
        }
        if qos == 1 {
            self.pending.push_back(publish.pkid);
        }
        Ok(())
    }

    /// The caller dropped [`crate::Events`]: the message is dropped, and a QoS 1 one is
    /// acknowledged with the others ([`Self::acknowledge_pending`]).
    fn discard(&mut self, qos: u8, packet_id: u16) {
        self.warn_consumer_gone();
        if qos == 1 {
            self.pending.push_back(packet_id);
        }
    }

    fn warn_consumer_gone(&mut self) {
        if !self.warned_consumer_gone {
            self.warned_consumer_gone = true;
            tracing::warn!(
                client_id = %self.target.client_id,
                "the mqtt events are no longer read; messages are acknowledged and dropped"
            );
        }
    }

    /// Hands a status event to [`crate::Events`]; [`Self::wait`] keeps them within
    /// [`MAX_QUEUED_EVENTS`].
    fn deliver_event(&self, event: Event) {
        self.budget.queued_events.fetch_add(1, Ordering::AcqRel);
        if self.deliveries.send(Delivery::Event(event)).is_err() {
            // Nobody reads the events any more; publishing goes on.
            self.budget.queued_events.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Gives a received message its own allocations and drops what is not delivered, so what it
/// holds is what [`message_cost`] counts.
fn detach(publish: &mut Publish) {
    publish.topic = Bytes::copy_from_slice(&publish.topic);
    publish.payload = Bytes::copy_from_slice(&publish.payload);
    publish.properties = None;
}

/// The session parameters from a successful CONNACK.
fn session(framed: Connection, connack: &ConnAck) -> Result<Session, ConnectionError> {
    if connack.code != ConnectReturnCode::Success {
        return Err(ConnectionError::Refused {
            code: codes::connect(connack.code),
            reason: connack
                .properties
                .as_ref()
                .and_then(|p| p.reason_string.clone()),
        });
    }
    let properties = connack.properties.as_ref();
    let keep_alive = properties
        .and_then(|p| p.server_keep_alive)
        .filter(|&seconds| seconds > 0)
        .map_or(KEEP_ALIVE, |seconds| {
            Duration::from_secs(u64::from(seconds))
        });
    let receive_maximum = properties
        .and_then(|p| p.receive_max)
        .unwrap_or(u16::MAX)
        .max(1);
    let max_outgoing = properties
        .and_then(|p| p.max_packet_size)
        .map_or(MAX_PACKET_BYTES, |max| max.min(MAX_PACKET_BYTES));
    let mut framed = framed;
    framed.codec_mut().max_outgoing_size = Some(max_outgoing);
    Ok(Session {
        framed,
        keep_alive,
        inflight_limit: usize::from(MAX_INFLIGHT.min(receive_maximum)),
        max_outgoing,
        early: Vec::new(),
        early_bytes: 0,
    })
}

fn publish_packet(topic: &str, payload: Bytes, options: PublishOptions, packet_id: u16) -> Publish {
    let properties = options.message_expiry.map(|expiry| PublishProperties {
        message_expiry_interval: Some(expiry_seconds(expiry)),
        ..PublishProperties::default()
    });
    let mut publish = Publish::new(topic, QoS::AtLeastOnce, payload, properties);
    publish.retain = options.retain;
    publish.pkid = packet_id;
    publish
}

/// Whole seconds, rounded up, at least 1 and at most `u32::MAX`.
fn expiry_seconds(expiry: Duration) -> u32 {
    let seconds = expiry
        .as_secs()
        .saturating_add(u64::from(expiry.subsec_nanos() > 0));
    u32::try_from(seconds.max(1)).unwrap_or(u32::MAX)
}

/// The next packet id not in flight. Terminates: fewer than 65535 publishes are in flight.
fn allocate<T>(next: &mut u16, inflight: &HashMap<u16, T>) -> u16 {
    loop {
        let id = *next;
        *next = if id == u16::MAX {
            SUBSCRIBE_PACKET_ID + 1
        } else {
            id + 1
        };
        if !inflight.contains_key(&id) {
            return id;
        }
    }
}

async fn read(framed: &mut Connection) -> Result<Packet, ConnectionError> {
    match framed.next().await {
        Some(Ok(packet)) => Ok(packet),
        Some(Err(error)) => Err(codec_error(error, "reading from the broker")),
        None => Err(ConnectionError::Closed),
    }
}

async fn send(framed: &mut Connection, packet: Packet) -> Result<(), ConnectionError> {
    match timeout(WRITE_TIMEOUT, framed.send(packet)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(codec_error(error, "writing to the broker")),
        Err(_) => Err(ConnectionError::Timeout {
            waiting_for: "the broker to accept a write",
        }),
    }
}

fn codec_error(error: CodecError, during: &'static str) -> ConnectionError {
    match error {
        CodecError::Io(source) => ConnectionError::Io { during, source },
        // The codec's messages name sizes and codes, never packet contents.
        other => ConnectionError::Protocol(other.to_string()),
    }
}

fn server_disconnect(disconnect: &Disconnect) -> ConnectionError {
    ConnectionError::ServerDisconnect {
        code: disconnect.reason_code as u8,
        reason: disconnect
            .properties
            .as_ref()
            .and_then(|p| p.reason_string.clone()),
    }
}

/// The packet's type, for errors: never its fields, which could hold credentials or payloads.
fn packet_name(packet: &Packet) -> &'static str {
    match packet {
        Packet::Auth(_) => "AUTH",
        Packet::Connect(..) => "CONNECT",
        Packet::ConnAck(_) => "CONNACK",
        Packet::Publish(_) => "PUBLISH",
        Packet::PubAck(_) => "PUBACK",
        Packet::PingReq(_) => "PINGREQ",
        Packet::PingResp(_) => "PINGRESP",
        Packet::Subscribe(_) => "SUBSCRIBE",
        Packet::SubAck(_) => "SUBACK",
        Packet::PubRec(_) => "PUBREC",
        Packet::PubRel(_) => "PUBREL",
        Packet::PubComp(_) => "PUBCOMP",
        Packet::Unsubscribe(_) => "UNSUBSCRIBE",
        Packet::UnsubAck(_) => "UNSUBACK",
        Packet::Disconnect(_) => "DISCONNECT",
    }
}

fn finish(done: Option<oneshot::Sender<()>>) {
    if let Some(done) = done {
        let _ = done.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_ids_skip_the_subscription_and_ids_in_flight() {
        let mut inflight = HashMap::new();
        let mut next = u16::MAX;
        assert_eq!(allocate(&mut next, &inflight), u16::MAX);
        assert_eq!(allocate(&mut next, &inflight), 2);
        inflight.insert(3, ());
        inflight.insert(4, ());
        assert_eq!(allocate(&mut next, &inflight), 5);
    }

    #[test]
    fn the_budget_stops_reading_before_a_message_could_exceed_it() {
        let budget = Budget::default();
        assert!(budget.fits(0));
        assert!(budget.fits_an_attempt());
        budget.held_bytes.store(READ_LIMIT, Ordering::Release);
        assert!(budget.fits(0));
        assert!(!budget.fits(1));
        budget.held_bytes.store(READ_LIMIT + 1, Ordering::Release);
        assert!(!budget.fits(0));
        assert!(!budget.fits_an_attempt());
        budget.message_taken(1);
        assert!(budget.fits(0));
        budget.queued_events.store(
            MAX_QUEUED_EVENTS - EVENTS_PER_ATTEMPT + 1,
            Ordering::Release,
        );
        assert!(!budget.fits_an_attempt());
        // Once the caller is gone, nothing is held for it.
        budget.held_bytes.store(MAX_HELD_BYTES, Ordering::Release);
        budget.consumer_dropped();
        assert!(budget.fits(0));
        assert!(budget.fits_an_attempt());
    }

    #[test]
    fn expiry_is_rounded_up_to_whole_seconds() {
        assert_eq!(expiry_seconds(Duration::from_secs(600)), 600);
        assert_eq!(expiry_seconds(Duration::from_millis(1500)), 2);
        assert_eq!(expiry_seconds(Duration::ZERO), 1);
        assert_eq!(expiry_seconds(Duration::from_secs(u64::MAX)), u32::MAX);
    }
}
