//! The delivery of the stored configurations over MQTT and the gateways' apply reports
//! (decision 7; Laysi/trustiot-v2#720; services/gateway-ops/README.md, "下發", "套用回報",
//! "重發").
//!
//! - **Publishing.** A gateway's latest version (read with [`DesiredConfigs::current`], so never
//!   one the read interface refuses) is published to its `down/config` topic retained, QoS 1,
//!   exactly as stored. Only the broker's PUBACK makes it a sent version: this replica remembers
//!   the highest version it sent per gateway and never publishes a lower one, and
//!   `gateway_ops.config_deliveries` records the publish in the version's tenant.
//! - **Coalescing.** The consumer's handler announces each version it stores
//!   ([`DeliveryHandle::changed`]); the announcements of one gateway within the throttle
//!   ([`schedule`]) become one publish of whatever is latest when it ends. The handler announces
//!   before its transaction commits, so a publish waits until the announced version is visible:
//!   it looks every throttle for [`VISIBILITY_LIMIT`], then every [`VISIBILITY_POLL`] with a
//!   WARN, and never gives up (a slow transaction may still commit; after a rollback the next
//!   version the gateway gets reuses the number).
//! - **Republishing.** Each new broker connection (the first one included) republishes every
//!   gateway's latest version, retained, because the broker may have lost its retained messages
//!   (docs/feasibility/2026-10-09-cloud-poc.md, "MQTT broker", item 4). The republish runs under
//!   the job lock [`REPUBLISH_JOB`] (`service_kit::job_lock`), so one replica does it at a time;
//!   a replica that finds the lock taken leaves it to the holder.
//! - **Reports.** The shared subscription `$share/gateway-ops/.../up/config-report` spreads the
//!   reports over the replicas; [`reports`] checks them, and accepted ones are recorded in the
//!   reported version's tenant. A report about a version older than the latest schedules the
//!   latest again (throttled, so a gateway that floods reports gets one publish per throttle).
//!   The connection's event loop only checks a report and puts it in [`reports::Inbox`] (at
//!   most one per gateway, its latest); a task of its own handles them one at a time, so a slow
//!   or failing database never holds up the connection's events. The client acknowledged a
//!   report when it handed it over, so one whose handling fails for a database error goes back
//!   into the inbox and is tried again for a bounded time.
//! - **Races between replicas.** Any replica may publish (its consumer's changes, a republish,
//!   a report). Each gateway's read and publish run under a transaction-level advisory lock of
//!   its own ([`publish_lock_key`]), held until the PUBACK: the next holder reads a version at
//!   least as new and sends after the broker took the previous one, so an older version never
//!   replaces a newer retained one. A replica that finds the lock taken tries again a throttle
//!   later. After each publish the latest version is read again, and a newer one is scheduled.
//! - **Connections.** A publish, whether scheduled or part of a republish, first takes one of
//!   [`CONCURRENCY`] publish slots and its read's permit, and only then the lock's connection of
//!   the service role, which it keeps until the PUBACK; it writes its record after releasing the
//!   lock, still in its slot. So publishing never uses more than [`CONCURRENCY`] service-role
//!   connections at once (`crate::config::MIN_DATABASE_CONNECTIONS` counts them).
//!
//! The service is ready (`mqtt`) while the connection is up and the report subscription is
//! granted with QoS 1.

pub mod records;
pub mod reports;
pub mod schedule;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use contract::TenantId;
use mqtt_client::{Client, Event, Events, Message, PublishError, PublishOptions};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use protocol::config::ConfigReport;
use protocol::topic::Channel;
use protocol::{ConfigVersion, GatewayId};
use service_kit::job_lock::{JobLock, lock_key};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use self::records::{RecordError, Records};
use self::reports::{
    Inbox, LogBudget, Offered, Pending, Received, Refusal, Retried, check_message, judge,
};
use self::schedule::{Added, Request, Schedule, Trigger};
use crate::desired::{DesiredConfigs, ReadError, StoredConfig};

/// The report subscription's share group: every replica is in it, so each report reaches one.
pub const SHARE_GROUP: &str = "gateway-ops";

/// The service name of the republish job's lock key (`service_kit::job_lock::lock_key`).
pub const LOCK_SERVICE: &str = "gateway-ops";

/// The republish job's lock name.
pub const REPUBLISH_JOB: &str = "config-republish";

/// Takes the advisory lock `$1` for the transaction, without waiting; `true` when taken.
const TRY_LOCK: &str = "SELECT pg_try_advisory_xact_lock($1)";

/// The advisory lock key that serializes the reads and publishes of `gateway` across replicas
/// (`service_kit::job_lock::lock_key`, job `config-publish/<hardware ID>`).
#[must_use]
pub fn publish_lock_key(gateway: &GatewayId) -> i64 {
    lock_key(LOCK_SERVICE, &format!("config-publish/{gateway}"))
}

