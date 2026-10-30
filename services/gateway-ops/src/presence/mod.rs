//! Whether each bound gateway is connected (Laysi/trustiot-v2#112, rebuild checklist C3;
//! services/gateway-ops/README.md, "在線狀態").
//!
//! - **Its own broker connection.** The presence task has a connection of its own (the same
//!   account, `service:gateway-ops`) with two subscriptions without a share group, every
//!   gateway's `up/status` and `up/heartbeat`: every replica receives every message of a
//!   gateway in the order the broker delivers them (and the retained statuses again after each
//!   (re)connection), so no replica applies a status without the heartbeats around it; the
//!   rules make the replicas' copies converge ([`rules`]). Readiness check `presence` passes
//!   while both are granted with QoS 1.
//! - **Status.** A gateway's `online` and `offline` are retained and its last will is an
//!   `offline` (`connection_lost`) the broker publishes when the connection ends without a
//!   DISCONNECT, or when nothing arrives for 1.5 times the gateway's keepalive (60 s, so 90 s;
//!   decision 6). Both reach the service as they are published: the will within moments of a
//!   dropped connection, and a half-open connection's will by the broker's keepalive check.
//! - **Heartbeats.** Each refreshes `last_seen_at`, the agent version, the interval it
//!   announces and the configuration version the gateway uses.
//! - **Heartbeat timeout.** Every `presence.sweep_interval`, the task looks for online gateways
//!   whose session went quiet for [`store::MISSED_HEARTBEATS`] of its announced intervals plus
//!   `presence.heartbeat_grace`, and marks them offline (`heartbeat_timeout`): the backstop for
//!   a gateway whose broker connection stays up while its agent stopped reporting, or whose
//!   last will was lost (a broker restart loses it with the retained statuses). Time counts only
//!   while the task could hear heartbeats: after its own broker connection or the database came
//!   back, it waits the whole timeout again, so an outage of the service never makes every
//!   gateway look offline.
//! - **Order and fairness.** The connection is read as fast as messages come: each one is
//!   checked and queued in its gateway's queue ([`backlog`]), and a worker applies them one at
//!   a time, the gateways taking turns, each message in one transaction that locks the gateway's
//!   row, by the rules of [`rules`]. A gateway that floods its topics fills only its own queue
//!   and delays the others by one message per turn; the sweep runs on its own schedule
//!   regardless.
//! - **Retained replays.** A retained status the broker replays on subscribing does not create a
//!   gateway's first row: it may be from before the gateway's current binding. The next live
//!   status or heartbeat does.
//! - **Database errors.** After one, the worker leaves the database alone for [`RETRY_DELAY`]
//!   while messages keep queueing (beyond the backlog's limits they are dropped), and the
//!   timeout's clock starts again after it.
//! - **Changes.** Each change of a gateway's state is one INFO line, `gateway presence changed`,
//!   and the counter `gateway_ops.presence.changes`. They are not outbox events yet: an outbox
//!   producer needs a consumer (tools/migrate), and none follows presence yet (README.md, "在線
//!   狀態").

pub mod backlog;
pub mod rules;
pub mod store;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mqtt_client::{Client, Event, Events, Message};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use protocol::GatewayId;
use protocol::health::{Heartbeat, Status};
use protocol::topic::{Channel, parse_topic};
use tokio::sync::Semaphore;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use self::backlog::{Backlog, Pending};
use self::rules::{Decision, Offline, Signal, State};
use self::store::{Binding, Overdue, PresenceStore, StoreError};
use crate::delivery::reports::LogBudget;

/// How long the task leaves the database alone after an error.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Most pages of overdue gateways one sweep handles; the next sweep continues.
pub const MAX_PAGES_PER_SWEEP: usize = 10;

/// `presence.*`: the heartbeat timeout's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Added to [`store::MISSED_HEARTBEATS`] intervals before a quiet gateway is offline.
    pub heartbeat_grace: Duration,
    /// How often the task looks for overdue gateways.
    pub sweep_interval: Duration,
}

/// The task's connections and settings.
pub struct Parts {
    /// The broker settings, the delivery's.
    pub mqtt: mqtt_client::Config,
    /// The reads and writes.
    pub store: PresenceStore,
    /// Bounds the platform-role reads, shared with the delivery's (`crate::PLATFORM_CONNECTIONS`
    /// keeps [`crate::delivery::CONCURRENCY`] connections for both).
    pub reads: Arc<Semaphore>,
    /// The timeout.
    pub settings: Settings,
}

