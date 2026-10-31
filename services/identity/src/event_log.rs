//! The event layer and operation log (Laysi/trustiot-v2#130; services/identity/README.md,
//! "事件層與操作紀錄"; decision 34 in docs/architecture/decisions.md).
//!
//! Every service writes its events to its own outbox in the transaction of the change
//! (crates/outbox), so an event exists exactly when the change committed. The identity service
//! consumes the outbox of every producer in [`crate::schema::CONSUMES`] with an
//! [`EventLogRecorder`] each, and keeps every event once: tenant events in `identity.event_log`
//! (row-level security), platform-wide events in `identity.platform_event_log`. Both are
//! append-only: the service's roles insert and read, and no role but the migration Job's owner may
//! update or delete a row (`migrations/postgres/0003_event_log.sql`).
//!
//! The log is the platform's operation log (checklist K2): read newest first, in pages.
//!
//! | Route | Caller | Events |
//! | --- | --- | --- |
//! | [`EVENT_LOG_PATH`] | an administrator of the token's tenant | that tenant's, under row-level security |
//! | [`PLATFORM_TENANT_EVENT_LOG_PATH`] | a platform administrator's own request (A2) | the tenant in the path's, as the platform role |
//! | [`PLATFORM_EVENT_LOG_PATH`] | a platform administrator's own request | platform-wide events, as the platform role |
//!
//! A platform administrator's own request is one whose token the ingress's cookie check issued:
//! a token another service exchanged on the administrator's behalf (`act`) gets 403, as for every
//! platform administrator action (Laysi/trustiot-v2#104, 2026-10-23; docs/architecture/permissions.md).
//!
//! Query parameters, all optional: `from` (inclusive) and `to` (exclusive), RFC 3339 times
//! compared with the event's `occurred_at`; `limit`, 1 to [`MAX_PAGE`] (default
//! [`DEFAULT_PAGE`]); `cursor`, the previous page's `next_cursor`.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not allowed (see the table above) | 403 `permission_denied` |
//! | A malformed parameter or tenant ID, or `from` not before `to` | 400 `invalid_request` |
//! | The database failed, or the event log is off (no platform role credentials) | 503 `unavailable` |
//! | Otherwise | 200 with [`EventLogPage`] |

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, SecondsFormat, Utc};
use contract::event::Actor;
use contract::{ErrorBody, ErrorCode, EventId, ServiceName, TenantId, TenantRole, UserId};
use outbox::{Delivery, Handler, HandlerError};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDb, TenantDbError};
use sqlx::{PgConnection, Postgres, Transaction};
use utoipa::ToSchema;

use crate::members::is_own_platform_admin;

/// A tenant administrator's view of their own tenant's events.
pub const EVENT_LOG_PATH: &str = "/api/identity/v1/event-log";

/// The back office's view of one tenant's events.
pub const PLATFORM_TENANT_EVENT_LOG_PATH: &str =
    "/api/identity/v1/platform/tenants/{tenant_id}/event-log";

/// The back office's view of the platform-wide events.
pub const PLATFORM_EVENT_LOG_PATH: &str = "/api/identity/v1/platform/event-log";

/// The outbox consumers the event log's two pools serve: one recorder per producer in
/// [`crate::schema::CONSUMES`], and the recorder of certs' confirmations of applied tenant states
/// ([`crate::state_applied`], Laysi/trustiot-v2#779), each handling one event at a time.
#[must_use]
pub fn consumers() -> u32 {
    u32::try_from(crate::schema::CONSUMES.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

/// Connections of the service role's event log pool: the consumers' transactions (one at a time
/// per consumer, [`consumers`]) and the tenant administrators' reads (one). Separate from the
/// session store's pool, so neither ever takes a connection the ext_authz check is waiting for. A
/// unit test holds it to [`consumers`] plus one.
pub const SERVICE_CONNECTIONS: u32 = 5;

/// How long the event log may wait for a connection (opening one included).
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections of the platform role's pool: one per consumer ([`consumers`]), each reading its
/// producer's log one poll at a time, plus two for the back office's reads.
#[must_use]
pub fn platform_connections() -> u32 {
    consumers().saturating_add(2)
}

/// Events per page when the request does not say.
pub const DEFAULT_PAGE: u32 = 100;

/// Most events per page.
pub const MAX_PAGE: u32 = 500;

/// The subscriptions' common prefix; [`subscription`] adds the producer. Never changed: a new
/// name is a new subscription, which reads the producer's whole log again (harmless, since the
/// log keeps each event once, but slow).
pub const SUBSCRIPTION_PREFIX: &str = "identity.event-log.";

/// The longest a statement of an event log read may run (`SET LOCAL statement_timeout` at the
/// start of every read transaction, never in the recorder's). The reads share their pools with
/// the recorders, so a pathological scan must not hold a pooled connection for long: past this
/// bound Postgres cancels it and the request answers 503.
pub const READ_STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Sets [`READ_STATEMENT_TIMEOUT`] for the rest of the transaction, like `SET LOCAL`.
const SET_READ_STATEMENT_TIMEOUT: &str = "SELECT set_config('statement_timeout', $1, true)";

/// Names the platform role's queries in its INFO log line (`platform database access`).
const PLATFORM_READ_PURPOSE: &str = "event-log-read";

/// One tenant event, once: a repeated delivery (a replay, another replica) finds the primary
/// key taken and inserts nothing. Row-level security checks that `$1` is the transaction's
/// tenant.
const INSERT_TENANT_EVENT: &str = "INSERT INTO identity.event_log
    (tenant_id, event_id, occurred_at, event_type, version, producer, aggregate_type,
     aggregate_id, aggregate_seq, actor_user_id, actor_service, payload)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9, $10::uuid, $11, $12::jsonb)
ON CONFLICT DO NOTHING";

/// One platform-wide event, once.
const INSERT_PLATFORM_EVENT: &str = "INSERT INTO identity.platform_event_log
    (event_id, occurred_at, event_type, version, producer, aggregate_type, aggregate_id,
     aggregate_seq, actor_user_id, actor_service, payload)
VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9::uuid, $10, $11::jsonb)
ON CONFLICT DO NOTHING";