/// How many gateways one replica publishes at once, scheduled publishes and the republish
/// together (each holds one service-role connection: its lock, then its record), and how many of
/// its platform-role reads run at once (`crate::PLATFORM_CONNECTIONS` keeps two connections for
/// them).
pub const CONCURRENCY: usize = 2;

/// The wait before a read that failed for a database error is tried again.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long an announced version is looked for every throttle; after that, every
/// [`VISIBILITY_POLL`], with a WARN each time.
pub const VISIBILITY_LIMIT: Duration = Duration::from_secs(60);

/// How often an announced version is looked for once [`VISIBILITY_LIMIT`] has passed.
pub const VISIBILITY_POLL: Duration = Duration::from_secs(60);

/// Whether the broker connection is up, and whether the service is ready: connected with the
/// report subscription granted with QoS 1 (readiness check `mqtt`). Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Connection {
    link: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
}

/// Why the `mqtt` readiness check fails.
#[derive(Debug, thiserror::Error)]
#[error("not connected to the mqtt broker with the apply report subscription granted")]
pub struct NotConnected;

impl Connection {
    fn set(&self, link: bool, ready: bool) {
        self.link.store(link, Ordering::Release);
        self.ready.store(ready, Ordering::Release);
    }

    /// Whether a connection is up (publishes can go out).
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.link.load(Ordering::Acquire)
    }

    /// The readiness check.
    ///
    /// # Errors
    ///
    /// [`NotConnected`] while the connection is down, being made, or the report subscription is
    /// not granted with QoS 1.
    pub fn check(&self) -> Result<(), NotConnected> {
        if self.ready.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(NotConnected)
        }
    }
}

/// What the delivery counted since it started; the same events as its metrics, for tests and
/// diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Publishes the broker acknowledged.
    pub published: u64,
    /// Of those, by the republish job.
    pub republished: u64,
    /// Announced changes merged into a waiting publish.
    pub coalesced: u64,
    /// Publishes that failed, or were not made because the read failed.
    pub publish_failures: u64,
    /// Republish runs completed.
    pub republish_runs: u64,
    /// Republish runs left to the replica that held the lock.
    pub republish_skipped: u64,
    /// Reports recorded.
    pub reports_recorded: u64,
    /// Reports refused.
    pub reports_refused: u64,
    /// Reports whose handling failed for a database error and wait for a retry.
    pub reports_retried: u64,
    /// Reports never handled because a later report of the same gateway took their place.
    pub reports_superseded: u64,
    /// Reports given up after their retries, or because the inbox was full (README.md,
    /// "套用回報").
    pub reports_lost: u64,
}

#[derive(Debug, Default)]
struct Counters {
    published: AtomicU64,
    republished: AtomicU64,
    coalesced: AtomicU64,
    publish_failures: AtomicU64,
    republish_runs: AtomicU64,
    republish_skipped: AtomicU64,
    reports_recorded: AtomicU64,
    reports_refused: AtomicU64,
    reports_retried: AtomicU64,
    reports_superseded: AtomicU64,
    reports_lost: AtomicU64,
}

fn add(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    fn snapshot(&self) -> Counts {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Counts {
            published: get(&self.published),
            republished: get(&self.republished),
            coalesced: get(&self.coalesced),
            publish_failures: get(&self.publish_failures),
            republish_runs: get(&self.republish_runs),
            republish_skipped: get(&self.republish_skipped),
            reports_recorded: get(&self.reports_recorded),
            reports_refused: get(&self.reports_refused),
            reports_retried: get(&self.reports_retried),
            reports_superseded: get(&self.reports_superseded),
            reports_lost: get(&self.reports_lost),
        }
    }
}

/// The delivery's metrics (README.md, "指標").
#[derive(Debug)]
struct Metrics {
    published: Counter<u64>,
    coalesced: Counter<u64>,
    publish_failures: Counter<u64>,
    republish_runs: Counter<u64>,
    reports: Counter<u64>,
    refused_reports: Counter<u64>,
    unhandled_reports: Counter<u64>,
}

