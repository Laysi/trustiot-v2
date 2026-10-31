//! A suspension is "processing" until the certs service confirms it applied it, end to end
//! (Laysi/trustiot-v2#779): the real binary on the product's schema, applied by the migration Job
//! (`testkit::product`), with `certs_app` writing `certs.tenant_state_applied` to certs' outbox
//! through `outbox::publish_as`, as the certs service will (Laysi/trustiot-v2#108), standing in
//! for the real producer.
//!
//! - A suspension reads `processing` until a confirmation of its version or a newer one arrives,
//!   then `suspended`.
//! - An older confirmation (late, replayed, out of order) never marks a newer suspension as
//!   suspended, and never takes back a newer confirmation.
//! - A resumption reads `active` at once, confirmed or not.
//! - Confirmations without a tenant's state behind them (a version the identity service never
//!   wrote, below 1, an unknown tenant) are dead-lettered and change nothing, also when replayed
//!   after the tenant reached that version; other certs events are skipped.
//! - The migration's trigger and CHECK hold the same rules for every writer, and the service
//!   role's new grant stays in the transaction's tenant.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::time::{Duration, Instant};

use contract::event::Actor;
use contract::events::certs::{TENANT_STATE_AGGREGATE, TenantStateApplied};
use contract::{EventId, ServiceName, TenantId, UserId};
use identity::state_applied::SUBSCRIPTION;
use outbox::{Aggregate, Position, Replay};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use support::{Service, request, request_with_body, start_on_product};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// Generous bound for a loaded host (CLAUDE.md, "Test time limits"): the consumer's handling of
/// a committed confirmation (it polls every second when idle, and backs off after a failure).
const EVENT_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// SQLSTATE `insufficient_privilege`: a missing grant and the migration's trigger.
const INSUFFICIENT_PRIVILEGE: &str = "42501";
/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, true)";
/// The confirmed version the consumer stored for a tenant.
const APPLIED_OF: &str =
    "SELECT certs_applied_version FROM identity.tenants WHERE tenant_id = $1::uuid";
/// Whether the confirmation subscription handled an event: processed, or dead-lettered.
const HANDLED: &str = "SELECT
    (SELECT count(*) FROM identity.outbox_inbox WHERE subscription = $1 AND event_id = $2::uuid)
    + (SELECT count(*) FROM identity.outbox_dead_letter
       WHERE subscription = $1 AND event_id = $2::uuid)";
const DEAD_LETTER: &str = "SELECT reason FROM identity.outbox_dead_letter
WHERE subscription = $1 AND event_id = $2::uuid";
const DEAD_LETTER_EVENT: &str = "SELECT d.event_id::text FROM identity.outbox_dead_letter d
JOIN certs.outbox_event e ON e.event_id = d.event_id
WHERE d.subscription = $1 AND e.tenant_id = $2::uuid
    AND (e.envelope -> 'payload' ->> 'state_version')::bigint = $3";
/// A platform-wide registration written to certs' outbox by hand, as the certs service writes
/// it: `$1` event ID, `$2` the envelope. The confirmation subscription must skip it.
const INSERT_REGISTRATION: &str = "INSERT INTO certs.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq,
     envelope)
VALUES (pg_current_xact_id()::text::bigint, $1::uuid, 'certs.gateway_registered', NULL,
        'gateway_registration', $1::text, 1, $2::jsonb)";
/// The confirmations the event log recorded for a tenant, with their actor.
const LOGGED_CONFIRMATIONS: &str = "SELECT count(*) FROM identity.event_log
WHERE tenant_id = $1::uuid AND event_type = 'certs.tenant_state_applied'
    AND producer = 'certs' AND actor_service = 'certs'";

fn token(issuer: &TestIssuer, user: UserId) -> String {
    let mut claims = TestIssuer::claims(ServiceName::Identity, None);
    claims.sub = user;
    claims.platform_admin = true;
    issuer.token(&claims)
}

fn state_path(tenant: &str) -> String {
    format!("/api/identity/v1/platform/tenants/{tenant}/state")
}

