//! Tenants' plans and entitlements end to end (Laysi/trustiot-v2#111): the real binary on the
//! product's schema, applied by the migration Job (`testkit::product`),
//! `GET`/`PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements` and
//! `GET /api/identity/v1/entitlements`.
//!
//! - A new tenant is on Starter at version 1 (its event is checked by tests/tenants.rs), whatever
//!   `TRUSTIOT_IDENTITY_BACKFILL_PLAN` says. A tenant without a snapshot gets its first at the
//!   service's start, with an event whose actor is the service: Enterprise (the default) or the
//!   setting's plan when it existed before the migration added entitlements, Starter when an older
//!   replica created it during the rollout. Until then the APIs show the plan it will get. The
//!   migration's own statements mark the rows that exist when it runs, and only those.
//! - Every change of the plan, the overrides or the snapshot adds one to the version and writes
//!   one `identity.tenant_entitlements_changed` with the whole snapshot; the same request again
//!   writes nothing. A change sent with a version the tenant has moved past is refused with 409
//!   and writes nothing, so concurrent identical requests make exactly one new version.
//! - An Enterprise override takes precedence over the plan's value; other plans take none.
//! - A downgrade below what the tenant has blocks no write and deletes nothing.
//! - Members read their own tenant's entitlements, never another's; only a platform
//!   administrator's own request reads or sets a tenant's plan, and a refusal changes nothing.
//! - The service role's new grants stay in the transaction's tenant and on the four columns, and
//!   the migration's trigger and checks refuse a version that does not go up by exactly one,
//!   whoever writes it.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary shares one product database.

mod support;

use std::time::{Duration, Instant};

use contract::identity::Actor as TokenActor;
use contract::{ServiceName, TenantGrant, TenantId, TenantRole, UserId};
use identity::entitlements::Plan;
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use support::{Service, request, request_with_body, start_on_product, start_on_product_with};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const TENANTS: &str = "/api/identity/v1/tenants";
const OWN: &str = "/api/identity/v1/entitlements";

/// Generous bound for a loaded host (CLAUDE.md, "Test time limits"): the backfill pass the
/// service runs right after it starts.
const BACKFILL_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

/// How many identical requests race.
const RACERS: usize = 4;

/// SQLSTATE `insufficient_privilege`: a missing grant and the migration's trigger.
const INSUFFICIENT_PRIVILEGE: &str = "42501";
/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

const INSERT_USER: &str =
    "INSERT INTO identity.users (id, platform_admin, email) VALUES ($1::uuid, $2, $3)";
/// A tenant as a replica without entitlements wrote it: the two columns only.
const INSERT_OLD_TENANT: &str =
    "INSERT INTO identity.tenants (tenant_id, name) VALUES ($1::uuid, $2)";
/// A tenant as it is after the migration ran on a database that had it.
const INSERT_PREDATING_TENANT: &str = "INSERT INTO identity.tenants (tenant_id, name, predates_entitlements) VALUES ($1::uuid, $2, true)";
const PREDATES: &str =
    "SELECT predates_entitlements FROM identity.tenants WHERE tenant_id = $1::uuid";
/// The migration that adds entitlements; [`the_migration_marks_the_tenants_it_finds`] runs its
/// statements on `predates_entitlements` again.
const MIGRATION: &str = include_str!("../migrations/postgres/0010_entitlements.sql");
const DROP_PREDATES: &str = "ALTER TABLE identity.tenants DROP COLUMN predates_entitlements";

/// The variable that names the plan of tenants predating entitlements.
const BACKFILL_PLAN: &str = "TRUSTIOT_IDENTITY_BACKFILL_PLAN";
/// The log lines of the backfill.
const GAVE_FIRST: &str = "gave a tenant its first entitlements";
const BACKFILL_PLAN_LOG: &str =
    "tenants that existed before entitlements get this plan as their first snapshot";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const MEMBERS_OF: &str = "SELECT count(*) FROM identity.memberships WHERE tenant_id = $1::uuid";

/// Every tenant's entitlement columns, so a snapshot shows any write.
const ENTITLEMENTS: &str = "SELECT tenant_id::text, plan, entitlement_overrides::text,
    entitlements::text, entitlements_version, entitlements_changed_at::text, predates_entitlements
FROM identity.tenants ORDER BY tenant_id";
const OUTBOX_COUNT: &str = "SELECT count(*) FROM identity.outbox_event";
const VERSION_OF: &str =
    "SELECT entitlements_version FROM identity.tenants WHERE tenant_id = $1::uuid";
