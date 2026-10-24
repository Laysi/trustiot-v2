//! The connection task: connects, subscribes, serves publishes and acknowledgements, and
//! reconnects with a bounded delay. It owns the socket; callers reach it through channels.
//!
//! Only rumqttc's MQTT 5 packet types and codec are used. The packet ids, the keepalive and the
//! matching of each PUBACK to its publish are kept here, so a reason code always reaches the
//! caller of the publish it belongs to (README.md, "為什麼這樣用 rumqttc").

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, timeout};
use tokio_util::codec::Framed;
use zeroize::Zeroize as _;

use crate::config::{Config, Password};
use crate::{
    CONNECT_TIMEOUT, ConnectionError, Event, KEEP_ALIVE, MAX_INFLIGHT, MAX_PACKET_BYTES,
    MAX_QUEUED_MESSAGES, Message, PublishError, PublishOptions, RECEIVE_MAXIMUM, STABLE_CONNECTION,
    SubscriptionOutcome, WRITE_TIMEOUT, backoff, codes,
};

/// The packet id of the SUBSCRIBE sent on every connection; publishes take the others.
const SUBSCRIBE_PACKET_ID: u16 = 1;

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
    Message { message: Message, ack: Option<Ack> },
}

type Connection = Framed<TcpStream, Codec>;

/// A connection that has its CONNACK and SUBACK.
struct Session {
    framed: Connection,
    keep_alive: Duration,
    inflight_limit: usize,
    /// The broker's Maximum Packet Size, or ours when it sent none.
    max_outgoing: u32,
    /// Messages that arrived before the SUBACK, delivered after [`Event::Connected`].
    early: Vec<Publish>,
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
    deliveries: mpsc::UnboundedSender<Delivery>,
    queued: Arc<AtomicUsize>,
    /// Numbers the connections, so an acknowledgement never goes to a later connection.
    connection: u64,
    /// QoS 1 messages of this connection delivered but not acknowledged yet.
    unacknowledged: usize,
    /// QoS 0 messages dropped because [`crate::Events`] was full, for the warning.
    dropped: u64,
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
        queued: Arc<AtomicUsize>,
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
            queued,
            connection: 0,
            unacknowledged: 0,
            dropped: 0,
        }
    }

    /// Connects, serves and reconnects until shut down.
    pub(crate) async fn run(mut self) {
        let mut failures: u32 = 0;
        loop {
            self.connection = self.connection.wrapping_add(1);
            self.unacknowledged = 0;
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
            self.deliver(Delivery::Event(Event::Disconnected { error, retry_in }));
            if let Some(done) = self.wait(retry_in).await {
                return finish(done);
            }
        }
    }

    /// Waits `delay` before the next attempt. Publishes fail at once meanwhile. Returns
    /// `Some` when the client shut down.
    async fn wait(&mut self, delay: Duration) -> Option<Option<oneshot::Sender<()>>> {
        let deadline = sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return None,
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
        let handshake = timeout(CONNECT_TIMEOUT, target.handshake(password));
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
        for outcome in subscriptions.iter().filter(|outcome| !outcome.is_granted()) {
            tracing::warn!(
                client_id = %self.target.client_id,
                filter = %outcome.filter,
                code = outcome.code,
                "the broker refused a subscription"
            );
        }
        self.deliver(Delivery::Event(Event::Connected { subscriptions }));
        Ok(session)
    }
}