/// One page of a tenant's events, newest first: `$1` the tenant, `$2`/`$3` the optional time
/// range, `$4`/`$5` the optional cursor (the last row of the previous page), `$6` the row limit.
/// Row-level security also limits a tenant transaction to its own tenant.
///
/// The cursor bound is always a row comparison with values, never behind an `OR`, so it stays
/// an index bound under a generic plan (`event_log_by_time`, `(tenant_id, occurred_at,
/// event_id)` scanned backwards) and a deep page does not walk every newer row. Without a cursor
/// it is `to` (or infinity) with the largest UUID, which the exclusive `occurred_at < to` then
/// narrows.
const SELECT_TENANT_EVENTS: &str = "SELECT event_id::text, tenant_id::text, occurred_at,
    recorded_at, event_type, version, producer, aggregate_type, aggregate_id, aggregate_seq,
    actor_user_id::text, actor_service, payload::text
FROM identity.event_log
WHERE tenant_id = $1::uuid
    AND occurred_at >= COALESCE($2::timestamptz, '-infinity')
    AND occurred_at < COALESCE($3::timestamptz, 'infinity')
    AND (occurred_at, event_id) < (
        COALESCE($4::timestamptz, $3::timestamptz, 'infinity'),
        COALESCE($5::uuid, 'ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid))
ORDER BY occurred_at DESC, event_id DESC
LIMIT $6";

/// [`SELECT_TENANT_EVENTS`] for the platform-wide log, which has no tenant: `$1` is unused but
/// typed, so both queries bind the same parameters. Its index is `platform_event_log_by_time`,
/// `(occurred_at, event_id)`.
const SELECT_PLATFORM_EVENTS: &str = "SELECT event_id::text, NULL::text, occurred_at,
    recorded_at, event_type, version, producer, aggregate_type, aggregate_id, aggregate_seq,
    actor_user_id::text, actor_service, payload::text
FROM identity.platform_event_log
WHERE $1::uuid IS NULL
    AND occurred_at >= COALESCE($2::timestamptz, '-infinity')
    AND occurred_at < COALESCE($3::timestamptz, 'infinity')
    AND (occurred_at, event_id) < (
        COALESCE($4::timestamptz, $3::timestamptz, 'infinity'),
        COALESCE($5::uuid, 'ffffffff-ffff-ffff-ffff-ffffffffffff'::uuid))
ORDER BY occurred_at DESC, event_id DESC
LIMIT $6";

/// The subscription that reads `producer`'s log, for example `identity.event-log.assets`.
#[must_use]
pub fn subscription(producer: &str) -> String {
    format!("{SUBSCRIPTION_PREFIX}{producer}")
}

/// Records every event of one producer's outbox in the event log. The consumer runs it in a
/// transaction of the service role in the event's tenant (no tenant for a platform-wide event),
/// so row-level security admits only that tenant's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLogRecorder {
    source: &'static str,
}

impl EventLogRecorder {
    /// The recorder of the producer whose schema is `source`, as listed in
    /// [`crate::schema::CONSUMES`].
    #[must_use]
    pub const fn new(source: &'static str) -> Self {
        Self { source }
    }

    /// The producer's schema.
    #[must_use]
    pub const fn source(&self) -> &'static str {
        self.source
    }
}

