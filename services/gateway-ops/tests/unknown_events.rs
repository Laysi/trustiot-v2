//! Events of the assets service this build does not know (README.md, "錯誤"): one of another
//! aggregate is acknowledged and the subscription moves on; one of a model, device or binding
//! aggregate (the unbinding of Laysi/trustiot-v2#115, when assets ships before gateway-ops) is
//! retried, never acknowledged or dead-lettered, and the events after it wait.
//!
//! One test in its own binary: the retried event holds the service's subscription for good, so
//! no other test may share its database. Starts a throwaway Postgres with TLS (podman or docker);
//! skipped without either unless `CI` is set.

mod support;

use contract::events::assets::{
    DEVICE_AGGREGATE, DEVICE_MODEL_AGGREGATE, GATEWAY_BINDING_AGGREGATE,
};
use contract::{Event, GatewayBindingId, ServiceName, TenantId};
use outbox::Outcome;
use serde::{Deserialize, Serialize};
use support::{
    Assets, EVENT_LIMIT, LOGINS, consumer, dead_letter, deliver, hardware_id, processed, versions,
    wait_for,
};
use testkit::product::product_postgres;

/// How many polls must keep stopping at the retried event: several, so a retry that gave up
/// after a few attempts would show.
const RETRIES: usize = 5;

/// An aggregate kind of the assets service's log that the handler does not follow.
const UNFOLLOWED: &str = "test_unfollowed";

/// An assets event this build does not know, in a binding's aggregate: what the unbinding of
/// Laysi/trustiot-v2#115 looks like to it.
#[derive(Serialize, Deserialize)]
struct FutureBindingEvent {
    binding_id: String,
}

impl Event for FutureBindingEvent {
    const TYPE: &'static str = "assets.test_future_binding_event";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

/// An assets event this build does not know, in an aggregate it does not follow.
#[derive(Serialize, Deserialize)]
struct UnfollowedEvent {
    note: String,
}

impl Event for UnfollowedEvent {
    const TYPE: &'static str = "assets.test_unfollowed_event";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_events_are_acknowledged_only_outside_the_followed_aggregates() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let assets = Assets::new(&product);
    let (consumer, _) = consumer(&product).await;
    let tenant = TenantId::generate();
    assert!(
        ![
            DEVICE_AGGREGATE,
            DEVICE_MODEL_AGGREGATE,
            GATEWAY_BINDING_AGGREGATE
        ]
        .contains(&UNFOLLOWED),
        "the test's aggregate must be one the handler does not follow"
    );

    // 1. Another aggregate: acknowledged, neither dead-lettered nor in the way.
    let unfollowed = assets
        .publish(
            tenant,
            UNFOLLOWED,
            "1",
            &UnfollowedEvent {
                note: "ignored".to_owned(),
            },
        )
        .await;
    deliver(&product, &consumer, &[unfollowed]).await;
    assert!(dead_letter(&mut admin, unfollowed).await.is_none());

    // 2. A binding's aggregate: retried at every poll, never processed or dead-lettered; a
    //    binding event after it waits.
    let binding = GatewayBindingId::generate();
    let future = assets
        .publish(
            tenant,
            GATEWAY_BINDING_AGGREGATE,
            &binding.to_string(),
            &FutureBindingEvent {
                binding_id: binding.to_string(),
            },
        )
        .await;
    let gateway = hardware_id("go-unknown");
    let (_, bound) = assets.bind(tenant, &gateway).await;
    for attempt in 0..RETRIES {
        let what = format!("poll {attempt} stopping at the unknown binding event");
        let retried = wait_for(&what, EVENT_LIMIT, || async {
            match consumer
                .poll_once()
                .await
                .expect("poll the assets log")
                .outcome
            {
                Outcome::Retry { event_id, error } => Some((event_id, error.to_string())),
                // Earlier events, or another poll still holding the lock: poll again.
                _ => None,
            }
        })
        .await;
        assert_eq!(retried.0, future, "the retried event: {retried:?}");
    }
    assert!(!processed(&mut admin, future).await, "never acknowledged");
    assert!(dead_letter(&mut admin, future).await.is_none());
    assert!(!processed(&mut admin, bound).await, "the next event waits");
    assert!(
        versions(&mut admin, &gateway).await.is_empty(),
        "no configuration before the unknown event is handled"
    );
    println!(
        "evidence: an unknown event of another aggregate acknowledged; an unknown event of a \
         binding aggregate retried {RETRIES} polls in a row, not processed, not dead-lettered, \
         and the binding event after it not processed"
    );
}