/// Why the task could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The broker client refused to start (its settings or its password file).
    #[error(transparent)]
    Mqtt(#[from] mqtt_client::StartError),
}

/// Why the `presence` readiness check fails.
#[derive(Debug, thiserror::Error)]
#[error("not connected to the mqtt broker with the status and heartbeat subscriptions granted")]
pub struct NotSubscribed;

/// Why a message was refused before the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The topic is not a gateway's `up/status` or `up/heartbeat`.
    Topic,
    /// A heartbeat carries the retain flag, which a gateway never sets on one.
    RetainedHeartbeat,
    /// The payload does not decode or breaks the protocol's rules.
    Decode,
    /// An `offline` with a reason this build does not know.
    UnknownReason,
}

impl Refusal {
    /// The name in logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::RetainedHeartbeat => "retained_heartbeat",
            Self::Decode => "decode",
            Self::UnknownReason => "unknown_reason",
        }
    }
}

/// A checked message: its gateway, and what it says (`None` for an empty status payload, a
/// cleared retained status, which says nothing about the connection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The gateway the topic names.
    pub gateway: GatewayId,
    /// What it says.
    pub signal: Option<Signal>,
}

/// Reads a message of the task's subscriptions. The gateway comes from the topic only, which
/// the broker lets a gateway publish to for itself only (crates/protocol, "ACL").
///
/// # Errors
///
/// The refusal, with the gateway when the topic named one.
pub fn check_message(message: &Message) -> Result<Checked, (Refusal, Option<GatewayId>)> {
    let Ok(parsed) = parse_topic(&message.topic) else {
        return Err((Refusal::Topic, None));
    };
    let gateway = parsed.gateway;
    let signal = match parsed.channel {
        Channel::Status => {
            if message.payload.is_empty() {
                return Ok(Checked {
                    gateway,
                    signal: None,
                });
            }
            match Status::decode(&message.payload) {
                Ok(Status::Online(online)) => Signal::Online {
                    session: online.session,
                    agent_version: online.agent_version,
                },
                Ok(Status::Offline(offline)) => {
                    match Signal::offline(offline.session, &offline.reason) {
                        Some(signal) => signal,
                        None => return Err((Refusal::UnknownReason, Some(gateway))),
                    }
                }
                Err(_) => return Err((Refusal::Decode, Some(gateway))),
            }
        }
        Channel::Heartbeat => {
            if message.retain {
                return Err((Refusal::RetainedHeartbeat, Some(gateway)));
            }
            match Heartbeat::decode(&message.payload) {
                Ok(heartbeat) => Signal::Heartbeat {
                    session: heartbeat.session,
                    sent_at: heartbeat.sent_at,
                    agent_version: heartbeat.agent_version,
                    interval_s: heartbeat.interval_s,
                    config_version: heartbeat.config_version,
                },
                Err(_) => return Err((Refusal::Decode, Some(gateway))),
            }
        }
        _ => return Err((Refusal::Topic, Some(gateway))),
    };
    Ok(Checked {
        gateway,
        signal: Some(signal),
    })
}

/// What the task counted since it started; the same events as its metrics, for tests and
/// diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Messages written to a row.
    pub written: u64,
    /// Changes of a gateway's state, the heartbeat timeouts included.
    pub changes: u64,
    /// Of those, heartbeat timeouts.
    pub timeouts: u64,
    /// Messages that left the row as it was ([`rules::Ignored`]), and cleared statuses.
    pub ignored: u64,
    /// Messages of gateways without a current binding.
    pub unbound: u64,
    /// Messages refused before the database ([`Refusal`]).
    pub refused: u64,
    /// Database errors.
    pub failures: u64,
    /// Messages dropped because the backlog was full ([`backlog::Full`]).
    pub dropped: u64,
}

#[derive(Debug, Default)]
struct Counters {
    written: AtomicU64,
    changes: AtomicU64,
    timeouts: AtomicU64,
    ignored: AtomicU64,
    unbound: AtomicU64,
    refused: AtomicU64,
    failures: AtomicU64,
    dropped: AtomicU64,
}

