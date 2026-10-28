//! The telemetry path (decision 8, rebuild checklist I1; services/ingest/README.md, "寫入流程"):
//! batches from every gateway's `up/telemetry` into ClickHouse, each acknowledged on the
//! gateway's `down/telemetry-ack` only once it is on disk with its completion record.
//!
//! - The service subscribes with the shared subscription `$share/ingest/trustiot/v1/gw/+/up/telemetry`
//!   (QoS 1), so replicas share the gateways' batches, and takes the gateway from the topic
//!   (`parse_topic`; the broker's ACL lets a gateway publish only under its own ID). A message
//!   with the retain flag is a gateway breaking the protocol (the channel is never retained, and
//!   the broker replays retained messages at every new subscription): it is dropped and logged.
//!   A message the broker delivered with QoS 0 is dropped and logged too; the gateway sends the
//!   batch again.
//! - A payload that does not decode, or a batch the table cannot hold ([`batch::prepare`]), is
//!   answered `rejected` with the first issue, addressed by the header `peek_header` reads; a
//!   payload without even a header is only logged.
//! - The tenant comes from the binding projection ([`crate::bindings`]), never from the payload:
//!   looked up once per attempt, when the message arrives, and used for the whole attempt. The
//!   projection itself follows the assets service's events through the outbox, so a rebinding
//!   reaches it moments after it happened, never at an exact instant; a batch whose attempt
//!   looked up the tenant before the rebinding arrived is written for the former tenant, as one
//!   received a moment earlier would be. The completion record names the binding used
//!   (`binding_id`). A gateway bound to no tenant is answered `retry`, and nothing is written.
//! - One replica handles a stream at a time: a Postgres advisory lock on gateway + stream
//!   (service-kit's [`JobLock`], whose session is watched while the batch is written). A
//!   replica that does not get it answers `retry`.
//! - Under the lock: the high-water mark from the completion table (gateway + stream); a batch at
//!   or below it is answered `duplicate` with the rows of its first commit. Otherwise the rows an
//!   earlier attempt left are deleted, whatever tenant they were written for (the aggregates of
//!   a tenant other than the current one are reconciled at once), the whole batch is inserted,
//!   the current tenant's aggregates are reconciled, and the completion record is written; only
//!   then is the batch answered `committed`. Every statement carries the stream's settings
//!   ([`store`]). A failure anywhere answers `retry` and writes no completion record, so the
//!   gateway sends the batch again and the next attempt starts over.
//!
//!   One window is left: an attempt that ends between deleting another tenant's rows and
//!   reconciling that tenant (the gateway was rebound between two attempts of one batch) leaves
//!   that tenant's aggregates counting the deleted rows, and the next attempt finds no rows to
//!   name the tenant by. The consistency check and repair of docs/runbooks/migrations.md
//!   (src/rollup.rs, `REPAIR_1M`, `REPAIR_1H`) find and correct it.
//!
//! The service's own clock stamps `received_at` when a message arrives, and `committed_at` once
//! the rows are on disk, never earlier than `received_at` (so a completion record never expires
//! before the rows it describes; services/ingest/README.md, "批次完成表").

pub mod batch;
pub mod hooks;
pub mod metrics;
pub mod store;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use mqtt_client::{Client, Event, Events, Message, PublishOptions};
use protocol::telemetry::{AckOutcome, TelemetryAck, TelemetryBatch};
use protocol::topic::{Channel, parse_topic};
use protocol::{GatewayId, Issue, IssueCode, Seq, StreamId, UnixMillis, V1};
use serde::Serialize;
use service_kit::job_lock::JobLock;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::bindings::{GatewayBindings, GatewayTenant};
use crate::clickhouse::ClickHouse;
use crate::telemetry::batch::{Prepared, datetime64};
use crate::telemetry::hooks::{Checkpoint, Hooks, Point};
use crate::telemetry::metrics::{Metrics, Outcome};
use crate::telemetry::store::{Store, StoreError};

/// The shared subscription's group: every replica of the service is in it, so each batch goes
/// to one of them.
pub const SHARE_GROUP: &str = "ingest";

