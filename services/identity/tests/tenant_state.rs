//! Platform administrators suspend and resume tenants, end to end (Laysi/trustiot-v2#776): the
//! real binary on the product's schema, applied by the migration Job (`testkit::product`), and
//! `GET`/`PUT /api/identity/v1/platform/tenants/{tenant_id}/state`.
//!
//! - A new tenant is active at version 1; every change of the state or the reason adds one and
//!   writes one `identity.tenant_state_changed` with the snapshot `{state, reason,
//!   state_version}`; the same state and reason again write nothing.
//! - Concurrent identical suspensions produce exactly one new version and one event.
//! - A terminated contract and a deleted tenant answer 409 to a change, and the version stays.
//! - Only a platform administrator's own request gets past 403, decided before anything is read,
//!   and a refusal changes nothing.
//! - The operation log records every change with the reason and the administrator as the actor.
//! - The service role's new grants stay in the transaction's tenant and on the four state
//!   columns, and the migration's trigger refuses a version that does not go up by exactly one
//!   and the resumption of a final state, whoever writes it.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use contract::identity::Actor as TokenActor;
use contract::{ServiceName, TenantGrant, TenantId, TenantRole, UserId};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::{Service, request, request_with_body, start_on_product};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const TENANTS: &str = "/api/identity/v1/tenants";
const EVENT_TYPE: &str = "identity.tenant_state_changed";

/// Generous bound for a loaded host (CLAUDE.md, "Test time limits"): the event log consumer's
/// delivery of committed events (it polls every second when idle).
const EVENT_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// How many identical suspensions race.
const RACERS: usize = 4;

/// SQLSTATE `insufficient_privilege`: a missing grant, a row-level security `WITH CHECK`, and
/// the migration's trigger.
const INSUFFICIENT_PRIVILEGE: &str = "42501";
/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)";

/// Every tenant's state columns, so a snapshot shows any write.
const STATES: &str = "SELECT tenant_id::text, state, suspension_reason, state_version,
    state_changed_at::text
FROM identity.tenants ORDER BY tenant_id";
const OUTBOX_COUNT: &str = "SELECT count(*) FROM identity.outbox_event";
/// The state events of one tenant, in order: sequence, envelope tenant, actor, payload.
const STATE_EVENTS: &str = "SELECT aggregate_seq, tenant_id::text,
    envelope->'actor'->>'user_id', (envelope->'payload')::text
FROM identity.outbox_event
WHERE event_type = 'identity.tenant_state_changed' AND aggregate_type = 'tenant'
    AND aggregate_id = $1
ORDER BY aggregate_seq";
/// The tenant's recorded state events.
const LOGGED_STATE_EVENTS: &str = "SELECT count(*) FROM identity.event_log
WHERE tenant_id = $1::uuid AND event_type = 'identity.tenant_state_changed'";
/// A tenant's state version.
const VERSION_OF: &str = "SELECT state_version FROM identity.tenants WHERE tenant_id = $1::uuid";
/// Deleting is Laysi/trustiot-v2#109's; the test makes a deleted tenant as a superuser, from
/// a terminated contract, the only state the trigger lets become `deleted`.
const TERMINATE_TENANT: &str = "UPDATE identity.tenants
SET state = 'suspended', suspension_reason = 'contract_terminated',
    state_version = state_version + 1
WHERE tenant_id = $1::uuid";
const DELETE_TENANT: &str = "UPDATE identity.tenants
SET state = 'deleted', state_version = state_version + 1
WHERE tenant_id = $1::uuid";
/// A time a superuser tries to write; the trigger keeps the tenant's own.
const FORGE_CHANGED_AT: &str = "UPDATE identity.tenants
SET state_changed_at = '2000-01-01T00:00:00Z'
WHERE tenant_id = $1::uuid";

/// A user in the database, and tokens for them.
struct Caller {
    id: UserId,
}

async fn caller(admin: &mut PgConnection, platform_admin: bool) -> Caller {
    let id = UserId::generate();
    sqlx::query(INSERT_USER)
        .bind(id.to_string())
        .bind(platform_admin)
        .execute(admin)
        .await
        .expect("insert the user");
    Caller { id }
}

