//! The renewal refuses while the tenant states may be stale (Laysi/trustiot-v2#778, acceptance
//! 4): once the consumer of the identity service's events has waited on an event longer than
//! `renewal.max_consumer_lag`, both steps answer 503 with `Retry-After`, as they do for the
//! bindings' consumer (`tests/renewal_lag.rs`). A tenant state event this build cannot apply (here
//! a version of `identity.tenant_state_changed` it does not know) is retried, never
//! dead-lettered, so it holds the consumer for good: this runs in a binary of its own, with a
//! database of its own (`testkit::product`).
//!
//! Skipped without podman or docker unless `CI` is set.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use certs::bindings::{Projection, TenantStatus};
use certs::renewal::BINDINGS_STALE;
use certs::tenant_states::SUBSCRIPTION;
use contract::events::identity::{TENANT_AGGREGATE, TenantCreated, TenantStateChanged};
use contract::{Event, ServiceName, TenantId};
use outbox::Aggregate;
use protocol::renewal::RENEWAL_PATH;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use support::{Service, TestCa, wait_for};
use testkit::product::{DATABASE, product_postgres};

const LOGINS: &[&str] = &["identity_app", "certs_app", "certs_platform"];

/// `identity.tenant_state_changed` at a version no build knows: the consumer retries it and its
/// lag grows.
#[derive(Serialize, Deserialize)]
struct NextStateChanged {
    state: String,
    state_version: i64,
}

impl Event for NextStateChanged {
    const TYPE: &'static str = TenantStateChanged::TYPE;
    const VERSION: u32 = TenantStateChanged::VERSION + 1;
    const PRODUCER: ServiceName = ServiceName::Identity;
}

/// The subscription's dead letters.
const DEAD_LETTERS: &str = "SELECT count(*) FROM certs.outbox_dead_letter WHERE subscription = $1";

/// Writes `event` to the identity service's log for `tenant`, as the identity service writes its
/// tenant events.
async fn publish<E: Event>(identity: &BackgroundTenantDb, tenant: TenantId, event: &E) {
    let mut tx = identity
        .begin(tenant, "test-tenant-event")
        .await
        .expect("a transaction of the tenant");
    let aggregate = Aggregate::new(TENANT_AGGREGATE, tenant.to_string()).expect("an aggregate");
    outbox::publish(tx.transaction(), &aggregate, event, Some(tenant))
        .await
        .expect("write the event");
    tx.commit().await.expect("commit the event");
}

/// Posts `body` to the renewal endpoint: the status, the body and `Retry-After`.
async fn send(service: &Service, body: &Value) -> (u16, Value, Option<String>) {
    let response = reqwest::Client::new()
        .post(format!("{}{RENEWAL_PATH}", service.base_url))
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("call the renewal endpoint");
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    let body = serde_json::from_str(&text).unwrap_or_else(|_| panic!("a JSON body: {text}"));
    (status, body, retry_after)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_state_lag_past_the_limit_refuses_the_renewal() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
            ("TRUSTIOT__CRL__VALIDITY", "1h"),
            ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
            ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
            ("TRUSTIOT__RENEWAL__MAX_CONSUMER_LAG", "1s"),
        ],
    )
    .await;
    // Each poll names a hardware ID of its own, so none reaches its rate limit.
    let polls = AtomicU32::new(0);
    let poll = || {
        let n = polls.fetch_add(1, Ordering::Relaxed);
        json!({"step": "challenge", "hardware_id": format!("tl-poll-{n}")})
    };
    wait_for("a renewal challenge", || async {
        let (status, ..) = send(&service, &poll()).await;
        (status == 200).then_some(())
    })
    .await;

    // A new tenant, then a change of its state this build cannot read.
    let (_, identity) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("the identity service role's pool");
    let tenant = TenantId::generate();
    publish(&identity, tenant, &TenantCreated {}).await;
    publish(
        &identity,
        tenant,
        &NextStateChanged {
            state: "active".to_owned(),
            state_version: 2,
        },
    )
    .await;

    // Within a lag measurement or two (every 15 s), the challenge step is refused.
    let retry_after = wait_for(
        "the renewal to refuse on the tenant states' lag",
        || async {
            let (status, body, retry_after) = send(&service, &poll()).await;
            if status == 200 {
                return None;
            }
            assert_eq!(
                (status, body["message_key"].as_str().unwrap_or_default()),
                (503, BINDINGS_STALE.as_str())
            );
            Some(retry_after)
        },
    )
    .await;
    assert_eq!(retry_after.as_deref(), Some("30"));

    // The event is retried, not dead-lettered, and the tenant keeps the last state this build
    // could read (the creation's).
    let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
        .bind(SUBSCRIPTION)
        .fetch_one(&mut product.superuser().await)
        .await
        .expect("count the dead letters");
    assert_eq!(dead, 0, "nothing dead-lettered");
    let retried = wait_for("the consumer's retry line", || async {
        service
            .stdout
            .messages("outbox event failed; retrying")
            .into_iter()
            .find(|line| {
                line["fields"]["subscription"] == SUBSCRIPTION
                    && line["fields"]["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("newer than this build"))
            })
    })
    .await;
    let status: Option<(String, i64)> = sqlx::query_as(
        "SELECT status, version FROM certs.tenant_statuses WHERE tenant_id = $1::uuid",
    )
    .bind(tenant.to_string())
    .fetch_optional(&mut product.superuser().await)
    .await
    .expect("read the status");
    assert_eq!(
        status,
        Some((TenantStatus::Active.as_str().to_owned(), 1)),
        "the creation is stored, the unreadable change is not"
    );
    let projection = Projection::new(
        PlatformDb::connect_lazy(
            product.postgres.options("certs_platform", DATABASE),
            PgPoolOptions::new().max_connections(1),
        )
        .expect("the certs platform role's pool"),
    );
    assert!(
        !projection
            .tenant_suspended(tenant, "test-tenant-state-lag")
            .await
            .expect("read")
    );
    println!(
        "evidence: tenant states' lag past 1 s: challenge 503 {}, Retry-After 30; 0 dead \
         letters; retry line: {}",
        BINDINGS_STALE.as_str(),
        retried["fields"]
    );
}
