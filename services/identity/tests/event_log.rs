//! The event layer and operation log end to end (Laysi/trustiot-v2#130): the real binary on the
//! product's schema, applied by the migration Job (`testkit::product`). It records every event of
//! the outbox once, with who made the change, and nothing of a rolled-back change, from every
//! producer it consumes (its own outbox and assets' device model events); its roles
//! cannot change or remove a recorded event; tenant administrators read their own tenant's log,
//! platform administrators any tenant's and the platform-wide log, by time and in pages.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body};
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-a";
const TENANTS: &str = "/api/identity/v1/tenants";
const LOG: &str = "/api/identity/v1/event-log";
const PLATFORM_LOG: &str = "/api/identity/v1/platform/event-log";
/// The consumer of the service's own outbox.
const SUBSCRIPTION: &str = "identity.event-log.identity";

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits"): starting the binary, and
/// the consumer's delivery of a committed event (it polls every second when idle).
const READY_LIMIT: Duration = Duration::from_secs(60);
const EVENT_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid, now() + interval '1 hour')";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const REVOKE_EVENTS: &str = "REVOKE INSERT ON identity.outbox_event FROM identity_app";
const GRANT_EVENTS: &str = "GRANT INSERT ON identity.outbox_event TO identity_app";

/// A tenant's recorded events: type, actor, aggregate.
const LOGGED: &str = "SELECT event_id::text, event_type, actor_user_id::text, actor_service,
    aggregate_type, aggregate_id, aggregate_seq
FROM identity.event_log WHERE tenant_id = $1::uuid ORDER BY occurred_at, event_id";
/// One row of `LOGGED`, in its column order.
type LoggedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
);
/// The outbox's events of a tenant: the event layer's source.
const OUTBOX_IDS: &str =
    "SELECT event_id::text FROM identity.outbox_event WHERE tenant_id = $1::uuid ORDER BY id";
const LOG_COUNT: &str = "SELECT count(*) FROM identity.event_log";
const OUTBOX_TENANT_COUNT: &str =
    "SELECT count(*) FROM identity.outbox_event WHERE tenant_id IS NOT NULL";
const TENANT_COUNT: &str = "SELECT count(*) FROM identity.tenants";
const LOGGED_BY_ID: &str = "SELECT count(*) FROM identity.event_log WHERE event_id = $1::uuid";
const PLATFORM_LOGGED_BY_ID: &str =
    "SELECT count(*) FROM identity.platform_event_log WHERE event_id = $1::uuid";
const INBOX_HAS: &str = "SELECT count(*) FROM identity.outbox_inbox
    WHERE subscription = $1 AND event_id = $2::uuid";
const DEAD_LETTER: &str = "SELECT reason FROM identity.outbox_dead_letter
    WHERE subscription = $1 AND event_id = $2::uuid";

/// An event written to the outbox by hand, as a producer would: `$1` event ID, `$2` type, `$3`
/// tenant (NULL for a platform-wide event), `$4` aggregate ID, `$5` the envelope.
const INSERT_OUTBOX: &str = "INSERT INTO identity.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (pg_current_xact_id()::text::bigint, $1::uuid, $2, $3::uuid, 'test', $4, 1, $5::jsonb)";

/// The consumer of assets' outbox (device models, Laysi/trustiot-v2#99).
const ASSETS_SUBSCRIPTION: &str = "identity.event-log.assets";
/// A device model event written to assets' outbox by hand, as the assets service writes it:
/// `$1` event ID, `$2` type, `$3` tenant, `$4` the device model ID, `$5` its sequence, `$6` the
/// envelope.
const INSERT_ASSETS_OUTBOX: &str = "INSERT INTO assets.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (pg_current_xact_id()::text::bigint, $1::uuid, $2, $3::uuid, 'device_model', $4, $5,
        $6::jsonb)";
/// The consumer of certs' outbox (factory registrations, Laysi/trustiot-v2#79).
const CERTS_SUBSCRIPTION: &str = "identity.event-log.certs";
/// A platform-wide registration event written to certs' outbox by hand, as the certs service
/// writes it: `$1` event ID, `$2` the envelope.
const INSERT_CERTS_OUTBOX: &str = "INSERT INTO certs.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (pg_current_xact_id()::text::bigint, $1::uuid, 'certs.gateway_registered', NULL,
        'gateway_registration', 'gw-0001', 1, $2::jsonb)";
