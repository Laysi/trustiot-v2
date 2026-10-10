//! Notification mode: the event only says "this aggregate changed"; the consumer reads the
//! producer's current state and stores it, keeping the newest version (rebuild checklist B1).
//!
//! A consumer may read the state for an old event after a newer reading was already stored: a
//! late delivery, a replay, or two replicas racing. [`store_if_newer`] makes that harmless: the
//! consumer stores a reading only if its version is newer than the stored one.
//!
//! The version is the aggregate's last event sequence number, which the producer reads with
//! [`crate::aggregate_version`] in the same snapshot as the state it returns (one statement, or
//! one `REPEATABLE READ` transaction): a version read in a later snapshot than the state could
//! label old state as new.
//!
//! ```text
//! async fn handle(&self, tx: &mut Transaction<'static, Postgres>, delivery: &Delivery)
//!     -> Result<(), HandlerError>
//! {
//!     // Internal API call to the producer: state plus version, from one snapshot.
//!     let Some(current) = self.client.tenant(delivery.aggregate.id()).await? else {
//!         return Ok(());
//!     };
//!     if outbox::notify::store_if_newer(tx, "tenant_copy", &delivery.aggregate, current.version)
//!         .await?
//!     {
//!         upsert_tenant_copy(tx, &current).await?;
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