impl Metrics {
    fn new() -> Self {
        let meter = opentelemetry::global::meter("gateway-ops");
        Self {
            published: meter
                .u64_counter("gateway_ops.delivery.published")
                .with_description(
                    "Retained configuration publishes the broker acknowledged, by trigger",
                )
                .build(),
            coalesced: meter
                .u64_counter("gateway_ops.delivery.coalesced")
                .with_description("Configuration changes merged into a waiting publish")
                .build(),
            publish_failures: meter
                .u64_counter("gateway_ops.delivery.failures")
                .with_description("Configuration publishes that failed or were not made, by reason")
                .build(),
            republish_runs: meter
                .u64_counter("gateway_ops.delivery.republish_runs")
                .with_description("Runs of the republish after a broker connection, by result")
                .build(),
            reports: meter
                .u64_counter("gateway_ops.reports.recorded")
                .with_description("Apply reports recorded, by outcome")
                .build(),
            refused_reports: meter
                .u64_counter("gateway_ops.reports.refused")
                .with_description("Apply reports refused, by reason")
                .build(),
            unhandled_reports: meter
                .u64_counter("gateway_ops.reports.unhandled")
                .with_description(
                    "Apply reports not handled: queued for a retry after a database error, \
                     superseded by a later report of the gateway, or lost",
                )
                .build(),
        }
    }
}

/// The delivery's settings and connections.
pub struct Parts {
    /// The broker settings.
    pub mqtt: mqtt_client::Config,
    /// The platform-role reads of the stored configurations.
    pub configs: DesiredConfigs,
    /// The service-role writes of publishes and reports.
    pub records: Records,
    /// The pool the republish job's lock is taken on (the service role's).
    pub locks: PgPool,
    /// The coalescing window (`delivery.throttle`).
    pub throttle: Duration,
}

/// Why the delivery could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The report subscription's filter could not be built.
    #[error("the apply report subscription is invalid: {0}")]
    Subscription(protocol::topic::TopicError),
    /// The broker client refused to start (its settings or its password file).
    #[error(transparent)]
    Mqtt(#[from] mqtt_client::StartError),
}

#[derive(Debug)]
struct State {
    schedule: Schedule,
    /// Gateways being published right now; one publish per gateway at a time.
    busy: HashSet<GatewayId>,
    /// The highest version this replica published per gateway (PUBACK received).
    sent: HashMap<GatewayId, ConfigVersion>,
}

struct Inner {
    client: Client,
    /// The service role's pool, for the per-gateway publish locks.
    locks: PgPool,
    configs: DesiredConfigs,
    records: Records,
    throttle: Duration,
    state: Mutex<State>,
    /// Wakes the scheduler: a new entry, a connection, a gateway no longer busy.
    wake: Notify,
    /// Wakes those waiting for a busy gateway.
    idle: Notify,
    /// Bounds the platform-role reads.
    reads: Semaphore,
    /// The publish slots: bounds the publishes in progress, scheduled and republished together,
    /// and so the service-role connections they hold.
    publishes: Semaphore,
    connection: Connection,
    counters: Counters,
    metrics: Metrics,
    log_budget: LogBudget,
    /// The checked reports waiting for the report task.
    inbox: Mutex<Inbox>,
    /// Wakes the report task: a report was put in the inbox.
    reports_waiting: Notify,
}

/// The delivery, started: its broker connection is being made. [`Delivery::run`] works until
/// stopped.
pub struct Delivery {
    inner: Arc<Inner>,
    events: Events,
    lock: JobLock,
}

/// What the rest of the service, and tests, use of a running delivery. Cheap to clone.
#[derive(Clone)]
pub struct DeliveryHandle(Arc<Inner>);

impl std::fmt::Debug for DeliveryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeliveryHandle").finish_non_exhaustive()
    }
}

impl DeliveryHandle {
    /// Announces that `version` of `gateway` was stored: it is published, coalesced with the
    /// gateway's other changes within the throttle. Never blocks on I/O.
    pub fn changed(&self, gateway: &GatewayId, version: ConfigVersion) {
        self.0.request(gateway, Trigger::Change, Some(version));
    }

    /// Whether nothing is waiting or being published for `gateway`.
    #[must_use]
    pub fn is_idle(&self, gateway: &GatewayId) -> bool {
        let state = self.0.state();
        !state.schedule.contains(gateway) && !state.busy.contains(gateway)
    }

    /// The counts so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.0.counters.snapshot()
    }

    /// The connection's state.
    #[must_use]
    pub fn connection(&self) -> Connection {
        self.0.connection.clone()
    }
}

/// How a gateway's delivery ended.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Published, or nothing to publish.
    Done,
    /// Try again at this time.
    RetryAt(Instant),
    /// The connection went away; the next connection's republish covers it.
    Interrupted,
}

