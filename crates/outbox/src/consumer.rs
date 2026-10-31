//! Reading a producer's log and processing each event once, in order.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use chrono::{DateTime, Utc};
use contract::{DecodeError, EventEnvelope, EventId, TenantId};
use service_kit::postgres::UnsafeSession;
use service_kit::tenant::{
    BackgroundTenantDb, PlatformActor, PlatformDb, PlatformReader, PlatformTransaction,
    TenantDbError, TenantTransaction,
};
use sqlx::{PgConnection, Postgres, Transaction};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::aggregate::Aggregate;
use crate::lag::{ClearOnDrop, Lag, LagCell};
use crate::position::Position;
use crate::redact::SafeSqlx;
use crate::schema::is_schema_name;

/// The purpose the consumer's platform reader is logged with.
const READ_PURPOSE: &str = "outbox-consumer-read";

/// The purpose of a delivery's transaction, logged at DEBUG.
const DELIVERY_PURPOSE: &str = "outbox-delivery";

/// The purpose of the consumer's own bookkeeping transactions (subscription, dead letters).
const BOOKKEEPING_PURPOSE: &str = "outbox-consumer";

/// Whether the reader sees every row of `outbox_event` in the producer's schema: row-level
/// security is on (service-kit refuses the session otherwise), a permissive policy that applies
/// to the reader's role allows reading with `USING (true)` (the `consumer_read` template), and
/// no restrictive policy narrows it. A reader without such a policy would see no tenant's events
/// and report no lag, so the consumer refuses to run instead.
const READS_ALL_TENANTS: &str = "\
    SELECT EXISTS ( \
        SELECT 1 FROM pg_policy p \
        WHERE p.polrelid = c.oid AND p.polpermissive AND p.polcmd IN ('r', '*') \
          AND p.polqual IS NOT NULL AND pg_get_expr(p.polqual, p.polrelid) = 'true' \
          AND (0 = ANY (p.polroles) OR EXISTS ( \
              SELECT 1 FROM unnest(p.polroles) AS role \
              WHERE role <> 0 AND pg_has_role(current_user, role, 'MEMBER'))) \
    ) AND NOT EXISTS ( \
        SELECT 1 FROM pg_policy p \
        WHERE p.polrelid = c.oid AND NOT p.polpermissive AND p.polcmd IN ('r', '*') \
          AND (0 = ANY (p.polroles) OR EXISTS ( \
              SELECT 1 FROM unnest(p.polroles) AS role \
              WHERE role <> 0 AND pg_has_role(current_user, role, 'MEMBER'))) \
    ) \
    FROM pg_class c WHERE c.oid = to_regclass('outbox_event') AND c.relrowsecurity";

/// Tries the transaction-level advisory lock of subscription `$1` in the consumer's schema.
/// Replicas take turns through it; it does not give the transaction an ID.
const TRY_LOCK_SUBSCRIPTION: &str = "SELECT pg_try_advisory_xact_lock(hashtextextended(\
     'outbox.subscription:' || coalesce(current_schema(), '') || ':' || $1, 0))";

/// Waits for the same lock, for [`replay_from`].
const LOCK_SUBSCRIPTION: &str = "SELECT pg_advisory_xact_lock(hashtextextended(\
     'outbox.subscription:' || coalesce(current_schema(), '') || ':' || $1, 0))";

/// The longest subscription name, in bytes.
pub const MAX_SUBSCRIPTION_LEN: usize = 100;

/// How many times in a row one event may make its handler panic in a replica: the first panics
/// are retried with backoff like [`HandlerError::retry`], and the event is dead-lettered
/// (`handler panicked repeatedly`) at this one, so a handler that always panics on an event
/// cannot stop the subscription for good. An attempt that ends without a panic (`begin` fails
/// or `handle` returns) starts the count over.
pub const MAX_HANDLER_PANICS: u32 = 3;

/// Where a new subscription starts reading. Ignored once the subscription exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StartFrom {
    /// With the first event in the log (the default): a new consumer sees the whole history.
    #[default]
    Beginning,
    /// After the history: a new consumer sees what is committed from now on. The start is the
    /// read horizon (the oldest write transaction still running), so no event committed after
    /// the subscription is created is missed, and a few committed just before may be included.
    Latest,
}

/// How a subscription reads one producer's log.
#[derive(Clone, Debug)]
pub struct ConsumerConfig {
    subscription: String,
    source_schema: String,
    start_from: StartFrom,
    batch_size: u16,
    poll_interval: Duration,
    max_backoff: Duration,
    lag_interval: Duration,
}

impl ConsumerConfig {
    /// A subscription named `subscription` (unique within the consuming service, kept forever:
    /// renaming it starts a new subscription) that reads the log in the producer's schema
    /// `source_schema`.
    ///
    /// Defaults: start from the beginning, batches of 100 events, poll every second when idle,
    /// back off up to 30 seconds after failures, sample the lag every 15 seconds.
    ///
    /// # Errors
    ///
    /// When the subscription name is not 1–100 bytes of lower case letters, digits, `_`, `.`,
    /// and `-` starting with a letter or digit, or the schema name would need quoting.
    pub fn new(
        subscription: impl Into<String>,
        source_schema: impl Into<String>,
    ) -> Result<Self, InvalidConfig> {
        let subscription = subscription.into();
        let source_schema = source_schema.into();
        if !is_subscription_name(&subscription) {
            return Err(InvalidConfig::Subscription);
        }
        if !is_schema_name(&source_schema) {
            return Err(InvalidConfig::SourceSchema);
        }
        Ok(Self {
            subscription,
            source_schema,
            start_from: StartFrom::Beginning,
            batch_size: 100,
            poll_interval: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            lag_interval: Duration::from_secs(15),
        })
    }

    /// Where a new subscription starts.
    #[must_use]
    pub fn start_from(mut self, start: StartFrom) -> Self {
        self.start_from = start;
        self
    }

    /// How many events one poll reads at most (at least 1).
    #[must_use]
    pub fn batch_size(mut self, size: u16) -> Self {
        self.batch_size = size.max(1);
        self
    }

    /// How long to wait before polling again once caught up. This bounds the delivery latency.
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// The longest wait between retries of a failing event.
    #[must_use]
    pub fn max_backoff(mut self, backoff: Duration) -> Self {
        self.max_backoff = backoff.max(Duration::from_millis(10));
        self
    }

    /// How often [`Consumer::run`] measures the lag. A measurement is reported for
    /// [`crate::lag::CURRENT_FOR_INTERVALS`] intervals at most.
    #[must_use]
    pub fn lag_interval(mut self, interval: Duration) -> Self {
        self.lag_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// The subscription's name.
    #[must_use]
    pub fn subscription(&self) -> &str {
        &self.subscription
    }

    /// The producer's schema.
    #[must_use]
    pub fn source_schema(&self) -> &str {
        &self.source_schema
    }
}

fn is_subscription_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SUBSCRIPTION_LEN
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

/// A [`ConsumerConfig`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidConfig {
    /// The subscription name is not allowed.
    #[error(
        "subscription names are 1-100 bytes of lower case letters, digits, `_`, `.`, and `-`, \
         starting with a letter or digit"
    )]
    Subscription,
    /// The schema name would need quoting.
    #[error(
        "source schema names are lower case letters, digits, and `_`, starting with a letter or \
         `_`, at most 63 bytes, not starting with `pg_`"
    )]
    SourceSchema,
}