/// A token of `user` for the identity service: a platform administrator or not, with a tenant
/// grant or not, exchanged by another service or not.
fn token(
    issuer: &TestIssuer,
    user: UserId,
    platform_admin: bool,
    tenant: Option<TenantGrant>,
    act: Option<ServiceName>,
) -> String {
    let mut claims = TestIssuer::claims(ServiceName::Identity, tenant);
    claims.sub = user;
    claims.platform_admin = platform_admin;
    if let Some(sub) = act {
        claims.act = Some(TokenActor { sub });
        claims.sid = None;
    }
    issuer.token(&claims)
}

fn state_path(tenant: &str) -> String {
    format!("/api/identity/v1/platform/tenants/{tenant}/state")
}

fn checked(response: support::Response) -> support::Response {
    // The handlers' answers; a 401 comes from the kit's identity extractor, before them.
    if response.status != 401 {
        assert_eq!(
            response.header_values("cache-control"),
            ["no-store"],
            "{}",
            response.raw()
        );
    }
    response
}

fn get_as(service: &Service, token: Option<&str>, tenant: &str) -> support::Response {
    let mut headers = Vec::new();
    if let Some(token) = token {
        headers.push(("x-internal-identity", token));
    }
    checked(
        request(service.address, "GET", &state_path(tenant), &headers).expect("call the state API"),
    )
}

fn put_as(
    service: &Service,
    token: Option<&str>,
    tenant: &str,
    content_type: Option<&str>,
    body: &str,
) -> support::Response {
    let mut headers = Vec::new();
    if let Some(content_type) = content_type {
        headers.push(("Content-Type", content_type));
    }
    if let Some(token) = token {
        headers.push(("x-internal-identity", token));
    }
    checked(
        request_with_body(
            service.address,
            "PUT",
            &state_path(tenant),
            &headers,
            body.as_bytes(),
        )
        .expect("call the state API"),
    )
}

fn put(service: &Service, token: &str, tenant: &str, body: &Value) -> support::Response {
    put_as(
        service,
        Some(token),
        tenant,
        Some("application/json"),
        &body.to_string(),
    )
}

fn create_tenant(service: &Service, token: &str, name: &str) -> (String, String) {
    let response = request_with_body(
        service.address,
        "POST",
        TENANTS,
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", token),
        ],
        json!({ "name": name }).to_string().as_bytes(),
    )
    .expect("call the tenant API");
    assert_eq!(response.status, 201, "{}", response.body);
    let body = response.json();
    (
        body["tenant_id"].as_str().expect("tenant_id").to_owned(),
        body["created_at"].as_str().expect("created_at").to_owned(),
    )
}

/// The `state`, `reason` and `state_version` of an answer.
fn summary(response: &support::Response) -> Value {
    assert_eq!(response.status, 200, "{}", response.body);
    let body = response.json();
    json!({
        "state": body["state"],
        "reason": body["reason"],
        "state_version": body["state_version"],
    })
}

fn snapshot_of(state: &str, reason: Option<&str>, version: i64) -> Value {
    json!({"state": state, "reason": reason, "state_version": version})
}

/// What a refused request must leave as it was.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    states: Vec<(String, String, Option<String>, i64, String)>,
    events: i64,
}

async fn snapshot(admin: &mut PgConnection) -> Snapshot {
    Snapshot {
        states: sqlx::query_as(STATES)
            .fetch_all(&mut *admin)
            .await
            .expect("states"),
        events: sqlx::query_scalar(OUTBOX_COUNT)
            .fetch_one(&mut *admin)
            .await
            .expect("events"),
    }
}

async fn state_events(
    admin: &mut PgConnection,
    tenant: &str,
) -> Vec<(i64, String, Option<String>, Value)> {
    let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(STATE_EVENTS)
        .bind(tenant)
        .fetch_all(admin)
        .await
        .expect("state events");
    rows.into_iter()
        .map(|(seq, tenant, actor, payload)| {
            let payload = serde_json::from_str(&payload).expect("payload JSON");
            (seq, tenant, actor, payload)
        })
        .collect()
}