/// Why a republish run did not complete.
#[derive(Debug, thiserror::Error)]
enum RepublishError {
    #[error("the gateways' latest versions could not be read: {0}")]
    Read(#[from] ReadError),
    #[error("the mqtt connection went away during the republish")]
    Interrupted,
}

impl Delivery {
    /// Starts the broker client with the report subscription.
    ///
    /// # Errors
    ///
    /// When the broker settings or the password file are unusable.
    pub fn start(parts: Parts) -> Result<Self, StartError> {
        let filter = Channel::ConfigReport
            .shared_filter(SHARE_GROUP)
            .map_err(StartError::Subscription)?;
        let (client, events) = Client::start(&parts.mqtt, vec![filter])?;
        let inner = Arc::new(Inner {
            client,
            locks: parts.locks.clone(),
            configs: parts.configs,
            records: parts.records,
            throttle: parts.throttle,
            state: Mutex::new(State {
                schedule: Schedule::new(parts.throttle),
                busy: HashSet::new(),
                sent: HashMap::new(),
            }),
            wake: Notify::new(),
            idle: Notify::new(),
            reads: Semaphore::new(CONCURRENCY),
            publishes: Semaphore::new(CONCURRENCY),
            connection: Connection::default(),
            counters: Counters::default(),
            metrics: Metrics::new(),
            log_budget: LogBudget::new(),
            inbox: Mutex::new(Inbox::default()),
            reports_waiting: Notify::new(),
        });
        Ok(Self {
            inner,
            events,
            lock: JobLock::new(parts.locks, LOCK_SERVICE, REPUBLISH_JOB),
        })
    }

    /// The handle for the consumer's handler and for tests.
    #[must_use]
    pub fn handle(&self) -> DeliveryHandle {
        DeliveryHandle(Arc::clone(&self.inner))
    }

    /// Handles the connection's events until `stop` is cancelled: publishes what is scheduled,
    /// republishes after each connection, and hands the reports to the report task. Once
    /// stopped, it disconnects.
    pub async fn run(self, stop: CancellationToken) {
        let Self {
            inner,
            mut events,
            lock,
        } = self;
        let republish = Arc::new(Notify::new());
        let mut tasks = JoinSet::new();
        tasks.spawn(Arc::clone(&inner).schedule_loop(stop.clone()));
        tasks.spawn(Arc::clone(&inner).republish_loop(lock, Arc::clone(&republish), stop.clone()));
        tasks.spawn(Arc::clone(&inner).report_loop(stop.clone()));
        loop {
            let event = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                event = events.recv() => event,
            };
            match event {
                None => {
                    inner.connection.set(false, false);
                    tracing::error!(
                        "the mqtt client ended; the configuration delivery stopped and the \
                         service is not ready"
                    );
                    break;
                }
                Some(Event::Connected { subscriptions }) => {
                    // `all_granted` is false for a refusal and for a grant of only QoS 0.
                    let granted = subscriptions.all_granted();
                    inner.connection.set(true, granted);
                    if granted {
                        tracing::info!(
                            "connected to the mqtt broker; apply reports subscribed; \
                             republishing every gateway's configuration"
                        );
                    } else {
                        let codes: Vec<u8> =
                            subscriptions.iter().map(|outcome| outcome.code).collect();
                        tracing::error!(
                            suback_codes = ?codes,
                            "the mqtt broker did not grant the apply report subscription with \
                             QoS 1"
                        );
                    }
                    // The broker may have restarted without its retained messages.
                    republish.notify_one();
                    inner.wake.notify_one();
                }
                Some(Event::Disconnected { error, retry_in }) => {
                    inner.connection.set(false, false);
                    tracing::warn!(
                        error = %error,
                        retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX),
                        "no connection to the mqtt broker; reconnecting"
                    );
                }
                Some(Event::Message(message)) => inner.offer_report(&message),
            }
        }
        inner.connection.set(false, false);
        // Publishes in hand are cut off; the next start republishes everything anyway.
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        inner.client.shutdown().await;
    }
}