/// One event handed to a [`Handler`].
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The event's position in the producer's log.
    pub position: Position,
    /// The aggregate the event belongs to.
    pub aggregate: Aggregate,
    /// The event's number within its aggregate, starting at 1.
    pub aggregate_seq: i64,
    /// The event.
    pub envelope: EventEnvelope,
    /// When the producer wrote the event.
    pub stored_at: DateTime<Utc>,
}

impl Delivery {
    /// The event's tenant, `None` for a platform-wide event. It is the tenant the producer's
    /// row-level security checked when the event was written (the log row's `tenant_id`, which
    /// the consumer compares with the envelope), and the tenant of the delivery's transaction.
    #[must_use]
    pub fn tenant(&self) -> Option<TenantId> {
        self.envelope.tenant_id
    }
}

/// Processes the events of one subscription.
///
/// For every event the consumer opens a transaction of the consuming service's role in the
/// event's tenant (decision 29: service-kit sets the tenant as the first statement; a
/// platform-wide event gets a transaction without a tenant), calls [`Handler::begin`], takes the
/// subscription's lock, checks the event ID against the inbox, calls [`Handler::handle`],
/// records the event ID, advances the subscription, and commits. Whatever the handler writes
/// through `tx` therefore commits exactly when the event counts as processed: a crash or error
/// before the commit leaves nothing behind, and the event is delivered again. Row-level security
/// limits the handler to the event's tenant: it sees no other tenant's rows and cannot write
/// any, and in a platform-wide event's transaction it sees and writes no tenant's rows at all.
///
/// Keep the time between the handler's first write and the commit short: from its first write
/// a transaction holds back the read horizon of every consumer in the Postgres cluster (see the
/// README). Do slow reads, such as a call to another service, before writing.
///
/// A panic in [`Handler::begin`] or [`Handler::handle`] does not stop the consumer. The
/// consumer catches it, rolls the delivery's transaction back, logs an ERROR without the panic
/// message, and retries the event with backoff; at the [`MAX_HANDLER_PANICS`]th panic in a row
/// it dead-letters the event. The same handler object is called again after a panic, so state
/// it shares must not be left half-updated (a poisoned `Mutex`, for example). A binary built
/// with `panic = "abort"` still exits.
pub trait Handler: Send + Sync + 'static {
    /// Runs first in each delivery's transaction, after the tenant and the consumer's
    /// transaction marker are set and before the consumer touches any table, for transaction
    /// settings such as `statement_timeout`. The default does nothing. The rules of
    /// [`Handler::handle`] for `tx` apply here too; they are checked whenever `begin` returns
    /// or panics, before the consumer takes the subscription's lock, so also for an event the
    /// inbox then skips as already processed (which never reaches `handle`).
    fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send {
        let _ = (tx, delivery);
        async { Ok(()) }
    }

    /// Applies one event. Write only through `tx`, which is in [`Delivery::tenant`]; side
    /// effects outside the database (calls, messages) may happen more than once, because an
    /// event whose transaction fails is delivered again.
    ///
    /// Never end `tx` (`COMMIT`, `ROLLBACK`) or change its tenant or role. After `handle`
    /// returns, the consumer checks that `tx` is still the delivery's transaction and in the
    /// event's tenant; if not, it rolls back what is still open and dead-letters the event
    /// (`handler ended the delivery's transaction`, `handler changed the delivery's tenant`)
    /// instead of retrying it. Writes made before a handler's own `COMMIT` are committed by
    /// then and stay.
    fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send;
}

/// Whether a failed event is retried or set aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Retry the same event with backoff; later events wait, so the order holds.
    Retry,
    /// Record the event in `outbox_dead_letter` and move on. Use only when no retry can ever
    /// succeed, such as an event that cannot be decoded.
    Permanent,
}

/// A handler could not process an event.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct HandlerError {
    kind: FailureKind,
    reason: Cow<'static, str>,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl HandlerError {
    /// A failure that may pass: the event is retried. `reason` is logged and must not contain
    /// event data or personal data.
    pub fn retry(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind: FailureKind::Retry,
            reason: reason.into(),
            source: None,
        }
    }

    /// A failure no retry can fix: the event is dead-lettered. `reason` is logged and stored
    /// and must not contain event data or personal data.
    pub fn permanent(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind: FailureKind::Permanent,
            reason: reason.into(),
            source: None,
        }
    }

    /// Attaches the underlying error. Only its kind (for database errors, the SQLSTATE) is
    /// logged, because error messages can quote values.
    #[must_use]
    pub fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Whether the event is retried or dead-lettered.
    #[must_use]
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// The reason given by the handler.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The SQLSTATE of an underlying database error, for logs.
    fn sqlstate(&self) -> Option<String> {
        let source = self.source.as_deref()?;
        let error = source.downcast_ref::<sqlx::Error>()?;
        sqlstate(error)
    }
}

impl From<sqlx::Error> for HandlerError {
    /// Database errors are retried: most are transient (connection loss, serialization
    /// failures, deadlocks), and a permanent one keeps failing visibly instead of losing the
    /// event.
    fn from(error: sqlx::Error) -> Self {
        Self::retry("database error").with_source(error)
    }
}

impl From<DecodeError> for HandlerError {
    /// A payload newer than this build is retried until the consumer is upgraded (the
    /// contract's rule); every other decode error is permanent.
    fn from(error: DecodeError) -> Self {
        if error.is_retryable() {
            Self::retry("event version is newer than this build").with_source(error)
        } else {
            Self::permanent("event cannot be decoded").with_source(error)
        }
    }
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(database) => database.code().map(Cow::into_owned),
        _ => None,
    }
}

/// How [`replay_from`] treats events the subscription has already processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    /// Skip them: the inbox still lists them, so only events never processed (and dead-lettered
    /// ones) reach the handler again. Use to re-read a range safely.
    SkipProcessed,
    /// Forget them, so the handler sees every event again. Use to rebuild state, clearing
    /// that state in the same transaction after [`replay_from`].
    Reprocess,
}

