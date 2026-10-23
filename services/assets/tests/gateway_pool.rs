//! The device pool and gateway binding (Laysi/trustiot-v2#101) against the real binary on the
//! product's schema, applied by the migration Job (`testkit::product`):
//!
//! - a gateway the certs service registers at the factory (`certs.gateway_registered`, written
//!   here through the certs service's platform role as its registration writes it) enters the
//!   pool; the same hardware ID again adds nothing (acceptance 1); an event whose values the
//!   table refuses is dead-lettered and does not hold up the gateways after it;
//! - a binding writes the binding and `assets.gateway_bound` in the target tenant, with the
//!   administrator as the actor (acceptance 3); the same binding again answers the existing one
//!   and writes nothing; another tenant is refused and nothing is written;
//! - a binding that fails midway (a trigger makes the event's insert fail) leaves nothing
//!   behind, and the retry succeeds (acceptance 2); two bindings at once give one binding;
//! - only a platform administrator's own request gets anywhere: everyone else is refused before
//!   any database access, and no token reaches the logs.
//!
//! The cross-tenant tests are `tests/cross_tenant.rs` (acceptance 4). Starts a throwaway
//! Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::events::certs::{GATEWAY_REGISTRATION_AGGREGATE, GatewayRegistered};
use contract::{Actor, EventEnvelope, InternalClaims, ServiceName, TenantId, TenantRole, UserId};
use outbox::Aggregate;
use protocol::GatewayId;
use reqwest::Method;
use serde_json::{Value, json};
use service_kit::tenant::{PlatformActor, PlatformDb};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::{EVENT_LIMIT, LOG_LIMIT, Reply, Service, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const POOL: &str = "/api/assets/v1/platform/gateways";

/// The INFO line of every platform role transaction (`PlatformDb::begin`).
const PLATFORM_TRANSACTION: &str = "platform database access";
/// The INFO line of every tenant transaction the binder opens for an administrator.
const TENANT_TRANSACTION: &str = "tenant transaction for a platform administrator";

/// The roles these tests log in with: the service's two, and the certs service's platform role,
/// which writes `certs.gateway_registered`.
const LOGINS: &[&str] = &[
    "identity_app",
    "assets_app",
    "assets_platform",
    "certs_platform",
];

const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)";
/// A gateway in the pool without going through the consumer, for the binding tests.
const INSERT_POOLED: &str =
    "INSERT INTO assets.gateway_pool (hardware_id, registered_at, workstation)
VALUES ($1, now(), 'ws-test')";
/// Whether the pool's subscription processed an event. `$1`: the event ID.
const INBOX: &str = "SELECT count(*) FROM assets.outbox_inbox
WHERE subscription = 'assets.certs-events' AND event_id = $1::uuid";
/// The pool's subscription's dead letters among events. `$1`: the event IDs.
const DEAD_LETTERS: &str = "SELECT count(*) FROM assets.outbox_dead_letter
WHERE subscription = 'assets.certs-events' AND event_id = ANY ($1::uuid[])";
const POOL_ROWS: &str = "SELECT count(*) FROM assets.gateway_pool WHERE hardware_id = $1";
const BINDINGS: &str = "SELECT tenant_id::text, binding_id::text, unbound_at IS NULL
FROM assets.gateway_bindings WHERE hardware_id = $1 ORDER BY bound_at";
/// Every `assets.gateway_bound` of a gateway, in order.
const BOUND_EVENTS: &str = "SELECT aggregate_type, aggregate_id, aggregate_seq, tenant_id::text,
    envelope::text
FROM assets.outbox_event
WHERE event_type = 'assets.gateway_bound' AND envelope -> 'payload' ->> 'hardware_id' = $1
ORDER BY tx_order, id";

/// Makes every `assets.gateway_bound` of a gateway whose hardware ID starts with `tg-fail-`
/// fail, as a database failure after the binding's insert would.
const CREATE_FAILURE: &str =
    "CREATE OR REPLACE FUNCTION public.fail_gateway_bound() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.event_type = 'assets.gateway_bound'
        AND NEW.envelope -> 'payload' ->> 'hardware_id' LIKE 'tg-fail-%' THEN
        RAISE EXCEPTION 'injected failure';
    END IF;
    RETURN NEW;
END
$$;
CREATE OR REPLACE TRIGGER fail_gateway_bound BEFORE INSERT ON assets.outbox_event
    FOR EACH ROW EXECUTE FUNCTION public.fail_gateway_bound()";
