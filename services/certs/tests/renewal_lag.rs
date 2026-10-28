//! The renewal refuses while the bindings may be stale (Laysi/trustiot-v2#81, acceptance 7):
//! once the consumer of the assets service's events has waited on an event longer than
//! `renewal.max_consumer_lag`, both steps answer 503 with `Retry-After`, before any challenge is
//! stored or taken. A binding event this build does not know holds the consumer for good, so
//! this runs in a binary of its own, with a database of its own (`testkit::product`). The lag
//! that is unknown, the other half of the criterion, is in `tests/renewal.rs`.
//!
//! Skipped without podman or docker unless `CI` is set.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use certs::renewal::BINDINGS_STALE;
use contract::events::assets::GATEWAY_BINDING_AGGREGATE;
use contract::{Event, GatewayBindingId, ServiceName, TenantId};
use outbox::Aggregate;
use protocol::renewal::{Challenge, RENEWAL_PATH};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use support::{Service, TestCa, wait_for};
use testkit::product::{DATABASE, product_postgres};

const LOGINS: &[&str] = &[
    "assets_app",
    "assets_platform",
    "certs_app",
    "certs_platform",
];

/// The CRL schedule of `tests/registration.rs`.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// A gateway binding event no build knows: the consumer retries it and its lag grows.
#[derive(Serialize, Deserialize)]
struct UnknownBindingEvent {
    binding_id: String,
}

impl Event for UnknownBindingEvent {
    const TYPE: &'static str = "assets.gateway_binding_test_unknown";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Assets;
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

fn message_key(body: &Value) -> &str {
    body["message_key"].as_str().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lag_past_the_limit_refuses_both_steps() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let mut env = FAST_CRLS.to_vec();
    env.push(("TRUSTIOT__RENEWAL__MAX_CONSUMER_LAG", "1s"));
    let service = Service::start(&product, &ca, &env).await;
    // The rate limits come before the lag check and count the 503s too, so each poll names a
    // hardware ID of its own: none of them reaches its limit, and the global one (100 at once,
    // 20 a second) is well above the polling.
    let polls = AtomicU32::new(0);
    let poll = || {
        let n = polls.fetch_add(1, Ordering::Relaxed);
        json!({"step": "challenge", "hardware_id": format!("lag-poll-{n}")})
    };
    let pending: Challenge = wait_for("a renewal challenge", || async {
        let (status, body, _) = send(&service, &poll()).await;
        (status == 200).then(|| serde_json::from_value(body).expect("a challenge"))
    })
    .await;

    // A binding event this build does not know, written as the assets service writes its events.
    let (_, assets) = TenantDb::connect_lazy_with_background(
        product.postgres.options("assets_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("the assets service role's pool");
    let tenant = TenantId::generate();
    let binding_id = GatewayBindingId::generate();
    let mut tx = assets
        .begin(tenant, "test-unknown-binding-event")
        .await
        .expect("a tenant transaction of the assets service");
    let aggregate =
        Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding_id.to_string()).expect("an aggregate");
    outbox::publish(
        tx.transaction(),
        &aggregate,
        &UnknownBindingEvent {
            binding_id: binding_id.to_string(),
        },
        Some(tenant),
    )
    .await
    .expect("write the event");
    tx.commit().await.expect("commit the event");

    // Within a lag measurement or two (every 15 s), the challenge step is refused.
    let retry_after = wait_for("the renewal to refuse on the lag", || async {
        let (status, body, retry_after) = send(&service, &poll()).await;
        if status == 200 {
            return None;
        }
        assert_eq!((status, message_key(&body)), (503, BINDINGS_STALE.as_str()));
        Some(retry_after)
    })
    .await;
    assert_eq!(retry_after.as_deref(), Some("30"));
    // The renew step too, before it takes the challenge (its hardware ID has used one request of
    // its limit).
    let renew = json!({
        "step": "renew",
        "hardware_id": pending.hardware_id,
        "nonce": pending.nonce,
        "signature": "MEUCIQ==",
    });
    let (status, body, retry_after) = send(&service, &renew).await;
    assert_eq!((status, message_key(&body)), (503, BINDINGS_STALE.as_str()));
    assert_eq!(retry_after.as_deref(), Some("30"));
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM certs.renewal_challenges WHERE nonce = $1")
            .bind(URL_SAFE_NO_PAD.decode(&pending.nonce).expect("a nonce"))
            .fetch_one(&mut product.superuser().await)
            .await
            .expect("count the challenges");
    assert_eq!(stored, 1, "the refused renew step left the challenge alone");
    // The renew step's line: the polls before the first lag measurement are refused the same way.
    let line = service
        .stdout
        .messages("refused a certificate renewal request")
        .into_iter()
        .find(|line| {
            line["fields"]["reason"] == "bindings_stale" && line["fields"]["step"] == "renew"
        })
        .expect("the refusal's log line");
    println!(
        "evidence: lag past 1 s: challenge and renew 503 {}, Retry-After 30; challenge kept; \
         log: {}",
        BINDINGS_STALE.as_str(),
        line["fields"]
    );
}
