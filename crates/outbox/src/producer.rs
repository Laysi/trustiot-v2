//! Writing events in the caller's transaction.

use contract::{EncodeError, Event, EventEnvelope, EventId, TenantId};
use sqlx::{PgConnection, Postgres, Transaction};

use crate::aggregate::Aggregate;

/// An event written to the outbox. It becomes visible to consumers when the caller's
/// transaction commits, and disappears with it when the transaction rolls back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// The envelope's ID.
    pub event_id: EventId,
    /// The event's number within its aggregate: 1 for the first event, then 2, 3, ...
    /// Notification-mode consumers use it as the aggregate's version.
    pub aggregate_seq: i64,
}

/// An event could not be written.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The event could not be wrapped in an envelope.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// The envelope could not be serialized.
    #[error("the event envelope cannot be serialized")]
    Serialize,
    /// The database rejected the write. The caller's transaction is now aborted and must be
    /// rolled back; a serialization failure or deadlock (SQLSTATE 40001, 40P01) can be retried
    /// with a new transaction.
    #[error("writing the event failed: {}", crate::redact::SafeSqlx(.0))]
    Database(#[from] sqlx::Error),
}

/// Wraps `event` in a new envelope and writes it in `tx`, the transaction that makes the
/// business change the event describes. `tenant` is the transaction's tenant (decision 29), or
/// `None` for a platform-wide event.
///
/// The write takes a row lock on the aggregate's sequence until `tx` ends, so concurrent writers
/// of one aggregate queue up behind each other; writers of different aggregates do not. Under
/// `REPEATABLE READ` or `SERIALIZABLE`, a concurrent writer of the same aggregate makes this
/// fail with a serialization error instead of waiting.
///
/// An aggregate belongs to the tenant of its events, or to the platform: its first event decides.
/// With row-level security (service-kit's `sql/outbox-aggregate.sql`), the service role writes
/// only aggregates of the transaction's tenant and the platform role only platform aggregates;
/// an event for an aggregate of another tenant fails with SQLSTATE 42501 and leaves that
/// aggregate's sequence as it was.
///
/// # Errors
///
/// [`PublishError`]; after a database error the transaction is aborted.
pub async fn publish<E: Event>(
    tx: &mut Transaction<'_, Postgres>,
    aggregate: &Aggregate,
    event: &E,
    tenant: Option<TenantId>,
) -> Result<Published, PublishError> {
    let envelope = EventEnvelope::new(event, tenant)?;
    write(tx, aggregate, &envelope).await
}

/// Writes an envelope that already exists in `tx`; see [`publish`]. An envelope can be written
/// once: writing the same event ID again fails with a unique violation (SQLSTATE 23505).
///
/// # Errors
///
/// [`PublishError`]; after a database error the transaction is aborted.
pub async fn write(
    tx: &mut Transaction<'_, Postgres>,
    aggregate: &Aggregate,
    envelope: &EventEnvelope,
) -> Result<Published, PublishError> {
    let json = serde_json::to_string(envelope).map_err(|_| PublishError::Serialize)?;
    let tenant = envelope.tenant_id.map(|tenant| *tenant.as_uuid());
    // The upsert locks the aggregate's row until the transaction ends. `last_tx` carries the
    // ordering transaction forward: an event never sorts before the previous event of its
    // aggregate, even when this transaction's ID is older than that event's transaction.
    // `tenant_id` is written by the first event only; under row-level security, the upsert of an
    // aggregate that belongs to another tenant fails (42501) instead of updating it.
    let (seq, tx_order): (i64, i64) = sqlx::query_as(
        "INSERT INTO outbox_aggregate AS a \
         (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx) \
         VALUES ($1, $2, $3, 1, pg_current_xact_id()::text::bigint) \
         ON CONFLICT (aggregate_type, aggregate_id) DO UPDATE \
         SET last_seq = a.last_seq + 1, last_tx = GREATEST(a.last_tx, EXCLUDED.last_tx) \
         RETURNING last_seq, last_tx",
    )
    .bind(aggregate.kind())
    .bind(aggregate.id())
    .bind(tenant)
    .fetch_one(&mut **tx)
    .await?;
    // No RETURNING: with row-level security, returning rows would also need a SELECT policy.
    sqlx::query(
        "INSERT INTO outbox_event (tx_order, event_id, event_type, tenant_id, aggregate_type, \
         aggregate_id, aggregate_seq, envelope) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb)",
    )
    .bind(tx_order)
    .bind(envelope.id.as_uuid())
    .bind(&envelope.event_type)
    .bind(tenant)
    .bind(aggregate.kind())
    .bind(aggregate.id())
    .bind(seq)
    .bind(json)
    .execute(&mut **tx)
    .await?;
    Ok(Published {
        event_id: envelope.id,
        aggregate_seq: seq,
    })
}

/// The sequence number of the last event written for `aggregate`, or `None` before the first.
///
/// A producer that serves notification-mode consumers returns it with the aggregate's state,
/// read in the same transaction (or snapshot) as the state, so the consumer can tell which of
/// two readings is newer; see [`crate::notify`]. Row-level security applies: in a tenant
/// transaction an aggregate of another tenant (or of the platform) reads as `None`, so read it in
/// the transaction of the tenant the state belongs to.
///
/// # Errors
///
/// When the query fails.
pub async fn aggregate_version(
    conn: &mut PgConnection,
    aggregate: &Aggregate,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT last_seq FROM outbox_aggregate WHERE aggregate_type = $1 AND aggregate_id = $2",
    )
    .bind(aggregate.kind())
    .bind(aggregate.id())
    .fetch_optional(conn)
    .await
}