/// The workstation name the CHECK below refuses; no other test uses it.
const REFUSED_WORKSTATION: &str = "ws-refused-by-the-table";
/// A CHECK the pool's handler does not know about, as a later migration might add, so the
/// insert itself fails with SQLSTATE 23514. `NOT VALID`: existing rows are not checked.
const ADD_REFUSING_CHECK: &str = "ALTER TABLE assets.gateway_pool
    ADD CONSTRAINT test_refused_workstation
    CHECK (workstation <> 'ws-refused-by-the-table') NOT VALID";
const DROP_REFUSING_CHECK: &str =
    "ALTER TABLE assets.gateway_pool DROP CONSTRAINT IF EXISTS test_refused_workstation";
const DROP_FAILURE: &str = "DROP TRIGGER IF EXISTS fail_gateway_bound ON assets.outbox_event";

/// A hardware ID no other test uses.
fn hardware_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        UserId::generate().to_string().replace('-', "")
    )
}

fn admin_claims() -> InternalClaims {
    InternalClaims {
        platform_admin: true,
        ..TestIssuer::claims(ServiceName::Assets, None)
    }
}

async fn known_tenant(admin: &mut PgConnection) -> TenantId {
    let tenant = TenantId::generate();
    sqlx::query(INSERT_TENANT)
        .bind(tenant.to_string())
        .execute(admin)
        .await
        .expect("insert the tenant");
    tenant
}

async fn pooled(admin: &mut PgConnection, prefix: &str) -> String {
    let hardware_id = hardware_id(prefix);
    sqlx::query(INSERT_POOLED)
        .bind(&hardware_id)
        .execute(admin)
        .await
        .expect("put the gateway in the pool");
    hardware_id
}

async fn bindings(admin: &mut PgConnection, hardware_id: &str) -> Vec<(String, String, bool)> {
    sqlx::query_as(BINDINGS)
        .bind(hardware_id)
        .fetch_all(admin)
        .await
        .expect("read the bindings")
}

/// One stored `assets.gateway_bound`.
#[derive(Debug)]
struct StoredEvent {
    aggregate_type: String,
    aggregate_id: String,
    aggregate_seq: i64,
    tenant_id: Option<String>,
    envelope: EventEnvelope,
}

async fn bound_events(admin: &mut PgConnection, hardware_id: &str) -> Vec<StoredEvent> {
    let rows: Vec<(String, String, i64, Option<String>, String)> = sqlx::query_as(BOUND_EVENTS)
        .bind(hardware_id)
        .fetch_all(admin)
        .await
        .expect("read the events");
    rows.into_iter()
        .map(
            |(aggregate_type, aggregate_id, aggregate_seq, tenant_id, text)| StoredEvent {
                aggregate_type,
                aggregate_id,
                aggregate_seq,
                tenant_id,
                envelope: serde_json::from_str(&text).expect("an envelope"),
            },
        )
        .collect()
}

fn binding_path(hardware_id: &str) -> String {
    format!("{POOL}/{hardware_id}/binding")
}

async fn bind(assets: &Service, token: &str, hardware_id: &str, tenant: TenantId) -> Reply {
    assets
        .json(
            Method::PUT,
            &binding_path(hardware_id),
            Some(token),
            &json!({"tenant_id": tenant.to_string()}),
        )
        .await
}

fn assert_refused(reply: &Reply, status: u16, code: &str, key: Option<&str>) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.header("cache-control"), Some("no-store"), "{reply:?}");
    let body = reply.json();
    assert_eq!(body["code"], code, "{}", reply.body);
    if let Some(key) = key {
        assert_eq!(body["message_key"], key, "{}", reply.body);
    }
}