impl Handler for EventLogRecorder {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let envelope = &delivery.envelope;
        // Only the producer's own roles can write its outbox (decision 29), so the log a row
        // came from proves who wrote it; an envelope that names another producer is forged or
        // broken, and recording it would put words in that producer's mouth.
        if envelope.producer.as_str().replace('-', "_") != self.source {
            return Err(HandlerError::permanent(
                "the envelope names another producer than the log it came from",
            ));
        }
        // The table's checks would refuse these rows, and a refused insert is retried forever;
        // only a producer that writes SQL by hand can make them.
        let version = i32::try_from(envelope.version)
            .ok()
            .filter(|version| *version >= 1)
            .ok_or_else(|| HandlerError::permanent("the envelope's version is out of range"))?;
        if !envelope.payload.is_object() {
            return Err(HandlerError::permanent(
                "the envelope's payload is not a JSON object",
            ));
        }
        if delivery.aggregate_seq < 1 {
            return Err(HandlerError::permanent(
                "the event's aggregate sequence number is out of range",
            ));
        }
        let payload = serde_json::to_string(&envelope.payload)
            .map_err(|_| HandlerError::permanent("the envelope's payload cannot be serialized"))?;
        let (actor_user, actor_service) =
            actor_columns(envelope.actor, envelope.producer, envelope.id)?;
        let event_id = envelope.id.to_string();
        let aggregate = &delivery.aggregate;
        let inserted = match delivery.tenant() {
            Some(tenant) => {
                sqlx::query(INSERT_TENANT_EVENT)
                    .bind(tenant.to_string())
                    .bind(&event_id)
                    .bind(envelope.occurred_at)
                    .bind(&envelope.event_type)
                    .bind(version)
                    .bind(envelope.producer.as_str())
                    .bind(aggregate.kind())
                    .bind(aggregate.id())
                    .bind(delivery.aggregate_seq)
                    .bind(actor_user)
                    .bind(actor_service)
                    .bind(payload)
                    .execute(&mut **tx)
                    .await?
            }
            None => {
                sqlx::query(INSERT_PLATFORM_EVENT)
                    .bind(&event_id)
                    .bind(envelope.occurred_at)
                    .bind(&envelope.event_type)
                    .bind(version)
                    .bind(envelope.producer.as_str())
                    .bind(aggregate.kind())
                    .bind(aggregate.id())
                    .bind(delivery.aggregate_seq)
                    .bind(actor_user)
                    .bind(actor_service)
                    .bind(payload)
                    .execute(&mut **tx)
                    .await?
            }
        };
        tracing::debug!(
            event_id = %envelope.id,
            event_type = envelope.event_type.as_str(),
            source = self.source,
            recorded = inserted.rows_affected() == 1,
            "recorded an event in the event log"
        );
        Ok(())
    }
}

/// The actor columns of event `event_id` from `producer`: the user, or the service, or neither
/// (no actor).
///
/// A service actor names a service's own work, so one that is not `producer` itself is broken
/// or forged: the event is recorded without an actor (and a WARN names the event, the producer
/// and the claimed service, never the payload), not dead-lettered, because the row still names
/// the verified producer and the log must not get a gap.
///
/// # Errors
///
/// An actor kind this build does not know is retried, like an event version newer than the
/// consumer (`DecodeError::NewerVersion`): a newer producer wrote it, and storing it as "no
/// actor" would erase who made the change from a record that is kept for good. The
/// subscription waits (its lag alerts) until this service is upgraded, and then records it.
type ActorColumns = (Option<String>, Option<&'static str>);

fn actor_columns(
    actor: Option<Actor>,
    producer: ServiceName,
    event_id: EventId,
) -> Result<ActorColumns, HandlerError> {
    match actor {
        Some(Actor::User { user_id }) => Ok((Some(user_id.to_string()), None)),
        Some(Actor::Service { service }) if service == producer => {
            Ok((None, Some(service.as_str())))
        }
        Some(Actor::Service { service }) => {
            tracing::warn!(
                %event_id,
                producer = producer.as_str(),
                claimed_service = service.as_str(),
                "an event names another service as its actor; it is recorded without one"
            );
            Ok((None, None))
        }
        None => Ok((None, None)),
        Some(Actor::Unrecognized) => Err(HandlerError::retry(
            "the envelope names an actor kind this build does not know",
        )),
    }
}

/// Who made a logged change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoggedActor {
    /// A person, through the API (a platform administrator too).
    User {
        /// The user.
        user_id: UserId,
    },
    /// A service's own work, such as a scheduled job.
    Service {
        /// The service, for example `assets`.
        service: String,
    },
}

/// One event in the log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LoggedEvent {
    /// The event's ID (UUIDv7).
    pub event_id: EventId,
    /// The event type, for example `identity.tenant_created`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The payload's version.
    pub version: u32,
    /// The service that wrote the event, for example `identity`.
    pub producer: String,
    /// The tenant; `null` for a platform-wide event.
    pub tenant_id: Option<TenantId>,
    /// When the change happened, by the producer's clock.
    pub occurred_at: DateTime<Utc>,
    /// When the event log stored the event, by the database clock.
    pub recorded_at: DateTime<Utc>,
    /// What changed: the aggregate's kind, for example `tenant`.
    pub aggregate_type: String,
    /// What changed: the aggregate's ID.
    pub aggregate_id: String,
    /// The event's number within its aggregate, from 1.
    pub aggregate_seq: i64,
    /// Who made the change; `null` when the producer did not say.
    pub actor: Option<LoggedActor>,
    /// The event's payload as the producer wrote it, in the version above; a JSON object.
    // Described as a map of any JSON values, so the spec allows any field (`value_type = Object`
    // alone gives an object without properties, which generated clients read as empty).
    #[schema(value_type = std::collections::BTreeMap<String, serde_json::Value>)]
    pub payload: serde_json::Value,
}