fn create_tenant(service: &Service, token: &str, name: &str) -> String {
    let response = request_with_body(
        service.address,
        "POST",
        "/api/identity/v1/tenants",
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", token),
        ],
        json!({ "name": name }).to_string().as_bytes(),
    )
    .expect("call the tenant API");
    assert_eq!(response.status, 201, "{}", response.body);
    response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned()
}

/// `(state, state_version, status)` of an answer.
fn summary(response: &support::Response) -> (String, i64, String) {
    assert_eq!(response.status, 200, "{}", response.body);
    let body = response.json();
    (
        body["state"].as_str().expect("state").to_owned(),
        body["state_version"].as_i64().expect("state_version"),
        body["status"].as_str().expect("status").to_owned(),
    )
}

fn expect(state: &str, version: i64, status: &str) -> (String, i64, String) {
    (state.to_owned(), version, status.to_owned())
}

fn read(service: &Service, token: &str, tenant: &str) -> (String, i64, String) {
    let response = request(
        service.address,
        "GET",
        &state_path(tenant),
        &[("x-internal-identity", token)],
    )
    .expect("call the state API");
    summary(&response)
}

fn set(service: &Service, token: &str, tenant: &str, body: &Value) -> (String, i64, String) {
    let response = request_with_body(
        service.address,
        "PUT",
        &state_path(tenant),
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", token),
        ],
        body.to_string().as_bytes(),
    )
    .expect("call the state API");
    summary(&response)
}

fn suspend(reason: &str) -> Value {
    json!({"state": "suspended", "reason": reason})
}

fn resume() -> Value {
    json!({"state": "active"})
}

/// Writes `certs.tenant_state_applied` with `version` for `tenant` as `certs_app`, in a
/// transaction of that tenant, as the certs service will; returns the event ID.
async fn confirm(certs: &BackgroundTenantDb, tenant: &str, version: i64) -> String {
    let tenant: TenantId = tenant.parse().expect("tenant id");
    let mut tx = certs
        .begin(tenant, "test-confirm-tenant-state")
        .await
        .expect("a transaction of certs_app");
    let aggregate =
        Aggregate::new(TENANT_STATE_AGGREGATE, tenant.to_string()).expect("the aggregate");
    let published = outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &TenantStateApplied {
            state_version: version,
        },
        Some(tenant),
        Actor::Service {
            service: ServiceName::Certs,
        },
    )
    .await
    .expect("write the confirmation to certs' outbox");
    tx.commit().await.expect("commit the confirmation");
    published.event_id.to_string()
}