/// Moves `subscription` so that its next poll delivers the events after `after`, for example
/// [`Position::START`] for the whole log, or [`Position::just_before`] of an event to include
/// it. Run it in a transaction of the consuming service; with [`Replay::Reprocess`], clear the
/// state to rebuild in the same transaction, after this call. It waits for an event being
/// processed to finish and then holds the subscription's lock until the transaction ends, so
/// call it before touching any table a delivery may write: the other order can deadlock with
/// a delivery in flight.
///
/// # Errors
///
/// When the subscription does not exist, or a query fails.
pub async fn replay_from(
    tx: &mut Transaction<'_, Postgres>,
    subscription: &str,
    after: Position,
    replay: Replay,
) -> Result<(), ConsumerError> {
    // Waits for a delivery in progress; deliveries started later wait for this transaction.
    sqlx::query(LOCK_SUBSCRIPTION)
        .bind(subscription)
        .execute(&mut **tx)
        .await?;
    let updated = sqlx::query(
        "UPDATE outbox_subscription SET position_tx = $2, position_id = $3, updated_at = now() \
         WHERE name = $1",
    )
    .bind(subscription)
    .bind(after.tx())
    .bind(after.id())
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ConsumerError::UnknownSubscription(subscription.to_owned()));
    }
    if replay == Replay::Reprocess {
        sqlx::query(
            "DELETE FROM outbox_inbox WHERE subscription = $1 \
             AND (position_tx, position_id) > ($2, $3)",
        )
        .bind(subscription)
        .bind(after.tx())
        .bind(after.id())
        .execute(&mut **tx)
        .await?;
    }
    tracing::info!(
        subscription,
        position = %after,
        reprocess = replay == Replay::Reprocess,
        "outbox subscription moved for replay"
    );
    Ok(())
}

/// A consumer-side failure outside the handler.
#[derive(Debug, thiserror::Error)]
pub enum ConsumerError {
    /// A query failed; usually transient.
    #[error("outbox query failed: {}", crate::redact::SafeSqlx(.0))]
    Database(#[from] sqlx::Error),
    /// The subscription has not been created; see [`Consumer::ensure_subscription`].
    #[error("outbox subscription `{0}` does not exist")]
    UnknownSubscription(String),
    /// The subscription exists but reads another producer's log; its positions mean nothing in
    /// this one. Pick a new subscription name instead.
    #[error(
        "outbox subscription `{subscription}` reads schema `{stored}`, not the configured \
         `{configured}`"
    )]
    SourceMismatch {
        /// The subscription.
        subscription: String,
        /// The schema it was created for.
        stored: String,
        /// The schema now configured.
        configured: String,
    },
    /// service-kit refused a database session: the role could bypass or escape row-level
    /// security (for example, a policy opens `outbox_event` across tenants to the service role
    /// rather than the platform role). The database's migration has to fix it.
    #[error("refused the database session: {0}")]
    UnsafeSession(UnsafeSession),
    /// The platform role reads the producer's `outbox_event` without a `USING (true)` read
    /// policy (the `consumer_read` template), so it would see no tenant's events and report no
    /// lag. The producer's migration has to grant it.
    #[error(
        "the platform role cannot read every tenant's events in schema `{source_schema}`: \
         `outbox_event` needs a consumer_read policy for it"
    )]
    CannotReadAllTenants {
        /// The producer's schema.
        source_schema: String,
    },
    /// Any other failure of the kit's database access.
    #[error("tenant database error")]
    TenantDb(#[source] TenantDbError),
}

impl ConsumerError {
    /// Whether retrying can help. A subscription that reads another producer cannot be fixed;
    /// a refused session or a missing read policy can, by a migration, so those are retried.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        !matches!(
            self,
            Self::UnknownSubscription(_) | Self::SourceMismatch { .. }
        )
    }
}

impl From<TenantDbError> for ConsumerError {
    fn from(error: TenantDbError) -> Self {
        match error {
            TenantDbError::Database(error) => Self::Database(error),
            TenantDbError::UnsafeSession(problem) => Self::UnsafeSession(problem),
            other => Self::TenantDb(other),
        }
    }
}

/// What one [`Consumer::poll_once`] did.
#[derive(Debug, Default)]
pub struct Progress {
    /// Events handed to the handler and committed.
    pub delivered: usize,
    /// Events skipped because the inbox already listed them.
    pub duplicates: usize,
    /// Events recorded in `outbox_dead_letter`.
    pub dead_lettered: usize,
    /// Why the poll stopped.
    pub outcome: Outcome,
}

/// Why a poll stopped.
#[derive(Debug, Default)]
pub enum Outcome {
    /// Every event readable now has been processed.
    #[default]
    CaughtUp,
    /// More events are waiting; poll again at once.
    MoreAvailable,
    /// Another replica is processing this subscription.
    Busy,
    /// The handler failed and asked for a retry; the event stays first in line.
    Retry {
        /// The event that failed.
        event_id: EventId,
        /// The handler's error.
        error: HandlerError,
    },
}

/// A row of the producer's log.
struct Row {
    position: Position,
    event_id: Uuid,
    event_type: String,
    /// The tenant the producer's row-level security checked on insert.
    tenant_id: Option<Uuid>,
    aggregate: Aggregate,
    aggregate_seq: i64,
    envelope: String,
    stored_at: DateTime<Utc>,
}

type RowTuple = (
    i64,
    i64,
    Uuid,
    String,
    Option<Uuid>,
    String,
    String,
    i64,
    String,
    DateTime<Utc>,
);

impl From<RowTuple> for Row {
    fn from(row: RowTuple) -> Self {
        let (
            id,
            tx,
            event_id,
            event_type,
            tenant_id,
            kind,
            aggregate_id,
            aggregate_seq,
            envelope,
            stored,
        ) = row;
        Self {
            position: Position::new(tx, id),
            event_id,
            event_type,
            tenant_id,
            aggregate: Aggregate::from_stored(kind, aggregate_id),
            aggregate_seq,
            envelope,
            stored_at: stored,
        }
    }
}

/// A delivery's transaction: in the event's tenant, or without a tenant for a platform-wide
/// event. The consumer commits it or rolls it back with [`roll_back`]; it is dropped (and rolled
/// back on drop) only when the delivery itself is dropped.
enum DeliveryTransaction {
    Tenant(TenantTransaction),
    NoTenant(Transaction<'static, Postgres>),
}

impl DeliveryTransaction {
    fn transaction(&mut self) -> &mut Transaction<'static, Postgres> {
        match self {
            Self::Tenant(tx) => tx.transaction(),
            Self::NoTenant(tx) => tx,
        }
    }

    async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            Self::Tenant(tx) => tx.commit().await,
            Self::NoTenant(tx) => tx.commit().await,
        }
    }

    async fn rollback(self) -> Result<(), sqlx::Error> {
        match self {
            Self::Tenant(tx) => tx.rollback().await,
            Self::NoTenant(tx) => tx.rollback().await,
        }
    }
}

/// The result of trying one event.
enum Step {
    Delivered,
    Duplicate,
    DeadLettered,
    /// Another replica holds the subscription.
    Busy,
    /// Another replica moved the subscription; read again from its position.
    Moved,
    Retry(HandlerError),
}