impl Inner {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every update of the state is a single step, so a panic elsewhere never leaves it
        // half-updated.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn request(&self, gateway: &GatewayId, trigger: Trigger, expected: Option<ConfigVersion>) {
        let added = self
            .state()
            .schedule
            .request(gateway, trigger, expected, Instant::now());
        if added == Added::Coalesced && trigger == Trigger::Change {
            add(&self.counters.coalesced);
            self.metrics.coalesced.add(1, &[]);
        }
        self.wake.notify_one();
    }

    fn failed(&self, reason: &'static str) {
        add(&self.counters.publish_failures);
        self.metrics
            .publish_failures
            .add(1, &[KeyValue::new("reason", reason)]);
    }

    /// Publishes the scheduled entries as they come due, [`CONCURRENCY`] gateways at a time,
    /// while connected.
    async fn schedule_loop(self: Arc<Self>, stop: CancellationToken) {
        let mut tasks = JoinSet::new();
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            while tasks.try_join_next().is_some() {}
            let mut next = None;
            if self.connection.is_up() && tasks.len() < CONCURRENCY {
                let taken = {
                    let mut state = self.state();
                    let State { schedule, busy, .. } = &mut *state;
                    let taken = schedule.take_due(Instant::now(), busy, CONCURRENCY - tasks.len());
                    busy.extend(taken.iter().map(|(gateway, _)| gateway.clone()));
                    next = schedule.next_due(busy);
                    taken
                };
                for (gateway, request) in taken {
                    let inner = Arc::clone(&self);
                    tasks.spawn(async move { inner.deliver_scheduled(gateway, request).await });
                }
            }
            // With every slot taken, a task's end wakes the loop instead of a due time.
            if tasks.len() >= CONCURRENCY {
                next = None;
            }
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                () = &mut notified => {}
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                () = sleep_until(next), if next.is_some() => {}
            }
        }
    }

    async fn deliver_scheduled(self: Arc<Self>, gateway: GatewayId, request: Request) {
        let next = self.deliver(&gateway, request).await;
        {
            let mut state = self.state();
            state.busy.remove(&gateway);
            match next {
                Next::Done => {}
                Next::RetryAt(due) => state.schedule.retry(&gateway, request, due),
                // Published again once connected (and by the connection's republish); one
                // throttle later, so a publish failing before the link is marked down cannot spin.
                Next::Interrupted => {
                    state
                        .schedule
                        .retry(&gateway, request, Instant::now() + self.throttle);
                }
            }
        }
        self.idle.notify_waiters();
        self.wake.notify_one();
    }

    /// Marks `gateway` busy, waiting while another publish of it runs.
    async fn acquire(&self, gateway: &GatewayId) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state().busy.insert(gateway.clone()) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self, gateway: &GatewayId) {
        self.state().busy.remove(gateway);
        self.idle.notify_waiters();
        self.wake.notify_one();
    }

    /// Publishes `gateway`'s latest version for `request`; see the module.
    async fn deliver(&self, gateway: &GatewayId, request: Request) -> Next {
        // The slot for the whole publish, the record and the read after it; then the read's
        // permit, so the lock's connection is taken only once nothing else is waited for but the
        // read under it and the PUBACK.
        let _slot = self.publishes.acquire().await;
        let read = self.reads.acquire().await;
        let lock = match self.lock_gateway(gateway).await {
            Ok(Some(lock)) => lock,
            // Another replica is publishing this gateway; read again after it.
            Ok(None) => return Next::RetryAt(Instant::now() + self.throttle),
            Err(error) => {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    error = %error,
                    "the gateway's publish lock was not taken; trying again"
                );
                self.failed("lock");
                return Next::RetryAt(Instant::now() + RETRY_DELAY);
            }
        };
        // The read stays under the lock: the next holder must read a version at least as new.
        let current = self.configs.current(gateway).await;
        drop(read);
        let outcome = self.deliver_locked(gateway, request, current).await;
        // Ending the transaction releases the lock; a failed commit ends it too (the server
        // rolls it back with the connection).
        if let Err(error) = lock.commit().await {
            tracing::warn!(
                hardware_id = gateway.as_str(),
                error = %error,
                "the gateway's publish lock was not released cleanly"
            );
        }
        match outcome {
            Ok((tenant, version)) => {
                self.after_publish(gateway, tenant, version).await;
                Next::Done
            }
            Err(next) => next,
        }
    }

    /// Takes `gateway`'s publish lock in a transaction of the service role, or `None` when
    /// another session holds it.
    async fn lock_gateway(
        &self,
        gateway: &GatewayId,
    ) -> Result<Option<Transaction<'static, Postgres>>, sqlx::Error> {
        let mut lock = self.locks.begin().await?;
        let taken: bool = sqlx::query_scalar(TRY_LOCK)
            .bind(publish_lock_key(gateway))
            .fetch_one(&mut *lock)
            .await?;
        if taken {
            Ok(Some(lock))
        } else {
            lock.rollback().await?;
            Ok(None)
        }
    }

    /// Publishes under the gateway's lock what `read` (made under the lock) found: the
    /// published tenant and version, or what to do instead.
    async fn deliver_locked(
        &self,
        gateway: &GatewayId,
        request: Request,
        read: Result<Option<StoredConfig>, ReadError>,
    ) -> Result<(TenantId, ConfigVersion), Next> {
        let config = match read {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Err(match request.expected {
                    Some(expected) => self.not_visible(gateway, expected, request),
                    None => Next::Done,
                });
            }
            Err(ReadError::Database(error)) => {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    error = %error,
                    "gateway configuration not read for publishing; trying again"
                );
                self.failed("read");
                return Err(Next::RetryAt(Instant::now() + RETRY_DELAY));
            }
            Err(error) => {
                let reason = match error {
                    ReadError::NotForLatestBinding(_) => "not_for_latest_binding",
                    _ => "corrupt",
                };
                tracing::error!(
                    hardware_id = gateway.as_str(),
                    reason,
                    error = %error,
                    "gateway configuration not published"
                );
                self.failed(reason);
                return Err(Next::Done);
            }
        };
        if let Some(expected) = request.expected
            && config.version < expected
        {
            return Err(self.not_visible(gateway, expected, request));
        }
        let sent = self.state().sent.get(gateway).copied();
        if let Some(sent) = sent {
            if config.version < sent {
                // Versions only grow; a lower latest version means the database went back.
                tracing::error!(
                    hardware_id = gateway.as_str(),
                    version = config.version.get(),
                    sent_version = sent.get(),
                    "the latest configuration is older than one this replica published; not \
                     published"
                );
                self.failed("older_than_sent");
                return Err(Next::Done);
            }
            if config.version == sent && !request.force {
                return Err(Next::Done);
            }
        }
        self.publish(gateway, config, request.trigger).await
    }

    fn not_visible(&self, gateway: &GatewayId, expected: ConfigVersion, request: Request) -> Next {
        let now = Instant::now();
        if now.saturating_duration_since(request.since) < VISIBILITY_LIMIT {
            return Next::RetryAt(now + self.throttle);
        }
        // A transaction slower than the limit may still commit, and one that rolled back leaves
        // its version number to the next version the gateway gets: never give up, poll slower.
        tracing::warn!(
            hardware_id = gateway.as_str(),
            version = expected.get(),
            waited_s = now.saturating_duration_since(request.since).as_secs(),
            "an announced configuration version is not visible yet; still waiting for it"
        );
        Next::RetryAt(now + VISIBILITY_POLL)
    }

    /// Publishes `config`, under the gateway's lock: its tenant and version once the broker
    /// acknowledged it.
    async fn publish(
        &self,
        gateway: &GatewayId,
        config: StoredConfig,
        trigger: Trigger,
    ) -> Result<(TenantId, ConfigVersion), Next> {
        let delivery = Channel::Config.delivery();
        let options = PublishOptions {
            retain: delivery.retain,
            message_expiry: delivery.message_expiry,
        };
        let StoredConfig {
            tenant_id,
            version,
            bound,
            payload,
            ..
        } = config;
        match self
            .client
            .publish(&Channel::Config.topic(gateway), payload, options)
            .await
        {
            Ok(()) => {}
            Err(
                error @ (PublishError::NotConnected
                | PublishError::ConnectionLost
                | PublishError::Closed),
            ) => {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    version = version.get(),
                    error = %error,
                    "gateway configuration not published; the connection went away"
                );
                self.failed("connection");
                return Err(Next::Interrupted);
            }
            Err(error) => {
                let reason = match error {
                    PublishError::Refused { .. } => "refused",
                    PublishError::TooLarge { .. } => "too_large",
                    _ => "invalid",
                };
                tracing::error!(
                    hardware_id = gateway.as_str(),
                    version = version.get(),
                    reason,
                    error = %error,
                    "the broker did not take the gateway configuration"
                );
                self.failed(reason);
                return Err(Next::Done);
            }
        }
        {
            let mut state = self.state();
            let sent = state.sent.entry(gateway.clone()).or_insert(version);
            *sent = (*sent).max(version);
        }
        add(&self.counters.published);
        if trigger == Trigger::Republish {
            add(&self.counters.republished);
        }
        self.metrics
            .published
            .add(1, &[KeyValue::new("trigger", trigger.name())]);
        if trigger == Trigger::Republish {
            tracing::debug!(
                hardware_id = gateway.as_str(),
                tenant_id = %tenant_id,
                version = version.get(),
                bound,
                trigger = trigger.name(),
                "gateway configuration published"
            );
        } else {
            tracing::info!(
                hardware_id = gateway.as_str(),
                tenant_id = %tenant_id,
                version = version.get(),
                bound,
                trigger = trigger.name(),
                "gateway configuration published"
            );
        }
        Ok((tenant_id, version))
    }

    /// Records a publish and schedules a newer version stored meanwhile; after the lock.
    async fn after_publish(
        &self,
        gateway: &GatewayId,
        tenant_id: TenantId,
        version: ConfigVersion,
    ) {
        if let Err(error) = self.records.published(tenant_id, gateway, version).await {
            tracing::warn!(
                hardware_id = gateway.as_str(),
                version = version.get(),
                error = %error,
                "the publish of a gateway configuration was not recorded"
            );
        }
        // A newer version committed after the read (its announcement may have gone to another
        // replica, which found this gateway locked): make sure the latest follows.
        let after = {
            let _permit = self.reads.acquire().await;
            self.configs.report_target(gateway, version).await
        };
        if let Ok(Some(target)) = after
            && target.latest > version
        {
            self.request(gateway, Trigger::Change, Some(target.latest));
        }
    }

    /// Republishes after each connection, under the job lock; see the module.
    async fn republish_loop(
        self: Arc<Self>,
        lock: JobLock,
        republish: Arc<Notify>,
        stop: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => return,
                () = republish.notified() => {}
            }
            while self.connection.is_up() {
                let inner = Arc::clone(&self);
                let run = lock.run_exclusive(|| async move { inner.republish_all().await });
                let retry = match run.await {
                    Ok(Some(Ok(count))) => {
                        add(&self.counters.republish_runs);
                        self.metrics
                            .republish_runs
                            .add(1, &[KeyValue::new("result", "done")]);
                        tracing::info!(
                            gateways = count,
                            "republished every gateway's configuration"
                        );
                        false
                    }
                    Ok(Some(Err(RepublishError::Interrupted))) => {
                        self.metrics
                            .republish_runs
                            .add(1, &[KeyValue::new("result", "interrupted")]);
                        tracing::warn!(
                            "republish interrupted by the connection; it runs again once \
                             connected"
                        );
                        false
                    }
                    Ok(Some(Err(error))) => {
                        self.metrics
                            .republish_runs
                            .add(1, &[KeyValue::new("result", "failed")]);
                        tracing::warn!(error = %error, "republish failed; trying again");
                        true
                    }
                    Ok(None) => {
                        add(&self.counters.republish_skipped);
                        self.metrics
                            .republish_runs
                            .add(1, &[KeyValue::new("result", "skipped")]);
                        tracing::info!(
                            "republish skipped; another replica holds its lock and republishes"
                        );
                        false
                    }
                    Err(error) => {
                        self.metrics
                            .republish_runs
                            .add(1, &[KeyValue::new("result", "failed")]);
                        tracing::warn!(error = %error, "republish lock failed; trying again");
                        true
                    }
                };
                if !retry {
                    break;
                }
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
        }
    }

    /// Publishes every gateway's latest version, [`CONCURRENCY`] at a time; returns how many
    /// gateways it went through. A gateway whose read failed is scheduled again.
    async fn republish_all(self: Arc<Self>) -> Result<usize, RepublishError> {
        let latest = {
            let _permit = self.reads.acquire().await;
            self.configs.latest_versions().await?
        };
        let count = latest.len();
        let mut tasks = JoinSet::new();
        let mut gateways = latest.into_iter().map(|latest| latest.hardware_id);
        let mut interrupted = false;
        loop {
            while tasks.len() < CONCURRENCY && !interrupted {
                let Some(gateway) = gateways.next() else {
                    break;
                };
                let inner = Arc::clone(&self);
                tasks.spawn(async move {
                    inner.acquire(&gateway).await;
                    let request = Request {
                        trigger: Trigger::Republish,
                        expected: None,
                        force: true,
                        since: Instant::now(),
                    };
                    let next = inner.deliver(&gateway, request).await;
                    inner.release(&gateway);
                    (gateway, next)
                });
            }
            let Some(joined) = tasks.join_next().await else {
                break;
            };
            match joined {
                Ok((_, Next::Done)) => {}
                Ok((gateway, Next::RetryAt(due))) => {
                    // Keeps the delay the failed read asked for.
                    let request = Request {
                        trigger: Trigger::Republish,
                        expected: None,
                        force: true,
                        since: Instant::now(),
                    };
                    self.state().schedule.retry(&gateway, request, due);
                    self.wake.notify_one();
                }
                Ok((_, Next::Interrupted)) => interrupted = true,
                Err(error) if error.is_panic() => {
                    tracing::error!("a gateway's republish panicked");
                }
                Err(_) => {}
            }
        }
        if interrupted {
            Err(RepublishError::Interrupted)
        } else {
            Ok(count)
        }
    }

    /// Checks a message from the report subscription and puts it in the inbox for the report
    /// task; never waits, so the connection's events keep flowing whatever the database does.
    fn offer_report(&self, message: &Message) {
        let received = match check_message(message) {
            Ok(received) => received,
            Err(refused) => {
                self.refused(refused.refusal, refused.gateway.as_ref(), None);
                return;
            }
        };
        let offered = self.inbox().offer(received, Instant::now());
        match offered {
            Offered::Waiting => {}
            Offered::Replaced(replaced) => self.superseded(&replaced.received),
            Offered::Evicted(evicted) => self.lost(&evicted.received),
        }
        self.reports_waiting.notify_one();
    }

    /// Handles the reports in the inbox one at a time, in line order, until stopped. After a
    /// database error it pauses for [`reports::REPORT_RETRY_DELAY`]: the database is likely down,
    /// and the waiting reports are not tried in a burst against it.
    async fn report_loop(self: Arc<Self>, stop: CancellationToken) {
        loop {
            let notified = self.reports_waiting.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let taken = {
                let mut inbox = self.inbox();
                inbox
                    .take_due(Instant::now())
                    .ok_or_else(|| inbox.next_due())
            };
            match taken {
                Ok(pending) => {
                    if !self.handle_pending(pending).await {
                        tokio::select! {
                            biased;
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(reports::REPORT_RETRY_DELAY) => {}
                        }
                    }
                }
                Err(next_due) => {
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        () = &mut notified => {}
                        () = sleep_until(next_due), if next_due.is_some() => {}
                    }
                }
            }
        }
    }

    /// Puts back a report whose handling failed for a database error.
    fn retry_later(&self, pending: Pending) {
        let received = pending.received.clone();
        let retried = self.inbox().retry(pending, Instant::now());
        match retried {
            Retried::Waiting => {
                add(&self.counters.reports_retried);
                self.metrics
                    .unhandled_reports
                    .add(1, &[KeyValue::new("result", "retry")]);
            }
            Retried::Superseded => self.superseded(&received),
            Retried::Expired => self.lost(&received),
            Retried::Evicted(evicted) => {
                add(&self.counters.reports_retried);
                self.metrics
                    .unhandled_reports
                    .add(1, &[KeyValue::new("result", "retry")]);
                self.lost(&evicted.received);
            }
        }
    }

    /// A report never handled because a later report of its gateway took its place.
    fn superseded(&self, received: &Received) {
        add(&self.counters.reports_superseded);
        self.metrics
            .unhandled_reports
            .add(1, &[KeyValue::new("result", "superseded")]);
        tracing::debug!(
            hardware_id = received.gateway.as_str(),
            version = received.report.version.get(),
            "apply report superseded by a later report of the gateway"
        );
    }

    /// A report given up: its retries ran out, or the inbox was full.
    fn lost(&self, received: &Received) {
        add(&self.counters.reports_lost);
        self.metrics
            .unhandled_reports
            .add(1, &[KeyValue::new("result", "lost")]);
        tracing::warn!(
            hardware_id = received.gateway.as_str(),
            version = received.report.version.get(),
            outcome = received.report.outcome.as_str(),
            "apply report lost: the database failed until its retries ran out, or too many \
             gateways' reports were waiting"
        );
    }

    /// Compares a checked report with what is stored, records it and answers it; a database
    /// error puts it back in the inbox. Returns `false` after a database error.
    async fn handle_pending(&self, pending: Pending) -> bool {
        let (gateway, report) = (&pending.received.gateway, &pending.received.report);
        let target = {
            let _permit = self.reads.acquire().await;
            self.configs.report_target(gateway, report.version).await
        };
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    version = report.version.get(),
                    error = %error,
                    "apply report not handled: the stored versions could not be read"
                );
                if matches!(error, ReadError::Database(_)) {
                    self.retry_later(pending);
                    return false;
                }
                return true;
            }
        };
        let verdict = judge(report, target.as_ref());
        if verdict.republish {
            self.request(gateway, Trigger::Report, None);
        }
        let (Ok(()), Some((tenant, _))) = (
            verdict.record,
            target.as_ref().and_then(|target| target.reported),
        ) else {
            if let Err(refusal) = verdict.record {
                self.refused(refusal, Some(gateway), Some(report));
            }
            return true;
        };
        match self.records.reported(tenant, gateway, report).await {
            Ok(()) => {
                add(&self.counters.reports_recorded);
                self.metrics
                    .reports
                    .add(1, &[KeyValue::new("outcome", report.outcome.to_string())]);
                let codes: Vec<&str> = report
                    .issues
                    .iter()
                    .map(|issue| issue.code.as_str())
                    .collect();
                tracing::info!(
                    hardware_id = gateway.as_str(),
                    tenant_id = %tenant,
                    version = report.version.get(),
                    outcome = report.outcome.as_str(),
                    active_version = report.active_version.map(ConfigVersion::get),
                    issue_codes = ?codes,
                    "apply report recorded"
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    hardware_id = gateway.as_str(),
                    version = report.version.get(),
                    error = %error,
                    "apply report not recorded"
                );
                if matches!(error, RecordError::Database(_)) {
                    self.retry_later(pending);
                    return false;
                }
                true
            }
        }
    }

    fn refused(
        &self,
        refusal: Refusal,
        gateway: Option<&GatewayId>,
        report: Option<&ConfigReport>,
    ) {
        add(&self.counters.reports_refused);
        self.metrics
            .refused_reports
            .add(1, &[KeyValue::new("reason", refusal.reason())]);
        if let Some(suppressed_logs) = self.log_budget.allow(Instant::now()) {
            tracing::warn!(
                reason = refusal.reason(),
                hardware_id = gateway.map(GatewayId::as_str),
                version = report.map(|report| report.version.get()),
                active_version =
                    report.and_then(|report| report.active_version.map(ConfigVersion::get)),
                suppressed_logs,
                "apply report refused"
            );
        }
    }
}

/// Sleeps until `at`; never resolves for `None` (the select's guard keeps it unpolled then).
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