/// One page of the log, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EventLogPage {
    /// The events, newest first (by `occurred_at`, then `event_id`).
    pub events: Vec<LoggedEvent>,
    /// Pass it as `cursor` for the next, older page; `null` on the last page. Opaque.
    pub next_cursor: Option<String>,
}

/// Where a page ends: the last event of the previous page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    occurred_at: DateTime<Utc>,
    event_id: EventId,
}

impl Cursor {
    fn encode(self) -> String {
        format!(
            "{}_{}",
            self.occurred_at
                .to_rfc3339_opts(SecondsFormat::Micros, true),
            self.event_id
        )
    }

    fn decode(text: &str) -> Option<Self> {
        let (time, id) = text.split_once('_')?;
        Some(Self {
            occurred_at: parse_time(time)?,
            event_id: id.parse().ok()?,
        })
    }
}

/// An RFC 3339 time, in UTC.
fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// The query parameters as they arrive; parsed by [`EventLogQuery::parse`]. Unknown parameters
/// are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct RawQuery {
    /// `from`.
    pub from: Option<String>,
    /// `to`.
    pub to: Option<String>,
    /// `limit`.
    pub limit: Option<String>,
    /// `cursor`.
    pub cursor: Option<String>,
}

/// Which page of the log to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLogQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: u32,
    cursor: Option<Cursor>,
}

impl Default for EventLogQuery {
    fn default() -> Self {
        Self {
            from: None,
            to: None,
            limit: DEFAULT_PAGE,
            cursor: None,
        }
    }
}

/// A query parameter broke the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid event log query parameter `{0}`")]
pub struct InvalidQuery(pub &'static str);

impl EventLogQuery {
    /// Checks the request's parameters.
    ///
    /// # Errors
    ///
    /// When `from` or `to` is not an RFC 3339 time, `from` is not before `to`, `limit` is not a
    /// number from 1 to [`MAX_PAGE`], or `cursor` is not a `next_cursor` this service made.
    pub fn parse(raw: &RawQuery) -> Result<Self, InvalidQuery> {
        let time = |value: &Option<String>, name| {
            value
                .as_deref()
                .map(|text| parse_time(text).ok_or(InvalidQuery(name)))
                .transpose()
        };
        let from = time(&raw.from, "from")?;
        let to = time(&raw.to, "to")?;
        if let (Some(from), Some(to)) = (from, to)
            && from >= to
        {
            return Err(InvalidQuery("from"));
        }
        let limit = match raw.limit.as_deref() {
            None => DEFAULT_PAGE,
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|limit| (1..=MAX_PAGE).contains(limit))
                .ok_or(InvalidQuery("limit"))?,
        };
        let cursor = raw
            .cursor
            .as_deref()
            .map(|text| Cursor::decode(text).ok_or(InvalidQuery("cursor")))
            .transpose()?;
        Ok(Self {
            from,
            to,
            limit,
            cursor,
        })
    }
}

/// Why the log was not read.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The caller may not read these events.
    #[error("the caller may not read this event log")]
    NotAllowed,
    /// The database failed or refused the session.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// A stored row could not be turned into an event; the database broke a rule of the table.
    #[error("a stored event is malformed: {0}")]
    Malformed(&'static str),
}

impl From<sqlx::Error> for QueryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl QueryError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(sqlx::Error::Database(error))) => error
                .code()
                .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
            Self::Database(TenantDbError::Database(sqlx::Error::PoolTimedOut)) => {
                "pool timed out".to_owned()
            }
            Self::Database(TenantDbError::Database(_)) => "database client".to_owned(),
            Self::Database(error) => error.to_string(),
            Self::NotAllowed | Self::Malformed(_) => self.to_string(),
        }
    }
}

/// A row as [`SELECT_TENANT_EVENTS`] returns it.
type Row = (
    String,
    Option<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    String,
    i32,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
);