impl Target {
    async fn handshake(
        &self,
        password: Password,
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
        let subscriptions = self.subscribe(&mut session).await?;
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
    /// arrive first are kept for after [`Event::Connected`].
    async fn subscribe(
        &self,
        session: &mut Session,
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
                Packet::Publish(publish) => {
                    if session.early.len() >= usize::from(RECEIVE_MAXIMUM) + MAX_QUEUED_MESSAGES {
                        return Err(ConnectionError::Protocol(
                            "too many messages before the SUBACK".to_owned(),
                        ));
                    }
                    session.early.push(publish);
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
        let mut inflight: HashMap<u16, oneshot::Sender<Result<(), PublishError>>> = HashMap::new();
        let stop = self.serve_loop(&mut session, &mut inflight).await;
        let failure = match &stop {
            Stop::Shutdown(_) => PublishError::Closed,
            Stop::Lost(_) => PublishError::ConnectionLost,
        };
        for (_, reply) in inflight.drain() {
            let _ = reply.send(Err(failure.clone()));
        }
        if let Stop::Shutdown(_) = &stop {
            let disconnect = Disconnect::new(DisconnectReasonCode::NormalDisconnection);
            // Best effort: the socket is closed either way.
            let _ = send(&mut session.framed, Packet::Disconnect(disconnect)).await;
            tracing::info!(client_id = %self.target.client_id, "disconnected from the mqtt broker");
        }
        stop
    }

    async fn serve_loop(
        &mut self,
        session: &mut Session,
        inflight: &mut HashMap<u16, oneshot::Sender<Result<(), PublishError>>>,
    ) -> Stop {
        for publish in std::mem::take(&mut session.early) {
            if let Err(error) = self.receive(publish) {
                return Stop::Lost(error);
            }
        }
        let mut next_packet_id = SUBSCRIBE_PACKET_ID + 1;
        let mut ping = interval_at(Instant::now() + session.keep_alive, session.keep_alive);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut awaiting_pong = false;
        loop {
            let can_publish = inflight.len() < session.inflight_limit;
            let result = tokio::select! {
                packet = session.framed.next() => match packet {
                    Some(Ok(packet)) => self.incoming(packet, inflight, &mut awaiting_pong),
                    Some(Err(error)) => Err(codec_error(error, "reading from the broker")),
                    None => Err(ConnectionError::Closed),
                },
                Some(ack) = self.acks.recv() => {
                    if ack.connection == self.connection {
                        self.unacknowledged = self.unacknowledged.saturating_sub(1);
                        let puback = PubAck::new(ack.packet_id, None);
                        send(&mut session.framed, Packet::PubAck(puback)).await
                    } else {
                        Ok(())
                    }
                }
                // Read even while the in-flight window is full.
                done = self.shutdown.recv() => return Stop::Shutdown(done),
                command = self.commands.recv(), if can_publish => match command {
                    None => return Stop::Shutdown(None),
                    Some(command) => {
                        self.publish(session, inflight, &mut next_packet_id, command).await
                    }
                },
                _ = ping.tick() => {
                    if awaiting_pong {
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

    /// Sends one publish and records it as in flight, unless its caller stopped waiting before
    /// it was taken from the queue or it is larger than the broker accepts.
    async fn publish(
        &self,
        session: &mut Session,
        inflight: &mut HashMap<u16, oneshot::Sender<Result<(), PublishError>>>,
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
        inflight.insert(packet_id, reply);
        send(&mut session.framed, Packet::Publish(publish)).await
    }

    /// One packet from the broker on an established connection.
    fn incoming(
        &mut self,
        packet: Packet,
        inflight: &mut HashMap<u16, oneshot::Sender<Result<(), PublishError>>>,
        awaiting_pong: &mut bool,
    ) -> Result<(), ConnectionError> {
        match packet {
            Packet::PubAck(puback) => {
                let Some(reply) = inflight.remove(&puback.pkid) else {
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
                let _ = reply.send(result);
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
    /// acknowledged when the caller takes it.
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
        let ack = match publish.qos {
            QoS::AtMostOnce => {
                if self.queued.load(Ordering::Relaxed) >= MAX_QUEUED_MESSAGES {
                    self.dropped = self.dropped.saturating_add(1);
                    if self.dropped.is_power_of_two() {
                        tracing::warn!(
                            client_id = %self.target.client_id,
                            dropped = self.dropped,
                            "the caller is not reading mqtt events; QoS 0 messages are dropped"
                        );
                    }
                    return Ok(());
                }
                None
            }
            QoS::AtLeastOnce => {
                if self.unacknowledged >= usize::from(RECEIVE_MAXIMUM) {
                    return Err(ConnectionError::Protocol(
                        "the broker sent more QoS 1 messages than the Receive Maximum".to_owned(),
                    ));
                }
                self.unacknowledged += 1;
                Some(Ack {
                    connection: self.connection,
                    packet_id: publish.pkid,
                })
            }
            QoS::ExactlyOnce => {
                return Err(ConnectionError::Protocol(
                    "a QoS 2 message, but every subscription is QoS 1".to_owned(),
                ));
            }
        };
        tracing::trace!(
            client_id = %self.target.client_id,
            topic = %topic,
            bytes = publish.payload.len(),
            "message"
        );
        let message = Message {
            topic: topic.to_owned(),
            payload: publish.payload,
            retain: publish.retain,
        };
        self.queued.fetch_add(1, Ordering::Relaxed);
        if self
            .deliveries
            .send(Delivery::Message { message, ack })
            .is_err()
        {
            // Nobody reads the events any more.
            self.queued.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn deliver(&self, delivery: Delivery) {
        // Nobody may read the events any more; publishing goes on.
        let _ = self.deliveries.send(delivery);
    }
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
    fn expiry_is_rounded_up_to_whole_seconds() {
        assert_eq!(expiry_seconds(Duration::from_secs(600)), 600);
        assert_eq!(expiry_seconds(Duration::from_millis(1500)), 2);
        assert_eq!(expiry_seconds(Duration::ZERO), 1);
        assert_eq!(expiry_seconds(Duration::from_secs(u64::MAX)), u32::MAX);
    }
}