/// How a delivery's transaction ends, decided while it is still open.
enum Ending {
    /// Commit: the event was processed, or skipped as already processed.
    Commit(Step),
    /// Roll back, and only then go on as [`Abandoned`] says.
    RollBack(Abandoned),
}

/// What follows a delivery whose transaction is rolled back.
enum Abandoned {
    /// Stop the poll here: another replica holds or moved the subscription.
    Stop(Step),
    /// The handler returned an error: retry the event, or dead-letter it if it is permanent.
    Failed(HandlerError),
    /// The handler ended the transaction or moved its tenant: dead-letter the event.
    Broken(HandlerError),
    /// The handler panicked; with what [`broken_after_panic`] found in the transaction.
    Panicked(Option<HandlerError>),
}

/// The subscription's lock outcome inside a delivery transaction.
enum Claim {
    Claimed,
    Busy,
    Moved,
}

/// Reads one subscription and hands each event to a [`Handler`]. Cheap to clone; clones share
/// the handler and the lag reading.
///
/// Run one per subscription in each replica: replicas take turns through a transaction-level
/// advisory lock per subscription, and every delivery advances the position only from the one
/// it started at, so events are processed one at a time, in order.
pub struct Consumer<H> {
    inner: Arc<Inner<H>>,
}

impl<H> Clone for Consumer<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<H> {
    /// The consuming service's platform role: reads the producer's log across tenants.
    events: PlatformReader,
    /// The consuming service's role: deliveries in the event's tenant, and the consumer tables.
    work: BackgroundTenantDb,
    config: ConsumerConfig,
    handler: H,
    lag: Arc<LagCell>,
    /// The event whose handler panicked last in this replica, and how many times in a row.
    panics: Mutex<Option<(Uuid, u32)>>,
}