/// The entitlement events of one tenant, in order: sequence, envelope tenant, actor, payload.
const EVENTS: &str = "SELECT aggregate_seq, tenant_id::text, (envelope->'actor')::text,
    (envelope->'payload')::text
FROM identity.outbox_event
WHERE event_type = 'identity.tenant_entitlements_changed'
    AND aggregate_type = 'tenant_entitlements' AND aggregate_id = $1
ORDER BY aggregate_seq";
/// A time a superuser tries to write; the trigger keeps the tenant's own.
const FORGE_CHANGED_AT: &str = "UPDATE identity.tenants
SET entitlements_changed_at = '2000-01-01T00:00:00Z' WHERE tenant_id = $1::uuid";

struct Caller {
    id: UserId,
    email: String,
}

async fn caller(admin: &mut PgConnection, platform_admin: bool) -> Caller {
    let id = UserId::generate();
    let email = format!("user-{id}@example.test");
    sqlx::query(INSERT_USER)
        .bind(id.to_string())
        .bind(platform_admin)
        .bind(&email)
        .execute(admin)
        .await
        .expect("insert the user");
    Caller { id, email }
}

/// A token of `user` for the identity service.
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

fn grant(tenant: &str, role: TenantRole) -> Option<TenantGrant> {
    Some(TenantGrant {
        id: tenant.parse().expect("tenant id"),
        role,
    })
}

fn path(tenant: &str) -> String {
    format!("/api/identity/v1/platform/tenants/{tenant}/entitlements")
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

fn get_path(service: &Service, token: Option<&str>, path: &str) -> support::Response {
    let mut headers = Vec::new();
    if let Some(token) = token {
        headers.push(("x-internal-identity", token));
    }
    checked(request(service.address, "GET", path, &headers).expect("call the API"))
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
            &path(tenant),
            &headers,
            body.as_bytes(),
        )
        .expect("call the API"),
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

fn create_tenant(service: &Service, token: &str, name: &str) -> String {
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
    response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned()
}

fn defaults(plan: Plan) -> Value {
    serde_json::to_value(plan.defaults()).expect("serialize")
}

/// The `plan`, `overrides`, `entitlements` and `entitlements_version` of a platform answer.
fn summary(response: &support::Response) -> Value {
    assert_eq!(response.status, 200, "{}", response.body);
    let body = response.json();
    json!({
        "plan": body["plan"],
        "overrides": body["overrides"],
        "entitlements": body["entitlements"],
        "entitlements_version": body["entitlements_version"],
    })
}

fn expected(plan: &str, overrides: Value, entitlements: Value, version: i64) -> Value {
    json!({
        "plan": plan, "overrides": overrides, "entitlements": entitlements,
        "entitlements_version": version,
    })
}

/// A row of `ENTITLEMENTS`.
type Row = (
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<String>,
    bool,
);

/// What a refused request must leave as it was.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    rows: Vec<Row>,
    events: i64,
}

async fn snapshot(admin: &mut PgConnection) -> Snapshot {
    Snapshot {
        rows: sqlx::query_as(ENTITLEMENTS)
            .fetch_all(&mut *admin)
            .await
            .expect("entitlements"),
        events: sqlx::query_scalar(OUTBOX_COUNT)
            .fetch_one(&mut *admin)
            .await
            .expect("events"),
    }
}

async fn events(admin: &mut PgConnection, tenant: &str) -> Vec<(i64, String, Value, Value)> {
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(EVENTS)
        .bind(tenant)
        .fetch_all(admin)
        .await
        .expect("entitlement events");
    rows.into_iter()
        .map(|(seq, tenant, actor, payload)| {
            (
                seq,
                tenant,
                serde_json::from_str(&actor).expect("actor JSON"),
                serde_json::from_str(&payload).expect("payload JSON"),
            )
        })
        .collect()
}

async fn members_of(admin: &mut PgConnection, tenant: &str) -> i64 {
    sqlx::query_scalar(MEMBERS_OF)
        .bind(tenant)
        .fetch_one(admin)
        .await
        .expect("count the members")
}

/// Waits until each of `tenants` has its first snapshot.
async fn backfilled(admin: &mut PgConnection, tenants: &[&str]) {
    let deadline = Instant::now() + BACKFILL_LIMIT;
    for tenant in tenants {
        while version_of(admin, tenant).await == 0 {
            assert!(
                Instant::now() < deadline,
                "tenant {tenant} got no entitlements within {BACKFILL_LIMIT:?} of the start"
            );
            tokio::time::sleep(POLL_EVERY).await;
        }
    }
}

/// The `fields` of the JSON log lines in `output` whose message is `message`.
fn logged(output: &str, message: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["message"] == message)
        .map(|line| line["fields"].clone())
        .collect()
}

