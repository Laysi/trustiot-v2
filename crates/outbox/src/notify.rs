//! Notification mode: the event carries a snapshot of the aggregate's state after the change
//! and the snapshot's version; the consumer keeps the snapshot with the newest version (rebuild
//! checklist B1; the 2026-10-30 decision of Laysi/trustiot-v2#107, README "通知模式").
//!
//! A consumer may see an old snapshot after a newer one was already stored: a late delivery, a
//! replay, or two replicas racing. [`store_if_newer`] makes that harmless: the consumer stores a
//! snapshot only if its version is newer than the stored one. Replaying the producer's log from
//! the start rebuilds every aggregate's state, so a handler never needs to call the producer.
//!
//! The producer chooses the version: a version column of its own data that goes up with every
//! change (the identity service's `tenants.state_version`, Laysi/trustiot-v2#776), or the
//! aggregate's event sequence number. A consumer that reads the producer's current state
//! instead must read the version in the same snapshot as the state ([`crate::aggregate_version`]
//! in one statement, or one `REPEATABLE READ` transaction): a version read in a later snapshot
//! than the state could label old state as new.
//!
//! ```text
//! async fn handle(&self, tx: &mut Transaction<'static, Postgres>, delivery: &Delivery)
//!     -> Result<(), HandlerError>
//! {
//!     let event: TenantStateChanged = delivery.envelope.decode()?;
//!     if outbox::notify::store_if_newer(tx, "tenant_state", &delivery.aggregate, event.state_version)
//!         .await?
//!     {
//!         upsert_tenant_state(tx, &event).await?;
//!     }
//!     Ok(())
//! }
//! ```

use sqlx::{Postgres, Transaction};

use crate::aggregate::Aggregate;

/// Records that the consumer's copy of `aggregate` in `scope` (a name for the copy, such as
/// the table it lives in) now holds `version`, if that is newer than the version stored so far.
/// Returns whether it was newer: write the reading in the same transaction only when it was.
///
/// The row stays locked until `tx` ends, so a concurrent reading of the same aggregate waits
/// and then compares against this one.
///
/// # Errors
///
/// When the query fails.
pub async fn store_if_newer(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    aggregate: &Aggregate,
    version: i64,
) -> Result<bool, sqlx::Error> {
    let stored: Option<i32> = sqlx::query_scalar(
        "INSERT INTO outbox_version AS v (scope, aggregate_type, aggregate_id, version) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (scope, aggregate_type, aggregate_id) \
         DO UPDATE SET version = EXCLUDED.version WHERE v.version < EXCLUDED.version \
         RETURNING 1",
    )
    .bind(scope)
    .bind(aggregate.kind())
    .bind(aggregate.id())
    .bind(version)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(stored.is_some())
}

/// The version of `aggregate` stored in `scope`, `None` before the first. A handler can skip
/// reading the producer when the event's [`crate::Delivery::aggregate_seq`] is not newer.
///
/// # Errors
///
/// When the query fails.
pub async fn stored_version(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    aggregate: &Aggregate,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT version FROM outbox_version \
         WHERE scope = $1 AND aggregate_type = $2 AND aggregate_id = $3",
    )
    .bind(scope)
    .bind(aggregate.kind())
    .bind(aggregate.id())
    .fetch_optional(&mut **tx)
    .await
}