impl<H: Handler> Consumer<H> {
    /// A consumer of the consuming service (decision 29):
    ///
    /// - `events` is the service's platform role. It reads `outbox_event` in the producer's
    ///   schema across tenants, through the producer's `consumer_read` policy, in read-only
    ///   transactions. The reader is made here and logged once at INFO with the actor (the
    ///   service of `work`) and the purpose `outbox-consumer-read`.
    /// - `work` is the service role's background handle. Each delivery runs in a transaction of
    ///   the event's tenant from it; the consumer tables are in the schema its role's
    ///   `search_path` starts with.
    ///
    /// The lag gauges are registered with the global meter provider on the first call in a
    /// process, so create consumers after telemetry is initialized (`service_kit::run` does that
    /// before calling the service's start function).
    pub fn new(
        events: &PlatformDb,
        work: BackgroundTenantDb,
        config: ConsumerConfig,
        handler: H,
    ) -> Self {
        let events = events.reader(PlatformActor::service(work.actor()), READ_PURPOSE);
        let lag = LagCell::register(
            &config.subscription,
            &config.source_schema,
            config.lag_interval,
        );
        Self {
            inner: Arc::new(Inner {
                events,
                work,
                config,
                handler,
                lag,
                panics: Mutex::new(None),
            }),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ConsumerConfig {
        &self.inner.config
    }

    /// The handler.
    #[must_use]
    pub fn handler(&self) -> &H {
        &self.inner.handler
    }

    /// Creates the subscription if it does not exist yet, starting as configured, and checks
    /// that the platform role reads every tenant's events.
    ///
    /// # Errors
    ///
    /// [`ConsumerError::SourceMismatch`] when the subscription exists for another schema,
    /// [`ConsumerError::CannotReadAllTenants`] when the producer's log has no read policy for
    /// the platform role, [`ConsumerError::UnsafeSession`] when service-kit refuses a session,
    /// or a query error.
    pub async fn ensure_subscription(&self) -> Result<(), ConsumerError> {
        let config = &self.inner.config;
        let start = match config.start_from {
            StartFrom::Beginning => Position::START,
            StartFrom::Latest => {
                let mut tx = self.source_transaction().await?;
                let horizon = horizon(&mut tx).await?;
                tx.commit().await?;
                // Every event below the horizon has committed or never will; every later one
                // sorts after this position.
                Position::new(horizon.saturating_sub(1), i64::MAX)
            }
        };
        let mut tx = self
            .inner
            .work
            .begin_without_tenant(BOOKKEEPING_PURPOSE)
            .await?;
        sqlx::query(
            "INSERT INTO outbox_subscription (name, source_schema, position_tx, position_id) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (name) DO NOTHING",
        )
        .bind(&config.subscription)
        .bind(&config.source_schema)
        .bind(start.tx())
        .bind(start.id())
        .execute(&mut *tx)
        .await?;
        let stored: String =
            sqlx::query_scalar("SELECT source_schema FROM outbox_subscription WHERE name = $1")
                .bind(&config.subscription)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        if stored != config.source_schema {
            return Err(ConsumerError::SourceMismatch {
                subscription: config.subscription.clone(),
                stored,
                configured: config.source_schema.clone(),
            });
        }
        let mut tx = self.source_transaction().await?;
        let reads_all: Option<bool> = sqlx::query_scalar(READS_ALL_TENANTS)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        // No row: the log is missing or row-level security is off. Then service-kit refuses the
        // platform role's session or reading fails, each with its own error.
        if reads_all == Some(false) {
            return Err(ConsumerError::CannotReadAllTenants {
                source_schema: config.source_schema.clone(),
            });
        }
        Ok(())
    }

    /// The position of the last event the subscription processed or dead-lettered.
    ///
    /// # Errors
    ///
    /// When the subscription does not exist, or the query fails.
    pub async fn position(&self) -> Result<Position, ConsumerError> {
        let mut tx = self
            .inner
            .work
            .begin_without_tenant(BOOKKEEPING_PURPOSE)
            .await?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT position_tx, position_id FROM outbox_subscription WHERE name = $1",
        )
        .bind(&self.inner.config.subscription)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(|(tx, id)| Position::new(tx, id)).ok_or_else(|| {
            ConsumerError::UnknownSubscription(self.inner.config.subscription.clone())
        })
    }

    /// Reads one batch and processes it in order, stopping early when the handler asks for a
    /// retry or another replica takes over.
    ///
    /// # Errors
    ///
    /// When a query of the consumer itself fails or the subscription does not exist. Handler
    /// failures are reported in [`Progress::outcome`] instead.
    pub async fn poll_once(&self) -> Result<Progress, ConsumerError> {
        self.poll(None).await
    }

    /// [`Consumer::poll_once`], stopping between events once `stop` is cancelled, so shutdown
    /// waits for one event at most rather than a whole batch.
    async fn poll(&self, stop: Option<&CancellationToken>) -> Result<Progress, ConsumerError> {
        let start = self.position().await?;
        let batch = self.fetch(start).await?;
        let full = batch.len() >= usize::from(self.inner.config.batch_size);
        let mut progress = Progress::default();
        let mut expected = start;
        for row in batch {
            if stop.is_some_and(CancellationToken::is_cancelled) {
                progress.outcome = Outcome::MoreAvailable;
                return Ok(progress);
            }
            let position = row.position;
            let event_id = EventId::from_uuid(row.event_id);
            match self.deliver(expected, row).await? {
                Step::Delivered => progress.delivered += 1,
                Step::Duplicate => progress.duplicates += 1,
                Step::DeadLettered => progress.dead_lettered += 1,
                Step::Busy => {
                    progress.outcome = Outcome::Busy;
                    return Ok(progress);
                }
                Step::Moved => {
                    progress.outcome = Outcome::MoreAvailable;
                    return Ok(progress);
                }
                Step::Retry(error) => {
                    progress.outcome = Outcome::Retry { event_id, error };
                    return Ok(progress);
                }
            }
            expected = position;
        }
        progress.outcome = if full {
            Outcome::MoreAvailable
        } else {
            Outcome::CaughtUp
        };
        Ok(progress)
    }

    /// The last lag measurement while it is current, which is what the gauges report: `None`
    /// before the first, once [`Consumer::monitor_lag`] has ended (shutdown, or its task
    /// dropped), and when no measurement has replaced it for
    /// [`crate::lag::CURRENT_FOR_INTERVALS`] lag intervals.
    #[must_use]
    pub fn lag(&self) -> Option<Lag> {
        self.inner.lag.get()
    }

    /// Measures the lag now and publishes it to the gauges.
    ///
    /// # Errors
    ///
    /// When a query fails or the subscription does not exist.
    pub async fn measure_lag(&self) -> Result<Lag, ConsumerError> {
        let position = self.position().await?;
        let mut tx = self.source_transaction().await?;
        let lag = crate::lag::measure(&mut tx, position).await?;
        tx.commit().await?;
        self.inner.lag.set(lag);
        Ok(lag)
    }

    /// Creates the subscription, then processes events and measures the lag until `stop` is
    /// cancelled. An event being processed when `stop` fires is finished first. Query failures
    /// are logged and retried with backoff, and panics are caught (see [`Handler`]), so only
    /// `stop` ends processing.
    ///
    /// The lag is measured in a task of its own ([`Consumer::monitor_lag`]), which ends with
    /// `stop` or when this future is dropped. It does not depend on processing: while
    /// processing is stuck it keeps measuring, so the lag and its age rise instead of freezing.
    /// If it cannot run at all (a handler that blocks the thread it needs, say), its last
    /// measurement goes stale and is no longer reported.
    ///
    /// # Errors
    ///
    /// Only for a configuration mistake ([`ConsumerError::SourceMismatch`]), which no retry
    /// can fix.
    pub async fn run(self, stop: CancellationToken) -> Result<(), ConsumerError> {
        let mut failures = 0;
        loop {
            match self.ensure_subscription().await {
                Ok(()) => break,
                Err(error) if !error.is_retryable() => {
                    tracing::error!(
                        subscription = self.inner.config.subscription,
                        error = %error,
                        "outbox consumer cannot start"
                    );
                    return Err(error);
                }
                Err(error) => {
                    failures += 1;
                    tracing::warn!(
                        subscription = self.inner.config.subscription,
                        error = %error,
                        attempt = failures,
                        "outbox subscription could not be created; retrying"
                    );
                    if !pause(&stop, self.backoff(failures)).await {
                        return Ok(());
                    }
                }
            }
        }
        let mut monitor = {
            let consumer = self.clone();
            let stop = stop.clone();
            AbortOnDrop(tokio::spawn(
                async move { consumer.monitor_lag(stop).await }.in_current_span(),
            ))
        };
        self.process(stop).await;
        // `stop` is cancelled, so the monitor ends at its next pause; it catches its panics.
        let _ = (&mut monitor.0).await;
        Ok(())
    }

    /// Processes events until `stop` is cancelled; the subscription must exist. A panic,
    /// whether in the handler or in a poll, is logged at ERROR and retried with backoff.
    pub async fn process(&self, stop: CancellationToken) {
        let config = &self.inner.config;
        let mut failures: u32 = 0;
        while !stop.is_cancelled() {
            // Handler panics are caught (and counted) per delivery; this catches any other, so
            // nothing ends the loop but `stop`. A panic drops the poll's transactions, which
            // roll back; the next poll starts again from the stored position.
            let wait = match catch_panic(self.poll(Some(&stop))).await {
                Ok(Ok(progress)) => match progress.outcome {
                    Outcome::CaughtUp | Outcome::Busy => {
                        failures = 0;
                        config.poll_interval
                    }
                    Outcome::MoreAvailable => {
                        failures = 0;
                        Duration::ZERO
                    }
                    Outcome::Retry { event_id, error } => {
                        failures = failures.saturating_add(1);
                        tracing::warn!(
                            subscription = config.subscription,
                            event_id = %event_id,
                            attempt = failures,
                            reason = error.reason(),
                            sqlstate = error.sqlstate(),
                            "outbox event failed; retrying"
                        );
                        self.backoff(failures)
                    }
                },
                Ok(Err(error)) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(
                        subscription = config.subscription,
                        error = %error,
                        attempt = failures,
                        "outbox poll failed; retrying"
                    );
                    self.backoff(failures)
                }
                Err(Panicked) => {
                    failures = failures.saturating_add(1);
                    tracing::error!(
                        subscription = config.subscription,
                        attempt = failures,
                        "outbox poll panicked; retrying"
                    );
                    self.backoff(failures)
                }
            };
            if !wait.is_zero() && !pause(&stop, wait).await {
                break;
            }
        }
    }

    /// Measures the lag every `lag_interval` until `stop` is cancelled. Failures are logged at
    /// WARN and panics at ERROR, and measuring goes on. However this ends (`stop`, or the future
    /// dropped with its task), the gauges stop reporting the last measurement, and so does a
    /// failing or stuck measurement after [`crate::lag::CURRENT_FOR_INTERVALS`] intervals.
    pub async fn monitor_lag(&self, stop: CancellationToken) {
        let _clear = ClearOnDrop(&self.inner.lag);
        let subscription = &self.inner.config.subscription;
        while !stop.is_cancelled() {
            match catch_panic(self.measure_lag()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(
                    subscription,
                    error = %error,
                    "outbox lag could not be measured"
                ),
                Err(Panicked) => tracing::error!(
                    subscription,
                    "outbox lag measurement panicked; measuring again next interval"
                ),
            }
            if !pause(&stop, self.inner.config.lag_interval).await {
                break;
            }
        }
    }

    fn backoff(&self, failures: u32) -> Duration {
        let config = &self.inner.config;
        let base = config.poll_interval.max(Duration::from_millis(100));
        let factor = 2_u32.saturating_pow(failures.saturating_sub(1).min(16));
        base.saturating_mul(factor).min(config.max_backoff)
    }