/// The service name of the stream locks' keys (service-kit's `job_lock::lock_key`).
pub const LOCK_SERVICE: &str = "ingest";

/// `retry` after a database or ClickHouse failure.
pub const RETRY_AFTER_MS: u32 = 5_000;

/// `retry` for a gateway bound to no tenant (yet): its binding event may still be on its way
/// through the outbox, or the gateway is not bound at all.
pub const UNBOUND_RETRY_AFTER_MS: u32 = 30_000;

/// `retry` while another replica holds the stream's lock: it is writing the same batch (a resend
/// that arrived while the first attempt was still running).
pub const BUSY_RETRY_AFTER_MS: u32 = 2_000;

/// How many batches a replica writes at once, for a service-role pool of `max_connections`:
/// each holds one connection for its stream's lock, and the binding consumer keeps one.
#[must_use]
pub fn concurrency(max_connections: u32) -> usize {
    usize::try_from(max_connections.saturating_sub(1).max(1)).unwrap_or(1)
}

/// The job name of a stream's lock: the topic's gateway and the stream. Gateway and stream IDs
/// never contain `/`, so two streams never share a name.
#[must_use]
pub fn lock_job(gateway: &GatewayId, stream: &StreamId) -> String {
    format!("telemetry/{gateway}/{stream}")
}

/// Whether the service is connected to the broker with its telemetry subscription granted: the
/// readiness check `mqtt`. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Connection(Arc<AtomicBool>);

/// Why the `mqtt` readiness check fails.
#[derive(Debug, thiserror::Error)]
#[error("not connected to the mqtt broker with the telemetry subscription granted")]
pub struct NotConnected;

impl Connection {
    fn set(&self, up: bool) {
        self.0.store(up, Ordering::Release);
    }

    /// Whether the connection is up and the subscription granted with QoS 1.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// The readiness check.
    ///
    /// # Errors
    ///
    /// [`NotConnected`] while the connection is down, being made, or the broker did not grant
    /// the subscription with QoS 1.
    pub fn check(&self) -> Result<(), NotConnected> {
        if self.is_up() {
            Ok(())
        } else {
            Err(NotConnected)
        }
    }
}

/// What the pipeline is built from.
pub struct Parts {
    /// The service's ClickHouse client.
    pub clickhouse: ClickHouse,
    /// The lookup of a gateway's tenant.
    pub bindings: GatewayBindings,
    /// The pool the streams' locks are taken on (the service role's).
    pub locks: PgPool,
    /// The broker settings.
    pub mqtt: mqtt_client::Config,
    /// How the pipeline subscribes: [`Subscription::Shared`] with [`SHARE_GROUP`] for the
    /// service.
    pub subscription: Subscription,
    /// How many batches are written at once: [`concurrency`] for the service.
    pub concurrency: usize,
    /// [`hooks::NoHooks`] for the service.
    pub hooks: Arc<dyn Hooks>,
}

/// How the pipeline subscribes to every gateway's telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subscription {
    /// The shared subscription `$share/<group>/trustiot/v1/gw/+/up/telemetry`: the replicas in
    /// one group share the batches. The service's.
    Shared(String),
    /// `trustiot/v1/gw/+/up/telemetry` without a group: every message reaches this pipeline, and
    /// the broker replays retained messages to it when it subscribes, which it does not for a
    /// shared subscription. For the tests of that replay.
    Direct,
}

/// Why the pipeline could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The share group is not a valid group name.
    #[error("the telemetry subscription's share group is invalid: {0}")]
    ShareGroup(protocol::topic::TopicError),
    /// The broker client refused to start (its settings or its password file).
    #[error(transparent)]
    Mqtt(#[from] mqtt_client::StartError),
}

/// The telemetry path, started: its broker connection is being made. [`Pipeline::run`] handles
/// the batches until stopped.
pub struct Pipeline {
    processor: Arc<Processor>,
    events: Events,
    connection: Connection,
    concurrency: usize,
}