/// The plan and its source of each startup line naming the backfill's plan.
fn backfill_plan_logged(output: &str) -> Vec<(Value, Value)> {
    logged(output, BACKFILL_PLAN_LOG)
        .into_iter()
        .map(|fields| (fields["plan"].clone(), fields["plan_source"].clone()))
        .collect()
}

/// The backfill's log line of `tenant`: its plan and where the plan came from.
fn backfill_logged(output: &str, tenant: &str) -> (Value, Value) {
    let lines: Vec<Value> = logged(output, GAVE_FIRST)
        .into_iter()
        .filter(|fields| fields["tenant_id"] == tenant)
        .collect();
    assert_eq!(lines.len(), 1, "{tenant}: {output}");
    (lines[0]["plan"].clone(), lines[0]["plan_source"].clone())
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
async fn tenants_entitlements_are_versioned_delivered_and_checked() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let mut admin = product.superuser().await;

    // 0. The migration marks exactly the tenants it finds as predating entitlements.
    the_migration_marks_the_tenants_it_finds(&mut admin).await;

    // Before the service starts, at version 0 with no snapshot: a tenant that existed when the
    // migration ran, and one an older replica created during the rollout (the two columns of
    // its INSERT, so the column's default).
    let old_tenant = TenantId::generate().to_string();
    sqlx::query(INSERT_PREDATING_TENANT)
        .bind(&old_tenant)
        .bind("Plant before entitlements")
        .execute(&mut admin)
        .await
        .expect("insert the old tenant");
    let rollout_tenant = TenantId::generate().to_string();
    sqlx::query(INSERT_OLD_TENANT)
        .bind(&rollout_tenant)
        .bind("Plant of an older replica")
        .execute(&mut admin)
        .await
        .expect("insert the rollout's tenant");
    for (tenant, predates) in [(&old_tenant, true), (&rollout_tenant, false)] {
        assert_eq!(version_of(&mut admin, tenant).await, 0);
        let marked: bool = sqlx::query_scalar(PREDATES)
            .bind(tenant)
            .fetch_one(&mut admin)
            .await
            .expect("the mark");
        assert_eq!(marked, predates, "{tenant}");
    }

    // `TRUSTIOT_IDENTITY_BACKFILL_PLAN` not set: Enterprise.
    let service = start_on_product(&product, &issuer, "entitlements");
    let platform = caller(&mut admin, true).await;
    let platform_token = token(&issuer, platform.id, true, None, None);
    let platform_actor = json!({"kind": "user", "user_id": platform.id.to_string()});
    let service_actor = json!({"kind": "service", "service": "identity"});
    let first = |plan: &str, entitlements: Value| json!({"plan": plan, "entitlements": entitlements, "entitlements_version": 1});

    // 1. Both get their first snapshot at the service's start, with an event whose actor is the
    //    service: the old tenant Enterprise, the rollout's tenant Starter like any new tenant.
    backfilled(&mut admin, &[&old_tenant, &rollout_tenant]).await;
    for (tenant, plan, values) in [
        (&old_tenant, "enterprise", defaults(Plan::Enterprise)),
        (&rollout_tenant, "starter", defaults(Plan::Starter)),
    ] {
        let response = get_path(&service, Some(&platform_token), &path(tenant));
        assert_eq!(
            summary(&response),
            expected(plan, json!({}), values.clone(), 1),
            "{tenant}"
        );
        assert!(response.json()["entitlements_changed_at"].is_string());
        assert_eq!(
            events(&mut admin, tenant).await,
            [(
                1,
                tenant.clone(),
                service_actor.clone(),
                first(plan, values)
            )]
        );
    }
    assert_eq!(defaults(Plan::Enterprise)["max_gateways"], 20);
    assert_eq!(defaults(Plan::Enterprise)["max_members"], 100);
    assert_eq!(defaults(Plan::Enterprise)["raw_queryable_days"], 180);
    assert_eq!(defaults(Plan::Enterprise)["max_alert_rules"], "unlimited");

    // A tenant that predates entitlements but that the start's pass did not see (it comes
    // after the pass; the next is `BACKFILL_PERIOD` later): both APIs show at version 0 the plan
    // and values the backfill will give it.
    let pending_tenant = TenantId::generate().to_string();
    sqlx::query(INSERT_PREDATING_TENANT)
        .bind(&pending_tenant)
        .bind("Plant still without entitlements")
        .execute(&mut admin)
        .await
        .expect("insert the pending tenant");
    let response = get_path(&service, Some(&platform_token), &path(&pending_tenant));
    assert_eq!(
        summary(&response),
        expected("enterprise", json!({}), defaults(Plan::Enterprise), 0)
    );
    assert!(response.json()["entitlements_changed_at"].is_null());
    let pending_viewer = caller(&mut admin, false).await;
    let pending_viewer_token = token(
        &issuer,
        pending_viewer.id,
        false,
        grant(&pending_tenant, TenantRole::Viewer),
        None,
    );
    let response = get_path(&service, Some(&pending_viewer_token), OWN);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.json(),
        json!({
            "tenant_id": pending_tenant, "plan": "enterprise",
            "entitlements": defaults(Plan::Enterprise), "entitlements_version": 0,
        })
    );

    // 2. A new tenant: Starter at version 1, for the back office and for its members.
    let tenant_a = create_tenant(&service, &platform_token, "Entitlements plant A");
    let tenant_b = create_tenant(&service, &platform_token, "Entitlements plant B");
    let response = get_path(&service, Some(&platform_token), &path(&tenant_a));
    assert_eq!(
        summary(&response),
        expected("starter", json!({}), defaults(Plan::Starter), 1)
    );
    assert_eq!(response.json()["tenant_id"], tenant_a.as_str());
    let viewer_a = caller(&mut admin, false).await;
    let viewer_a_token = token(
        &issuer,
        viewer_a.id,
        false,
        grant(&tenant_a, TenantRole::Viewer),
        None,
    );
    let own = |token: &str| {
        let response = get_path(&service, Some(token), OWN);
        assert_eq!(response.status, 200, "{}", response.body);
        response.json()
    };
    assert_eq!(
        own(&viewer_a_token),
        json!({
            "tenant_id": tenant_a, "plan": "starter", "entitlements": defaults(Plan::Starter),
            "entitlements_version": 1,
        })
    );

    // 3. Upgrade to Business: version 2 and one event with the whole snapshot.
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"plan": "business", "expected_version": 1}),
    );
    assert_eq!(
        summary(&response),
        expected("business", json!({}), defaults(Plan::Business), 2)
    );
    // The same again (with `overrides` null or empty): nothing written.
    let before = snapshot(&mut admin).await;
    for body in [
        json!({"plan": "business", "expected_version": 2}),
        json!({"plan": "business", "overrides": null, "expected_version": 2}),
        json!({"plan": "business", "overrides": {}, "expected_version": 2}),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_eq!(
            summary(&response),
            expected("business", json!({}), defaults(Plan::Business), 2),
            "{body}"
        );
    }
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "an unchanged plan writes nothing"
    );

    // 4. Enterprise with contract overrides: they take precedence over the plan's values.
    let overrides = json!({"max_gateways": 50, "max_members": "unlimited", "sso": false});
    let mut enterprise = defaults(Plan::Enterprise);
    enterprise["max_gateways"] = json!(50);
    enterprise["max_members"] = json!("unlimited");
    enterprise["sso"] = json!(false);
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"plan": "enterprise", "overrides": overrides, "expected_version": 2}),
    );
    assert_eq!(
        summary(&response),
        expected("enterprise", overrides.clone(), enterprise.clone(), 3)
    );
    assert_eq!(
        own(&viewer_a_token)["entitlements"],
        enterprise,
        "the members see the overridden values"
    );
    // The overrides removed: the plan's values again.
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"plan": "enterprise", "expected_version": 3}),
    );
    assert_eq!(
        summary(&response),
        expected("enterprise", json!({}), defaults(Plan::Enterprise), 4)
    );
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"plan": "enterprise", "overrides": overrides, "expected_version": 4}),
    );
    assert_eq!(
        summary(&response),
        expected("enterprise", overrides.clone(), enterprise.clone(), 5)
    );
    // A second administrator who also read version 4 sends other overrides: refused, so the
    // first one's overrides are not silently replaced. A stale request that would change
    // nothing is refused too.
    let before_stale = snapshot(&mut admin).await;
    for body in [
        json!({"plan": "enterprise", "overrides": {"sso": false}, "expected_version": 4}),
        json!({"plan": "enterprise", "overrides": overrides, "expected_version": 4}),
        json!({"plan": "enterprise", "overrides": overrides, "expected_version": 6}),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_eq!(response.status, 409, "{body}: {}", response.body);
        assert_eq!(
            response.json(),
            json!({
                "code": "conflict", "message_key": "entitlements.version_conflict",
                "details": {"current_version": 5},
            }),
            "{body}"
        );
    }
    assert_eq!(
        snapshot(&mut admin).await,
        before_stale,
        "a stale change writes nothing"
    );
    let payload = |plan: &str, entitlements: &Value, version: i64| json!({"plan": plan, "entitlements": entitlements, "entitlements_version": version});
    let expected_events: Vec<(i64, String, Value, Value)> = [
        payload("starter", &defaults(Plan::Starter), 1),
        payload("business", &defaults(Plan::Business), 2),
        payload("enterprise", &enterprise, 3),
        payload("enterprise", &defaults(Plan::Enterprise), 4),
        payload("enterprise", &enterprise, 5),
    ]
    .into_iter()
    .zip(1..)
    .map(|(payload, seq)| (seq, tenant_a.clone(), platform_actor.clone(), payload))
    .collect();
    assert_eq!(events(&mut admin, &tenant_a).await, expected_events);

    // 5. A downgrade below what the tenant has blocks no write and deletes nothing: tenant A has
    //    five members, more than Starter's limit; after the downgrade they all stay, and a
    //    member's role can still be changed.
    let mut members = Vec::new();
    for _ in 0..5 {
        let member = caller(&mut admin, false).await;
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(&tenant_a)
            .bind(member.id.to_string())
            .bind("viewer")
            .execute(&mut admin)
            .await
            .expect("insert the membership");
        members.push(member);
    }
    assert_eq!(members_of(&mut admin, &tenant_a).await, 5);
    let response = put(
        &service,
        &platform_token,
        &tenant_a,
        &json!({"plan": "starter", "expected_version": 5}),
    );
    assert_eq!(
        summary(&response),
        expected("starter", json!({}), defaults(Plan::Starter), 6)
    );
    assert_eq!(defaults(Plan::Starter)["max_members"], 3);
    assert_eq!(
        members_of(&mut admin, &tenant_a).await,
        5,
        "a downgrade deletes no member"
    );
    let response = request_with_body(
        service.address,
        "PUT",
        &format!("/api/identity/v1/platform/tenants/{tenant_a}/members"),
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", &platform_token),
        ],
        json!({"email": members[0].email, "role": "editor"})
            .to_string()
            .as_bytes(),
    )
    .expect("call the member API");
    assert_eq!(
        response.status, 200,
        "a member's role changes after a downgrade: {}",
        response.body
    );
    assert_eq!(members_of(&mut admin, &tenant_a).await, 5);

    // 6. Members read only their own tenant: B's viewer sees B, a service acting for A's viewer
    //    sees A.
    let viewer_b = caller(&mut admin, false).await;
    let viewer_b_token = token(
        &issuer,
        viewer_b.id,
        false,
        grant(&tenant_b, TenantRole::Viewer),
        None,
    );
    let b = own(&viewer_b_token);
    assert_eq!(b["tenant_id"], tenant_b.as_str());
    assert_eq!(b["entitlements_version"], 1);
    let exchanged = token(
        &issuer,
        viewer_a.id,
        false,
        grant(&tenant_a, TenantRole::Viewer),
        Some(ServiceName::Assets),
    );
    let a = own(&exchanged);
    assert_eq!(a["tenant_id"], tenant_a.as_str());
    assert_eq!(a["entitlements_version"], 6);

    // 7. Concurrent identical upgrades of B from version 1: exactly one new version and one
    //    event; the others find version 2 and are refused.
    let before_race = events(&mut admin, &tenant_b).await.len();
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (address, token, tenant) =
                (service.address, platform_token.clone(), tenant_b.clone());
            std::thread::spawn(move || {
                request_with_body(
                    address,
                    "PUT",
                    &path(&tenant),
                    &[
                        ("Content-Type", "application/json"),
                        ("x-internal-identity", &token),
                    ],
                    json!({"plan": "business", "expected_version": 1})
                        .to_string()
                        .as_bytes(),
                )
                .expect("call the API")
            })
        })
        .collect();
    let mut answers: Vec<(u16, Option<i64>)> = racers
        .into_iter()
        .map(|racer| {
            let response = racer.join().expect("the request thread");
            let body = response.json();
            let version = if response.status == 200 {
                body["entitlements_version"].as_i64()
            } else {
                assert_eq!(body["code"], "conflict", "{}", response.body);
                body["details"]["current_version"].as_i64()
            };
            (response.status, version)
        })
        .collect();
    answers.sort_unstable();
    let mut won = vec![(200, Some(2))];
    won.extend(std::iter::repeat_n((409, Some(2)), RACERS - 1));
    assert_eq!(answers, won);
    assert_eq!(events(&mut admin, &tenant_b).await.len(), before_race + 1);

    // 8. Refusals change nothing: who may not, and what is malformed.
    let before = snapshot(&mut admin).await;
    let tenant_admin = caller(&mut admin, false).await;
    let tenant_admin_token = token(
        &issuer,
        tenant_admin.id,
        false,
        grant(&tenant_a, TenantRole::Admin),
        None,
    );
    let exchanged_platform = token(&issuer, platform.id, true, None, Some(ServiceName::Assets));
    let upgrade = json!({"plan": "enterprise", "expected_version": 6});
    for (who, token) in [
        ("a tenant's viewer", &viewer_a_token),
        ("a tenant's administrator", &tenant_admin_token),
        (
            "a platform administrator's exchanged token",
            &exchanged_platform,
        ),
    ] {
        let response = get_path(&service, Some(token), &path(&tenant_a));
        assert_refused(
            &response,
            403,
            "permission_denied",
            &format!("GET by {who}"),
        );
        let response = put(&service, token, &tenant_a, &upgrade);
        assert_refused(
            &response,
            403,
            "permission_denied",
            &format!("PUT by {who}"),
        );
    }
    let response = get_path(&service, None, &path(&tenant_a));
    assert_refused(&response, 401, "unauthenticated", "GET without a token");
    let response = put_as(
        &service,
        None,
        &tenant_a,
        Some("application/json"),
        &upgrade.to_string(),
    );
    assert_refused(&response, 401, "unauthenticated", "PUT without a token");
    let response = get_path(&service, None, OWN);
    assert_refused(&response, 401, "unauthenticated", "own without a token");
    // A platform administrator without a tenant role has no tenant of their own.
    let response = get_path(&service, Some(&platform_token), OWN);
    assert_refused(&response, 403, "permission_denied", "own without a tenant");
    let suspended = {
        let mut claims =
            TestIssuer::claims(ServiceName::Identity, grant(&tenant_a, TenantRole::Admin));
        claims.sub = viewer_a.id;
        claims.tenant_suspended = true;
        issuer.token(&claims)
    };
    let response = get_path(&service, Some(&suspended), OWN);
    assert_refused(
        &response,
        403,
        "tenant_suspended",
        "own in a suspended tenant",
    );
    // A token for another service.
    let mut for_assets = TestIssuer::claims(ServiceName::Assets, None);
    for_assets.sub = platform.id;
    for_assets.platform_admin = true;
    let for_assets = issuer.token(&for_assets);
    let response = put(&service, &for_assets, &tenant_a, &upgrade);
    assert_refused(&response, 401, "unauthenticated", "a token for assets");
    // Unknown and malformed tenants.
    let unknown = TenantId::generate().to_string();
    let response = get_path(&service, Some(&platform_token), &path(&unknown));
    assert_refused(&response, 404, "not_found", "GET an unknown tenant");
    let response = put(&service, &platform_token, &unknown, &upgrade);
    assert_refused(&response, 404, "not_found", "PUT an unknown tenant");
    let response = get_path(&service, Some(&platform_token), &path("not-a-uuid"));
    assert_refused(
        &response,
        400,
        "invalid_request",
        "GET a tenant ID that is not a UUID",
    );
    let response = put(&service, &platform_token, "not-a-uuid", &upgrade);
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
            &tenant_a,
            content_type,
            &upgrade.to_string(),
        );
        assert_refused(
            &response,
            400,
            "invalid_request",
            &format!("{content_type:?}"),
        );
    }
    let oversized = json!({"plan": "enterprise", "padding": "x".repeat(9000)}).to_string();
    let response = put_as(
        &service,
        Some(&platform_token),
        &tenant_a,
        Some("application/json"),
        &oversized,
    );
    assert_refused(&response, 400, "invalid_request", "a body over the limit");
    for body in [
        json!("enterprise"),
        json!({}),
        json!({"plan": 1}),
        json!({"plan": "enterprise", "overrides": [], "expected_version": 6}),
        json!({"plan": "enterprise", "overrides": "none", "expected_version": 6}),
        // No version, or not a whole number of at least 0.
        json!({"plan": "enterprise"}),
        json!({"plan": "enterprise", "expected_version": null}),
        json!({"plan": "enterprise", "expected_version": "6"}),
        json!({"plan": "enterprise", "expected_version": 6.5}),
        json!({"plan": "enterprise", "expected_version": -1}),
        // A misspelt `overrides` must not clear the overrides, nor any other unknown field pass.
        json!({"plan": "enterprise", "override": {"max_gateways": 80}, "expected_version": 6}),
        json!({"plan": "enterprise", "expected_version": 6, "tenant_id": tenant_b}),
    ] {
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_refused(&response, 400, "invalid_request", &body.to_string());
    }
    let plan = ("/plan".to_owned(), "entitlements.plan_invalid".to_owned());
    let invalid = (
        "/overrides".to_owned(),
        "entitlements.overrides_invalid".to_owned(),
    );
    let not_allowed = (
        "/overrides".to_owned(),
        "entitlements.overrides_not_allowed".to_owned(),
    );
    let range = (
        "/overrides/raw_queryable_days".to_owned(),
        "entitlements.override_out_of_range".to_owned(),
    );
    for (body, expected) in [
        (json!({"plan": "free"}), vec![plan.clone()]),
        (
            json!({"plan": "platinum", "overrides": {"bogus": 1}}),
            vec![plan.clone(), invalid.clone()],
        ),
        (
            json!({"plan": "enterprise", "overrides": {"max_gateways": -1}}),
            vec![invalid.clone()],
        ),
        (
            json!({"plan": "business", "overrides": {"max_gateways": 50}}),
            vec![not_allowed.clone()],
        ),
        (
            json!({"plan": "enterprise", "overrides": {"raw_queryable_days": 0}}),
            vec![range.clone()],
        ),
    ] {
        let mut body = body;
        body["expected_version"] = json!(6);
        let response = put(&service, &platform_token, &tenant_a, &body);
        assert_eq!(field_errors(&response), expected, "{body}");
    }
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "a refusal changes nothing"
    );

    // 9. The database's own rules: no writer, a superuser included, chooses the time of a
    //    snapshot; then the service role's grants, the trigger and the checks.
    sqlx::query(FORGE_CHANGED_AT)
        .bind(&tenant_a)
        .execute(&mut admin)
        .await
        .expect("the superuser's update runs");
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "the trigger kept the time of the snapshot"
    );
    grants_and_trigger(&product, &tenant_a, &tenant_b).await;
    assert_eq!(
        snapshot(&mut admin).await,
        before,
        "the probes were rolled back"
    );

    // 10. The log names the changes, and for each first snapshot its plan and where the plan
    //     came from.
    let output = service.stop();
    assert!(
        output.contains("changed a tenant's entitlements"),
        "{output}"
    );
    assert_eq!(
        backfill_plan_logged(&output),
        [(json!("enterprise"), json!("default"))],
        "{output}"
    );
    assert_eq!(
        backfill_logged(&output, &old_tenant),
        (json!("enterprise"), json!("default"))
    );
    assert_eq!(
        backfill_logged(&output, &rollout_tenant),
        (json!("starter"), json!("created_during_rollout"))
    );
    assert_eq!(version_of(&mut admin, &pending_tenant).await, 0);

    // 11. With `TRUSTIOT_IDENTITY_BACKFILL_PLAN=starter`: the pending tenant gets Starter, and a
    //     new tenant starts on Starter as always.
    let service = start_on_product_with(
        &product,
        &issuer,
        "entitlements-starter",
        &[(BACKFILL_PLAN, "starter")],
    );
    backfilled(&mut admin, &[&pending_tenant]).await;
    let response = get_path(&service, Some(&platform_token), &path(&pending_tenant));
    assert_eq!(
        summary(&response),
        expected("starter", json!({}), defaults(Plan::Starter), 1)
    );
    assert_eq!(
        events(&mut admin, &pending_tenant).await,
        [(
            1,
            pending_tenant.clone(),
            service_actor.clone(),
            first("starter", defaults(Plan::Starter)),
        )]
    );
    let tenant_c = create_tenant(&service, &platform_token, "Entitlements plant C");
    let response = get_path(&service, Some(&platform_token), &path(&tenant_c));
    assert_eq!(
        summary(&response),
        expected("starter", json!({}), defaults(Plan::Starter), 1)
    );
    let output = service.stop();
    assert_eq!(
        backfill_plan_logged(&output),
        [(json!("starter"), json!("setting"))],
        "{output}"
    );
    assert_eq!(
        backfill_logged(&output, &pending_tenant),
        (json!("starter"), json!("setting"))
    );
}