    /// A read-only transaction of the platform role whose `search_path` is the producer's
    /// schema only, so the log is found there or not at all.
    async fn source_transaction(&self) -> Result<PlatformTransaction, ConsumerError> {
        let mut tx = self.inner.events.begin().await?;
        sqlx::query("SELECT set_config('search_path', $1, true)")
            .bind(&self.inner.config.source_schema)
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// The next events after `after` that can no longer be preceded by a later commit.
    async fn fetch(&self, after: Position) -> Result<Vec<Row>, ConsumerError> {
        let mut tx = self.source_transaction().await?;
        let horizon = horizon(&mut tx).await?;
        let rows: Vec<RowTuple> = sqlx::query_as(
            "SELECT id, tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, \
             aggregate_seq, envelope::text, stored_at FROM outbox_event \
             WHERE (tx_order, id) > ($1, $2) AND tx_order < $3 \
             ORDER BY tx_order, id LIMIT $4",
        )
        .bind(after.tx())
        .bind(after.id())
        .bind(horizon)
        .bind(i64::from(self.inner.config.batch_size))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(Row::from).collect())
    }

    async fn deliver(&self, expected: Position, row: Row) -> Result<Step, ConsumerError> {
        let span = tracing::info_span!(
            "outbox.deliver",
            subscription = self.inner.config.subscription,
            event_id = %row.event_id,
            event_type = row.event_type,
            position = %row.position,
        );
        async move {
            let Ok(envelope) = serde_json::from_str::<EventEnvelope>(&row.envelope) else {
                let error = HandlerError::permanent("stored envelope cannot be decoded");
                return self.dead_letter(expected, &row, &error).await;
            };
            // The producer's row-level security checked the row's tenant, not the envelope's:
            // an event whose envelope names another tenant must not run in that tenant.
            if envelope.tenant_id.map(|tenant| *tenant.as_uuid()) != row.tenant_id {
                let error = HandlerError::permanent("envelope tenant differs from the log row");
                return self.dead_letter(expected, &row, &error).await;
            }
            let delivery = Delivery {
                position: row.position,
                aggregate: row.aggregate.clone(),
                aggregate_seq: row.aggregate_seq,
                envelope,
                stored_at: row.stored_at,
            };
            let mut delivery_tx = match delivery.tenant() {
                Some(tenant) => DeliveryTransaction::Tenant(
                    self.inner.work.begin(tenant, DELIVERY_PURPOSE).await?,
                ),
                None => DeliveryTransaction::NoTenant(
                    self.inner
                        .work
                        .begin_without_tenant(DELIVERY_PURPOSE)
                        .await?,
                ),
            };
            let abandoned = match self
                .attempt(&mut delivery_tx, expected, &row, &delivery)
                .await
            {
                Ok(Ending::Commit(step)) => {
                    // A COMMIT that fails has ended the transaction as well (Postgres ends the
                    // transaction block either way), so nothing is left to roll back.
                    delivery_tx.commit().await?;
                    match step {
                        Step::Duplicate => {
                            tracing::debug!("outbox event already processed; skipped")
                        }
                        _ => tracing::debug!("outbox event processed"),
                    }
                    return Ok(step);
                }
                Ok(Ending::RollBack(abandoned)) => Ok(abandoned),
                Err(error) => Err(error),
            };
            roll_back(delivery_tx).await;
            match abandoned? {
                Abandoned::Stop(step) => Ok(step),
                Abandoned::Failed(error) => self.failed(expected, &row, error).await,
                Abandoned::Broken(error) => self.dead_letter(expected, &row, &error).await,
                Abandoned::Panicked(broken) => self.after_panic(expected, &row, broken).await,
            }
        }
        .instrument(span)
        .await
    }