async fn version_of(admin: &mut PgConnection, tenant: &str) -> i64 {
    sqlx::query_scalar(VERSION_OF)
        .bind(tenant)
        .fetch_one(admin)
        .await
        .expect("the tenant's version")
}

fn assert_refused(response: &support::Response, status: u16, code: &str, what: &str) {
    assert_eq!(response.status, status, "{what}: {}", response.body);
    assert_eq!(response.json()["code"], code, "{what}");
}

fn field_errors(response: &support::Response) -> Vec<(String, String)> {
    assert_eq!(response.status, 422, "{}", response.body);
    let body = response.json();
    assert_eq!(body["code"], "validation_failed");
    body["field_errors"]
        .as_array()
        .expect("field errors")
        .iter()
        .map(|error| {
            (
                error["field"].as_str().expect("field").to_owned(),
                error["message_key"].as_str().expect("key").to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn platform_administrators_suspend_and_resume_tenants_with_versioned_events() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = start_on_product(&product, &issuer, "tenant-state");
    let mut admin = product.superuser().await;

    let platform = caller(&mut admin, true).await;
    let platform_token = token(&issuer, platform.id, true, None, None);
    let (tenant_a, created_a) = create_tenant(&service, &platform_token, "State plant A");
    let (tenant_b, _) = create_tenant(&service, &platform_token, "State plant B");
    let (tenant_c, _) = create_tenant(&service, &platform_token, "State plant C");

    // 1. A new tenant is active at version 1, since it was created.
    let response = get_as(&service, Some(&platform_token), &tenant_a);
    assert_eq!(summary(&response), snapshot_of("active", None, 1));
    let body = response.json();
    assert_eq!(body["tenant_id"], tenant_a.as_str());
    let changed_at: chrono::DateTime<chrono::Utc> = body["state_changed_at"]
        .as_str()
        .expect("state_changed_at")
        .parse()
        .expect("a time");
    let created_at: chrono::DateTime<chrono::Utc> = created_a.parse().expect("a time");
    assert_eq!(changed_at, created_at, "active since it was created");

    // 2. Suspend, the same again (no write), change the reason, resume.
    let suspend = |reason: &str| json!({"state": "suspended", "reason": reason});
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &suspend("non_payment"),
    );
    assert_eq!(
        summary(&response),
        snapshot_of("suspended", Some("non_payment"), 2)
    );
    let suspended_at: chrono::DateTime<chrono::Utc> = response.json()["state_changed_at"]
        .as_str()
        .expect("state_changed_at")
        .parse()
        .expect("a time");
    assert!(
        suspended_at > created_at,
        "a change sets the time of the state: {suspended_at} after {created_at}"
    );
    let before = snapshot(&mut admin).await;
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &suspend("non_payment"),
    );
    assert_eq!(
        summary(&response),
        snapshot_of("suspended", Some("non_payment"), 2)
    );
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "the same state and reason write nothing"
    );
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &suspend("security_or_abuse"),
    );
    assert_eq!(
        summary(&response),
        snapshot_of("suspended", Some("security_or_abuse"), 3),
        "a new reason is a new version"
    );
    for resume in [
        json!({"state": "active"}),
        json!({"state": "active", "reason": null}),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &resume);
        assert_eq!(
            summary(&response),
            snapshot_of("active", None, 4),
            "{resume}"
        );
    }
    let response = get_as(&service, Some(&platform_token), &tenant_a);
    assert_eq!(summary(&response), snapshot_of("active", None, 4));

    // 3. Identical suspensions at once: all answer the same snapshot, one new version, one event.
    let address = service.address;
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (token, path) = (platform_token.clone(), state_path(&tenant_a));
            std::thread::spawn(move || {
                request_with_body(
                    address,
                    "PUT",
                    &path,
                    &[
                        ("Content-Type", "application/json"),
                        ("x-internal-identity", &token),
                    ],
                    json!({"state": "suspended", "reason": "non_payment"})
                        .to_string()
                        .as_bytes(),
                )
                .expect("call the state API")
            })
        })
        .collect();
    let answers: BTreeSet<String> = racers
        .into_iter()
        .map(|racer| {
            let response = racer.join().expect("the request thread");
            summary(&response).to_string()
        })
        .collect();
    assert_eq!(
        answers,
        BTreeSet::from([snapshot_of("suspended", Some("non_payment"), 5).to_string()]),
        "every racer saw the one new version"
    );
    assert_eq!(version_of(&mut admin, &tenant_a).await, 5);

    // 4. A terminated contract is final: 409 to every change, the version stays; the same
    //    termination again is no change.
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &suspend("contract_terminated"),
    );
    assert_eq!(
        summary(&response),
        snapshot_of("suspended", Some("contract_terminated"), 6)
    );
    let before = snapshot(&mut admin).await;
    for body in [
        json!({"state": "active"}),
        suspend("non_payment"),
        suspend("security_or_abuse"),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_refused(&response, 409, "conflict", &body.to_string());
        assert_eq!(response.json()["message_key"], "tenant.state_final");
    }
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &suspend("contract_terminated"),
    );
    assert_eq!(
        summary(&response),
        snapshot_of("suspended", Some("contract_terminated"), 6)
    );
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "nothing changed a terminated contract"
    );

    // 5. A deleted tenant is final too, whatever is asked.
    for (statement, what) in [
        (TERMINATE_TENANT, "terminate tenant B's contract"),
        (DELETE_TENANT, "delete tenant B"),
    ] {
        sqlx::query(statement)
            .bind(&tenant_b)
            .execute(&mut admin)
            .await
            .expect(what);
    }
    let response = get_as(&service, Some(&platform_token), &tenant_b);
    assert_eq!(
        summary(&response),
        snapshot_of("deleted", Some("contract_terminated"), 3)
    );
    let before = snapshot(&mut admin).await;
    for body in [
        json!({"state": "active"}),
        suspend("non_payment"),
        suspend("contract_terminated"),
    ] {
        let response = put(&service, &platform_token, &tenant_b, &body);
        assert_refused(&response, 409, "conflict", &body.to_string());
        assert_eq!(response.json()["message_key"], "tenant.state_final");
    }
    assert_eq!(snapshot(&mut admin).await, before, "a deleted tenant stays");

    // 6. One event per change, in the tenant's outbox, after `identity.tenant_created` (sequence
    //    1): the administrator as the actor and the snapshot with its version.
    let actor = Some(platform.id.to_string());
    let expected: Vec<(i64, String, Option<String>, Value)> = [
        ("suspended", Some("non_payment"), 2),
        ("suspended", Some("security_or_abuse"), 3),
        ("active", None, 4),
        ("suspended", Some("non_payment"), 5),
        ("suspended", Some("contract_terminated"), 6),
    ]
    .into_iter()
    .map(|(state, reason, version)| {
        (
            version,
            tenant_a.clone(),
            actor.clone(),
            snapshot_of(state, reason, version),
        )
    })
    .collect();
    assert_eq!(state_events(&mut admin, &tenant_a).await, expected);
    assert!(
        state_events(&mut admin, &tenant_b).await.is_empty(),
        "the owner's deletion wrote no event; the refusals neither"
    );

    // 7. Refusals: 403 for everyone but a platform administrator's own request, before anything
    //    is read. A nonexistent tenant and a body that is not even JSON show the order.
    let before = snapshot(&mut admin).await;
    let member = caller(&mut admin, false).await;
    let grant = |tenant: &str, role| TenantGrant {
        id: tenant.parse().expect("tenant id"),
        role,
    };
    let refused_tokens = [
        (
            "tenant C's administrator",
            token(
                &issuer,
                member.id,
                false,
                Some(grant(&tenant_c, TenantRole::Admin)),
                None,
            ),
        ),
        (
            "tenant A's viewer",
            token(
                &issuer,
                member.id,
                false,
                Some(grant(&tenant_a, TenantRole::Viewer)),
                None,
            ),
        ),
        (
            "a user without a tenant",
            token(&issuer, member.id, false, None, None),
        ),
        (
            "a platform administrator's token exchanged by the assistant",
            token(
                &issuer,
                platform.id,
                true,
                None,
                Some(ServiceName::Assistant),
            ),
        ),
        (
            "a platform administrator's token exchanged by assets",
            token(&issuer, platform.id, true, None, Some(ServiceName::Assets)),
        ),
    ];
    let unknown_tenant = TenantId::generate().to_string();
    let resume = json!({"state": "active"}).to_string();
    let oversized = json!({
        "state": "active",
        "padding": "a".repeat(identity::tenant_state::MAX_BODY_BYTES),
    })
    .to_string();
    for (who, token) in &refused_tokens {
        for tenant in [&tenant_a, &tenant_c, &unknown_tenant] {
            let response = get_as(&service, Some(token), tenant);
            assert_refused(&response, 403, "permission_denied", &format!("GET {who}"));
        }
        for (tenant, content_type, body) in [
            (tenant_a.as_str(), Some("application/json"), resume.as_str()),
            (tenant_c.as_str(), Some("application/json"), resume.as_str()),
            (
                unknown_tenant.as_str(),
                Some("application/json"),
                resume.as_str(),
            ),
            ("not-a-uuid", Some("text/plain"), "not json"),
            (
                tenant_a.as_str(),
                Some("application/json"),
                oversized.as_str(),
            ),
        ] {
            let response = put_as(&service, Some(token), tenant, content_type, body);
            assert_refused(
                &response,
                403,
                "permission_denied",
                &format!("PUT {who} on {tenant}"),
            );
        }
    }
    let response = get_as(&service, None, &tenant_a);
    assert_refused(&response, 401, "unauthenticated", "GET without a token");
    let response = put_as(&service, None, &tenant_a, Some("application/json"), &resume);
    assert_refused(&response, 401, "unauthenticated", "PUT without a token");
    let mut for_assets = TestIssuer::claims(ServiceName::Assets, None);
    for_assets.sub = platform.id;
    for_assets.platform_admin = true;
    let for_assets = issuer.token(&for_assets);
    let response = put_as(
        &service,
        Some(&for_assets),
        &tenant_c,
        Some("application/json"),
        &resume,
    );
    assert_refused(&response, 401, "unauthenticated", "a token for assets");
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "a refusal changes nothing"
    );

    // 8. Malformed requests and unknown tenants: nothing written.
    let response = get_as(&service, Some(&platform_token), &unknown_tenant);
    assert_refused(&response, 404, "not_found", "GET an unknown tenant");
    let response = put(
        &service,
        &platform_token,
        &unknown_tenant,
        &suspend("non_payment"),
    );
    assert_refused(&response, 404, "not_found", "PUT an unknown tenant");
    let response = get_as(&service, Some(&platform_token), "not-a-uuid");
    assert_refused(
        &response,
        400,
        "invalid_request",
        "GET a tenant ID that is not a UUID",
    );
    let response = put(
        &service,
        &platform_token,
        "not-a-uuid",
        &suspend("non_payment"),
    );
    assert_refused(
        &response,
        400,
        "invalid_request",
        "PUT a tenant ID that is not a UUID",
    );
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
    ] {
        let response = put_as(
            &service,
            Some(&platform_token),
            &tenant_c,
            content_type,
            &resume,
        );
        assert_refused(
            &response,
            400,
            "invalid_request",
            &format!("{content_type:?}"),
        );
    }
    for body in [
        json!("suspended"),
        json!({"reason": "non_payment"}),
        json!({"state": 1}),
        json!({"state": "suspended", "reason": 2}),
    ] {
        let response = put(&service, &platform_token, &tenant_c, &body);
        assert_refused(&response, 400, "invalid_request", &body.to_string());
    }
    let response = put_as(
        &service,
        Some(&platform_token),
        &tenant_c,
        Some("application/json"),
        &oversized,
    );
    assert_refused(&response, 400, "invalid_request", "a body over the limit");
    let state = ("/state".to_owned(), "tenant.state_invalid".to_owned());
    let reason = (
        "/reason".to_owned(),
        "tenant.suspension_reason_invalid".to_owned(),
    );
    for (body, expected) in [
        (json!({"state": "suspended"}), vec![reason.clone()]),
        (
            json!({"state": "suspended", "reason": null}),
            vec![reason.clone()],
        ),
        (
            json!({"state": "suspended", "reason": "boredom"}),
            vec![reason.clone()],
        ),
        (
            json!({"state": "active", "reason": "non_payment"}),
            vec![reason.clone()],
        ),
        (json!({"state": "deleted"}), vec![state.clone()]),
        (
            json!({"state": "deleted", "reason": "contract_terminated"}),
            vec![state.clone()],
        ),
        (
            json!({"state": "unrecognized", "reason": "boredom"}),
            vec![state.clone(), reason.clone()],
        ),
    ] {
        let response = put(&service, &platform_token, &tenant_c, &body);
        assert_eq!(field_errors(&response), expected, "{body}");
    }
    assert_eq!(snapshot(&mut admin).await, before, "nothing was written");

    // 9. The operation log, read by the back office, has every change of tenant A with the
    //    reason and the administrator as the actor.
    let deadline = Instant::now() + EVENT_LIMIT;
    loop {
        let logged: i64 = sqlx::query_scalar(LOGGED_STATE_EVENTS)
            .bind(&tenant_a)
            .fetch_one(&mut admin)
            .await
            .expect("count the logged events");
        if logged == 5 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the event log recorded {logged} of tenant A's 5 state changes within {EVENT_LIMIT:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
    let response = request(
        service.address,
        "GET",
        &format!("/api/identity/v1/platform/tenants/{tenant_a}/event-log"),
        &[("x-internal-identity", &platform_token)],
    )
    .expect("read the operation log");
    assert_eq!(response.status, 200, "{}", response.body);
    let logged: Vec<Value> = response.json()["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["type"] == EVENT_TYPE)
        .map(|event| {
            json!({
                "tenant_id": event["tenant_id"],
                "producer": event["producer"],
                "aggregate": [event["aggregate_type"], event["aggregate_id"]],
                "actor": event["actor"],
                "payload": event["payload"],
            })
        })
        .collect();
    let actor = json!({"kind": "user", "user_id": platform.id.to_string()});
    let expected_log: Vec<Value> = expected
        .iter()
        .rev() // newest first
        .map(|(_, _, _, payload)| {
            json!({
                "tenant_id": tenant_a, "producer": "identity",
                "aggregate": ["tenant", tenant_a], "actor": actor, "payload": payload,
            })
        })
        .collect();
    assert_eq!(logged, expected_log);

    // 10. The database's own rules: no writer, a superuser included, chooses the time of a state;
    //     then the service role's grants and the trigger.
    for tenant in [&tenant_a, &tenant_c] {
        sqlx::query(FORGE_CHANGED_AT)
            .bind(tenant)
            .execute(&mut admin)
            .await
            .expect("the superuser's update runs");
    }
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "the trigger kept each tenant's time of its state"
    );
    grants_and_trigger(&product, &tenant_a, &tenant_c).await;
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "the probes were rolled back"
    );

    // 11. The log names who changed which tenant, and the refusals.
    let output = service.stop();
    assert!(output.contains("changed a tenant's state"), "{output}");
    assert!(
        output.contains("refused to change a tenant's state"),
        "{output}"
    );
    assert!(
        output.contains("refused to read a tenant's state"),
        "{output}"
    );
}