fn event(row: Row) -> Result<LoggedEvent, QueryError> {
    let (
        event_id,
        tenant_id,
        occurred_at,
        recorded_at,
        event_type,
        version,
        producer,
        aggregate_type,
        aggregate_id,
        aggregate_seq,
        actor_user,
        actor_service,
        payload,
    ) = row;
    let actor = match (actor_user, actor_service) {
        (Some(user), _) => Some(LoggedActor::User {
            user_id: user.parse().map_err(|_| QueryError::Malformed("actor"))?,
        }),
        (None, Some(service)) => Some(LoggedActor::Service { service }),
        (None, None) => None,
    };
    Ok(LoggedEvent {
        event_id: event_id
            .parse()
            .map_err(|_| QueryError::Malformed("event_id"))?,
        event_type,
        version: u32::try_from(version).map_err(|_| QueryError::Malformed("version"))?,
        producer,
        tenant_id: tenant_id
            .map(|tenant| tenant.parse())
            .transpose()
            .map_err(|_| QueryError::Malformed("tenant_id"))?,
        occurred_at,
        recorded_at,
        aggregate_type,
        aggregate_id,
        aggregate_seq,
        actor,
        payload: serde_json::from_str(&payload).map_err(|_| QueryError::Malformed("payload"))?,
    })
}

/// Which log a page is read from.
#[derive(Clone, Copy, Debug)]
enum Log {
    /// One tenant's events ([`SELECT_TENANT_EVENTS`]).
    Tenant(TenantId),
    /// The platform-wide events ([`SELECT_PLATFORM_EVENTS`]).
    Platform,
}

/// Reads one page of `log`, the first statements of an event log read transaction.
async fn page(
    conn: &mut PgConnection,
    log: Log,
    query: &EventLogQuery,
) -> Result<EventLogPage, QueryError> {
    sqlx::query(SET_READ_STATEMENT_TIMEOUT)
        .bind(format!("{}ms", READ_STATEMENT_TIMEOUT.as_millis()))
        .execute(&mut *conn)
        .await?;
    // Each arm names its statement as a constant, so tools/sql-check sees static SQL only.
    let (statement, tenant) = match log {
        Log::Tenant(tenant) => (sqlx::query_as::<_, Row>(SELECT_TENANT_EVENTS), Some(tenant)),
        Log::Platform => (sqlx::query_as::<_, Row>(SELECT_PLATFORM_EVENTS), None),
    };
    // One row more than the page tells whether an older page exists.
    let rows: Vec<Row> = statement
        .bind(tenant.map(|tenant| tenant.to_string()))
        .bind(query.from)
        .bind(query.to)
        .bind(query.cursor.map(|cursor| cursor.occurred_at))
        .bind(query.cursor.map(|cursor| cursor.event_id.to_string()))
        .bind(i64::from(query.limit) + 1)
        .fetch_all(conn)
        .await?;
    let more = rows.len() > usize::try_from(query.limit).unwrap_or(usize::MAX);
    let mut events = rows.into_iter().map(event).collect::<Result<Vec<_>, _>>()?;
    events.truncate(usize::try_from(query.limit).unwrap_or(usize::MAX));
    let next_cursor = more.then(|| events.last()).flatten().map(|last| {
        Cursor {
            occurred_at: last.occurred_at,
            event_id: last.event_id,
        }
        .encode()
    });
    Ok(EventLogPage {
        events,
        next_cursor,
    })
}

/// Reads the event log for the API: the service role's pool for a tenant administrator, the
/// platform role's for the back office.
#[derive(Clone, Debug)]
pub struct EventLog {
    db: TenantDb,
    platform: PlatformDb,
}

impl EventLog {
    /// Reads through `db` (the service role) and `platform` (the platform role).
    #[must_use]
    pub fn new(db: TenantDb, platform: PlatformDb) -> Self {
        Self { db, platform }
    }

    /// A page of the events of `user`'s tenant, for an administrator of that tenant, in a
    /// transaction of that tenant.
    ///
    /// # Errors
    ///
    /// [`QueryError::NotAllowed`] when the token has no tenant or its role there is not
    /// `admin`; otherwise when the database fails.
    pub async fn tenant_events(
        &self,
        user: &UserIdentity,
        query: &EventLogQuery,
    ) -> Result<EventLogPage, QueryError> {
        if user.meets(Requirement::Tenant(TenantRole::Admin)).is_err() {
            return Err(QueryError::NotAllowed);
        }
        let mut tx = self.db.begin(user).await?;
        let tenant = tx.tenant_id();
        let page = page(&mut tx, Log::Tenant(tenant), query).await?;
        tx.commit().await?;
        Ok(page)
    }

    /// A page of `tenant`'s events, for a platform administrator's own request, read as the
    /// platform role (logged at INFO with the administrator and the purpose).
    ///
    /// # Errors
    ///
    /// [`QueryError::NotAllowed`] when `admin` is not a platform administrator, or the token was
    /// exchanged by another service ([`is_own_platform_admin`]); otherwise when the database
    /// fails.
    pub async fn tenant_events_for_platform(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
        query: &EventLogQuery,
    ) -> Result<EventLogPage, QueryError> {
        let actor = own_platform_actor(admin)?;
        let mut tx = self.platform.begin(actor, PLATFORM_READ_PURPOSE).await?;
        let page = page(&mut tx, Log::Tenant(tenant), query).await?;
        tx.commit().await?;
        Ok(page)
    }