impl Pipeline {
    /// Starts the broker client with the telemetry subscription.
    ///
    /// # Errors
    ///
    /// When the share group, the broker settings or the password file are unusable.
    pub fn start(parts: Parts) -> Result<Self, StartError> {
        let filter = match &parts.subscription {
            Subscription::Shared(group) => Channel::Telemetry
                .shared_filter(group)
                .map_err(StartError::ShareGroup)?,
            Subscription::Direct => Channel::Telemetry.filter(),
        };
        let (client, events) = Client::start(&parts.mqtt, vec![filter])?;
        Ok(Self {
            processor: Arc::new(Processor {
                clickhouse: parts.clickhouse,
                bindings: parts.bindings,
                locks: parts.locks,
                client,
                hooks: parts.hooks,
                metrics: Metrics::new(),
            }),
            events,
            connection: Connection::default(),
            concurrency: parts.concurrency.max(1),
        })
    }

    /// The broker connection's state, for readiness.
    #[must_use]
    pub fn connection(&self) -> Connection {
        self.connection.clone()
    }

    /// Handles the connection's events until `stop` is cancelled: each batch in a task of its
    /// own, at most the pipeline's concurrency at once (the next message is not read before one
    /// ends, so the broker holds the rest). Once stopped, it lets the batches in hand finish and
    /// send their acknowledgements, then disconnects.
    pub async fn run(self, stop: CancellationToken) {
        let Self {
            processor,
            mut events,
            connection,
            concurrency,
        } = self;
        let permits = Arc::new(Semaphore::new(concurrency));
        let mut handlers = JoinSet::new();
        loop {
            let event = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                event = events.recv() => event,
            };
            while handlers.try_join_next().is_some() {}
            match event {
                None => {
                    // The client's task ended without a shutdown: it panicked and logged it.
                    connection.set(false);
                    tracing::error!(
                        "the mqtt client ended; the telemetry path stopped and the service is \
                         not ready"
                    );
                    break;
                }
                Some(Event::Connected { subscriptions }) => {
                    // `all_granted` is false for a refusal and for a grant of only QoS 0
                    // (crates/mqtt-client, `Subscriptions::all_granted`): the service is ready
                    // only with QoS 1, the one this path accepts.
                    let granted = subscriptions.all_granted();
                    connection.set(granted);
                    if granted {
                        tracing::info!("connected to the mqtt broker; telemetry subscribed");
                    } else {
                        let codes: Vec<u8> =
                            subscriptions.iter().map(|outcome| outcome.code).collect();
                        tracing::error!(
                            suback_codes = ?codes,
                            "the mqtt broker did not grant the telemetry subscription with QoS 1"
                        );
                    }
                }
                Some(Event::Disconnected { error, retry_in }) => {
                    connection.set(false);
                    tracing::warn!(
                        error = %error,
                        retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX),
                        "no connection to the mqtt broker; reconnecting"
                    );
                }
                Some(Event::Message(message)) => {
                    let permit = tokio::select! {
                        biased;
                        () = stop.cancelled() => break,
                        permit = Arc::clone(&permits).acquire_owned() => permit,
                    };
                    let Ok(permit) = permit else { break };
                    let processor = Arc::clone(&processor);
                    handlers.spawn(async move {
                        processor.process(message).await;
                        drop(permit);
                    });
                }
            }
        }
        connection.set(false);
        // The batches in hand finish and send their acknowledgements; service-kit's drain
        // timeout cuts them off if they take longer.
        while handlers.join_next().await.is_some() {}
        processor.client.shutdown().await;
    }
}

/// What a batch came to.
enum Written {
    Committed,
    Duplicate { records: u32 },
}

/// Why writing a batch stopped before its completion record.
#[derive(Debug)]
enum WriteError {
    Store(StoreError),
    /// A row could not be written as JSON (a decoded batch never has such a value).
    Encode,
}

impl From<StoreError> for WriteError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// The completion record in `JSONEachRow`.
#[derive(Serialize)]
struct CommitRecord<'a> {
    tenant_id: &'a str,
    binding_id: &'a str,
    gateway_id: &'a str,
    stream_id: &'a str,
    seq: u64,
    row_count: u32,
    committed_at: &'a str,
}

