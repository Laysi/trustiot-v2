//! Each tenant's state, kept from the identity service's events (Laysi/trustiot-v2#778, rebuild
//! checklist B1, decision 12; `README.md`, "租戶狀態").
//!
//! The renewal (Laysi/trustiot-v2#81) refuses the gateways of a tenant that is not active, and it
//! never calls the identity service on its request path, so this service keeps its own copy of
//! every tenant's state in `certs.tenant_statuses` ([`crate::bindings::store_tenant_status`]).
//! The identity service writes a snapshot with a version for every change (the notification
//! mode, `crates/outbox`, "通知模式"):
//!
//! - `identity.tenant_created` is the state `active` at version 1;
//! - `identity.tenant_state_changed` carries `{state, reason, state_version}`, 2 for the first
//!   change and one more with every change after it.
//!
//! [`TenantStateHandler`] stores the snapshot in the event's tenant transaction, as the service
//! role, only when its version is newer than the stored one, so a late, repeated or replayed
//! event never takes a tenant back to an older state. The subscription reads the identity
//! service's log from the start, so replaying it rebuilds every tenant's state.
//!
//! The renewal reads a tenant without a row as suspended (fail closed, the user's decision of
//! 2026-10-30): a tenant whose events have not arrived yet is refused for a moment and its
//! gateways retry. So a tenant event this service cannot apply must never be skipped: a skipped
//! suspension would leave the tenant active. Every failure of a tenant event is retried, never
//! dead-lettered, and holds the subscription where its lag shows; the renewal answers 503 while
//! that lag is too high ([`crate::renewal::bindings_current`]). The identity service's events
//! of other aggregates (memberships, users, AI model roles) are not for this service.

use contract::TenantId;
use contract::events::identity::{TENANT_AGGREGATE, TenantCreated, TenantStateChanged};
use outbox::{Delivery, Handler, HandlerError};
use sqlx::{Postgres, Transaction};

use crate::bindings::{TenantStatus, store_tenant_status};

/// The consumer's subscription to the identity service's events; never renamed (a new name
/// starts reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "certs.identity-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "identity";

/// The version `identity.tenant_created` stands for (`TenantStateChanged`'s documentation).
pub const CREATED_VERSION: i64 = 1;

/// Stores the state of every `identity.tenant_created` and `identity.tenant_state_changed`;
/// ignores the identity service's other events.
#[derive(Clone, Copy, Debug, Default)]
pub struct TenantStateHandler;

impl Handler for TenantStateHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let Some(snapshot) = snapshot(delivery)? else {
            return Ok(());
        };
        let stored = store_tenant_status(tx, snapshot.tenant, snapshot.status, snapshot.version)
            .await
            .map_err(|error| {
                HandlerError::retry("could not store the tenant's state").with_source(error)
            })?;
        tracing::info!(
            tenant_id = %snapshot.tenant,
            status = snapshot.status.as_str(),
            version = snapshot.version,
            event_type = delivery.envelope.event_type.as_str(),
            event_id = %delivery.envelope.id,
            stored,
            "applied a tenant state"
        );
        Ok(())
    }
}

/// A tenant's state as one event states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The tenant: the envelope's, which the identity service's row-level security checked when
    /// the event was written, never the payload's.
    pub tenant: TenantId,
    /// The state.
    pub status: TenantStatus,
    /// Its version.
    pub version: i64,
}

/// The state `delivery` states; `None` for an event that states none (another aggregate's, or
/// another event of a tenant). A tenant event that cannot be read or breaks the contract is an
/// error the consumer retries ([`HandlerError::retry`]): dead-lettering it could lose a
/// suspension.
///
/// # Errors
///
/// When a tenant event cannot be decoded (including a version newer than this build), has no
/// tenant, belongs to another aggregate than its tenant's, or has a version the contract does
/// not allow.
pub fn snapshot(delivery: &Delivery) -> Result<Option<Snapshot>, HandlerError> {
    let envelope = &delivery.envelope;
    let (status, version) = if envelope.is::<TenantCreated>() {
        let TenantCreated {} = envelope.decode().map_err(undecodable)?;
        (TenantStatus::Active, CREATED_VERSION)
    } else if envelope.is::<TenantStateChanged>() {
        let changed: TenantStateChanged = envelope.decode().map_err(undecodable)?;
        // The first change is version 2 (`identity.tenant_created` is 1): a lower one would
        // lose to the creation, and a suspension stored that way would read as active.
        if changed.state_version <= CREATED_VERSION {
            return Err(HandlerError::retry(
                "identity.tenant_state_changed with a version the contract does not allow",
            ));
        }
        (
            TenantStatus::from_event(changed.state),
            changed.state_version,
        )
    } else {
        // Another event: a membership's, a user's, an AI model role's. A tenant's state changes
        // only through `identity.tenant_state_changed` (crates/contract), so another event of
        // the tenant aggregate states none either.
        return Ok(None);
    };
    let Some(tenant) = delivery.tenant() else {
        return Err(HandlerError::retry("a tenant state event without a tenant"));
    };
    if delivery.aggregate.kind() != TENANT_AGGREGATE
        || delivery.aggregate.id() != tenant.to_string()
    {
        return Err(HandlerError::retry(
            "a tenant state event outside its tenant's aggregate",
        ));
    }
    Ok(Some(Snapshot {
        tenant,
        status,
        version,
    }))
}

/// A tenant event this build cannot decode: retried, whatever the reason (see the module).
fn undecodable(error: contract::DecodeError) -> HandlerError {
    let reason = if error.is_retryable() {
        "a tenant state event of a version newer than this build"
    } else {
        "a tenant state event this build cannot decode"
    };
    HandlerError::retry(reason).with_source(error)
}