/// Statements of the service role in transactions of `tenant_c` (active) and `tenant_a` (its
/// contract terminated), through the kit's `BackgroundTenantDb` as the store opens it; each
/// statement in its own transaction, rolled back.
async fn grants_and_trigger(product: &ProductPostgres, tenant_a: &str, tenant_c: &str) {
    let (_requests, db): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("a verified connection");
    let in_c: TenantId = tenant_c.parse().expect("tenant id");
    let in_a: TenantId = tenant_a.parse().expect("tenant id");
    let run = |tenant: TenantId, statement: &'static str, args: &'static [&'static str]| {
        let db = db.clone();
        async move {
            let mut tx = db
                .begin(tenant, "test-tenant-state")
                .await
                .expect("a transaction");
            let mut query = sqlx::query(statement).bind(tenant.to_string());
            for arg in args {
                query = query.bind(*arg);
            }
            let result = query
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

    // Allowed: what the API does.
    let allowed: [(TenantId, &'static str, &'static [&'static str]); 2] = [
        (
            in_c,
            "UPDATE identity.tenants SET state = 'suspended', suspension_reason = 'non_payment',
                state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
            &[],
        ),
        (
            in_c,
            "SELECT state FROM identity.tenants WHERE tenant_id = $1::uuid FOR NO KEY UPDATE",
            &[],
        ),
    ];
    for (tenant, statement, args) in allowed {
        run(tenant, statement, args)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }

    // Refused by the grants: other columns (the time of the state is the trigger's), inserting a
    // chosen state, deleting.
    let not_granted: [&'static str; 7] = [
        "UPDATE identity.tenants SET name = 'Renamed' WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET state_changed_at = now() WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET created_at = now() WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET tenant_id = tenant_id WHERE tenant_id = $1::uuid",
        "INSERT INTO identity.tenants (tenant_id, name, state, suspension_reason)
            VALUES ($1::uuid, 'Forged', 'suspended', 'non_payment')",
        "INSERT INTO identity.tenants (tenant_id, name, state_version)
            VALUES ($1::uuid, 'Forged', 99)",
        "DELETE FROM identity.tenants WHERE tenant_id = $1::uuid",
    ];
    for statement in not_granted {
        let error = run(in_c, statement, &[]).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }

    // Refused by the trigger: a version that does not go up by exactly one with a change, a
    // version change without one, leaving a terminated contract, and deleting a tenant whose
    // contract was not terminated.
    let trigger: [(TenantId, &'static str); 7] = [
        // An active tenant deleted, with a correct version and the only reason a deleted tenant
        // may have.
        (
            in_c,
            "UPDATE identity.tenants SET state = 'deleted',
                suspension_reason = 'contract_terminated', state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
        // A change with the same version.
        (
            in_c,
            "UPDATE identity.tenants SET state = 'suspended', suspension_reason = 'non_payment'
            WHERE tenant_id = $1::uuid",
        ),
        // A change that skips a version.
        (
            in_c,
            "UPDATE identity.tenants SET state = 'suspended', suspension_reason = 'non_payment',
                state_version = state_version + 2
            WHERE tenant_id = $1::uuid",
        ),
        // A new version without a change.
        (
            in_c,
            "UPDATE identity.tenants SET state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
        // An older version (tenant A is at 6).
        (
            in_a,
            "UPDATE identity.tenants SET state_version = 5 WHERE tenant_id = $1::uuid",
        ),
        // Leaving a terminated contract, with a correct version.
        (
            in_a,
            "UPDATE identity.tenants SET state = 'active', suspension_reason = NULL,
                state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
        (
            in_a,
            "UPDATE identity.tenants SET suspension_reason = 'non_payment',
                state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
    ];
    for (tenant, statement) in trigger {
        let error = run(tenant, statement, &[]).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }
    // The table's CHECK: a suspension without a reason, and a terminated contract deleted without
    // its reason.
    for (tenant, statement) in [
        (
            in_c,
            "UPDATE identity.tenants SET state = 'suspended', state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
        (
            in_a,
            "UPDATE identity.tenants SET state = 'deleted', suspension_reason = NULL,
                state_version = state_version + 1
            WHERE tenant_id = $1::uuid",
        ),
    ] {
        let error = run(tenant, statement, &[]).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(CHECK_VIOLATION),
            "{statement}"
        );
    }
    // What Laysi/trustiot-v2#109 will do: a terminated contract becomes deleted.
    run(
        in_a,
        "UPDATE identity.tenants SET state = 'deleted', state_version = state_version + 1
        WHERE tenant_id = $1::uuid",
        &[],
    )
    .await
    .expect("a terminated contract is deleted");

    // Another tenant's row is invisible, so not changed.
    let mut tx = db
        .begin(in_c, "test-tenant-state")
        .await
        .expect("a transaction");
    let changed = sqlx::query(
        "UPDATE identity.tenants SET state = 'suspended', suspension_reason = 'non_payment',
            state_version = state_version + 1
        WHERE tenant_id = $1::uuid",
    )
    .bind(tenant_a)
    .execute(&mut *tx)
    .await
    .expect("the update runs")
    .rows_affected();
    assert_eq!(changed, 0, "another tenant's state was changed");
    tx.rollback().await.expect("roll back");
}