    /// Runs one delivery in `delivery_tx` up to the point where it commits or rolls back:
    /// [`Handler::begin`], the subscription's lock, the inbox, [`Handler::handle`], the inbox
    /// entry, and the new position. [`Consumer::deliver`] ends the transaction as the result
    /// says; nothing here ends it.
    async fn attempt(
        &self,
        delivery_tx: &mut DeliveryTransaction,
        expected: Position,
        row: &Row,
        delivery: &Delivery,
    ) -> Result<Ending, ConsumerError> {
        let tx = delivery_tx.transaction();
        // A transaction-level marker identifies the transaction without giving it an ID (a
        // read, like everything before the handler's first write, so the transaction holds back
        // no consumer's horizon yet); it ends with the transaction. It is set before the
        // handler's hooks run, so the checks after them cover `begin` too.
        let marker = Uuid::now_v7().to_string();
        sqlx::query("SELECT set_config('outbox.delivery', $1, true)")
            .bind(&marker)
            .execute(&mut **tx)
            .await?;
        match catch_panic(self.inner.handler.begin(tx, delivery)).await {
            Ok(Ok(())) => {
                // Checked before anything else runs in the transaction: after a hook that
                // ended it, the lock, the inbox entry, the new position, and the handler's
                // writes would each commit on their own. This also covers an event the inbox
                // then skips, which never reaches `handle` and its check.
                if let Some(broken) = transaction_broken(tx, delivery, &marker).await? {
                    self.forget_panics(row.event_id);
                    tracing::error!(
                        reason = broken.reason(),
                        "outbox handler's begin hook broke its delivery's transaction"
                    );
                    return Ok(Ending::RollBack(Abandoned::Broken(broken)));
                }
            }
            Ok(Err(error)) => {
                // The attempt ends without a panic, so panics in a row start over.
                self.forget_panics(row.event_id);
                // As after `handle`: a hook that ended the transaction or moved its tenant is
                // dead-lettered, not retried. If the check cannot run (the transaction is
                // aborted), the hook's error stands.
                if let Ok(Some(broken)) = transaction_broken(tx, delivery, &marker).await {
                    tracing::error!(
                        reason = broken.reason(),
                        "outbox handler's begin hook broke its delivery's transaction"
                    );
                    return Ok(Ending::RollBack(Abandoned::Broken(broken)));
                }
                return Ok(Ending::RollBack(Abandoned::Failed(error)));
            }
            Err(Panicked) => {
                let broken = broken_after_panic(tx, delivery, &marker).await;
                return Ok(Ending::RollBack(Abandoned::Panicked(broken)));
            }
        }
        match self.claim(tx, expected).await? {
            Claim::Claimed => {}
            Claim::Busy => return Ok(Ending::RollBack(Abandoned::Stop(Step::Busy))),
            Claim::Moved => return Ok(Ending::RollBack(Abandoned::Stop(Step::Moved))),
        }
        // Only reads so far: the transaction gets an ID (and holds back every consumer's
        // horizon) no earlier than the handler's first write.
        let fresh: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM outbox_inbox \
             WHERE subscription = $1 AND event_id = $2)",
        )
        .bind(&self.inner.config.subscription)
        .bind(row.event_id)
        .fetch_one(&mut **tx)
        .await?;
        if fresh {
            let Ok(handled) = catch_panic(self.inner.handler.handle(tx, delivery)).await else {
                let broken = broken_after_panic(tx, delivery, &marker).await;
                return Ok(Ending::RollBack(Abandoned::Panicked(broken)));
            };
            self.forget_panics(row.event_id);
            // A handler's own failure leaves the transaction aborted, so the check cannot run;
            // that error then stands.
            let broken = match transaction_broken(tx, delivery, &marker).await {
                Ok(broken) => broken,
                Err(_) if handled.is_err() => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(broken) = broken {
                tracing::error!(
                    reason = broken.reason(),
                    "outbox handler broke its delivery's transaction; if it ended the \
                     transaction, the writes before that may have committed"
                );
                return Ok(Ending::RollBack(Abandoned::Broken(broken)));
            }
            if let Err(error) = handled {
                return Ok(Ending::RollBack(Abandoned::Failed(error)));
            }
            sqlx::query(
                "INSERT INTO outbox_inbox (subscription, event_id, position_tx, position_id) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(&self.inner.config.subscription)
            .bind(row.event_id)
            .bind(row.position.tx())
            .bind(row.position.id())
            .execute(&mut **tx)
            .await?;
        }
        if !self.advance(tx, expected, row.position).await? {
            return Ok(Ending::RollBack(Abandoned::Stop(Step::Moved)));
        }
        Ok(Ending::Commit(if fresh {
            Step::Delivered
        } else {
            Step::Duplicate
        }))
    }

    async fn failed(
        &self,
        expected: Position,
        row: &Row,
        error: HandlerError,
    ) -> Result<Step, ConsumerError> {
        match error.kind() {
            FailureKind::Retry => Ok(Step::Retry(error)),
            FailureKind::Permanent => self.dead_letter(expected, row, &error).await,
        }
    }

    /// [`Handler::begin`] or [`Handler::handle`] panicked while delivering `row`, and the
    /// delivery's transaction has been rolled back; `broken` is what [`broken_after_panic`]
    /// found in it first.
    ///
    /// A handler that ended the transaction or moved it to another tenant before panicking is
    /// dead-lettered at once, because no retry fixes that. Otherwise the panic counts as a
    /// retryable failure until [`MAX_HANDLER_PANICS`] panics in a row for this event, and then
    /// as a permanent one.
    async fn after_panic(
        &self,
        expected: Position,
        row: &Row,
        broken: Option<HandlerError>,
    ) -> Result<Step, ConsumerError> {
        if let Some(broken) = broken {
            self.forget_panics(row.event_id);
            // The panic hook has logged the panic with its message; this line carries nothing a
            // handler could put in one.
            tracing::error!(
                reason = broken.reason(),
                "outbox handler panicked after breaking its delivery's transaction; if it ended \
                 the transaction, the writes before that may have committed"
            );
            return self.dead_letter(expected, row, &broken).await;
        }
        let attempt = self.note_panic(row.event_id);
        if attempt >= MAX_HANDLER_PANICS {
            self.forget_panics(row.event_id);
            tracing::error!(
                attempt,
                "outbox handler panicked; too many panics in a row for this event"
            );
            let error = HandlerError::permanent("handler panicked repeatedly");
            return self.dead_letter(expected, row, &error).await;
        }
        tracing::error!(
            attempt,
            limit = MAX_HANDLER_PANICS,
            "outbox handler panicked; the event is retried"
        );
        Ok(Step::Retry(HandlerError::retry("handler panicked")))
    }

    /// Counts a panic of `event`'s handler; returns how many in a row it has caused.
    fn note_panic(&self, event: Uuid) -> u32 {
        let mut panics = self
            .inner
            .panics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let count = match *panics {
            Some((last, count)) if last == event => count.saturating_add(1),
            _ => 1,
        };
        *panics = Some((event, count));
        count
    }

    /// Resets `event`'s panic count: an attempt ended without a panic (`begin` failed or `handle`
    /// returned), or the event was dead-lettered.
    fn forget_panics(&self, event: Uuid) {
        let mut panics = self
            .inner
            .panics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(*panics, Some((last, _)) if last == event) {
            *panics = None;
        }
    }

    /// Records a permanently failed event and moves past it, in a transaction of its own (the
    /// handler's writes were rolled back).
    async fn dead_letter(
        &self,
        expected: Position,
        row: &Row,
        error: &HandlerError,
    ) -> Result<Step, ConsumerError> {
        let mut tx = self
            .inner
            .work
            .begin_without_tenant(BOOKKEEPING_PURPOSE)
            .await?;
        match self.claim(&mut tx, expected).await? {
            Claim::Claimed => {}
            Claim::Busy => return Ok(Step::Busy),
            Claim::Moved => return Ok(Step::Moved),
        }
        sqlx::query(
            "INSERT INTO outbox_dead_letter \
             (subscription, event_id, position_tx, position_id, reason) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (subscription, event_id) \
             DO UPDATE SET reason = EXCLUDED.reason, failed_at = now()",
        )
        .bind(&self.inner.config.subscription)
        .bind(row.event_id)
        .bind(row.position.tx())
        .bind(row.position.id())
        .bind(error.reason())
        .execute(&mut *tx)
        .await?;
        if !self.advance(&mut tx, expected, row.position).await? {
            return Ok(Step::Moved);
        }
        tx.commit().await?;
        tracing::error!(
            reason = error.reason(),
            sqlstate = error.sqlstate(),
            "outbox event failed permanently; recorded in outbox_dead_letter"
        );
        Ok(Step::DeadLettered)
    }

    /// Takes the subscription's lock for this transaction, unless another replica holds it, and
    /// checks that nobody processed past `expected` since the batch was read.
    ///
    /// The lock is a transaction-level advisory lock rather than a row lock: locking a row
    /// gives the transaction an ID at once, and a transaction with an ID holds back the read
    /// horizon of every consumer in the cluster for as long as the handler runs.
    async fn claim(
        &self,
        tx: &mut PgConnection,
        expected: Position,
    ) -> Result<Claim, ConsumerError> {
        let locked: bool = sqlx::query_scalar(TRY_LOCK_SUBSCRIPTION)
            .bind(&self.inner.config.subscription)
            .fetch_one(&mut *tx)
            .await?;
        if !locked {
            return Ok(Claim::Busy);
        }
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT position_tx, position_id FROM outbox_subscription WHERE name = $1",
        )
        .bind(&self.inner.config.subscription)
        .fetch_optional(tx)
        .await?;
        Ok(match row {
            Some((tx, id)) if Position::new(tx, id) == expected => Claim::Claimed,
            Some(_) => Claim::Moved,
            None => {
                return Err(ConsumerError::UnknownSubscription(
                    self.inner.config.subscription.clone(),
                ));
            }
        })
    }

    /// Moves the subscription from `from` to `to`; false when it is no longer at `from`
    /// (moved by [`replay_from`] or another replica), so the transaction must not commit.
    async fn advance(
        &self,
        tx: &mut PgConnection,
        from: Position,
        to: Position,
    ) -> Result<bool, sqlx::Error> {
        let updated = sqlx::query(
            "UPDATE outbox_subscription SET position_tx = $2, position_id = $3, updated_at = now() \
             WHERE name = $1 AND position_tx = $4 AND position_id = $5",
        )
        .bind(&self.inner.config.subscription)
        .bind(to.tx())
        .bind(to.id())
        .bind(from.tx())
        .bind(from.id())
        .execute(tx)
        .await?;
        Ok(updated.rows_affected() == 1)
    }
}