    /// A page of the platform-wide events, for a platform administrator's own request.
    ///
    /// # Errors
    ///
    /// As [`EventLog::tenant_events_for_platform`].
    pub async fn platform_events(
        &self,
        admin: &UserIdentity,
        query: &EventLogQuery,
    ) -> Result<EventLogPage, QueryError> {
        let actor = own_platform_actor(admin)?;
        let mut tx = self.platform.begin(actor, PLATFORM_READ_PURPOSE).await?;
        let page = page(&mut tx, Log::Platform, query).await?;
        tx.commit().await?;
        Ok(page)
    }
}

/// The platform role's actor for `admin`, if it may read the back office's logs: a platform
/// administrator's own request ([`is_own_platform_admin`]).
fn own_platform_actor(admin: &UserIdentity) -> Result<PlatformActor, QueryError> {
    if !is_own_platform_admin(admin) {
        return Err(QueryError::NotAllowed);
    }
    PlatformActor::admin(admin).ok_or(QueryError::NotAllowed)
}

/// Refuses every caller of the back office's logs but a platform administrator's own request,
/// from the token alone; service-kit's shared check logs the refusal at INFO.
fn refuse_platform_caller(user: &UserIdentity) -> Option<Response> {
    user.require(
        Requirement::OwnPlatformAdmin,
        "read the back office's event log",
    )
    .err()
    .map(IntoResponse::into_response)
}

/// The state of the event log routes.
#[derive(Clone)]
pub struct EventLogRoutes {
    verifier: IdentityVerifier,
    /// `None` while the event log is off ([`crate::config::Config::platform_role`]).
    log: Option<Arc<EventLog>>,
}

impl FromRef<EventLogRoutes> for IdentityVerifier {
    fn from_ref(state: &EventLogRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The event log routes: [`EVENT_LOG_PATH`], [`PLATFORM_TENANT_EVENT_LOG_PATH`] and
/// [`PLATFORM_EVENT_LOG_PATH`].
pub fn routes(verifier: IdentityVerifier, log: Option<EventLog>) -> Router {
    Router::new()
        .route(EVENT_LOG_PATH, get(tenant_event_log))
        .route(
            PLATFORM_TENANT_EVENT_LOG_PATH,
            get(platform_tenant_event_log),
        )
        .route(PLATFORM_EVENT_LOG_PATH, get(platform_event_log))
        .with_state(EventLogRoutes {
            verifier,
            log: log.map(Arc::new),
        })
}

/// The caller's tenant's operation log. Administrators of the tenant only.
#[utoipa::path(
    get,
    path = "/api/identity/v1/event-log",
    tag = "identity",
    operation_id = "getTenantEventLog",
    params(
        ("from" = Option<String>, Query, description = "Only events that occurred at or after this RFC 3339 time."),
        ("to" = Option<String>, Query, description = "Only events that occurred before this RFC 3339 time; after `from`."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 500, description = "Events per page, 1 to 500; default 100."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor`, for the next, older page."),
    ),
    responses(
        (status = 200, description = "A page of the tenant's events, newest first.", body = EventLogPage),
        (status = 400, description = "A malformed parameter, or `from` not before `to` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "The user has no tenant, or is not an administrator of it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed, or the event log is off (`unavailable`).", body = ErrorBody),
    )
)]
async fn tenant_event_log(
    State(state): State<EventLogRoutes>,
    user: UserIdentity,
    raw: Result<Query<RawQuery>, QueryRejection>,
) -> Response {
    if let Err(refused) = user.require(
        Requirement::Tenant(TenantRole::Admin),
        "read the tenant's event log",
    ) {
        return refused.into_response();
    }
    let Some(query) = parse_query(raw) else {
        return error(ErrorCode::InvalidRequest);
    };
    let Some(log) = state.log.as_deref() else {
        return error(ErrorCode::Unavailable);
    };
    respond(&user, log.tenant_events(&user, &query).await)
}

/// One tenant's operation log, for the back office. Platform administrators only.
#[utoipa::path(
    get,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/event-log",
    tag = "identity",
    operation_id = "getPlatformTenantEventLog",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID. An ID no tenant has gives an empty page."),
        ("from" = Option<String>, Query, description = "Only events that occurred at or after this RFC 3339 time."),
        ("to" = Option<String>, Query, description = "Only events that occurred before this RFC 3339 time; after `from`."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 500, description = "Events per page, 1 to 500; default 100."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor`, for the next, older page."),
    ),
    responses(
        (status = 200, description = "A page of the tenant's events, newest first.", body = EventLogPage),
        (status = 400, description = "A malformed tenant ID or parameter, or `from` not before `to` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed, or the event log is off (`unavailable`).", body = ErrorBody),
    )
)]
async fn platform_tenant_event_log(
    State(state): State<EventLogRoutes>,
    user: UserIdentity,
    Path(tenant): Path<String>,
    raw: Result<Query<RawQuery>, QueryRejection>,
) -> Response {
    if let Some(refusal) = refuse_platform_caller(&user) {
        return refusal;
    }
    let Ok(tenant) = tenant.parse::<TenantId>() else {
        return error(ErrorCode::InvalidRequest);
    };
    let Some(query) = parse_query(raw) else {
        return error(ErrorCode::InvalidRequest);
    };
    let Some(log) = state.log.as_deref() else {
        return error(ErrorCode::Unavailable);
    };
    respond(
        &user,
        log.tenant_events_for_platform(&user, tenant, &query).await,
    )
}

/// The platform-wide operation log, for the back office. Platform administrators only.
#[utoipa::path(
    get,
    path = "/api/identity/v1/platform/event-log",
    tag = "identity",
    operation_id = "getPlatformEventLog",
    params(
        ("from" = Option<String>, Query, description = "Only events that occurred at or after this RFC 3339 time."),
        ("to" = Option<String>, Query, description = "Only events that occurred before this RFC 3339 time; after `from`."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 500, description = "Events per page, 1 to 500; default 100."),
        ("cursor" = Option<String>, Query, description = "The previous page's `next_cursor`, for the next, older page."),
    ),
    responses(
        (status = 200, description = "A page of the platform-wide events, newest first.", body = EventLogPage),
        (status = 400, description = "A malformed parameter, or `from` not before `to` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed, or the event log is off (`unavailable`).", body = ErrorBody),
    )
)]
async fn platform_event_log(
    State(state): State<EventLogRoutes>,
    user: UserIdentity,
    raw: Result<Query<RawQuery>, QueryRejection>,
) -> Response {
    if let Some(refusal) = refuse_platform_caller(&user) {
        return refusal;
    }
    let Some(query) = parse_query(raw) else {
        return error(ErrorCode::InvalidRequest);
    };
    let Some(log) = state.log.as_deref() else {
        return error(ErrorCode::Unavailable);
    };
    respond(&user, log.platform_events(&user, &query).await)
}