#[cfg(test)]
mod tests {
    use contract::Event;
    use contract::event::EventEnvelope;
    use contract::events::identity::{
        MemberRoleChanged, SuspensionReason, TENANT_AGGREGATE, TenantState,
    };
    use contract::{ServiceName, TenantRole, UserId};
    use outbox::{Aggregate, FailureKind, Position};
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;

    fn delivery<E: Event>(event: &E, tenant: Option<TenantId>, aggregate: &str) -> Delivery {
        let owner = tenant.map_or_else(|| "platform".to_owned(), |tenant| tenant.to_string());
        Delivery {
            position: Position::new(1, 1),
            aggregate: Aggregate::new(aggregate, owner).expect("an aggregate"),
            aggregate_seq: 1,
            envelope: EventEnvelope::new(event, tenant).expect("an envelope"),
            stored_at: chrono::Utc::now(),
        }
    }

    fn changed(state: TenantState, version: i64) -> TenantStateChanged {
        TenantStateChanged {
            state,
            reason: (state != TenantState::Active).then_some(SuspensionReason::NonPayment),
            state_version: version,
        }
    }

    fn retried(result: Result<Option<Snapshot>, HandlerError>) -> String {
        let error = result.expect_err("refused");
        assert_eq!(error.kind(), FailureKind::Retry, "{error}");
        error.to_string()
    }

    #[test]
    fn tenant_events_state_their_snapshot() {
        let tenant = TenantId::generate();
        let created = snapshot(&delivery(&TenantCreated {}, Some(tenant), TENANT_AGGREGATE));
        assert_eq!(
            created.expect("read"),
            Some(Snapshot {
                tenant,
                status: TenantStatus::Active,
                version: 1
            })
        );
        for (state, status) in [
            (TenantState::Active, TenantStatus::Active),
            (TenantState::Suspended, TenantStatus::Suspended),
            (TenantState::Deleted, TenantStatus::Deleted),
        ] {
            let read = snapshot(&delivery(
                &changed(state, 7),
                Some(tenant),
                TENANT_AGGREGATE,
            ));
            assert_eq!(
                read.expect("read"),
                Some(Snapshot {
                    tenant,
                    status,
                    version: 7
                })
            );
        }
    }

    /// `identity.tenant_state_changed` as a newer identity service could write it.
    #[derive(Serialize, Deserialize)]
    struct Future {
        state: String,
        reason: Option<String>,
        state_version: i64,
        extra: String,
    }

    impl Event for Future {
        const TYPE: &'static str = TenantStateChanged::TYPE;
        const VERSION: u32 = 1;
        const PRODUCER: ServiceName = ServiceName::Identity;
    }

    /// The same event at a version this build does not know.
    #[derive(Serialize, Deserialize)]
    struct NextVersion {
        state: String,
        state_version: i64,
    }

    impl Event for NextVersion {
        const TYPE: &'static str = TenantStateChanged::TYPE;
        const VERSION: u32 = 2;
        const PRODUCER: ServiceName = ServiceName::Identity;
    }

    #[test]
    fn an_unknown_state_is_stored_as_unrecognized() {
        let tenant = TenantId::generate();
        let future = Future {
            state: "archived".to_owned(),
            reason: Some("audit_hold".to_owned()),
            state_version: 3,
            extra: "ignored".to_owned(),
        };
        let read = snapshot(&delivery(&future, Some(tenant), TENANT_AGGREGATE));
        assert_eq!(
            read.expect("read"),
            Some(Snapshot {
                tenant,
                status: TenantStatus::Unrecognized,
                version: 3
            })
        );
    }

    #[test]
    fn a_tenant_event_that_cannot_be_applied_is_retried_never_dead_lettered() {
        let tenant = TenantId::generate();
        let next = NextVersion {
            state: "suspended".to_owned(),
            state_version: 2,
        };
        let reason = retried(snapshot(&delivery(&next, Some(tenant), TENANT_AGGREGATE)));
        assert!(reason.contains("newer"), "{reason}");
        let mut broken = delivery(
            &changed(TenantState::Suspended, 2),
            Some(tenant),
            TENANT_AGGREGATE,
        );
        broken.envelope.payload = json!({"state": "suspended"});
        let reason = retried(snapshot(&broken));
        assert!(reason.contains("cannot decode"), "{reason}");
        for version in [1, 0, -1] {
            retried(snapshot(&delivery(
                &changed(TenantState::Suspended, version),
                Some(tenant),
                TENANT_AGGREGATE,
            )));
        }
        // No tenant, or not its tenant's aggregate.
        retried(snapshot(&delivery(
            &changed(TenantState::Suspended, 2),
            None,
            TENANT_AGGREGATE,
        )));
        retried(snapshot(&delivery(
            &TenantCreated {},
            None,
            TENANT_AGGREGATE,
        )));
        let mut elsewhere = delivery(
            &changed(TenantState::Suspended, 2),
            Some(tenant),
            TENANT_AGGREGATE,
        );
        elsewhere.aggregate =
            Aggregate::new(TENANT_AGGREGATE, TenantId::generate().to_string()).expect("aggregate");
        retried(snapshot(&elsewhere));
        retried(snapshot(&delivery(
            &changed(TenantState::Suspended, 2),
            Some(tenant),
            "membership",
        )));
    }

    #[test]
    fn other_identity_events_state_nothing() {
        let tenant = TenantId::generate();
        let role = MemberRoleChanged {
            user_id: UserId::generate(),
            previous_role: None,
            role: TenantRole::Viewer,
        };
        assert_eq!(
            snapshot(&delivery(&role, Some(tenant), "membership")).expect("read"),
            None
        );
    }
}