fn add(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    fn snapshot(&self) -> Counts {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Counts {
            written: get(&self.written),
            changes: get(&self.changes),
            timeouts: get(&self.timeouts),
            ignored: get(&self.ignored),
            unbound: get(&self.unbound),
            refused: get(&self.refused),
            failures: get(&self.failures),
            dropped: get(&self.dropped),
        }
    }
}

/// The task's metrics (README.md, "指標").
#[derive(Debug)]
struct Metrics {
    changes: Counter<u64>,
    ignored: Counter<u64>,
    refused: Counter<u64>,
    failures: Counter<u64>,
    dropped: Counter<u64>,
}

impl Metrics {
    fn new() -> Self {
        let meter = opentelemetry::global::meter("gateway-ops");
        Self {
            changes: meter
                .u64_counter("gateway_ops.presence.changes")
                .with_description(
                    "Changes of a gateway's presence, by the new state and the offline reason",
                )
                .build(),
            ignored: meter
                .u64_counter("gateway_ops.presence.ignored")
                .with_description(
                    "Gateway statuses and heartbeats that left the stored presence as it was",
                )
                .build(),
            refused: meter
                .u64_counter("gateway_ops.presence.refused")
                .with_description("Gateway statuses and heartbeats refused before the database")
                .build(),
            failures: meter
                .u64_counter("gateway_ops.presence.failures")
                .with_description("Database errors of the presence task")
                .build(),
            dropped: meter
                .u64_counter("gateway_ops.presence.dropped")
                .with_description(
                    "Gateway statuses and heartbeats dropped because the presence task's \
                     backlog was full",
                )
                .build(),
        }
    }
}

struct Inner {
    store: PresenceStore,
    reads: Arc<Semaphore>,
    settings: Settings,
    ready: AtomicBool,
    counters: Counters,
    metrics: Metrics,
    log_budget: LogBudget,
    backlog: Backlog,
    hearing: Mutex<Hearing>,
}

/// The presence task, started: its broker connection is being made. [`Presence::run`] works
/// until stopped.
pub struct Presence {
    inner: Arc<Inner>,
    client: Client,
    events: Events,
}

/// What the rest of the service, and tests, use of a running task. Cheap to clone.
#[derive(Clone)]
pub struct PresenceHandle(Arc<Inner>);

impl std::fmt::Debug for PresenceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresenceHandle").finish_non_exhaustive()
    }
}

impl PresenceHandle {
    /// The counts so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.0.counters.snapshot()
    }

    /// The readiness check.
    ///
    /// # Errors
    ///
    /// [`NotSubscribed`] while the connection is down, being made, or a subscription is not
    /// granted with QoS 1.
    pub fn check(&self) -> Result<(), NotSubscribed> {
        if self.0.ready.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(NotSubscribed)
        }
    }
}

/// Since when the task could hear every gateway, and until when it leaves the database alone.
#[derive(Debug, Default)]
struct Hearing {
    /// Set while connected with the subscriptions granted: the later of the connection and the
    /// end of the last pause.
    since: Option<Instant>,
    /// The end of the pause after a database error.
    paused_until: Option<Instant>,
}

impl Hearing {
    fn paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|until| now < until)
    }

    fn pause(&mut self, now: Instant) {
        let until = now + RETRY_DELAY;
        self.paused_until = Some(until);
        // Messages are dropped until then: the timeout's clock starts again at its end.
        if self.since.is_some() {
            self.since = Some(until);
        }
    }

    /// How long the task has heard every gateway without a gap, `None` while it cannot.
    fn heard_for(&self, now: Instant) -> Option<Duration> {
        if self.paused(now) {
            return None;
        }
        self.since.map(|since| now.saturating_duration_since(since))
    }
}

impl Presence {
    /// Starts the broker client with the status and heartbeat subscriptions.
    ///
    /// # Errors
    ///
    /// When the broker settings or the password file are unusable.
    pub fn start(parts: Parts) -> Result<Self, StartError> {
        let (client, events) = Client::start(
            &parts.mqtt,
            vec![Channel::Status.filter(), Channel::Heartbeat.filter()],
        )?;
        Ok(Self {
            inner: Arc::new(Inner {
                store: parts.store,
                reads: parts.reads,
                settings: parts.settings,
                ready: AtomicBool::new(false),
                counters: Counters::default(),
                metrics: Metrics::new(),
                log_budget: LogBudget::new(),
                backlog: Backlog::default(),
                hearing: Mutex::new(Hearing::default()),
            }),
            client,
            events,
        })
    }