/// A recorded event's producer, type, aggregate and actor, by its ID.
const LOGGED_ROW: &str =
    "SELECT tenant_id::text, producer, event_type, aggregate_type, aggregate_id,
    aggregate_seq, actor_user_id::text
FROM identity.event_log WHERE event_id = $1::uuid";
/// One row of `LOGGED_ROW`, in its column order.
type LoggedAssetsRow = (String, String, String, String, String, i64, Option<String>);

/// A row of the log written directly, at a chosen time: `$1` tenant, `$2` event ID, `$3`
/// `occurred_at`, `$4` aggregate ID (a canary), `$5` the payload.
const SEED_LOG: &str = "INSERT INTO identity.event_log
    (tenant_id, event_id, occurred_at, event_type, version, producer, aggregate_type,
     aggregate_id, aggregate_seq, actor_service, payload)
VALUES ($1::uuid, $2::uuid, $3::timestamptz, 'test.seeded', 1, 'identity', 'test', $4, 1,
        'identity', $5::jsonb)";

/// Changes no role of the service may make to a recorded event, on both tables.
const CHANGES: [&str; 6] = [
    "UPDATE identity.event_log SET event_type = 'rewritten'",
    "DELETE FROM identity.event_log",
    "TRUNCATE identity.event_log",
    "UPDATE identity.platform_event_log SET event_type = 'rewritten'",
    "DELETE FROM identity.platform_event_log",
    "TRUNCATE identity.platform_event_log",
];
/// Writes the platform role may not make.
const PLATFORM_INSERTS: [&str; 2] = [
    "INSERT INTO identity.event_log SELECT * FROM identity.event_log WHERE false",
    "INSERT INTO identity.platform_event_log SELECT * FROM identity.platform_event_log WHERE false",
];
/// The service role writes platform-wide events but does not read them.
const APP_READS_PLATFORM_LOG: &str = "SELECT count(*) FROM identity.platform_event_log";

fn start(product: &ProductPostgres) -> Service {
    let files = KeyFiles::new("event-log", &TestKey::new(130), KID);
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        (
            "TRUSTIOT__DATABASE__PLATFORM_USERNAME".into(),
            "identity_platform".into(),
        ),
        (
            "TRUSTIOT__DATABASE__PLATFORM_PASSWORD".into(),
            TEST_PASSWORD.into(),
        ),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(READY_LIMIT);
    service
}

/// A user, optionally a member of `member`'s tenant with its role, and an active session of
/// theirs (in that tenant); the cookie and the user ID.
async fn session(
    admin: &mut PgConnection,
    platform_admin: bool,
    member: Option<(&str, &str)>,
) -> (String, String) {
    let user = contract::UserId::generate().to_string();
    sqlx::query(INSERT_USER)
        .bind(&user)
        .bind(platform_admin)
        .execute(&mut *admin)
        .await
        .expect("insert the user");
    if let Some((tenant, role)) = member {
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(tenant)
            .bind(&user)
            .bind(role)
            .execute(&mut *admin)
            .await
            .expect("insert the membership");
    }
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&user)
        .bind(member.map(|(tenant, _)| tenant))
        .execute(&mut *admin)
        .await
        .expect("insert the session");
    (cookie(&secret), user)
}

/// The internal identity token the ingress gateway would forward for `cookie`, fresh (tokens
/// live 60 s, shorter than this test).
fn token(service: &Service, cookie: &str) -> String {
    let response = request(
        service.address,
        "POST",
        &format!("/internal/ext-authz{LOG}"),
        &[("Host", APP), ("Cookie", cookie)],
    )
    .expect("call the ext_authz check");
    assert_eq!(response.status, 200, "{}", response.body);
    response.header_values("x-internal-identity")[0].to_owned()
}

/// `GET target` as the session's user (`None`: without a token).
fn get(service: &Service, cookie: Option<&str>, target: &str) -> support::Response {
    let token = cookie.map(|cookie| token(service, cookie));
    let mut headers = vec![("Host", APP)];
    if let Some(token) = &token {
        headers.push(("x-internal-identity", token.as_str()));
    }
    request(service.address, "GET", target, &headers).expect("call the event log API")
}