/// Waits until the confirmation subscription processed or dead-lettered `event`.
async fn wait_handled(admin: &mut PgConnection, event: &str, what: &str) {
    let deadline = Instant::now() + EVENT_LIMIT;
    loop {
        let handled: i64 = sqlx::query_scalar(HANDLED)
            .bind(SUBSCRIPTION)
            .bind(event)
            .fetch_one(&mut *admin)
            .await
            .expect("whether the event was handled");
        if handled > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the confirmation subscription did not handle {what} within {EVENT_LIMIT:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

async fn dead_letter(admin: &mut PgConnection, event: &str) -> Option<String> {
    sqlx::query_scalar(DEAD_LETTER)
        .bind(SUBSCRIPTION)
        .bind(event)
        .fetch_optional(admin)
        .await
        .expect("the dead letter")
}

/// The ID of the dead-lettered confirmation of `version` for `tenant`.
async fn dead_letter_event(admin: &mut PgConnection, tenant: &str, version: i64) -> String {
    sqlx::query_scalar(DEAD_LETTER_EVENT)
        .bind(SUBSCRIPTION)
        .bind(tenant)
        .bind(version)
        .fetch_one(admin)
        .await
        .expect("the dead-lettered confirmation")
}

async fn applied_of(admin: &mut PgConnection, tenant: &str) -> i64 {
    sqlx::query_scalar(APPLIED_OF)
        .bind(tenant)
        .fetch_one(admin)
        .await
        .expect("the confirmed version")
}

/// Confirms `version` for `tenant`, waits until it was handled, and checks it was not
/// dead-lettered.
async fn confirm_and_wait(
    certs: &BackgroundTenantDb,
    admin: &mut PgConnection,
    tenant: &str,
    version: i64,
) {
    let event = confirm(certs, tenant, version).await;
    wait_handled(
        admin,
        &event,
        &format!("the confirmation of version {version}"),
    )
    .await;
    assert_eq!(
        dead_letter(admin, &event).await,
        None,
        "the confirmation of version {version} was dead-lettered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suspension_is_processing_until_certs_confirms_its_version() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform", "certs_app"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = start_on_product(&product, &issuer, "state-applied");
    let mut admin = product.superuser().await;
    let (_requests, certs): (TenantDb, BackgroundTenantDb) =
        TenantDb::connect_lazy_with_background(
            product.postgres.options("certs_app", DATABASE),
            PgPoolOptions::new().max_connections(1),
            ServiceIdentity::new(ServiceName::Certs),
        )
        .expect("a verified connection as certs_app");

    let platform = UserId::generate();
    sqlx::query(INSERT_USER)
        .bind(platform.to_string())
        .execute(&mut admin)
        .await
        .expect("insert the platform administrator");
    let token = token(&issuer, platform);
    let tenant_a = create_tenant(&service, &token, "Applied plant A");
    let tenant_b = create_tenant(&service, &token, "Applied plant B");
    let tenant_c = create_tenant(&service, &token, "Applied plant C");

    // 1. A new tenant is active; a suspension is processing at once, in the answer and after.
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("active", 1, "active")
    );
    assert_eq!(
        set(&service, &token, &tenant_a, &suspend("non_payment")),
        expect("suspended", 2, "processing")
    );
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 2, "processing")
    );

    // 2. An older confirmation (version 1, the creation) does not end it.
    confirm_and_wait(&certs, &mut admin, &tenant_a, 1).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 2, "processing"),
        "a confirmation of version 1 marked version 2 suspended"
    );

    // 3. The confirmation of its own version does.
    confirm_and_wait(&certs, &mut admin, &tenant_a, 2).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 2, "suspended")
    );
    assert_eq!(applied_of(&mut admin, &tenant_a).await, 2);

    // 4. A new reason is a new version: processing again until it is confirmed; a late or
    //    replayed confirmation of the previous version does not end it.
    assert_eq!(
        set(&service, &token, &tenant_a, &suspend("security_or_abuse")),
        expect("suspended", 3, "processing")
    );
    confirm_and_wait(&certs, &mut admin, &tenant_a, 2).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 3, "processing"),
        "a replayed confirmation of version 2 marked version 3 suspended"
    );
    confirm_and_wait(&certs, &mut admin, &tenant_a, 3).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 3, "suspended")
    );

    // 5. A resumption is active at once, before any confirmation.
    assert_eq!(
        set(&service, &token, &tenant_a, &resume()),
        expect("active", 4, "active")
    );
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("active", 4, "active")
    );

    // 6. Suspended again: the resumption's confirmation (an older version) does not end it, and a
    //    confirmation that skips versions covers the older ones.
    assert_eq!(
        set(&service, &token, &tenant_a, &suspend("non_payment")),
        expect("suspended", 5, "processing")
    );
    confirm_and_wait(&certs, &mut admin, &tenant_a, 4).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 5, "processing"),
        "the resumption's confirmation marked the next suspension suspended"
    );
    confirm_and_wait(&certs, &mut admin, &tenant_a, 5).await;
    assert_eq!(
        read(&service, &token, &tenant_a),
        expect("suspended", 5, "suspended")
    );

    // 7. Out of order: tenant B is suspended, resumed and suspended again (versions 2 to 4); the
    //    newest confirmation arrives first, and the older ones after it change nothing.
    set(&service, &token, &tenant_b, &suspend("non_payment"));
    set(&service, &token, &tenant_b, &resume());
    assert_eq!(
        set(&service, &token, &tenant_b, &suspend("security_or_abuse")),
        expect("suspended", 4, "processing")
    );
    for version in [4, 2, 3] {
        confirm_and_wait(&certs, &mut admin, &tenant_b, version).await;
        assert_eq!(
            read(&service, &token, &tenant_b),
            expect("suspended", 4, "suspended"),
            "after the confirmation of version {version}"
        );
    }
    assert_eq!(
        applied_of(&mut admin, &tenant_b).await,
        4,
        "an older confirmation took back a newer one"
    );

    // 8. Confirmations no state stands behind are dead-lettered and change nothing: a version
    //    tenant C never had (it is at 2), a version below 1, and a tenant identity does not have.
    assert_eq!(
        set(&service, &token, &tenant_c, &suspend("non_payment")),
        expect("suspended", 2, "processing")
    );
    let unknown = TenantId::generate().to_string();
    for (tenant, version, reason) in [
        (
            &tenant_c,
            7,
            "the confirmed state version is newer than the tenant's",
        ),
        (&tenant_c, 0, "the confirmed state version is out of range"),
        (
            &unknown,
            2,
            "the confirmation names a tenant the identity service does not have",
        ),
    ] {
        let event = confirm(&certs, tenant, version).await;
        wait_handled(
            &mut admin,
            &event,
            &format!("version {version} of {tenant}"),
        )
        .await;
        assert_eq!(
            dead_letter(&mut admin, &event).await.as_deref(),
            Some(reason),
            "version {version} of {tenant}"
        );
    }
    assert_eq!(
        read(&service, &token, &tenant_c),
        expect("suspended", 2, "processing"),
        "a confirmation ahead of the tenant's version marked it suspended"
    );
    assert_eq!(applied_of(&mut admin, &tenant_c).await, 0);
    // Suspended later up to that version, it still waits for certs.
    for body in [resume(), suspend("non_payment"), resume()] {
        set(&service, &token, &tenant_c, &body);
    }
    for (reason, version) in [("security_or_abuse", 6), ("non_payment", 7)] {
        assert_eq!(
            set(&service, &token, &tenant_c, &suspend(reason)),
            expect("suspended", version, "processing")
        );
    }
    // Replaying the dead letters (crates/outbox/README.md) does not accept the early
    // confirmation of version 7 either: certs wrote it before the tenant entered version 7. The
    // consumer handles in log order, so once a later confirmation (of version 6, older) is
    // handled, the replayed ones were too.
    let ahead = dead_letter_event(&mut admin, &tenant_c, 7).await;
    let mut as_app = product.postgres.connect("identity_app", DATABASE).await;
    let mut tx = as_app.begin().await.expect("begin");
    outbox::replay_from(
        &mut tx,
        SUBSCRIPTION,
        Position::START,
        Replay::SkipProcessed,
    )
    .await
    .expect("replay the confirmation subscription");
    tx.commit().await.expect("commit the replay");
    confirm_and_wait(&certs, &mut admin, &tenant_c, 6).await;
    assert_eq!(
        dead_letter(&mut admin, &ahead).await.as_deref(),
        Some("the confirmation was written before the tenant's state version"),
        "the replayed early confirmation of version 7"
    );
    assert_eq!(
        read(&service, &token, &tenant_c),
        expect("suspended", 7, "processing"),
        "a replayed early confirmation marked version 7 suspended"
    );
    assert_eq!(applied_of(&mut admin, &tenant_c).await, 6);
    // certs' own confirmation of version 7, written after it, does.
    confirm_and_wait(&certs, &mut admin, &tenant_c, 7).await;
    assert_eq!(
        read(&service, &token, &tenant_c),
        expect("suspended", 7, "suspended")
    );

    // 9. Other certs events are not confirmations: a platform-wide registration is skipped (no
    //    tenant transaction, no dead letter).
    let registration = EventId::generate().to_string();
    let envelope = json!({
        "id": registration,
        "type": "certs.gateway_registered",
        "version": 1,
        "tenant_id": null,
        "occurred_at": "2026-10-30T09:00:00.000001Z",
        "producer": "certs",
        "payload": {"hardware_id": "gw-0779"},
        "actor": {"kind": "user", "user_id": platform.to_string()},
    });
    sqlx::query(INSERT_REGISTRATION)
        .bind(&registration)
        .bind(envelope.to_string())
        .execute(&mut admin)
        .await
        .expect("write a registration to certs' outbox");
    wait_handled(&mut admin, &registration, "a registration").await;
    assert_eq!(dead_letter(&mut admin, &registration).await, None);

    // 10. The operation log records certs' confirmations of tenant A (versions 1, 2, 2, 3, 4 and
    //     5) like any other certs event, with certs as the actor.
    let deadline = Instant::now() + EVENT_LIMIT;
    loop {
        let logged: i64 = sqlx::query_scalar(LOGGED_CONFIRMATIONS)
            .bind(&tenant_a)
            .fetch_one(&mut admin)
            .await
            .expect("count the logged confirmations");
        if logged == 6 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the event log recorded {logged} of tenant A's 6 confirmations within {EVENT_LIMIT:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }

    // 11. The database's own rules, for the service role and a superuser.
    database_rules(&product, &mut admin, &tenant_a, &tenant_b).await;

    // 12. The log names what was stored and what was refused.
    let output = service.stop();
    assert!(
        output.contains("certs applied a tenant's state"),
        "{output}"
    );
    assert!(
        output.contains("certs confirmed a tenant state version the identity service never wrote"),
        "{output}"
    );
    assert!(
        output.contains("certs confirmed the state of a tenant this service does not have"),
        "{output}"
    );
    assert!(
        output.contains(
            "certs confirmed a tenant state version before the identity service wrote it"
        ),
        "{output}"
    );
}