/// The parsed parameters; `None` when the query string or a parameter is malformed.
fn parse_query(raw: Result<Query<RawQuery>, QueryRejection>) -> Option<EventLogQuery> {
    let Query(raw) = raw.ok()?;
    EventLogQuery::parse(&raw).ok()
}

fn respond(user: &UserIdentity, result: Result<EventLogPage, QueryError>) -> Response {
    match result {
        Ok(page) => ([(CACHE_CONTROL, "no-store")], axum::Json(page)).into_response(),
        Err(QueryError::NotAllowed | QueryError::Database(TenantDbError::NoTenant)) => {
            error(ErrorCode::PermissionDenied)
        }
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not read the event log"
            );
            error(ErrorCode::Unavailable)
        }
    }
}

fn error(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<&str>,
        cursor: Option<&str>,
    ) -> RawQuery {
        RawQuery {
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
            limit: limit.map(str::to_owned),
            cursor: cursor.map(str::to_owned),
        }
    }

    #[test]
    fn parameters_are_checked() {
        assert_eq!(
            EventLogQuery::parse(&RawQuery::default()),
            Ok(EventLogQuery::default())
        );
        let query = EventLogQuery::parse(&raw(
            Some("2026-10-21T08:00:00+08:00"),
            Some("2026-10-22T00:00:00Z"),
            Some("500"),
            None,
        ))
        .expect("valid");
        assert_eq!(
            query.from,
            parse_time("2026-10-21T00:00:00Z"),
            "converted to UTC"
        );
        assert_eq!(query.limit, MAX_PAGE);

        for (bad, name) in [
            (raw(Some("yesterday"), None, None, None), "from"),
            (raw(None, Some("2026-10-21"), None, None), "to"),
            (
                raw(
                    Some("2026-10-21T00:00:00Z"),
                    Some("2026-10-21T00:00:00Z"),
                    None,
                    None,
                ),
                "from",
            ),
            (
                raw(
                    Some("2026-10-22T00:00:00Z"),
                    Some("2026-10-21T00:00:00Z"),
                    None,
                    None,
                ),
                "from",
            ),
            (raw(None, None, Some("0"), None), "limit"),
            (raw(None, None, Some("501"), None), "limit"),
            (raw(None, None, Some("-1"), None), "limit"),
            (raw(None, None, Some("ten"), None), "limit"),
            (raw(None, None, None, Some("")), "cursor"),
            (
                raw(None, None, None, Some("2026-10-21T00:00:00Z")),
                "cursor",
            ),
            (
                raw(None, None, None, Some("2026-10-21T00:00:00Z_nope")),
                "cursor",
            ),
            (
                raw(
                    None,
                    None,
                    None,
                    Some("x_0192f000-0000-7000-8000-000000000000"),
                ),
                "cursor",
            ),
        ] {
            assert_eq!(
                EventLogQuery::parse(&bad),
                Err(InvalidQuery(name)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn cursors_round_trip_with_microseconds() {
        let cursor = Cursor {
            occurred_at: parse_time("2026-10-21T08:00:00.123456Z").expect("time"),
            event_id: EventId::generate(),
        };
        let text = cursor.encode();
        assert_eq!(Cursor::decode(&text), Some(cursor), "{text}");
        let query = EventLogQuery::parse(&raw(None, None, None, Some(&text))).expect("valid");
        assert_eq!(query.cursor, Some(cursor));
    }

    #[test]
    fn actors_become_columns() {
        let user = UserId::generate();
        let (assets, event) = (ServiceName::Assets, EventId::generate());
        assert_eq!(
            actor_columns(Some(Actor::User { user_id: user }), assets, event).ok(),
            Some((Some(user.to_string()), None))
        );
        assert_eq!(
            actor_columns(
                Some(Actor::Service {
                    service: ServiceName::Assets
                }),
                assets,
                event
            )
            .ok(),
            Some((None, Some("assets")))
        );
        assert_eq!(actor_columns(None, assets, event).ok(), Some((None, None)));
        // A newer producer's actor is never stored as "no actor": the event waits for an
        // upgrade instead of being recorded without it or dead-lettered.
        let unknown =
            actor_columns(Some(Actor::Unrecognized), assets, event).expect_err("an unknown actor");
        assert_eq!(unknown.kind(), outbox::FailureKind::Retry);
    }

    /// Each consumer has a connection of each pool, and the reads one more (two on the platform
    /// pool), so a burst of reads never keeps a consumer waiting.
    #[test]
    fn every_consumer_has_a_connection_of_each_pool() {
        assert_eq!(
            consumers(),
            4,
            "the event log's three producers and certs' confirmations"
        );
        assert_eq!(SERVICE_CONNECTIONS, consumers() + 1);
        assert_eq!(platform_connections(), consumers() + 2);
    }

    #[test]
    fn subscriptions_are_named_after_the_producer() {
        assert_eq!(subscription("identity"), "identity.event-log.identity");
        assert!(
            outbox::ConsumerConfig::new(subscription("telemetry_query"), "telemetry_query").is_ok()
        );
    }

    /// The status of `GET path` on the event log routes, with the event log off (so a caller who
    /// gets past the role check gets 503), as `claims`.
    async fn status(path: &str, claims: &contract::InternalClaims) -> u16 {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let issuer = testkit::identity::TestIssuer::new();
        let router = routes(issuer.verifier(ServiceName::Identity), None);
        let request = Request::get(path)
            .header(contract::INTERNAL_IDENTITY_HEADER, issuer.token(claims))
            .body(Body::empty())
            .expect("a request");
        router
            .oneshot(request)
            .await
            .expect("the router answers")
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn the_back_office_logs_refuse_exchanged_tokens() {
        use contract::{Actor as TokenActor, TenantGrant};
        use testkit::identity::TestIssuer;

        let tenant_path = PLATFORM_TENANT_EVENT_LOG_PATH
            .replace("{tenant_id}", &TenantId::generate().to_string());
        let admin = |act: Option<ServiceName>| {
            let mut claims = TestIssuer::claims(ServiceName::Identity, None);
            claims.platform_admin = true;
            if let Some(sub) = act {
                claims.act = Some(TokenActor { sub });
                claims.sid = None;
            }
            claims
        };
        let mut tenant_admin = TestIssuer::claims(
            ServiceName::Identity,
            Some(TenantGrant {
                id: TenantId::generate(),
                role: TenantRole::Admin,
            }),
        );
        tenant_admin.platform_admin = false;
        for path in [tenant_path.as_str(), PLATFORM_EVENT_LOG_PATH] {
            // The administrator's own request gets past the check (503: the log is off here).
            assert_eq!(status(path, &admin(None)).await, 503, "{path}");
            // Exchanged by another service on the administrator's behalf.
            for service in [ServiceName::Assistant, ServiceName::Assets] {
                assert_eq!(
                    status(path, &admin(Some(service))).await,
                    403,
                    "{path} exchanged by {service:?}"
                );
            }
            assert_eq!(status(path, &tenant_admin).await, 403, "{path}");
        }
    }

    #[test]
    fn the_spec_documents_the_page_limits_the_handlers_accept() {
        let spec = serde_json::to_value(crate::openapi()).expect("the spec");
        for path in [
            EVENT_LOG_PATH,
            PLATFORM_TENANT_EVENT_LOG_PATH,
            PLATFORM_EVENT_LOG_PATH,
        ] {
            let parameters = spec["paths"][path]["get"]["parameters"]
                .as_array()
                .unwrap_or_else(|| panic!("{path} has parameters"));
            let limit = parameters
                .iter()
                .find(|parameter| parameter["name"] == "limit")
                .unwrap_or_else(|| panic!("{path} documents limit"));
            assert_eq!(limit["schema"]["minimum"], 1, "{path}");
            assert_eq!(limit["schema"]["maximum"], MAX_PAGE, "{path}");
        }
    }
}