/// `GET target`, which must answer 200 with `no-store`; the page.
fn page(service: &Service, cookie: &str, target: &str) -> Value {
    let response = get(service, Some(cookie), target);
    assert_eq!(response.status, 200, "{target}: {}", response.body);
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    response.json()
}

/// The event IDs of a page, in order.
fn ids(page: &Value) -> Vec<String> {
    page["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["event_id"].as_str().expect("event_id").to_owned())
        .collect()
}

fn create_tenant(service: &Service, cookie: &str, name: &str) -> support::Response {
    let token = token(service, cookie);
    request_with_body(
        service.address,
        "POST",
        TENANTS,
        &[
            ("Host", APP),
            ("Content-Type", "application/json"),
            ("x-internal-identity", &token),
        ],
        json!({ "name": name }).to_string().as_bytes(),
    )
    .expect("call the tenant API")
}

async fn count(admin: &mut PgConnection, statement: &'static str, args: &[&str]) -> i64 {
    let mut query = sqlx::query_scalar(statement);
    for arg in args {
        query = query.bind(*arg);
    }
    query
        .fetch_one(admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"))
}

/// Waits until `statement` with `args` counts at least one row.
async fn wait_for(admin: &mut PgConnection, what: &str, statement: &'static str, args: &[&str]) {
    let deadline = Instant::now() + EVENT_LIMIT;
    while count(admin, statement, args).await == 0 {
        assert!(
            Instant::now() < deadline,
            "{what}: not within {EVENT_LIMIT:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// An envelope as a producer writes it.
fn envelope(id: &str, event_type: &str, tenant: Option<&str>, producer: &str) -> String {
    json!({
        "id": id,
        "type": event_type,
        "version": 1,
        "tenant_id": tenant,
        "occurred_at": "2026-10-21T08:00:00.000001Z",
        "producer": producer,
        "payload": {"probe": id},
        "actor": {"kind": "service", "service": "identity"},
    })
    .to_string()
}

/// Writes an event to the outbox by hand, in a transaction of its own; its ID.
async fn write_outbox(
    admin: &mut PgConnection,
    event_type: &str,
    tenant: Option<&str>,
    producer: &str,
) -> String {
    let id = contract::EventId::generate().to_string();
    sqlx::query(INSERT_OUTBOX)
        .bind(&id)
        .bind(event_type)
        .bind(tenant)
        .bind(format!("aggregate-{id}"))
        .bind(envelope(&id, event_type, tenant, producer))
        .execute(admin)
        .await
        .expect("write an outbox event");
    id
}

/// Writes one of assets' device model events to its outbox by hand, in a transaction of its
/// own, with `user` as the actor; its ID.
async fn write_device_model_event(
    admin: &mut PgConnection,
    event_type: &str,
    tenant: &str,
    model: &str,
    seq: i64,
    user: &str,
) -> String {
    let id = contract::EventId::generate().to_string();
    let envelope = json!({
        "id": id,
        "type": event_type,
        "version": 1,
        "tenant_id": tenant,
        "occurred_at": "2026-10-21T09:00:00.000001Z",
        "producer": "assets",
        "payload": {"device_model_id": model, "probe": id},
        "actor": {"kind": "user", "user_id": user},
    });
    sqlx::query(INSERT_ASSETS_OUTBOX)
        .bind(&id)
        .bind(event_type)
        .bind(tenant)
        .bind(model)
        .bind(seq)
        .bind(envelope.to_string())
        .execute(admin)
        .await
        .expect("write a device model event to assets' outbox");
    id
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// Runs `statement` as `role` and expects the privilege check to refuse it.
async fn refused(product: &ProductPostgres, role: &str, statement: &'static str) {
    let mut conn = product.postgres.connect(role, DATABASE).await;
    let result = sqlx::raw_sql(statement).execute(&mut conn).await;
    let error = result.expect_err(&format!("{role} may run `{statement}`"));
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{role}: `{statement}`: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_log_records_each_operation_once_and_serves_it_by_tenant_and_time() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let service = start(&product);
    let mut admin = product.superuser().await;
    let (platform_cookie, platform_user) = session(&mut admin, true, None).await;

    // 1. One operation, one event: a platform administrator creates a tenant; the consumer
    //    records its one event, with the administrator as the actor.
    let response = create_tenant(&service, &platform_cookie, "Plant A");
    assert_eq!(response.status, 201, "{}", response.body);
    let tenant_a = response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned();
    let outbox: Vec<String> = sqlx::query_scalar(OUTBOX_IDS)
        .bind(&tenant_a)
        .fetch_all(&mut admin)
        .await
        .expect("the outbox events");
    assert_eq!(outbox.len(), 1, "one operation writes one event");
    wait_for(
        &mut admin,
        "the tenant's event in the log",
        LOGGED_BY_ID,
        &[&outbox[0]],
    )
    .await;
    let logged: Vec<LoggedRow> = sqlx::query_as(LOGGED)
        .bind(&tenant_a)
        .fetch_all(&mut admin)
        .await
        .expect("the log");
    assert_eq!(
        logged,
        [(
            outbox[0].clone(),
            "identity.tenant_created".to_owned(),
            Some(platform_user.clone()),
            None,
            "tenant".to_owned(),
            tenant_a.clone(),
            1
        )],
        "exactly the one event, with who made the change"
    );

    // 2. A rolled-back operation leaves no event: when the event cannot be written, the tenant
    //    is not created, and the log gets nothing. A later operation's event arrives, so the
    //    consumer has passed the point where the lost one would have been.
    let tenants_before = count(&mut admin, TENANT_COUNT, &[]).await;
    let log_before = count(&mut admin, LOG_COUNT, &[]).await;
    sqlx::raw_sql(REVOKE_EVENTS)
        .execute(&mut admin)
        .await
        .expect("take away the event write");
    let response = create_tenant(&service, &platform_cookie, "Plant rolled back");
    assert_eq!(response.status, 503, "{}", response.body);
    sqlx::raw_sql(GRANT_EVENTS)
        .execute(&mut admin)
        .await
        .expect("give the event write back");
    let response = create_tenant(&service, &platform_cookie, "Plant B");
    assert_eq!(response.status, 201, "{}", response.body);
    let tenant_b = response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned();
    let event_b: String = sqlx::query_scalar(OUTBOX_IDS)
        .bind(&tenant_b)
        .fetch_one(&mut admin)
        .await
        .expect("tenant B's event");
    wait_for(
        &mut admin,
        "tenant B's event in the log",
        LOGGED_BY_ID,
        &[&event_b],
    )
    .await;
    assert_eq!(
        count(&mut admin, TENANT_COUNT, &[]).await,
        tenants_before + 1
    );
    assert_eq!(
        count(&mut admin, LOG_COUNT, &[]).await,
        log_before + 1,
        "only the committed operation was recorded"
    );
    assert_eq!(
        count(&mut admin, LOG_COUNT, &[]).await,
        count(&mut admin, OUTBOX_TENANT_COUNT, &[]).await,
        "every committed tenant event of the outbox is in the log, once"
    );

    // 3. Once: an event the log already holds (a replay, another replica) is not recorded again
    //    and does not stop the subscription.
    let replayed = contract::EventId::generate().to_string();
    sqlx::query(SEED_LOG)
        .bind(&tenant_a)
        .bind(&replayed)
        .bind("2000-02-01T00:00:00.000001Z")
        .bind("already-recorded")
        .bind(json!({"first": true}).to_string())
        .execute(&mut admin)
        .await
        .expect("record the event first");
    sqlx::query(INSERT_OUTBOX)
        .bind(&replayed)
        .bind("test.replayed")
        .bind(&tenant_a)
        .bind(format!("aggregate-{replayed}"))
        .bind(envelope(
            &replayed,
            "test.replayed",
            Some(&tenant_a),
            "identity",
        ))
        .execute(&mut admin)
        .await
        .expect("write the same event to the outbox");
    wait_for(
        &mut admin,
        "the replayed event processed",
        INBOX_HAS,
        &[SUBSCRIPTION, &replayed],
    )
    .await;
    assert_eq!(count(&mut admin, LOGGED_BY_ID, &[&replayed]).await, 1);
    assert_eq!(
        count(&mut admin, DEAD_LETTER_COUNT, &[SUBSCRIPTION, &replayed]).await,
        0
    );

    // 4. Only the producer's own events: an envelope in identity's outbox that names another
    //    producer is forged or broken. It goes to the dead letters, not the log.
    let forged = write_outbox(&mut admin, "assets.forged", Some(&tenant_a), "assets").await;
    wait_for(
        &mut admin,
        "the forged event's dead letter",
        DEAD_LETTER_COUNT,
        &[SUBSCRIPTION, &forged],
    )
    .await;
    let reason: String = sqlx::query_scalar(DEAD_LETTER)
        .bind(SUBSCRIPTION)
        .bind(&forged)
        .fetch_one(&mut admin)
        .await
        .expect("the dead letter");
    assert!(reason.contains("another producer"), "{reason}");
    assert_eq!(count(&mut admin, LOGGED_BY_ID, &[&forged]).await, 0);

    // 5. Platform-wide events go to their own log.
    let platform_event = write_outbox(&mut admin, "identity.test_platform", None, "identity").await;
    wait_for(
        &mut admin,
        "the platform-wide event in the log",
        PLATFORM_LOGGED_BY_ID,
        &[&platform_event],
    )
    .await;
    assert_eq!(count(&mut admin, LOGGED_BY_ID, &[&platform_event]).await, 0);

    // 5b. Every producer in CONSUMES: assets' device model events (Laysi/trustiot-v2#99) are
    //     recorded under their tenant, with their producer and actor, and only there.
    let model = contract::DeviceModelId::generate().to_string();
    let mut device_model_events = Vec::new();
    for (seq, event_type) in [
        "assets.device_model_created",
        "assets.device_model_updated",
        "assets.device_model_deleted",
    ]
    .into_iter()
    .enumerate()
    {
        let seq = i64::try_from(seq).expect("a small sequence") + 1;
        let id = write_device_model_event(
            &mut admin,
            event_type,
            &tenant_a,
            &model,
            seq,
            &platform_user,
        )
        .await;
        device_model_events.push((id, event_type, seq));
    }
    for (id, event_type, seq) in &device_model_events {
        wait_for(
            &mut admin,
            "assets' device model event in the log",
            LOGGED_BY_ID,
            &[id],
        )
        .await;
        let row: LoggedAssetsRow = sqlx::query_as(LOGGED_ROW)
            .bind(id)
            .fetch_one(&mut admin)
            .await
            .expect("the recorded device model event");
        assert_eq!(
            row,
            (
                tenant_a.clone(),
                "assets".to_owned(),
                (*event_type).to_owned(),
                "device_model".to_owned(),
                model.clone(),
                *seq,
                Some(platform_user.clone()),
            ),
            "recorded as assets wrote it, under its tenant"
        );
        assert_eq!(
            count(&mut admin, INBOX_HAS, &[ASSETS_SUBSCRIPTION, id]).await,
            1
        );
        assert_eq!(count(&mut admin, PLATFORM_LOGGED_BY_ID, &[id]).await, 0);
    }
    let device_model_ids: Vec<String> = device_model_events
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect();

    // 5c. certs' factory registrations (Laysi/trustiot-v2#79) are platform-wide: recorded in the
    //     platform log only.
    let registration = contract::EventId::generate().to_string();
    let envelope = json!({
        "id": registration,
        "type": "certs.gateway_registered",
        "version": 1,
        "tenant_id": null,
        "occurred_at": "2026-10-23T09:00:00.000001Z",
        "producer": "certs",
        "payload": {"hardware_id": "gw-0001", "probe": registration},
        "actor": {"kind": "user", "user_id": platform_user},
    });
    sqlx::query(INSERT_CERTS_OUTBOX)
        .bind(&registration)
        .bind(envelope.to_string())
        .execute(&mut admin)
        .await
        .expect("write a registration event to certs' outbox");
    wait_for(
        &mut admin,
        "certs' registration event in the platform log",
        PLATFORM_LOGGED_BY_ID,
        &[&registration],
    )
    .await;
    assert_eq!(count(&mut admin, LOGGED_BY_ID, &[&registration]).await, 0);
    assert_eq!(
        count(&mut admin, INBOX_HAS, &[CERTS_SUBSCRIPTION, &registration]).await,
        1
    );

    // 6. Append-only: neither of the service's roles may change or remove a recorded event,
    //    and the platform role writes nothing.
    for role in ["identity_app", "identity_platform"] {
        for statement in CHANGES {
            refused(&product, role, statement).await;
        }
    }
    for statement in PLATFORM_INSERTS {
        refused(&product, "identity_platform", statement).await;
    }
    refused(&product, "identity_app", APP_READS_PLATFORM_LOG).await;
    assert_eq!(
        count(&mut admin, LOGGED_BY_ID, &[&outbox[0]]).await,
        1,
        "the recorded event is still there"
    );

    // 7. A tenant administrator reads their tenant's log, newest first, by time and in pages.
    //    Five more events of tenant A, a day apart, with canaries in the aggregate ID.
    let mut seeded = Vec::new();
    for day in 1..=5 {
        let id = contract::EventId::generate().to_string();
        sqlx::query(SEED_LOG)
            .bind(&tenant_a)
            .bind(&id)
            .bind(format!("2000-01-0{day}T12:00:00Z"))
            .bind(format!("canary-a-{day}"))
            .bind(json!({"day": day}).to_string())
            .execute(&mut admin)
            .await
            .expect("seed the log");
        seeded.push(id);
    }
    let (a_admin, _) = session(&mut admin, false, Some((&tenant_a, "admin"))).await;
    let (b_admin, _) = session(&mut admin, false, Some((&tenant_b, "admin"))).await;
    let (a_viewer, _) = session(&mut admin, false, Some((&tenant_a, "viewer"))).await;

    let all = page(&service, &a_admin, LOG);
    let all_ids = ids(&all);
    assert_eq!(all["next_cursor"], Value::Null);
    // Newest first: the tenant's creation (now), assets' device model events (2026-10-21,
    // newest ID first), the event recorded before its replay (2000-02-01), then the seeded days.
    let mut expected_order = vec![outbox[0].clone()];
    let mut by_id = device_model_ids.clone();
    by_id.sort();
    expected_order.extend(by_id.into_iter().rev());
    expected_order.push(replayed.clone());
    expected_order.extend(seeded.iter().rev().cloned());
    assert_eq!(all_ids, expected_order, "{all}");
    let created = &all["events"][all_ids
        .iter()
        .position(|id| *id == outbox[0])
        .expect("created")];
    let device_model = &all["events"][all_ids
        .iter()
        .position(|id| *id == device_model_ids[0])
        .expect("the device model event")];
    assert_eq!(device_model["type"], "assets.device_model_created");
    assert_eq!(device_model["producer"], "assets");
    assert_eq!(device_model["aggregate_type"], "device_model");
    assert_eq!(device_model["aggregate_id"], model.as_str());
    assert_eq!(
        device_model["actor"],
        json!({"kind": "user", "user_id": platform_user})
    );
    assert_eq!(
        device_model["payload"],
        json!({"device_model_id": model, "probe": device_model_ids[0]})
    );
    assert_eq!(created["type"], "identity.tenant_created");
    assert_eq!(created["producer"], "identity");
    assert_eq!(created["tenant_id"], tenant_a.as_str());
    assert_eq!(created["aggregate_type"], "tenant");
    assert_eq!(
        created["actor"],
        json!({"kind": "user", "user_id": platform_user})
    );
    assert_eq!(created["payload"], json!({}));
    assert!(created["occurred_at"].is_string() && created["recorded_at"].is_string());

    // By time: `from` inclusive, `to` exclusive.
    let range = page(
        &service,
        &a_admin,
        &format!("{LOG}?from=2000-01-02T12:00:00Z&to=2000-01-04T12:00:00Z"),
    );
    assert_eq!(ids(&range), [seeded[2].clone(), seeded[1].clone()]);
    assert_eq!(range["events"][0]["aggregate_id"], "canary-a-3");
    assert_eq!(range["events"][0]["payload"], json!({"day": 3}));
    assert_eq!(
        range["events"][0]["actor"],
        json!({"kind": "service", "service": "identity"})
    );

    // In pages: every event once, in the same order, and the last page has no cursor.
    let mut paged = Vec::new();
    let mut target = format!("{LOG}?limit=3");
    let mut pages = 0;
    loop {
        let next = page(&service, &a_admin, &target);
        pages += 1;
        paged.extend(ids(&next));
        match next["next_cursor"].as_str() {
            Some(cursor) => target = format!("{LOG}?limit=3&cursor={cursor}"),
            None => break,
        }
        assert!(pages < 10, "the pages never end");
    }
    assert_eq!(pages, 4, "ten events, three a page");
    assert_eq!(paged, all_ids);

    // Another tenant's administrator sees only their own tenant.
    let other = page(&service, &b_admin, LOG);
    assert_eq!(ids(&other), std::slice::from_ref(&event_b));
    assert!(
        !other.to_string().contains(&tenant_a)
            && !other.to_string().contains("canary-a")
            && !other.to_string().contains(&model),
        "{other}"
    );

    // Refusals.
    let response = get(&service, Some(&a_viewer), LOG);
    assert_eq!(response.status, 403, "{}", response.body);
    assert_eq!(response.json()["code"], "permission_denied");
    let response = get(&service, Some(&platform_cookie), LOG);
    assert_eq!(
        response.status, 403,
        "a platform administrator without a tenant"
    );
    let response = get(&service, None, LOG);
    assert_eq!(response.status, 401, "{}", response.body);
    assert_eq!(response.json()["code"], "unauthenticated");
    for query in [
        "limit=0",
        "limit=501",
        "limit=many",
        "from=yesterday",
        "to=2026-01-01",
        "from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
        "cursor=nope",
        "limit=1&limit=2",
    ] {
        let response = get(&service, Some(&a_admin), &format!("{LOG}?{query}"));
        assert_eq!(response.status, 400, "{query}: {}", response.body);
        assert_eq!(response.json()["code"], "invalid_request", "{query}");
    }

    // 8. The back office: a platform administrator reads any tenant's log by time, and the
    //    platform-wide log.
    let back_office = page(
        &service,
        &platform_cookie,
        &format!(
            "/api/identity/v1/platform/tenants/{tenant_a}/event-log\
             ?from=2000-01-02T12:00:00Z&to=2000-01-04T12:00:00Z"
        ),
    );
    assert_eq!(ids(&back_office), [seeded[2].clone(), seeded[1].clone()]);
    let whole = page(
        &service,
        &platform_cookie,
        &format!("/api/identity/v1/platform/tenants/{tenant_a}/event-log"),
    );
    assert_eq!(ids(&whole), all_ids);
    let tenant_b_log = page(
        &service,
        &platform_cookie,
        &format!("/api/identity/v1/platform/tenants/{tenant_b}/event-log"),
    );
    assert_eq!(ids(&tenant_b_log), std::slice::from_ref(&event_b));
    let unknown = contract::TenantId::generate();
    let empty = page(
        &service,
        &platform_cookie,
        &format!("/api/identity/v1/platform/tenants/{unknown}/event-log"),
    );
    assert_eq!(empty, json!({"events": [], "next_cursor": null}));
    let platform_wide = page(&service, &platform_cookie, PLATFORM_LOG);
    let platform_ids = ids(&platform_wide);
    assert!(platform_ids.contains(&platform_event), "{platform_wide}");
    assert!(platform_ids.contains(&registration), "{platform_wide}");
    assert!(
        !platform_ids.contains(&outbox[0]),
        "tenant events are not platform-wide"
    );
    let response = get(
        &service,
        Some(&platform_cookie),
        "/api/identity/v1/platform/tenants/not-a-tenant/event-log",
    );
    assert_eq!(response.status, 400, "{}", response.body);
    for target in [
        format!("/api/identity/v1/platform/tenants/{tenant_a}/event-log"),
        PLATFORM_LOG.to_owned(),
    ] {
        let response = get(&service, Some(&a_admin), &target);
        assert_eq!(response.status, 403, "{target}: {}", response.body);
        assert_eq!(response.json()["code"], "permission_denied");
        let response = get(&service, None, &target);
        assert_eq!(response.status, 401, "{target}: {}", response.body);
    }

    // 9. The back office's reads are logged with the administrator and the purpose; the log
    //    never holds a payload.
    let output = service.stop();
    assert!(
        output.contains("platform database access") && output.contains("event-log-read"),
        "{output}"
    );
    assert!(
        !output.contains("canary-a"),
        "event data reached the log:\n{output}"
    );
}

const DEAD_LETTER_COUNT: &str = "SELECT count(*) FROM identity.outbox_dead_letter
    WHERE subscription = $1 AND event_id = $2::uuid";