/// The migration's trigger and CHECK, and the service role's new grant: `tenant_a` is at version
/// 5 with 5 confirmed, `tenant_b` at 4 with 4 confirmed. Each statement of the service role runs
/// in its own transaction of the tenant, rolled back.
async fn database_rules(
    product: &ProductPostgres,
    admin: &mut PgConnection,
    tenant_a: &str,
    tenant_b: &str,
) {
    let (_requests, db): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("a verified connection");
    let in_a: TenantId = tenant_a.parse().expect("tenant id");
    let run = |statement: &'static str| {
        let db = db.clone();
        async move {
            let mut tx = db
                .begin(in_a, "test-state-applied")
                .await
                .expect("a transaction");
            let result = sqlx::query(statement)
                .bind(in_a.to_string())
                .execute(&mut *tx)
                .await
                .map(|done| done.rows_affected());
            tx.rollback().await.expect("roll back");
            result
        }
    };
    let sqlstate = |error: sqlx::Error| {
        error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned())
    };

    // Allowed: the same version again (no change) is not refused; the consumer never writes it.
    run(
        "UPDATE identity.tenants SET certs_applied_version = certs_applied_version
        WHERE tenant_id = $1::uuid",
    )
    .await
    .expect("an unchanged confirmed version");
    // Refused by the trigger: going back.
    for statement in [
        "UPDATE identity.tenants SET certs_applied_version = certs_applied_version - 1
        WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET certs_applied_version = 0 WHERE tenant_id = $1::uuid",
    ] {
        let error = run(statement).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }
    // Refused by the CHECK: ahead of the tenant's version.
    let statement = "UPDATE identity.tenants SET certs_applied_version = state_version + 1
        WHERE tenant_id = $1::uuid";
    let error = run(statement).await.expect_err(statement);
    assert_eq!(
        sqlstate(error).as_deref(),
        Some(CHECK_VIOLATION),
        "{statement}"
    );

    // Row-level security: tenant B's row is invisible in tenant A's transaction, so unchanged.
    let mut tx = db
        .begin(in_a, "test-state-applied")
        .await
        .expect("a transaction");
    let changed = sqlx::query(
        "UPDATE identity.tenants SET certs_applied_version = 1 WHERE tenant_id = $1::uuid",
    )
    .bind(tenant_b)
    .execute(&mut *tx)
    .await
    .expect("the update runs")
    .rows_affected();
    assert_eq!(changed, 0, "another tenant's confirmed version was changed");
    tx.rollback().await.expect("roll back");

    // The trigger binds the superuser too.
    let error = sqlx::query(
        "UPDATE identity.tenants SET certs_applied_version = 1 WHERE tenant_id = $1::uuid",
    )
    .bind(tenant_a)
    .execute(&mut *admin)
    .await
    .expect_err("a superuser takes a confirmation back");
    assert_eq!(sqlstate(error).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    assert_eq!(applied_of(admin, tenant_a).await, 5);
    assert_eq!(applied_of(admin, tenant_b).await, 4);
}