/// Whether the handler broke the delivery's transaction, checked after it returns: the
/// transaction must still carry `marker`, which the consumer set transaction-locally in
/// `outbox.delivery` before calling the handler (a handler that sends `COMMIT` or `ROLLBACK`
/// ends the transaction and the marker with it, and what follows runs in other transactions;
/// transaction timestamps are not unique, so they cannot tell), and still be in the
/// event's tenant (a handler that sets `trustiot.tenant_id` moves it). Either is a handler bug
/// that no retry fixes, so it is a permanent failure. A handler that switches the tenant and
/// back before it returns is not detected; decision 29 leaves SQL that sets the tenant to
/// `tools/sql-check`.
async fn transaction_broken(
    tx: &mut PgConnection,
    delivery: &Delivery,
    marker: &str,
) -> Result<Option<HandlerError>, sqlx::Error> {
    let (same, tenant): (bool, Option<String>) = sqlx::query_as(
        "SELECT current_setting('outbox.delivery', true) IS NOT DISTINCT FROM $1, \
         NULLIF(current_setting('trustiot.tenant_id', true), '')",
    )
    .bind(marker)
    .fetch_one(tx)
    .await?;
    if !same {
        return Ok(Some(HandlerError::permanent(
            "handler ended the delivery's transaction",
        )));
    }
    // A value that is no UUID is a change too (row-level security would fail on it).
    let current = tenant.map(|tenant| Uuid::parse_str(&tenant).ok());
    let expected = delivery.tenant().map(|tenant| Some(*tenant.as_uuid()));
    if current != expected {
        return Ok(Some(HandlerError::permanent(
            "handler changed the delivery's tenant",
        )));
    }
    Ok(None)
}

/// [`transaction_broken`] after a panic in [`Handler::begin`] or [`Handler::handle`].
///
/// The panic unwound out of the handler's future mid-poll and dropped it; the delivery's
/// transaction is still here, possibly with a statement the handler started and never finished.
/// sqlx reads such a statement's remaining results before the next one runs, so the connection
/// can still be used, and the check runs as for a handler that returned. A transaction the
/// handler left aborted cannot run the check; then the panic stands (`None`).
async fn broken_after_panic(
    tx: &mut PgConnection,
    delivery: &Delivery,
    marker: &str,
) -> Option<HandlerError> {
    transaction_broken(tx, delivery, marker)
        .await
        .ok()
        .flatten()
}

/// Rolls a delivery's transaction back and waits for the answer, on every path that does not
/// commit it (Laysi/trustiot-v2#350).
///
/// Dropping the transaction instead would only queue the `ROLLBACK` on its connection, to be
/// sent when the pool takes the connection back in a task of its own. Until then the
/// transaction, and with it the subscription's advisory lock, is still open: the dead-letter
/// transaction or the next poll that follows at once would find the lock taken and report
/// [`Outcome::Busy`], and the handler would see the event once more. Once this returns, the
/// server has ended the transaction and released its locks.
///
/// The connection then goes back to the pool through service-kit's release check, which closes
/// a session left with a session-level tenant, and sqlx closes one that does not answer. A
/// rollback that fails is logged; sqlx queues another `ROLLBACK` on the connection, and the
/// pool closes it if that fails too, which ends the transaction on the server.
async fn roll_back(delivery_tx: DeliveryTransaction) {
    if let Err(error) = delivery_tx.rollback().await {
        tracing::warn!(
            error = %SafeSqlx(&error),
            "outbox delivery could not be rolled back"
        );
    }
}

/// The oldest transaction still running. Every transaction below it has finished, so every
/// event ordered below it is either visible to the next statement or never will be.
pub(crate) async fn horizon(tx: &mut PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text::bigint")
        .fetch_one(tx)
        .await
}

/// A future panicked. The panic payload is deliberately not kept: the panic hook has logged it,
/// and the consumer's own log lines must carry nothing a handler could put in a panic message.
struct Panicked;

/// Awaits `future`, turning a panic while it is polled into `Err(Panicked)`; the same pattern as
/// service-kit's periodic jobs (Laysi/trustiot-v2#284).
///
/// The future is polled in place under `catch_unwind` rather than spawned as a task: the
/// handler borrows the delivery's transaction, which the consumer needs back after a panic to
/// check and roll it back (a task would need it moved in, and a panic would drop it inside the
/// task); cancellation stays as before (dropping the delivery drops the handler with it); and
/// the handler stays in the delivery's span. `AssertUnwindSafe` is sound here because nothing
/// the future touched is used after a panic without being checked: the future is dropped, the
/// transaction is checked and rolled back, the consumer's own state is behind a lock that
/// tolerates poisoning, and the handler's state is the handler's to keep consistent (see
/// [`Handler`]).
async fn catch_panic<Fut: Future>(future: Fut) -> Result<Fut::Output, Panicked> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|context| {
        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => {
                drop(payload);
                Poll::Ready(Err(Panicked))
            }
        }
    })
    .await
}

/// Aborts the task when dropped, so a dropped [`Consumer::run`] does not leave its lag monitor
/// running.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sleeps for `duration`; false when `stop` was cancelled first.
async fn pause(stop: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        () = stop.cancelled() => false,
        () = tokio::time::sleep(duration) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_names_are_checked() {
        for name in ["assets.tenant-sync", "a", "9_x"] {
            assert!(ConsumerConfig::new(name, "identity").is_ok(), "{name}");
        }
        for name in ["", "A", "-a", "a b", "a/b", "a".repeat(101).as_str()] {
            assert_eq!(
                ConsumerConfig::new(name, "identity").err(),
                Some(InvalidConfig::Subscription),
                "{name}"
            );
        }
        assert_eq!(
            ConsumerConfig::new("a", "Identity").err(),
            Some(InvalidConfig::SourceSchema)
        );
    }

    #[test]
    fn decode_errors_retry_only_newer_versions() {
        let newer = DecodeError::NewerVersion {
            event_type: "example.widget_renamed",
            found: 3,
            newest_known: 2,
        };
        assert_eq!(HandlerError::from(newer).kind(), FailureKind::Retry);
        let unknown = DecodeError::UnknownVersion {
            event_type: "example.widget_renamed",
            version: 0,
        };
        assert_eq!(HandlerError::from(unknown).kind(), FailureKind::Permanent);
    }
}