    /// The handle for readiness and tests.
    #[must_use]
    pub fn handle(&self) -> PresenceHandle {
        PresenceHandle(Arc::clone(&self.inner))
    }

    /// Reads the connection's events and applies the messages and the timeout's sweeps until
    /// `stop` is cancelled; then it disconnects.
    pub async fn run(self, stop: CancellationToken) {
        let Self {
            inner,
            client,
            mut events,
        } = self;
        let receive = async {
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => break,
                    event = events.recv() => match event {
                        None => {
                            inner.ready.store(false, Ordering::Release);
                            inner.hearing().since = None;
                            tracing::error!(
                                "the mqtt client ended; gateway presence is no longer followed \
                                 and the service is not ready"
                            );
                            break;
                        }
                        Some(event) => inner.event(event),
                    },
                }
            }
        };
        tokio::join!(receive, Arc::clone(&inner).work(stop.clone()));
        inner.ready.store(false, Ordering::Release);
        client.shutdown().await;
    }
}

impl Inner {
    fn hearing(&self) -> MutexGuard<'_, Hearing> {
        self.hearing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One event of the connection; never waits.
    fn event(&self, event: Event) {
        match event {
            Event::Connected { subscriptions } => {
                let granted = subscriptions.all_granted();
                self.ready.store(granted, Ordering::Release);
                self.hearing().since = granted.then(Instant::now);
                if granted {
                    tracing::info!(
                        "connected to the mqtt broker; gateway statuses and heartbeats subscribed"
                    );
                } else {
                    let codes: Vec<u8> = subscriptions.iter().map(|outcome| outcome.code).collect();
                    tracing::error!(
                        suback_codes = ?codes,
                        "the mqtt broker did not grant the gateway status and heartbeat \
                         subscriptions with QoS 1"
                    );
                }
            }
            Event::Disconnected { error, retry_in } => {
                self.ready.store(false, Ordering::Release);
                self.hearing().since = None;
                tracing::warn!(
                    error = %error,
                    retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX),
                    "no presence connection to the mqtt broker; reconnecting"
                );
            }
            Event::Message(message) => self.receive(&message),
        }
    }

    /// Checks a message and queues it for the worker.
    fn receive(&self, message: &Message) {
        let checked = match check_message(message) {
            Ok(checked) => checked,
            Err((refusal, gateway)) => {
                add(&self.counters.refused);
                self.metrics
                    .refused
                    .add(1, &[KeyValue::new("refusal", refusal.as_str())]);
                if let Some(suppressed) = self.log_budget.allow(Instant::now()) {
                    tracing::warn!(
                        hardware_id = gateway.as_ref().map(GatewayId::as_str),
                        refusal = refusal.as_str(),
                        suppressed,
                        "gateway status or heartbeat refused"
                    );
                }
                return;
            }
        };
        let Some(signal) = checked.signal else {
            add(&self.counters.ignored);
            self.metrics
                .ignored
                .add(1, &[KeyValue::new("reason", "cleared")]);
            return;
        };
        let now = Instant::now();
        let gateway = checked.gateway;
        let pending = Pending {
            gateway: gateway.clone(),
            signal,
            replayed: message.retain,
            received: now,
        };
        if let Err(full) = self.backlog.push(pending) {
            add(&self.counters.dropped);
            self.metrics
                .dropped
                .add(1, &[KeyValue::new("reason", full.as_str())]);
            if full == backlog::Full::Total {
                // Any gateway's heartbeat may be among the dropped: the timeout's clock starts
                // again.
                let mut hearing = self.hearing();
                if hearing.since.is_some() {
                    hearing.since = Some(now);
                }
            }
            if let Some(suppressed) = self.log_budget.allow(now) {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    backlog = full.as_str(),
                    suppressed,
                    "gateway status or heartbeat dropped: too many waiting"
                );
            }
        }
    }

    /// Applies the queued messages and runs the sweeps until `stop` is cancelled.
    async fn work(self: Arc<Self>, stop: CancellationToken) {
        let mut sweeps = tokio::time::interval(self.settings.sweep_interval);
        sweeps.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                // Ready once per interval, so it cannot starve the messages, nor they it.
                _ = sweeps.tick() => self.sweep().await,
                pending = self.backlog.next() => self.apply(pending).await,
            }
            let paused_until = self.hearing().paused_until;
            if let Some(until) = paused_until.filter(|until| Instant::now() < *until) {
                tokio::select! {
                    () = stop.cancelled() => break,
                    () = tokio::time::sleep_until(until) => {}
                }
            }
        }
    }

    /// Applies one message.
    async fn apply(&self, pending: Pending) {
        let Pending {
            gateway,
            signal,
            replayed,
            ..
        } = pending;
        let binding = {
            let _permit = self.reads.acquire().await;
            self.store.binding(&gateway).await
        };
        let binding = match binding {
            Ok(Some(binding)) if binding.bound => binding,
            Ok(_) => {
                self.unbound(&gateway);
                return;
            }
            Err(error) => {
                self.failed(&error, Some(&gateway));
                return;
            }
        };
        match self
            .store
            .apply(&binding, &gateway, &signal, replayed)
            .await
        {
            Ok(None) => self.unbound(&gateway),
            Ok(Some(applied)) => match applied.decision {
                Decision::Write(write) => {
                    add(&self.counters.written);
                    if write.changed {
                        self.changed(&binding, &gateway, write.state, write.session.as_str());
                    }
                }
                Decision::Ignore(ignored) => {
                    add(&self.counters.ignored);
                    self.metrics
                        .ignored
                        .add(1, &[KeyValue::new("reason", ignored.as_str())]);
                }
            },
            Err(error) => self.failed(&error, Some(&gateway)),
        }
    }

    /// A message of a gateway without a current binding, or whose binding ended before the
    /// write.
    fn unbound(&self, gateway: &GatewayId) {
        // An unbound gateway has no broker role and cannot publish (decision 6); one whose
        // unbinding the service has seen but the broker not yet is ignored.
        add(&self.counters.unbound);
        self.metrics
            .ignored
            .add(1, &[KeyValue::new("reason", "unbound")]);
        tracing::debug!(
            hardware_id = gateway.as_str(),
            "status or heartbeat of a gateway without a current binding ignored"
        );
    }

    /// Marks the overdue gateways offline.
    async fn sweep(&self) {
        if !self.ready.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        let Some(heard_for) = self.hearing().heard_for(now) else {
            return;
        };
        // A gateway's latest message may still wait in the backlog: everything received before
        // the oldest waiting one has been applied, so the timeout counts only up to then.
        let lag = self.backlog.oldest().map_or(Duration::ZERO, |oldest| {
            now.saturating_duration_since(oldest)
        });
        let grace = self.settings.heartbeat_grace + lag;
        for _ in 0..MAX_PAGES_PER_SWEEP {
            let page = {
                let _permit = self.reads.acquire().await;
                self.store.overdue(heard_for, grace).await
            };
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    self.failed(&error, None);
                    return;
                }
            };
            for overdue in &page {
                match self.store.time_out(overdue).await {
                    Ok(true) => {
                        add(&self.counters.timeouts);
                        self.timed_out(overdue);
                    }
                    // A status or heartbeat changed the row since the search.
                    Ok(false) => {}
                    Err(error) => {
                        self.failed(&error, Some(&overdue.hardware_id));
                        return;
                    }
                }
            }
            if page.len() < usize::try_from(store::OVERDUE_PAGE).unwrap_or(usize::MAX) {
                return;
            }
        }
    }
    fn changed(&self, binding: &Binding, gateway: &GatewayId, state: State, session: &str) {
        add(&self.counters.changes);
        let reason = state.reason().map(Offline::as_str);
        self.metrics.changes.add(
            1,
            &[
                KeyValue::new("state", state.column()),
                KeyValue::new("reason", reason.unwrap_or("none")),
            ],
        );
        tracing::info!(
            hardware_id = gateway.as_str(),
            tenant_id = %binding.tenant,
            binding_id = %binding.binding,
            state = state.column(),
            offline_reason = reason,
            session,
            "gateway presence changed"
        );
    }

    fn timed_out(&self, overdue: &Overdue) {
        let binding = Binding {
            tenant: overdue.tenant,
            binding: overdue.binding,
            bound: true,
        };
        self.changed(
            &binding,
            &overdue.hardware_id,
            State::Offline(Offline::HeartbeatTimeout),
            &overdue.session,
        );
    }

    fn failed(&self, error: &StoreError, gateway: Option<&GatewayId>) {
        add(&self.counters.failures);
        self.metrics.failures.add(1, &[]);
        let corrupt = matches!(error, StoreError::Corrupt(_));
        if corrupt {
            tracing::error!(
                hardware_id = gateway.map(GatewayId::as_str),
                error_kind = error.log_kind(),
                "gateway presence not written: a stored row breaks the rules"
            );
        } else {
            tracing::warn!(
                hardware_id = gateway.map(GatewayId::as_str),
                error_kind = error.log_kind(),
                retry_in_ms = u64::try_from(RETRY_DELAY.as_millis()).unwrap_or(u64::MAX),
                "gateway presence not written: the database failed; pausing"
            );
            self.hearing().pause(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use protocol::health::{Offline as OfflineStatus, OfflineReason, Online};
    use protocol::{AgentVersion, SessionId, V1};

    use super::*;

    fn message(topic: &str, payload: Vec<u8>, retain: bool) -> Message {
        Message {
            topic: topic.to_owned(),
            payload: Bytes::from(payload),
            qos: 1,
            retain,
        }
    }

    fn gateway() -> GatewayId {
        GatewayId::new("gw-0001").expect("a gateway ID")
    }

    fn session() -> SessionId {
        SessionId::new("s1").expect("a session ID")
    }

    #[test]
    fn statuses_are_read_from_their_gateways_topic() {
        let topic = Channel::Status.topic(&gateway());
        let online = Status::Online(Online {
            v: V1,
            session: session(),
            agent_version: AgentVersion::new("1.2.3").expect("a version"),
            protocol_rev: 1,
        });
        let checked = check_message(&message(&topic, online.encode().expect("encode"), true))
            .expect("accepted");
        assert_eq!(checked.gateway, gateway());
        assert!(matches!(checked.signal, Some(Signal::Online { .. })));

        let will = Status::last_will(session()).encode().expect("encode");
        let checked = check_message(&message(&topic, will, true)).expect("accepted");
        assert_eq!(
            checked.signal,
            Some(Signal::Offline {
                session: session(),
                reason: Offline::ConnectionLost
            })
        );

        let cleared = check_message(&message(&topic, Vec::new(), true)).expect("accepted");
        assert_eq!(cleared.signal, None);
    }

    #[test]
    fn bad_messages_are_refused() {
        let status = Channel::Status.topic(&gateway());
        assert_eq!(
            check_message(&message(&status, vec![0xff, 0x00], false)).expect_err("garbage"),
            (Refusal::Decode, Some(gateway()))
        );
        let unknown = Status::Offline(OfflineStatus {
            v: V1,
            session: session(),
            reason: OfflineReason::Other("later".into()),
        });
        assert_eq!(
            check_message(&message(&status, unknown.encode().expect("encode"), false))
                .expect_err("unknown reason"),
            (Refusal::UnknownReason, Some(gateway()))
        );
        let heartbeat = Channel::Heartbeat.topic(&gateway());
        assert_eq!(
            check_message(&message(&heartbeat, vec![0xa0], true)).expect_err("retained"),
            (Refusal::RetainedHeartbeat, Some(gateway()))
        );
        assert_eq!(
            check_message(&message(&heartbeat, vec![0xa0], false)).expect_err("empty map"),
            (Refusal::Decode, Some(gateway()))
        );
        assert_eq!(
            check_message(&message(
                &Channel::ConfigReport.topic(&gateway()),
                vec![0xa0],
                false
            ))
            .expect_err("another channel"),
            (Refusal::Topic, Some(gateway()))
        );
        assert_eq!(
            check_message(&message("trustiot/v1/gw/+/up/status", vec![0xa0], false))
                .expect_err("not a gateway"),
            (Refusal::Topic, None)
        );
    }

    #[test]
    fn the_timeout_clock_restarts_after_a_gap() {
        let start = Instant::now();
        let mut hearing = Hearing::default();
        assert_eq!(hearing.heard_for(start), None);
        hearing.since = Some(start);
        assert_eq!(
            hearing.heard_for(start + Duration::from_secs(30)),
            Some(Duration::from_secs(30))
        );
        let failed = start + Duration::from_secs(30);
        hearing.pause(failed);
        assert!(hearing.paused(failed));
        assert_eq!(hearing.heard_for(failed + Duration::from_secs(1)), None);
        let after = failed + RETRY_DELAY + Duration::from_secs(2);
        assert!(!hearing.paused(after));
        assert_eq!(hearing.heard_for(after), Some(Duration::from_secs(2)));
    }
}