/// Writes `certs.gateway_registered` as the certs service's registration does: the platform
/// role, a transaction for a platform administrator, a platform-wide event.
async fn register_at_factory(
    product: &ProductPostgres,
    issuer: &TestIssuer,
    hardware_id: &str,
    workstation: &str,
) -> String {
    let certs = PlatformDb::connect_lazy(
        product.postgres.options("certs_platform", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the certs platform pool");
    let user = issuer.user(
        ServiceName::Certs,
        &InternalClaims {
            platform_admin: true,
            ..TestIssuer::claims(ServiceName::Certs, None)
        },
    );
    let actor = PlatformActor::admin(&user).expect("a platform administrator");
    let mut tx = certs
        .begin(actor, "test-register")
        .await
        .expect("a certs platform transaction");
    let aggregate =
        Aggregate::new(GATEWAY_REGISTRATION_AGGREGATE, hardware_id).expect("an aggregate");
    let event = GatewayRegistered {
        hardware_id: GatewayId::new(hardware_id).expect("a hardware ID"),
        public_key_sha256: "ab".repeat(32),
        endorsement_key_sha256: "cd".repeat(32),
        certificate_serial: "01".to_owned(),
        workstation: workstation.to_owned(),
    };
    let published = outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &event,
        None,
        contract::event::Actor::User {
            user_id: user.user_id(),
        },
    )
    .await
    .expect("write certs.gateway_registered");
    tx.commit().await.expect("commit the registration");
    published.event_id.to_string()
}

/// The `applied certs.gateway_registered` lines for `hardware_id`.
fn applied(assets: &Service, hardware_id: &str) -> Vec<Value> {
    assets
        .stdout
        .messages("applied certs.gateway_registered")
        .into_iter()
        .filter(|line| line["fields"]["hardware_id"] == hardware_id)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_registrations_fill_the_pool() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let token = issuer.token(&admin_claims());
    let gateway = hardware_id("tg-pool");
    let path = format!("{POOL}/{gateway}");

    // 1. Registered at the factory: the gateway shows up in the pool, bound to no tenant.
    let first = register_at_factory(&product, &issuer, &gateway, "ws-line-1").await;
    let entry = wait_for("the gateway in the pool", EVENT_LIMIT, || async {
        let reply = assets.call(Method::GET, &path, Some(&token), None).await;
        (reply.status == 200).then_some(reply)
    })
    .await;
    assert_eq!(entry.header("cache-control"), Some("no-store"));
    let entry = entry.json();
    assert_eq!(entry["hardware_id"], gateway.as_str());
    assert_eq!(entry["workstation"], "ws-line-1");
    assert_eq!(entry["binding"], Value::Null);
    assert!(entry["registered_at"].is_string() && entry["pooled_at"].is_string());

    // The unbound listing has it; the bound one does not. Pages follow `next_after`.
    let mut found = false;
    let mut after: Option<String> = None;
    loop {
        let query = after
            .as_ref()
            .map_or_else(String::new, |after| format!("&after={after}"));
        let reply = assets
            .call(
                Method::GET,
                &format!("{POOL}?state=unbound&limit=1{query}"),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let page = reply.json();
        let gateways = page["gateways"].as_array().expect("gateways");
        assert!(gateways.len() <= 1);
        for listed in gateways {
            assert_eq!(listed["binding"], Value::Null, "{listed}");
            found |= listed["hardware_id"] == gateway.as_str();
        }
        match page["next_after"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    assert!(found, "the unbound listing has the new gateway");
    let bound = assets
        .call(
            Method::GET,
            &format!("{POOL}?state=bound"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(bound.status, 200, "{}", bound.body);
    assert!(
        !bound.body.contains(&gateway),
        "an unbound gateway is not in the bound listing"
    );

    // 2. The same hardware ID again (a replay or a second delivery): still one row, unchanged.
    // The tests of this binary share the database and each starts a service, so any of their
    // consumers may apply the event: wait for the subscription's record of it.
    let second = register_at_factory(&product, &issuer, &gateway, "ws-line-2").await;
    wait_for("the second event processed", EVENT_LIMIT, || {
        let product = &product;
        let second = second.clone();
        async move {
            let mut admin = product.superuser().await;
            let processed: i64 = sqlx::query_scalar(INBOX)
                .bind(&second)
                .fetch_one(&mut admin)
                .await
                .expect("read the inbox");
            (processed == 1).then_some(())
        }
    })
    .await;
    let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
        .bind(vec![first, second])
        .fetch_one(&mut admin)
        .await
        .expect("read the dead letters");
    assert_eq!(dead, 0, "both events were applied");
    let created: Vec<Value> = applied(&assets, &gateway)
        .iter()
        .map(|line| line["fields"]["created"].clone())
        .collect();
    assert!(
        created
            .iter()
            .filter(|created| **created == json!(true))
            .count()
            <= 1,
        "{created:?}"
    );
    let rows: i64 = sqlx::query_scalar(POOL_ROWS)
        .bind(&gateway)
        .fetch_one(&mut admin)
        .await
        .expect("count the pool rows");
    assert_eq!(rows, 1);
    let again = assets.call(Method::GET, &path, Some(&token), None).await;
    assert_eq!(again.json()["workstation"], "ws-line-1");
    // Reads of the pool are logged with the administrator. The service's stdout reaches the
    // test through a reader thread, so a line can arrive after its response does.
    wait_for("the pool read's log line", LOG_LIMIT, || async {
        assets
            .stdout
            .text()
            .contains(PLATFORM_TRANSACTION)
            .then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn events_the_pool_refuses_are_dead_lettered_and_later_gateways_still_enter() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let token = issuer.token(&admin_claims());

    // 1. A workstation name longer than the table stores: refused by the handler's own check.
    let too_long = hardware_id("tg-long");
    let first = register_at_factory(&product, &issuer, &too_long, &"w".repeat(256)).await;
    // 2. A value only the table refuses (a CHECK this test adds): the insert's SQLSTATE 23514.
    sqlx::raw_sql(ADD_REFUSING_CHECK)
        .execute(&mut admin)
        .await
        .expect("add the refusing CHECK");
    let refused = hardware_id("tg-check");
    let second = register_at_factory(&product, &issuer, &refused, REFUSED_WORKSTATION).await;
    // 3. A valid gateway after both: it enters the pool, so neither held the subscription up.
    let valid = hardware_id("tg-after");
    register_at_factory(&product, &issuer, &valid, "ws-line-1").await;
    let path = format!("{POOL}/{valid}");
    wait_for("the later gateway in the pool", EVENT_LIMIT, || async {
        let reply = assets.call(Method::GET, &path, Some(&token), None).await;
        (reply.status == 200).then_some(())
    })
    .await;
    sqlx::raw_sql(DROP_REFUSING_CHECK)
        .execute(&mut admin)
        .await
        .expect("drop the refusing CHECK");

    let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
        .bind(vec![first, second])
        .fetch_one(&mut admin)
        .await
        .expect("read the dead letters");
    assert_eq!(dead, 2, "both refused events were dead-lettered");
    for gateway in [&too_long, &refused] {
        let rows: i64 = sqlx::query_scalar(POOL_ROWS)
            .bind(gateway)
            .fetch_one(&mut admin)
            .await
            .expect("count the pool rows");
        assert_eq!(rows, 0, "{gateway} is not in the pool");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_is_written_with_its_event_once() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let claims = admin_claims();
    let token = issuer.token(&claims);
    let tenant = known_tenant(&mut admin).await;
    let other = known_tenant(&mut admin).await;
    let gateway = pooled(&mut admin, "tg-bind").await;

    // 1. Bound: 201, the binding, one event in the tenant with the administrator as the actor.
    let created = bind(&assets, &token, &gateway, tenant).await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.header("cache-control"), Some("no-store"));
    let binding = created.json();
    assert_eq!(binding["hardware_id"], gateway.as_str());
    assert_eq!(binding["tenant_id"], tenant.to_string());
    let binding_id = binding["binding_id"]
        .as_str()
        .expect("a binding ID")
        .to_owned();
    assert_eq!(
        bindings(&mut admin, &gateway).await,
        [(tenant.to_string(), binding_id.clone(), true)]
    );
    let stored = bound_events(&mut admin, &gateway).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let event = &stored[0];
    assert_eq!(event.aggregate_type, GATEWAY_BINDING_AGGREGATE);
    assert_eq!(event.aggregate_id, binding_id);
    assert_eq!(event.aggregate_seq, 1);
    assert_eq!(
        event.tenant_id.as_deref(),
        Some(tenant.to_string().as_str())
    );
    assert_eq!(event.envelope.tenant_id, Some(tenant));
    assert_eq!(
        event.envelope.actor,
        Some(contract::event::Actor::User {
            user_id: claims.sub
        })
    );
    let payload: GatewayBound = event.envelope.decode().expect("assets.gateway_bound");
    assert_eq!(payload.binding_id.to_string(), binding_id);
    assert_eq!(payload.hardware_id.as_str(), gateway);
    assert_eq!(
        serde_json::to_value(&event.envelope).expect("JSON")["payload"]
            .as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        ["binding_id", "hardware_id"],
        "the event carries the binding and the gateway, nothing else"
    );

    // 2. The same binding again (a retry after a lost response): 200, the same binding, nothing
    // written.
    let again = bind(&assets, &token, &gateway, tenant).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), binding);
    assert_eq!(bindings(&mut admin, &gateway).await.len(), 1);
    assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);

    // 3. Another tenant while it is bound: 409, nothing written, the holder not named.
    let refused = bind(&assets, &token, &gateway, other).await;
    assert_refused(
        &refused,
        409,
        "conflict",
        Some("gateway_binding.bound_to_another_tenant"),
    );
    assert!(
        !refused.body.contains(&tenant.to_string()),
        "{}",
        refused.body
    );
    assert_eq!(bindings(&mut admin, &gateway).await.len(), 1);
    assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);

    // 4. The pool shows the binding.
    let entry = assets
        .call(
            Method::GET,
            &format!("{POOL}/{gateway}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(entry.status, 200, "{}", entry.body);
    assert_eq!(entry.json()["binding"], binding);
    let bound = assets
        .call(
            Method::GET,
            &format!("{POOL}?state=bound&limit=500"),
            Some(&token),
            None,
        )
        .await;
    assert!(bound.body.contains(&gateway), "{}", bound.body);

    // 5. The log line of each binding. The service's stdout reaches the test through a reader
    // thread, so a line can arrive after its response does.
    let lines: Vec<Value> = wait_for("both bindings' log lines", LOG_LIMIT, || async {
        let lines: Vec<Value> = assets
            .stdout
            .messages("bound a gateway")
            .into_iter()
            .filter(|line| line["fields"]["hardware_id"] == gateway.as_str())
            .collect();
        (lines.len() >= 2).then_some(lines)
    })
    .await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["fields"]["created"], true);
    assert_eq!(lines[1]["fields"]["created"], false);
    assert_eq!(lines[0]["fields"]["user_id"], claims.sub.to_string());
    assert_eq!(lines[0]["fields"]["binding_id"], binding_id);
    assert!(
        !assets.stdout.text().contains(&token),
        "no token in the logs"
    );
    assert!(
        !assets.stderr.text().contains(&token),
        "no token in the logs"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_binding_leaves_nothing_and_the_retry_succeeds() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let token = issuer.token(&admin_claims());
    let tenant = known_tenant(&mut admin).await;
    let other = known_tenant(&mut admin).await;
    let gateway = pooled(&mut admin, "tg-fail").await;

    // 1. The event's insert fails after the binding's: 503, and neither is left behind.
    sqlx::raw_sql(CREATE_FAILURE)
        .execute(&mut admin)
        .await
        .expect("make the event fail");
    let failed = bind(&assets, &token, &gateway, tenant).await;
    sqlx::raw_sql(DROP_FAILURE)
        .execute(&mut admin)
        .await
        .expect("let the event through again");
    assert_refused(&failed, 503, "unavailable", None);
    assert!(bindings(&mut admin, &gateway).await.is_empty());
    assert!(bound_events(&mut admin, &gateway).await.is_empty());
    // The service's stdout reaches the test through a reader thread, so a line can arrive after
    // its response does.
    let errors = wait_for("the failed binding's log line", LOG_LIMIT, || async {
        let errors: Vec<Value> = assets
            .stdout
            .messages("could not bind a gateway")
            .into_iter()
            .filter(|line| line["fields"]["hardware_id"] == gateway.as_str())
            .collect();
        (!errors.is_empty()).then_some(errors)
    })
    .await;
    assert!(
        errors
            .iter()
            .any(|line| line["fields"]["error_kind"] == "outbox database"),
        "{errors:?}"
    );
    assert!(
        !assets.stdout.text().contains("injected failure"),
        "the database's message is not logged, only the kind"
    );

    // 2. Nothing is stuck: another tenant could take the gateway now, and the retry for the
    // same tenant binds it.
    let retried = bind(&assets, &token, &gateway, tenant).await;
    assert_eq!(retried.status, 201, "{}", retried.body);
    assert_eq!(bindings(&mut admin, &gateway).await.len(), 1);
    assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);
    let again = bind(&assets, &token, &gateway, tenant).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), retried.json());
    let refused = bind(&assets, &token, &gateway, other).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_bindings_give_one_binding() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let token = issuer.token(&admin_claims());
    let first = known_tenant(&mut admin).await;
    let second = known_tenant(&mut admin).await;

    // Two tenants at once: one wins (201), the other is refused (409).
    for _ in 0..5 {
        let gateway = pooled(&mut admin, "tg-race").await;
        let (a, b) = tokio::join!(
            bind(&assets, &token, &gateway, first),
            bind(&assets, &token, &gateway, second)
        );
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [201, 409], "{a:?} {b:?}");
        assert_eq!(bindings(&mut admin, &gateway).await.len(), 1);
        assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);
    }
    // The same tenant twice at once: one binds (201), the other finds it bound (200).
    for _ in 0..5 {
        let gateway = pooled(&mut admin, "tg-race").await;
        let (a, b) = tokio::join!(
            bind(&assets, &token, &gateway, first),
            bind(&assets, &token, &gateway, first)
        );
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 201], "{a:?} {b:?}");
        assert_eq!(a.json(), b.json());
        assert_eq!(bindings(&mut admin, &gateway).await.len(), 1);
        assert_eq!(bound_events(&mut admin, &gateway).await.len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_platform_administrators_own_request_gets_anywhere() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let gateway = pooled(&mut admin, "tg-auth").await;
    let tenant_admin_claims = InternalClaims {
        tenant: Some(contract::TenantGrant {
            id: tenant,
            role: TenantRole::Admin,
        }),
        ..TestIssuer::claims(ServiceName::Assets, None)
    };
    let exchanged_claims = InternalClaims {
        act: Some(Actor {
            sub: ServiceName::Identity,
        }),
        ..admin_claims()
    };
    let refused_users = [&tenant_admin_claims, &exchanged_claims];
    let tenant_admin = issuer.token(&tenant_admin_claims);
    let exchanged = issuer.token(&exchanged_claims);
    let elsewhere = issuer.token(&InternalClaims {
        platform_admin: true,
        ..TestIssuer::claims(ServiceName::Identity, None)
    });
    let body = json!({"tenant_id": tenant.to_string()});
    let requests = [
        (Method::GET, POOL.to_owned(), None),
        (Method::GET, format!("{POOL}/{gateway}"), None),
        (Method::PUT, binding_path(&gateway), Some(&body)),
    ];
    for (method, path, body) in &requests {
        for (token, status, code) in [
            (None, 401, "unauthenticated"),
            (Some(elsewhere.as_str()), 401, "unauthenticated"),
            (Some(tenant_admin.as_str()), 403, "permission_denied"),
            (Some(exchanged.as_str()), 403, "permission_denied"),
        ] {
            let reply = match body {
                Some(body) => assets.json(method.clone(), path, token, body).await,
                None => assets.call(method.clone(), path, token, None).await,
            };
            assert_refused(&reply, status, code, None);
        }
    }
    assert!(bindings(&mut admin, &gateway).await.is_empty());

    // The control: the platform administrator's own requests read the pool and open the
    // tenant's transaction, and both are logged with the administrator's user ID.
    let allowed_claims = admin_claims();
    let allowed = issuer.token(&allowed_claims);
    let read = assets
        .call(
            Method::GET,
            &format!("{POOL}/{gateway}"),
            Some(&allowed),
            None,
        )
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    let bound = bind(&assets, &allowed, &gateway, tenant).await;
    assert_eq!(bound.status, 201, "{}", bound.body);
    let accesses = |user: &str| -> Vec<Value> {
        [PLATFORM_TRANSACTION, TENANT_TRANSACTION]
            .into_iter()
            .flat_map(|message| assets.stdout.messages(message))
            .filter(|line| line["fields"]["user_id"] == user)
            .collect()
    };
    // The service's stdout reaches the test through a reader thread, so a line can arrive after
    // its response does. The lines arrive in order: once the binding's line is here, so are the
    // lines of every request before it.
    let opened = wait_for(
        "the binding's tenant transaction line",
        LOG_LIMIT,
        || async {
            let opened = accesses(&allowed_claims.sub.to_string());
            opened
                .iter()
                .any(|line| line["message"] == TENANT_TRANSACTION)
                .then_some(opened)
        },
    )
    .await;
    let tenant_lines: Vec<&Value> = opened
        .iter()
        .filter(|line| line["message"] == TENANT_TRANSACTION)
        .collect();
    assert_eq!(tenant_lines.len(), 1, "{opened:?}");
    assert_eq!(tenant_lines[0]["level"], "INFO", "{opened:?}");
    assert_eq!(tenant_lines[0]["fields"]["tenant_id"], tenant.to_string());
    assert_eq!(tenant_lines[0]["fields"]["hardware_id"], gateway.as_str());
    assert!(
        opened
            .iter()
            .any(|line| line["message"] == PLATFORM_TRANSACTION),
        "{opened:?}"
    );

    // Refused before anything is read: no transaction was opened for the refused users.
    let text = assets.stdout.text();
    for claims in refused_users {
        let user = claims.sub.to_string();
        let opened = accesses(&user);
        assert!(opened.is_empty(), "{opened:?}");
        assert!(text.contains(&user), "the refusal is logged with the user");
    }
    for token in [&tenant_admin, &exchanged, &elsewhere, &allowed] {
        assert!(!text.contains(token.as_str()), "no token in the logs");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_refused_and_write_nothing() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let mut admin = product.superuser().await;
    let token = issuer.token(&admin_claims());
    let tenant = known_tenant(&mut admin).await;
    let gateway = pooled(&mut admin, "tg-bad").await;
    let path = binding_path(&gateway);

    // Paths: a hardware ID that breaks the rule, or one the pool does not have.
    // `a%2Fb` decodes to `a/b`; a literal `..` would be resolved by the client.
    for missing in ["a%2Fb", "-x", &"x".repeat(65)] {
        let reply = assets
            .call(
                Method::GET,
                &format!("{POOL}/{missing}"),
                Some(&token),
                None,
            )
            .await;
        assert_refused(&reply, 404, "not_found", None);
        let reply = bind(&assets, &token, missing, tenant).await;
        assert_refused(&reply, 404, "not_found", None);
    }
    let unknown = hardware_id("tg-nowhere");
    let reply = assets
        .call(
            Method::GET,
            &format!("{POOL}/{unknown}"),
            Some(&token),
            None,
        )
        .await;
    assert_refused(&reply, 404, "not_found", None);
    let reply = bind(&assets, &token, &unknown, tenant).await;
    assert_refused(&reply, 404, "not_found", None);

    // Listing parameters.
    for query in [
        "state=all",
        "limit=0",
        "limit=501",
        "limit=x",
        "after=..",
        "other=1",
    ] {
        let reply = assets
            .call(Method::GET, &format!("{POOL}?{query}"), Some(&token), None)
            .await;
        assert_refused(&reply, 400, "invalid_request", None);
    }

    // Bodies.
    let json_body = |value: &Value| Some(("application/json", serde_json::to_vec(value).unwrap()));
    let tenant_body = json!({"tenant_id": tenant.to_string()});
    let reply = assets
        .call(
            Method::PUT,
            &path,
            Some(&token),
            Some(("text/plain", serde_json::to_vec(&tenant_body).unwrap())),
        )
        .await;
    assert_refused(&reply, 400, "invalid_request", None);
    let reply = assets
        .call(
            Method::PUT,
            &path,
            Some(&token),
            Some(("application/json", vec![b' '; 4 * 1024 + 1])),
        )
        .await;
    assert_refused(
        &reply,
        400,
        "invalid_request",
        Some("gateway_binding.body_too_large"),
    );
    let reply = assets
        .call(Method::PUT, &path, Some(&token), json_body(&json!([])))
        .await;
    assert_refused(
        &reply,
        400,
        "invalid_request",
        Some("gateway_binding.body_invalid"),
    );
    for (body, field, key) in [
        (json!({}), "/tenant_id", "gateway_binding.field_required"),
        (
            json!({"tenant_id": "acme"}),
            "/tenant_id",
            "gateway_binding.tenant_id_invalid",
        ),
        (
            json!({"tenant_id": tenant.to_string(), "hardware_id": "tg-1"}),
            "/hardware_id",
            "gateway_binding.unknown_field",
        ),
        (
            json!({"tenant_id": TenantId::generate().to_string()}),
            "/tenant_id",
            "gateway_binding.tenant_unknown",
        ),
    ] {
        let reply = assets
            .call(Method::PUT, &path, Some(&token), json_body(&body))
            .await;
        assert_refused(&reply, 422, "validation_failed", None);
        let errors = reply.json()["field_errors"].clone();
        assert_eq!(
            errors,
            json!([{"field": field, "message_key": key}]),
            "{body}"
        );
    }
    assert!(bindings(&mut admin, &gateway).await.is_empty());
    assert!(bound_events(&mut admin, &gateway).await.is_empty());
}