/// The milliseconds since the Unix epoch on the service's clock.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

struct Processor {
    clickhouse: ClickHouse,
    bindings: GatewayBindings,
    locks: PgPool,
    client: Client,
    hooks: Arc<dyn Hooks>,
    metrics: Metrics,
}

/// An answer to send, and to whom.
struct Reply {
    gateway: GatewayId,
    stream: StreamId,
    seq: Seq,
    outcome: AckOutcome,
}

impl Processor {
    async fn process(&self, message: Message) {
        let started = Instant::now();
        let received_at = self.hooks.received_at().unwrap_or_else(now_millis);
        let (outcome, reply) = self.handle(&message, received_at).await;
        if let Some(reply) = reply {
            self.send(reply).await;
        }
        self.metrics.handled(outcome, started.elapsed());
    }

    async fn handle(&self, message: &Message, received_at: u64) -> (Outcome, Option<Reply>) {
        let payload_bytes = message.payload.len();
        let gateway = match parse_topic(&message.topic) {
            Ok(parsed) if parsed.channel == Channel::Telemetry => parsed.gateway,
            Ok(_) => {
                tracing::warn!(
                    payload_bytes,
                    "dropped a message on a topic other than telemetry"
                );
                return (Outcome::Undecodable, None);
            }
            Err(error) => {
                tracing::warn!(
                    reason = %error,
                    payload_bytes,
                    "dropped a message whose topic is not a gateway's telemetry topic"
                );
                return (Outcome::Undecodable, None);
            }
        };
        if message.retain {
            tracing::warn!(
                gateway_id = %gateway,
                payload_bytes,
                "dropped a retained telemetry message: telemetry is never retained, so the \
                 gateway broke the protocol and the broker replays an old message"
            );
            return (Outcome::RetainDropped, None);
        }
        if message.qos == 0 {
            tracing::warn!(
                gateway_id = %gateway,
                payload_bytes,
                "dropped a telemetry message delivered with QoS 0; the gateway sends it again"
            );
            return (Outcome::QosDropped, None);
        }
        let batch = match TelemetryBatch::decode(&message.payload) {
            Ok(batch) => batch,
            Err(error) => {
                let issue =
                    error.to_issues().into_iter().next().unwrap_or_else(|| {
                        Issue::new(IssueCode::DecodeFailed, "", error.to_string())
                    });
                let Ok(header) = TelemetryBatch::peek_header(&message.payload) else {
                    tracing::warn!(
                        gateway_id = %gateway,
                        payload_bytes,
                        code = issue.code.as_str(),
                        "dropped a telemetry message without a readable batch header"
                    );
                    return (Outcome::Undecodable, None);
                };
                return self.rejected(gateway, header.stream, header.seq, &issue);
            }
        };
        let (stream, seq) = (batch.stream.clone(), batch.seq);
        let prepared = match batch::prepare(gateway.clone(), batch) {
            Ok(prepared) => prepared,
            Err(issue) => return self.rejected(gateway, stream, seq, &issue),
        };
        let retry = |after_ms| Reply {
            gateway: gateway.clone(),
            stream: stream.clone(),
            seq,
            outcome: AckOutcome::Retry { after_ms },
        };
        let tenant = match self.bindings.gateway_tenant(&gateway).await {
            Ok(Some(tenant)) => tenant,
            Ok(None) => {
                tracing::info!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    "telemetry from a gateway bound to no tenant; answered retry, nothing written"
                );
                return (Outcome::Retry, Some(retry(UNBOUND_RETRY_AFTER_MS)));
            }
            Err(error) => {
                self.metrics.failed("postgres", "gateway_tenant");
                tracing::warn!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    error = %error,
                    "could not look up the gateway's tenant; answered retry"
                );
                return (Outcome::Retry, Some(retry(RETRY_AFTER_MS)));
            }
        };
        let lock = JobLock::new(
            self.locks.clone(),
            LOCK_SERVICE,
            &lock_job(&gateway, &stream),
        );
        let written = lock
            .run_exclusive(|| self.write(&prepared, tenant, received_at))
            .await;
        match written {
            Ok(Some(Ok(Written::Committed))) => {
                self.metrics.committed(prepared.records);
                tracing::debug!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    records = prepared.records,
                    "telemetry batch committed"
                );
                let outcome = AckOutcome::Committed {
                    records: prepared.records,
                };
                (
                    Outcome::Committed,
                    Some(Reply {
                        gateway,
                        stream,
                        seq,
                        outcome,
                    }),
                )
            }
            Ok(Some(Ok(Written::Duplicate { records }))) => {
                tracing::debug!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    records,
                    "telemetry batch at or below the stream's high-water mark; answered duplicate"
                );
                let outcome = AckOutcome::Duplicate { records };
                (
                    Outcome::Duplicate,
                    Some(Reply {
                        gateway,
                        stream,
                        seq,
                        outcome,
                    }),
                )
            }
            Ok(Some(Err(error))) => {
                let step = match &error {
                    WriteError::Store(error) => error.step.as_str(),
                    WriteError::Encode => "encode",
                };
                self.metrics.failed("clickhouse", step);
                tracing::warn!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    step,
                    error = ?error,
                    "writing a telemetry batch failed; answered retry, no completion record"
                );
                (Outcome::Retry, Some(retry(RETRY_AFTER_MS)))
            }
            Ok(None) => {
                tracing::info!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    "another replica is writing this stream; answered retry"
                );
                (Outcome::Retry, Some(retry(BUSY_RETRY_AFTER_MS)))
            }
            Err(error) => {
                self.metrics.failed("postgres", "stream_lock");
                tracing::warn!(
                    gateway_id = %gateway,
                    stream_id = %stream,
                    seq = seq.get(),
                    error = %error,
                    "the stream's lock failed or was lost; answered retry, no completion record"
                );
                (Outcome::Retry, Some(retry(RETRY_AFTER_MS)))
            }
        }
    }

    /// `rejected` with `issue`, logged by code and path (the message may quote the payload).
    fn rejected(
        &self,
        gateway: GatewayId,
        stream: StreamId,
        seq: Seq,
        issue: &Issue,
    ) -> (Outcome, Option<Reply>) {
        tracing::warn!(
            gateway_id = %gateway,
            stream_id = %stream,
            seq = seq.get(),
            code = issue.code.as_str(),
            path = %issue.path,
            "rejected a telemetry batch"
        );
        let message = if issue.path.is_empty() {
            issue.message.clone()
        } else {
            format!("{}: {}", issue.path, issue.message)
        };
        // Issue::new keeps the message within the acknowledgement's limit and without U+0000.
        let message = Issue::new(issue.code.clone(), "", message).message;
        let outcome = AckOutcome::Rejected {
            code: issue.code.clone(),
            message,
        };
        (
            Outcome::Rejected,
            Some(Reply {
                gateway,
                stream,
                seq,
                outcome,
            }),
        )
    }

    async fn checkpoint(&self, point: Point, batch: &Prepared) {
        self.hooks
            .reached(Checkpoint {
                point,
                gateway: batch.gateway.clone(),
                stream: batch.stream.clone(),
                seq: batch.seq.get(),
            })
            .await;
    }

    /// Writes `batch` for `tenant` under the stream's lock (rule I1; module documentation).
    async fn write(
        &self,
        batch: &Prepared,
        tenant: GatewayTenant,
        received_at: u64,
    ) -> Result<Written, WriteError> {
        let store = Store::new(&self.clickhouse, batch);
        let mark = store.high_water_mark().await?;
        if batch.seq.get() <= mark.max_seq {
            if mark.committed_rows.is_none() {
                tracing::warn!(
                    gateway_id = %batch.gateway,
                    stream_id = %batch.stream,
                    seq = batch.seq.get(),
                    high_water_mark = mark.max_seq,
                    "a batch below the stream's high-water mark has no completion record; \
                     answered duplicate with 0 records"
                );
            }
            let records = mark
                .committed_rows
                .map_or(0, |rows| u32::try_from(rows).unwrap_or(u32::MAX));
            return Ok(Written::Duplicate { records });
        }
        self.checkpoint(Point::Locked, batch).await;
        let tenant_id = tenant.tenant_id.to_string();
        // An earlier attempt's rows, of any tenant: the gateway may have been bound to another
        // tenant since, and the batch is written once, for the tenant it is bound to now.
        let leftover = store.leftover_tenants().await?;
        if !leftover.is_empty() {
            store.delete_rows().await?;
            // The aggregates of another tenant still count the deleted rows: reconcile them now.
            // This attempt's own tenant is reconciled after the insert.
            for other in leftover.iter().filter(|other| **other != tenant_id) {
                tracing::warn!(
                    gateway_id = %batch.gateway,
                    stream_id = %batch.stream,
                    seq = batch.seq.get(),
                    "an earlier attempt wrote this batch for another tenant; its rows are \
                     deleted and the batch is written for the gateway's current tenant"
                );
                store.reconcile(other).await?;
            }
            self.checkpoint(Point::Deleted, batch).await;
        }
        let received = datetime64(received_at).ok_or(WriteError::Encode)?;
        let rows = batch
            .rows(&tenant_id, &received)
            .map_err(|_| WriteError::Encode)?;
        store.insert_rows(&rows).await?;
        self.checkpoint(Point::RowsWritten, batch).await;
        store.reconcile(&tenant_id).await?;
        self.checkpoint(Point::BeforeCommitRecord, batch).await;
        // Taken once the rows are on disk, and never before they were received, so the record
        // does not expire before them.
        let committed_at = datetime64(now_millis().max(received_at)).ok_or(WriteError::Encode)?;
        let binding_id = tenant.binding_id.to_string();
        let record = CommitRecord {
            tenant_id: &tenant_id,
            binding_id: &binding_id,
            gateway_id: batch.gateway.as_str(),
            stream_id: batch.stream.as_str(),
            seq: batch.seq.get(),
            row_count: batch.records,
            committed_at: &committed_at,
        };
        let mut line = serde_json::to_string(&record).map_err(|_| WriteError::Encode)?;
        line.push('\n');
        store.commit(&line).await?;
        Ok(Written::Committed)
    }

    async fn send(&self, reply: Reply) {
        let ack = TelemetryAck {
            v: V1,
            stream: reply.stream,
            seq: reply.seq,
            outcome: reply.outcome,
            server_time: UnixMillis::new(now_millis()),
        };
        let payload = match protocol::encode(&ack) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(
                    gateway_id = %reply.gateway,
                    stream_id = %ack.stream,
                    seq = ack.seq.get(),
                    error = %error,
                    "could not encode a telemetry acknowledgement"
                );
                return;
            }
        };
        let delivery = Channel::TelemetryAck.delivery();
        let options = PublishOptions {
            retain: delivery.retain,
            message_expiry: delivery.message_expiry,
        };
        let topic = Channel::TelemetryAck.topic(&reply.gateway);
        if let Err(error) = self.client.publish(&topic, payload, options).await {
            tracing::warn!(
                gateway_id = %reply.gateway,
                stream_id = %ack.stream,
                seq = ack.seq.get(),
                error = %error,
                "could not publish a telemetry acknowledgement; the gateway sends the batch again"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_connection_stays_with_the_binding_consumer() {
        assert_eq!(concurrency(2), 1);
        assert_eq!(concurrency(10), 9);
        assert_eq!(concurrency(1), 1);
        assert_eq!(concurrency(0), 1);
    }

    #[test]
    fn every_stream_has_its_own_lock() {
        let id = |text: &str| GatewayId::new(text).unwrap();
        let stream = |text: &str| StreamId::new(text).unwrap();
        assert_ne!(
            lock_job(&id("a-b"), &stream("c")),
            lock_job(&id("a"), &stream("b-c"))
        );
        assert_eq!(lock_job(&id("gw"), &stream("s")), "telemetry/gw/s");
    }
}