/// Runs the migration's statements on `predates_entitlements` again, on a table that has a
/// tenant, in a transaction rolled back: the tenant is marked, one inserted afterwards is not.
async fn the_migration_marks_the_tenants_it_finds(admin: &mut PgConnection) {
    let statements: Vec<&'static str> = MIGRATION
        .lines()
        .filter(|line| {
            line.starts_with("ALTER TABLE identity.tenants")
                && line.contains("predates_entitlements")
        })
        .collect();
    assert_eq!(statements.len(), 2, "the migration's two statements");
    let mut tx = admin.begin().await.expect("a transaction");
    sqlx::query(DROP_PREDATES)
        .execute(&mut *tx)
        .await
        .expect("drop the column");
    let existing = TenantId::generate().to_string();
    sqlx::query(INSERT_OLD_TENANT)
        .bind(&existing)
        .bind("Plant before the migration")
        .execute(&mut *tx)
        .await
        .expect("insert a tenant before the migration");
    for statement in statements {
        sqlx::raw_sql(statement)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    let later = TenantId::generate().to_string();
    sqlx::query(INSERT_OLD_TENANT)
        .bind(&later)
        .bind("Plant after the migration")
        .execute(&mut *tx)
        .await
        .expect("insert a tenant after the migration");
    for (tenant, predates) in [(&existing, true), (&later, false)] {
        let marked: bool = sqlx::query_scalar(PREDATES)
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await
            .expect("the mark");
        assert_eq!(marked, predates, "{tenant}");
    }
    tx.rollback().await.expect("roll back");
}

/// Statements of the service role in transactions of `tenant_a` (Starter at version 6) and
/// `tenant_b` (Business at version 2), through the kit's `BackgroundTenantDb` as the store opens
/// it; each statement in its own transaction, rolled back.
async fn grants_and_trigger(product: &ProductPostgres, tenant_a: &str, tenant_b: &str) {
    let (_requests, db): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("a verified connection");
    let in_a: TenantId = tenant_a.parse().expect("tenant id");
    let run = |tenant: TenantId, statement: &'static str| {
        let db = db.clone();
        async move {
            let mut tx = db
                .begin(tenant, "test-entitlements")
                .await
                .expect("a transaction");
            let result = sqlx::query(statement)
                .bind(tenant.to_string())
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
    for statement in [
        "UPDATE identity.tenants SET plan = 'business', entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
        "SELECT plan FROM identity.tenants WHERE tenant_id = $1::uuid FOR NO KEY UPDATE",
    ] {
        assert_eq!(
            run(in_a, statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}")),
            1,
            "{statement}"
        );
    }

    // Refused by the grants: the time of the snapshot is the trigger's, and a new tenant starts
    // on Starter at version 0 whatever its writer wants.
    for statement in [
        "UPDATE identity.tenants SET entitlements_changed_at = now() WHERE tenant_id = $1::uuid",
        "INSERT INTO identity.tenants (tenant_id, name, plan) VALUES ($1::uuid, 'Forged', 'enterprise')",
        "INSERT INTO identity.tenants (tenant_id, name, entitlements_version)
            VALUES ($1::uuid, 'Forged', 9)",
        "UPDATE identity.tenants SET predates_entitlements = true WHERE tenant_id = $1::uuid",
        "INSERT INTO identity.tenants (tenant_id, name, predates_entitlements)
            VALUES ($1::uuid, 'Forged', true)",
    ] {
        let error = run(in_a, statement).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }

    // Refused by the trigger: a change with the same version, one that skips a version, a new
    // version without a change, an older version.
    for statement in [
        "UPDATE identity.tenants SET plan = 'business' WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET plan = 'business', entitlements_version = entitlements_version + 2
        WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET entitlements_version = 1 WHERE tenant_id = $1::uuid",
    ] {
        let error = run(in_a, statement).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }

    // Refused by the checks: overrides on a plan that is not an Enterprise plan, an unknown
    // plan, a snapshot taken away, overrides that are not an object.
    for statement in [
        r#"UPDATE identity.tenants SET entitlement_overrides = '{"max_gateways": 9}',
            entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid"#,
        "UPDATE identity.tenants SET plan = 'platinum', entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET entitlements = NULL, entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
        "UPDATE identity.tenants SET plan = 'enterprise', entitlement_overrides = '[]',
            entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
    ] {
        let error = run(in_a, statement).await.expect_err(statement);
        assert_eq!(
            sqlstate(error).as_deref(),
            Some(CHECK_VIOLATION),
            "{statement}"
        );
    }

    // Another tenant's row is invisible, so not changed.
    let mut tx = db
        .begin(in_a, "test-entitlements")
        .await
        .expect("a transaction");
    let changed = sqlx::query(
        "UPDATE identity.tenants SET plan = 'enterprise_on_prem',
            entitlements_version = entitlements_version + 1
        WHERE tenant_id = $1::uuid",
    )
    .bind(tenant_b)
    .execute(&mut *tx)
    .await
    .expect("the update runs")
    .rows_affected();
    assert_eq!(changed, 0, "another tenant's entitlements were changed");
    let seen: Option<String> =
        sqlx::query_scalar("SELECT plan FROM identity.tenants WHERE tenant_id = $1::uuid")
            .bind(tenant_b)
            .fetch_optional(&mut *tx)
            .await
            .expect("the read runs");
    assert_eq!(seen, None, "another tenant's plan was read");
    tx.rollback().await.expect("roll back");
}
