//! The certs service's confirmations that it applied a tenant's state (Laysi/trustiot-v2#779,
//! checklist B1; services/identity/README.md, "停權的處理中").
//!
//! The back office shows a suspended tenant as "processing" until the tenant's gateways are
//! disconnected and its CRL is in effect, and only then as "suspended". The certs service does
//! that work (Laysi/trustiot-v2#108) and then writes `certs.tenant_state_applied` with the state
//! version it applied. [`StateAppliedRecorder`] consumes certs' outbox on its own subscription
//! ([`SUBSCRIPTION`]) and keeps, per tenant, the largest confirmed version in
//! `identity.tenants.certs_applied_version`, only ever upwards; the tenant state API compares it
//! with the tenant's `state_version` ([`crate::tenant_state::TenantStatus`]).
//!
//! | Delivery | What happens |
//! | --- | --- |
//! | Another type of certs' events (registrations, renewals) | Nothing: the event log records those |
//! | A confirmation of a version larger than the one stored and older than the tenant's current one, or of the current one written after the tenant entered it | Stored, INFO `certs applied a tenant's state` |
//! | A confirmation of the stored version or an older one (late, repeated, replayed) | Nothing (DEBUG) |
//! | A confirmation without a tenant, of a version below 1, of a tenant this service does not have, or of a version larger than the tenant's current one (never written by this service) | Dead letter, with a WARN naming the tenant and version: certs is broken or the event is forged, and storing it could mark a later suspension applied before certs saw it |
//! | A confirmation of the tenant's current version written before the tenant entered that version (a confirmation dead-lettered as ahead, replayed after the tenant caught up) | Dead letter, with a WARN: certs confirmed a state it could not have seen |
//! | A newer version of the event than this build knows | Retried until the service is upgraded (the subscription's lag alerts) |
//!
//! A confirmation counts for the tenant's current version only if certs wrote it after the tenant
//! entered that version: the log row's `stored_at` (the database clock, when certs wrote the
//! event) is later than the tenant's `state_changed_at` (the database clock, when the identity
//! service wrote the version). Without it, a confirmation dead-lettered because it was ahead would
//! be accepted once the tenant really reached that version and the dead letter was replayed
//! (`outbox::replay_from`), showing a suspension certs never applied as suspended.

use contract::Event;
use contract::events::certs::TenantStateApplied;
use outbox::{Delivery, Handler, HandlerError};
use sqlx::{Postgres, Transaction};

/// The producer whose outbox the recorder reads. It is in [`crate::schema::CONSUMES`], so the
/// migration Job already lets the platform role read that outbox.
pub const SOURCE: &str = "certs";

/// The subscription that reads [`SOURCE`]'s outbox for confirmations. Never changed: a new name is
/// a new subscription, which reads certs' whole log again (harmless: the stored versions only go
/// up).
pub const SUBSCRIPTION: &str = "identity.tenant-state-applied.certs";

/// The confirmed version (`$2`), if it is newer than the stored one and older than the tenant's
/// state, or the tenant's state itself and written (`$3`, the event's `stored_at`) after the
/// tenant entered it. Row-level security keeps it to the transaction's tenant, the envelope's; the
/// migration's trigger and CHECK hold "only upwards" and "never ahead" for every writer.
const STORE_APPLIED: &str = "UPDATE identity.tenants
SET certs_applied_version = $2
WHERE tenant_id = $1::uuid AND certs_applied_version < $2
    AND (state_version > $2 OR (state_version = $2 AND state_changed_at < $3))";

/// Why [`STORE_APPLIED`] stored nothing: the tenant's state version and confirmed version, if
/// this tenant exists.
const TENANT_VERSION: &str = "SELECT state_version, certs_applied_version FROM identity.tenants
WHERE tenant_id = $1::uuid";

/// Records certs' confirmations of applied tenant states. The consumer runs it in a transaction
/// of the service role in the event's tenant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateAppliedRecorder;

impl Handler for StateAppliedRecorder {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let envelope = &delivery.envelope;
        if envelope.event_type != TenantStateApplied::TYPE {
            return Ok(());
        }
        // Checks the producer too: only certs confirms.
        let applied: TenantStateApplied = envelope.decode()?;
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "a tenant state confirmation names no tenant",
            ));
        };
        if applied.state_version < 1 {
            tracing::warn!(
                event_id = %envelope.id,
                tenant_id = %tenant,
                state_version = applied.state_version,
                "certs confirmed a tenant state version below 1"
            );
            return Err(HandlerError::permanent(
                "the confirmed state version is out of range",
            ));
        }
        let stored = sqlx::query(STORE_APPLIED)
            .bind(tenant.to_string())
            .bind(applied.state_version)
            .bind(delivery.stored_at)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if stored == 1 {
            tracing::info!(
                tenant_id = %tenant,
                state_version = applied.state_version,
                "certs applied a tenant's state"
            );
            return Ok(());
        }
        let current: Option<(i64, i64)> = sqlx::query_as(TENANT_VERSION)
            .bind(tenant.to_string())
            .fetch_optional(&mut **tx)
            .await?;
        match current {
            None => {
                tracing::warn!(
                    event_id = %envelope.id,
                    tenant_id = %tenant,
                    state_version = applied.state_version,
                    "certs confirmed the state of a tenant this service does not have"
                );
                Err(HandlerError::permanent(
                    "the confirmation names a tenant the identity service does not have",
                ))
            }
            Some((current, _)) if applied.state_version > current => {
                tracing::warn!(
                    event_id = %envelope.id,
                    tenant_id = %tenant,
                    state_version = applied.state_version,
                    current_state_version = current,
                    "certs confirmed a tenant state version the identity service never wrote"
                );
                Err(HandlerError::permanent(
                    "the confirmed state version is newer than the tenant's",
                ))
            }
            Some((current, stored))
                if applied.state_version == current && applied.state_version > stored =>
            {
                tracing::warn!(
                    event_id = %envelope.id,
                    tenant_id = %tenant,
                    state_version = applied.state_version,
                    "certs confirmed a tenant state version before the identity service wrote it"
                );
                Err(HandlerError::permanent(
                    "the confirmation was written before the tenant's state version",
                ))
            }
            // The stored version is this one or newer; or the tenant moved past it after the
            // update above, and an older version decides nothing.
            Some(_) => {
                tracing::debug!(
                    tenant_id = %tenant,
                    state_version = applied.state_version,
                    "an older or repeated confirmation of a tenant's state"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recorder_reads_a_producer_the_migration_job_grants() {
        assert!(crate::schema::CONSUMES.contains(&SOURCE));
        assert!(outbox::ConsumerConfig::new(SUBSCRIPTION, SOURCE).is_ok());
        // Not one of the event log's subscriptions, which record every certs event.
        assert_ne!(SUBSCRIPTION, crate::event_log::subscription(SOURCE));
    }
}
